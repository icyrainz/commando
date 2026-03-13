use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{error, info};

use crate::handler::ConcurrencyLimiter;
use crate::session::{SessionMap, StreamExecResult};
use commando_common::auth;
use commando_common::commando_capnp::{authenticator, command_agent, output_receiver};

/// Result of executing a command on a remote agent.
pub struct RemoteExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub truncated: bool,
    pub request_id: String,
}

/// Result of pinging a remote agent.
pub struct RemotePingResult {
    pub hostname: String,
    pub uptime_secs: u64,
    pub shell: String,
    pub version: String,
}

/// Execute a command on a remote agent. Opens a fresh TCP connection,
/// authenticates, runs the command, and disconnects.
#[allow(clippy::too_many_arguments)]
pub async fn remote_exec(
    host: &str,
    port: u16,
    psk: &str,
    command: &str,
    work_dir: &str,
    timeout_secs: u32,
    extra_env: &[(String, String)],
    request_id: &str,
    connect_timeout_secs: u64,
) -> Result<RemoteExecResult> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let addr = format!("{host}:{port}");

            // Connect with timeout
            let stream = timeout(
                Duration::from_secs(connect_timeout_secs),
                TcpStream::connect(&addr),
            )
            .await
            .context("connect timeout")?
            .context("TCP connect failed")?;

            stream.set_nodelay(true)?;
            let stream = stream.compat();
            let (reader, writer) = stream.split();

            let network = capnp_rpc::twoparty::VatNetwork::new(
                futures::io::BufReader::new(reader),
                futures::io::BufWriter::new(writer),
                capnp_rpc::rpc_twoparty_capnp::Side::Client,
                Default::default(),
            );

            let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), None);
            let disconnector = rpc_system.get_disconnector();
            let auth_client: authenticator::Client =
                rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);

            tokio::task::spawn_local(rpc_system);

            // Authenticate
            let agent_client = authenticate(&auth_client, psk.as_bytes()).await?;

            // Build exec request
            let mut request = agent_client.exec_request();
            {
                let mut req_builder = request.get().init_request();
                req_builder.set_command(command);
                req_builder.set_work_dir(work_dir);
                req_builder.set_timeout_secs(timeout_secs);
                req_builder.set_request_id(request_id);

                if !extra_env.is_empty() {
                    let mut env_list = req_builder.init_extra_env(extra_env.len() as u32);
                    for (i, (key, value)) in extra_env.iter().enumerate() {
                        let mut entry = env_list.reborrow().get(i as u32);
                        entry.set_key(key);
                        entry.set_value(value);
                    }
                }
            }

            let response = request.send().promise.await?;
            let result = response.get()?.get_result()?;

            let exec_result = RemoteExecResult {
                stdout: result.get_stdout()?.to_vec(),
                stderr: result.get_stderr()?.to_vec(),
                exit_code: result.get_exit_code(),
                duration_ms: result.get_duration_ms(),
                timed_out: result.get_timed_out(),
                truncated: result.get_truncated(),
                request_id: result.get_request_id()?.to_str()?.to_string(),
            };

            // Clean up
            drop(agent_client);
            drop(auth_client);
            let _ = disconnector.await;

            Ok(exec_result)
        })
        .await
}

/// Ping a remote agent. Returns agent metadata.
pub async fn remote_ping(
    host: &str,
    port: u16,
    psk: &str,
    connect_timeout_secs: u64,
) -> Result<RemotePingResult> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let addr = format!("{host}:{port}");

            let stream = timeout(
                Duration::from_secs(connect_timeout_secs),
                TcpStream::connect(&addr),
            )
            .await
            .context("connect timeout")?
            .context("TCP connect failed")?;

            stream.set_nodelay(true)?;
            let stream = stream.compat();
            let (reader, writer) = stream.split();

            let network = capnp_rpc::twoparty::VatNetwork::new(
                futures::io::BufReader::new(reader),
                futures::io::BufWriter::new(writer),
                capnp_rpc::rpc_twoparty_capnp::Side::Client,
                Default::default(),
            );

            let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), None);
            let disconnector = rpc_system.get_disconnector();
            let auth_client: authenticator::Client =
                rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);

            tokio::task::spawn_local(rpc_system);

            let agent_client = authenticate(&auth_client, psk.as_bytes()).await?;

            let response = agent_client.ping_request().send().promise.await?;
            let pong = response.get()?.get_pong()?;

            let ping_result = RemotePingResult {
                hostname: pong.get_hostname()?.to_str()?.to_string(),
                uptime_secs: pong.get_uptime_secs(),
                shell: pong.get_shell()?.to_str()?.to_string(),
                version: pong.get_version()?.to_str()?.to_string(),
            };

            drop(agent_client);
            drop(auth_client);
            let _ = disconnector.await;

            Ok(ping_result)
        })
        .await
}

