use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{error, info};

use crate::audit::ExecAudit;
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

/// Idle time before the kernel probes a silent agent connection.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(60);
/// Interval between keepalive probes.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// Upper bound for the graceful RPC disconnect after a successful call.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Slack added to the command timeout for the `exec_stream` call deadline.
const EXEC_GRACE: Duration = Duration::from_secs(30);
/// Floor for the authentication deadline: the agent sleeps 1 s after three
/// failed attempts from one IP, so a 1 s limit would misreport that as a timeout.
const AUTH_MIN: Duration = Duration::from_secs(2);

type RpcDisconnector = capnp_rpc::Disconnector<capnp_rpc::rpc_twoparty_capnp::Side>;

/// Aborts the RpcSystem task on drop, which closes the TCP connection.
struct AbortOnDrop(tokio::task::JoinHandle<Result<(), capnp::Error>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Connect to the agent with a deadline, TCP_NODELAY and keepalive.
async fn connect_stream(host: &str, port: u16, connect_timeout_secs: u64) -> Result<TcpStream> {
    let addr = format!("{host}:{port}");
    let stream = timeout(
        Duration::from_secs(connect_timeout_secs),
        TcpStream::connect(&addr),
    )
    .await
    .map_err(|_| anyhow!("connect to {addr} timed out after {connect_timeout_secs}s"))?
    .map_err(|e| anyhow!("connect to {addr} failed: {e}"))?;

    stream.set_nodelay(true)?;
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    if let Err(e) = socket2::SockRef::from(&stream).set_tcp_keepalive(&keepalive) {
        tracing::warn!(addr = %addr, error = %e, "failed to set TCP keepalive");
    }
    Ok(stream)
}

/// Start the RpcSystem on `stream`; the task lives as long as the returned guard.
fn start_rpc(stream: TcpStream) -> (authenticator::Client, RpcDisconnector, AbortOnDrop) {
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
    let task = AbortOnDrop(tokio::task::spawn_local(rpc_system));
    (auth_client, disconnector, task)
}

/// The agent's message without capnp's "Failed: " and "remote exception: " prefixes.
/// A kind other than `Failed` (for example `Disconnected`) stays in the text.
fn rpc_message(e: &capnp::Error) -> String {
    let mut msg = e.extra.as_str();
    loop {
        let stripped = msg
            .strip_prefix("Failed: ")
            .or_else(|| msg.strip_prefix("remote exception: "));
        match stripped {
            Some(rest) => msg = rest,
            None => break,
        }
    }
    if msg.is_empty() {
        e.to_string()
    } else if e.kind == capnp::ErrorKind::Failed {
        msg.to_string()
    } else {
        format!("{:?}: {msg}", e.kind)
    }
}

/// Error text for a failed `step`, without a doubled step name.
fn step_error(step: &str, e: &capnp::Error) -> anyhow::Error {
    let msg = rpc_message(e);
    if msg.starts_with(step) {
        anyhow!(msg)
    } else {
        anyhow!("{step} failed: {msg}")
    }
}

/// Deadline for the authentication exchange.
fn auth_deadline(connect_timeout_secs: u64) -> Duration {
    Duration::from_secs(connect_timeout_secs).max(AUTH_MIN)
}

/// Deadline for the `exec_stream` call. The agent maps a timeout of 0 to 60 s.
fn exec_deadline(timeout_secs: u32) -> Duration {
    let secs = if timeout_secs == 0 { 60 } else { timeout_secs };
    Duration::from_secs(u64::from(secs)) + EXEC_GRACE
}

/// Run `fut` under a deadline; the error names the step and the limit.
async fn within<T>(
    step: &str,
    limit: Duration,
    fut: impl std::future::Future<Output = Result<T, capnp::Error>>,
) -> Result<T> {
    match timeout(limit, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(step_error(step, &e)),
        Err(_) => Err(anyhow!("{step} timed out after {}s", limit.as_secs())),
    }
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
            let stream = connect_stream(host, port, connect_timeout_secs).await?;
            let (auth_client, disconnector, _rpc_task) = start_rpc(stream);

            let agent_client =
                authenticate(&auth_client, psk.as_bytes(), connect_timeout_secs).await?;

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
    let deadline = Duration::from_secs(connect_timeout_secs)
        .saturating_add(auth_deadline(connect_timeout_secs));
    let local = tokio::task::LocalSet::new();
    let run = local.run_until(async {
        let stream = connect_stream(host, port, connect_timeout_secs).await?;
        let (auth_client, disconnector, _rpc_task) = start_rpc(stream);

        let agent_client = authenticate(&auth_client, psk.as_bytes(), connect_timeout_secs).await?;

        let response = within(
            "ping",
            Duration::from_secs(connect_timeout_secs),
            agent_client.ping_request().send().promise,
        )
        .await?;
        let ping_result = (|| -> Result<RemotePingResult, capnp::Error> {
            let pong = response.get()?.get_pong()?;
            Ok(RemotePingResult {
                hostname: pong.get_hostname()?.to_str()?.to_string(),
                uptime_secs: pong.get_uptime_secs(),
                shell: pong.get_shell()?.to_str()?.to_string(),
                version: pong.get_version()?.to_str()?.to_string(),
            })
        })()
        .map_err(|e| step_error("ping result", &e))?;

        drop(agent_client);
        drop(auth_client);
        let _ = timeout(DISCONNECT_TIMEOUT, disconnector).await;

        Ok(ping_result)
    });
    timeout(deadline, run)
        .await
        .map_err(|_| anyhow!("ping timed out after {}s", deadline.as_secs()))?
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
                    0 => session.push_stdout(data),
                    1 => session.push_stderr(data),
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
///
/// Create it at the moment `try_acquire` succeeds so every early return or
/// panic afterwards releases the slot.
pub struct LimiterGuard {
    limiter: Arc<ConcurrencyLimiter>,
    target: String,
}

impl LimiterGuard {
    pub fn new(limiter: Arc<ConcurrencyLimiter>, target: String) -> Self {
        Self { limiter, target }
    }
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
/// running on the main `LocalSet`. The caller passes the `LimiterGuard` for
/// the slot it acquired; the spawned task owns it and releases the slot on drop.
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
    guard: LimiterGuard,
    target_name: String,
    audit: Option<ExecAudit>,
) -> tokio::task::JoinHandle<()> {
    // Clone everything we need into the spawned task.
    let host = host.to_string();
    let psk = psk.to_string();
    let command = command.to_string();
    let work_dir = work_dir.to_string();
    let extra_env = extra_env.to_vec();
    let request_id = request_id.to_string();

    tokio::task::spawn_local(async move {
        let _guard = guard;
        // Dropped without a result (task aborted) it writes the `aborted` end event.
        let mut audit = audit;

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

        // Write the end event before the session reads as completed, so a client
        // that sees the final page can rely on the log being complete.
        if let Some(a) = audit.as_mut() {
            match &result {
                Ok((exit_code, duration_ms, timed_out, _)) => {
                    a.exec_finished(*exit_code, *duration_ms, *timed_out);
                }
                Err(e) => a.rpc_failed(e.to_string()),
            }
        }
        drop(audit);

        // Mark session completed regardless of success/failure.
        // Clone notify before dropping the borrow to avoid holding RefCell across notify_one()
        let notify = {
            let mut map = session_map.borrow_mut();
            if let Some(session) = map.get_by_id_mut(&session_id) {
                match result {
                    Ok((exit_code, duration_ms, timed_out, rpc_profile)) => {
                        session.completed = true;
                        session.exec_result = Some(StreamExecResult {
                            exit_code,
                            duration_ms,
                            timed_out,
                            error: None,
                        });
                        session.rpc_profile = Some(rpc_profile);
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
                            error: Some(e.to_string()),
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
/// Returns `(exit_code, duration_ms, timed_out, rpc_profile)` on success.
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
) -> Result<(i32, u64, bool, crate::session::RpcProfile)> {
    use tokio::time::Instant;

    let t0 = Instant::now();

    let stream = connect_stream(host, port, connect_timeout_secs).await?;
    let t_connected = Instant::now();

    // Dropped on every exit path (and when the task is aborted), which closes the connection.
    let (auth_client, disconnector, rpc_task) = start_rpc(stream);

    // Authenticate
    let agent_client = authenticate(&auth_client, psk.as_bytes(), connect_timeout_secs).await?;
    let t_authed = Instant::now();

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

    let response = within(
        "exec_stream",
        exec_deadline(timeout_secs),
        request.send().promise,
    )
    .await?;
    let (exit_code, duration_ms, timed_out, agent_profile_json) =
        (|| -> Result<_, capnp::Error> {
            let result = response.get()?.get_result()?;
            let agent_profile_json = result
                .get_profiler_json()
                .ok()
                .and_then(|s| s.to_str().ok())
                .unwrap_or("")
                .to_string();
            Ok((
                result.get_exit_code(),
                result.get_duration_ms(),
                result.get_timed_out(),
                agent_profile_json,
            ))
        })()
        .map_err(|e| step_error("exec_stream result", &e))?;
    let t_exec_done = Instant::now();

    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let rpc_profile = crate::session::RpcProfile {
        tcp_connect_ms: ms(t_connected.duration_since(t0)),
        auth_ms: ms(t_authed.duration_since(t_connected)),
        exec_rpc_ms: ms(t_exec_done.duration_since(t_authed)),
        agent_profile_json,
    };

    // Clean up RPC connection — fire-and-forget to avoid blocking on TCP teardown.
    // The disconnector triggers a graceful shutdown that can take 20-30ms on LAN;
    // the task guard moves along so the connection closes even if it stalls.
    drop(agent_client);
    drop(auth_client);
    tokio::task::spawn_local(async move {
        let _rpc_task = rpc_task;
        let _ = timeout(DISCONNECT_TIMEOUT, disconnector).await;
    });

    Ok((exit_code, duration_ms, timed_out, rpc_profile))
}

/// Perform HMAC challenge-response authentication, returns CommandAgent capability.
/// The whole exchange must finish within `connect_timeout_secs` (at least `AUTH_MIN`).
async fn authenticate(
    auth_client: &authenticator::Client,
    psk: &[u8],
    connect_timeout_secs: u64,
) -> Result<command_agent::Client> {
    within(
        "authentication",
        auth_deadline(connect_timeout_secs),
        async {
            // Step 1: Get challenge nonce
            let challenge_response = auth_client.challenge_request().send().promise.await?;
            let nonce = challenge_response.get()?.get_nonce()?;

            // Step 2: Compute HMAC
            let hmac = auth::compute_hmac(psk, nonce);

            // Step 3: Authenticate
            let mut auth_request = auth_client.authenticate_request();
            auth_request.get().set_hmac(&hmac);
            let auth_response = auth_request.send().promise.await?;

            auth_response.get()?.get_agent()
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_deadline_adds_grace() {
        assert_eq!(exec_deadline(10), Duration::from_secs(40));
    }

    #[test]
    fn exec_deadline_maps_zero_to_agent_default() {
        assert_eq!(exec_deadline(0), Duration::from_secs(90));
    }

    #[test]
    fn auth_deadline_has_a_floor() {
        assert_eq!(auth_deadline(1), Duration::from_secs(2));
        assert_eq!(auth_deadline(5), Duration::from_secs(5));
    }

    #[test]
    fn rpc_message_keeps_non_failed_kind() {
        let e = capnp::Error::disconnected("connection reset".into());
        assert_eq!(rpc_message(&e), "Disconnected: connection reset");
    }

    #[test]
    fn rpc_message_strips_capnp_prefixes() {
        let e =
            capnp::Error::failed("remote exception: authentication failed: invalid HMAC".into());
        assert_eq!(rpc_message(&e), "authentication failed: invalid HMAC");
    }
}
