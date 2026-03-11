use std::time::Duration;

use anyhow::{Context, Result};
use futures::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::compat::TokioAsyncReadCompatExt;

use commando_common::auth;
use commando_common::commando_capnp::{authenticator, command_agent};

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
    local.run_until(async {
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
    }).await
}

/// Ping a remote agent. Returns agent metadata.
pub async fn remote_ping(
    host: &str,
    port: u16,
    psk: &str,
    connect_timeout_secs: u64,
) -> Result<RemotePingResult> {
    let local = tokio::task::LocalSet::new();
    local.run_until(async {
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
    }).await
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