/// Cap'n Proto callback that receives streaming output chunks from the agent.
struct OutputReceiverImpl {
    session_map: Rc<RefCell<SessionMap>>,
    session_id: String,
}

impl output_receiver::Server for OutputReceiverImpl {
    async fn receive(
        self: Rc<Self>,
        params: output_receiver::ReceiveParams,
        _results: output_receiver::ReceiveResults,
    ) -> Result<(), capnp::Error> {
        let reader = params
            .get()
            .map_err(|e| capnp::Error::failed(format!("failed to get params: {e}")))?;
        let data = reader
            .get_data()
            .map_err(|e| capnp::Error::failed(format!("failed to get data: {e}")))?;
        let stream = reader.get_stream();

        // Clone notify before dropping the borrow to avoid holding RefCell across notify_one()
        let notify = {
            let mut map = self.session_map.borrow_mut();
            if let Some(session) = map.get_by_id_mut(&self.session_id) {
                match stream {
                    0 => session.stdout_buffer.extend_from_slice(data),
                    1 => session.stderr_buffer.extend_from_slice(data),
                    _ => {} // ignore unknown streams
                }
                Some(session.notify.clone())
            } else {
                None
            }
        };
        if let Some(notify) = notify {
            notify.notify_one();
        }

        Ok(())
    }
}

/// RAII guard that releases a concurrency slot on drop.
struct LimiterGuard {
    limiter: Arc<ConcurrencyLimiter>,
    target: String,
}

impl Drop for LimiterGuard {
    fn drop(&mut self) {
        self.limiter.release(&self.target);
    }
}

/// Spawn a long-lived RPC task that streams output into the session map.
///
/// Unlike `remote_exec()` which creates a throwaway `LocalSet` per request,
/// this function uses `spawn_local` directly — the caller must already be
/// running on the main `LocalSet`. The caller must have already acquired
/// a concurrency slot; the spawned task releases it via the RAII guard.
#[allow(clippy::too_many_arguments)]
pub fn start_remote_exec_stream(
    host: &str,
    port: u16,
    psk: &str,
    command: &str,
    work_dir: &str,
    timeout_secs: u32,
    extra_env: &[(String, String)],
    request_id: &str,
    connect_timeout_secs: u64,
    session_map: Rc<RefCell<SessionMap>>,
    session_id: String,
    limiter: Arc<ConcurrencyLimiter>,
    target_name: String,
) -> tokio::task::JoinHandle<()> {
    // Clone everything we need into the spawned task.
    let host = host.to_string();
    let psk = psk.to_string();
    let command = command.to_string();
    let work_dir = work_dir.to_string();
    let extra_env = extra_env.to_vec();
    let request_id = request_id.to_string();

    tokio::task::spawn_local(async move {
        let _guard = LimiterGuard {
            limiter,
            target: target_name.clone(),
        };

        let result = exec_stream_inner(
            &host,
            port,
            &psk,
            &command,
            &work_dir,
            timeout_secs,
            &extra_env,
            &request_id,
            connect_timeout_secs,
            session_map.clone(),
            session_id.clone(),
        )
        .await;

        // Mark session completed regardless of success/failure.
        // Clone notify before dropping the borrow to avoid holding RefCell across notify_one()
        let notify = {
            let mut map = session_map.borrow_mut();
            if let Some(session) = map.get_by_id_mut(&session_id) {
                match result {
                    Ok((exit_code, duration_ms, timed_out)) => {
                        session.completed = true;
                        session.exec_result = Some(StreamExecResult {
                            exit_code,
                            duration_ms,
                            timed_out,
                        });
                    }
                    Err(e) => {
                        error!(
                            target = %target_name,
                            request_id = %request_id,
                            error = %e,
                            "exec_stream RPC failed"
                        );
                        session.completed = true;
                        session.exec_result = Some(StreamExecResult {
                            exit_code: -1,
                            duration_ms: 0,
                            timed_out: false,
                        });
                    }
                }
                Some(session.notify.clone())
            } else {
                None
            }
        };
        if let Some(notify) = notify {
            notify.notify_one();
        }
    })
}

/// Inner helper: connect, authenticate, send exec_stream request, await result.
/// Returns `(exit_code, duration_ms, timed_out)` on success.
#[allow(clippy::too_many_arguments)]
async fn exec_stream_inner(
    host: &str,
    port: u16,
    psk: &str,
    command: &str,
    work_dir: &str,
    timeout_secs: u32,
    extra_env: &[(String, String)],
    request_id: &str,
    connect_timeout_secs: u64,
    session_map: Rc<RefCell<SessionMap>>,
    session_id: String,
) -> Result<(i32, u64, bool)> {
    let addr = format!("{host}:{port}");

    // Connect with timeout
    let stream = timeout(
        Duration::from_secs(connect_timeout_secs),
        TcpStream::connect(&addr),
    )
    .await
    .context("connect timeout")?
    .context("TCP connect failed")?;

    stream.set_nodelay(true)?;
    let stream = stream.compat();
    let (reader, writer) = stream.split();

    let network = capnp_rpc::twoparty::VatNetwork::new(
        futures::io::BufReader::new(reader),
        futures::io::BufWriter::new(writer),
        capnp_rpc::rpc_twoparty_capnp::Side::Client,
        Default::default(),
    );

    let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), None);
    let disconnector = rpc_system.get_disconnector();
    let auth_client: authenticator::Client =
        rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);

    tokio::task::spawn_local(rpc_system);

    // Authenticate
    let agent_client = authenticate(&auth_client, psk.as_bytes()).await?;

    // Build exec_stream request
    let receiver_impl = OutputReceiverImpl {
        session_map,
        session_id,
    };
    let receiver_client: output_receiver::Client = capnp_rpc::new_client(receiver_impl);

    let mut request = agent_client.exec_stream_request();
    {
        let mut params = request.get();
        let mut req_builder = params.reborrow().init_request();
        req_builder.set_command(command);
        req_builder.set_work_dir(work_dir);
        req_builder.set_timeout_secs(timeout_secs);
        req_builder.set_request_id(request_id);

        if !extra_env.is_empty() {
            let mut env_list = req_builder.init_extra_env(extra_env.len() as u32);
            for (i, (key, value)) in extra_env.iter().enumerate() {
                let mut entry = env_list.reborrow().get(i as u32);
                entry.set_key(key);
                entry.set_value(value);
            }
        }

        params.set_receiver(receiver_client);
    }

    info!(request_id = %request_id, "sending exec_stream request");

    let response = request.send().promise.await?;
    let result = response.get()?.get_result()?;

    let exit_code = result.get_exit_code();
    let duration_ms = result.get_duration_ms();
    let timed_out = result.get_timed_out();

    // Clean up RPC connection
    drop(agent_client);
    drop(auth_client);
    let _ = disconnector.await;

    Ok((exit_code, duration_ms, timed_out))
}

/// Perform HMAC challenge-response authentication, returns CommandAgent capability.
async fn authenticate(
    auth_client: &authenticator::Client,
    psk: &[u8],
) -> Result<command_agent::Client> {
    // Step 1: Get challenge nonce
    let challenge_response = auth_client.challenge_request().send().promise.await?;
    let nonce = challenge_response.get()?.get_nonce()?;

    // Step 2: Compute HMAC
    let hmac = auth::compute_hmac(psk, nonce);

    // Step 3: Authenticate
    let mut auth_request = auth_client.authenticate_request();
    auth_request.get().set_hmac(&hmac);
    let auth_response = auth_request.send().promise.await?;

    let agent_client = auth_response.get()?.get_agent()?;
    Ok(agent_client)
}
