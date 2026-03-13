use std::cell::RefCell;
use std::collections::HashMap;
use std::net::IpAddr;
use std::rc::Rc;
use std::time::Instant;

use anyhow::Result;
use futures::AsyncReadExt;
use tokio_util::compat::TokioAsyncReadCompatExt;

use commando_common::auth;
use commando_common::commando_capnp::{authenticator, command_agent, output_receiver};

use crate::config::AgentConfig;
use crate::process::{self, ExecOpts};

/// Rate-limiting state per peer IP
pub struct RateLimitState {
    pub failures: u32,
    pub last_failure: Instant,
}

pub struct AuthenticatorImpl {
    psk: Vec<u8>,
    nonce: RefCell<Option<[u8; 32]>>,
    config: Rc<AgentConfig>,
    peer_ip: IpAddr,
    rate_limits: Rc<RefCell<HashMap<IpAddr, RateLimitState>>>,
    concurrency_guard: Rc<ConcurrencyGuard>,
    agent_start_time: Instant,
}

/// Tracks concurrent exec calls. ConcurrencyPermit releases on Drop (RAII).
pub struct ConcurrencyGuard {
    active: RefCell<usize>,
    max: usize,
}

/// RAII permit — decrements active count on drop
pub struct ConcurrencyPermit {
    guard: Rc<ConcurrencyGuard>,
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        let mut active = self.guard.active.borrow_mut();
        *active = active.saturating_sub(1);
    }
}

impl ConcurrencyGuard {
    pub fn new(max: usize) -> Self {
        Self {
            active: RefCell::new(0),
            max,
        }
    }

    pub fn try_acquire(self: &Rc<Self>) -> Option<ConcurrencyPermit> {
        let mut active = self.active.borrow_mut();
        if *active >= self.max {
            return None;
        }
        *active += 1;
        Some(ConcurrencyPermit {
            guard: self.clone(),
        })
    }
}

impl AuthenticatorImpl {
    pub fn new(
        config: Rc<AgentConfig>,
        peer_ip: IpAddr,
        rate_limits: Rc<RefCell<HashMap<IpAddr, RateLimitState>>>,
        concurrency_guard: Rc<ConcurrencyGuard>,
        agent_start_time: Instant,
    ) -> Self {
        Self {
            psk: config.psk.as_bytes().to_vec(),
            nonce: RefCell::new(None),
            config,
            peer_ip,
            rate_limits,
            concurrency_guard,
            agent_start_time,
        }
    }
}

impl authenticator::Server for AuthenticatorImpl {
    async fn challenge(
        self: Rc<Self>,
        _params: authenticator::ChallengeParams,
        mut results: authenticator::ChallengeResults,
    ) -> Result<(), capnp::Error> {
        let nonce = auth::generate_nonce();
        results.get().set_nonce(&nonce);
        *self.nonce.borrow_mut() = Some(nonce);
        Ok(())
    }

    async fn authenticate(
        self: Rc<Self>,
        params: authenticator::AuthenticateParams,
        mut results: authenticator::AuthenticateResults,
    ) -> Result<(), capnp::Error> {
        // Check rate limiting: delay 1s after 3+ failures per IP
        let should_delay = {
            let limits = self.rate_limits.borrow();
            if let Some(state) = limits.get(&self.peer_ip) {
                state.failures >= 3
            } else {
                false
            }
        };

        if should_delay {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }

        let received_hmac = params
            .get()
            .map_err(|e| capnp::Error::failed(format!("failed to get params: {e}")))?
            .get_hmac()
            .map_err(|e| capnp::Error::failed(format!("failed to get hmac: {e}")))?;

        let nonce = self.nonce.borrow_mut().take().ok_or_else(|| {
            capnp::Error::failed("no challenge nonce — call challenge() first".to_string())
        })?;

        let valid = auth::verify_hmac(&self.psk, &nonce, received_hmac);

        if !valid {
            // Record failure
            let mut limits = self.rate_limits.borrow_mut();
            let state = limits
                .entry(self.peer_ip)
                .or_insert_with(|| RateLimitState {
                    failures: 0,
                    last_failure: Instant::now(),
                });
            state.failures += 1;
            state.last_failure = Instant::now();
            tracing::warn!(peer = %self.peer_ip, "authentication failed");
            return Err(capnp::Error::failed(
                "authentication failed: invalid HMAC".to_string(),
            ));
        }

        // Clear failure count on success
        {
            let mut limits = self.rate_limits.borrow_mut();
            limits.remove(&self.peer_ip);
        }

        let agent_impl = CommandAgentImpl {
            config: self.config.clone(),
            concurrency_guard: self.concurrency_guard.clone(),
            agent_start_time: self.agent_start_time,
        };

        let agent_client: command_agent::Client = capnp_rpc::new_client(agent_impl);
        let mut r = results.get();
        r.set_agent(agent_client);
        r.set_agent_version(env!("CARGO_PKG_VERSION"));

        tracing::info!(peer = %self.peer_ip, "authentication successful");
        Ok(())
    }
}

pub(crate) struct CommandAgentImpl {
    config: Rc<AgentConfig>,
    concurrency_guard: Rc<ConcurrencyGuard>,
    agent_start_time: Instant,
}

impl command_agent::Server for CommandAgentImpl {
    async fn exec(
        self: Rc<Self>,
        params: command_agent::ExecParams,
        mut results: command_agent::ExecResults,
    ) -> Result<(), capnp::Error> {
        let _permit = self
            .concurrency_guard
            .try_acquire()
            .ok_or_else(|| capnp::Error::failed("too many concurrent exec requests".to_string()))?;

        let params_reader = params
            .get()
            .map_err(|e| capnp::Error::failed(format!("failed to get params: {e}")))?;

        let request = params_reader
            .get_request()
            .map_err(|e| capnp::Error::failed(format!("failed to get request: {e}")))?;

        let command = request
            .get_command()
            .map_err(|e| capnp::Error::failed(format!("failed to get command: {e}")))?
            .to_str()
            .map_err(|e| capnp::Error::failed(format!("invalid UTF-8 in command: {e}")))?
            .to_owned();

        let work_dir = request
            .get_work_dir()
            .map_err(|e| capnp::Error::failed(format!("failed to get work_dir: {e}")))?
            .to_str()
            .map_err(|e| capnp::Error::failed(format!("invalid UTF-8 in work_dir: {e}")))?
            .to_owned();

        let timeout_secs = request.get_timeout_secs();

        let request_id = request
            .get_request_id()
            .map_err(|e| capnp::Error::failed(format!("failed to get request_id: {e}")))?
            .to_str()
            .map_err(|e| capnp::Error::failed(format!("invalid UTF-8 in request_id: {e}")))?
            .to_owned();

        let extra_env_list = request
            .get_extra_env()
            .map_err(|e| capnp::Error::failed(format!("failed to get extra_env: {e}")))?;

        let mut extra_env: Vec<(String, String)> = Vec::new();
        for env_var in extra_env_list.iter() {
            let key = env_var
                .get_key()
                .map_err(|e| capnp::Error::failed(format!("failed to get env key: {e}")))?
                .to_str()
                .map_err(|e| capnp::Error::failed(format!("invalid UTF-8 in env key: {e}")))?
                .to_owned();
            let value = env_var
                .get_value()
                .map_err(|e| capnp::Error::failed(format!("failed to get env value: {e}")))?
                .to_str()
                .map_err(|e| capnp::Error::failed(format!("invalid UTF-8 in env value: {e}")))?
                .to_owned();
            extra_env.push((key, value));
        }

        tracing::info!(
            request_id = %request_id,
            command = %command,
            work_dir = %work_dir,
            timeout_secs = timeout_secs,
            "exec request"
        );

        let opts = ExecOpts {
            shell: self.config.shell.clone(),
            max_output_bytes: self.config.max_output_bytes,
            wrapper: self.config.wrapper_binary().map(String::from),
        };

        let exec_result = process::execute(&command, &work_dir, timeout_secs, &extra_env, &opts)
            .await
            .map_err(|e| capnp::Error::failed(format!("exec error: {e}")))?;

        tracing::info!(
            request_id = %request_id,
            exit_code = exec_result.exit_code,
            duration_ms = exec_result.duration_ms,
            timed_out = exec_result.timed_out,
            truncated = exec_result.truncated,
            "command completed"
        );

        let mut r = results.get().init_result();
        r.set_stdout(&exec_result.stdout);
        r.set_stderr(&exec_result.stderr);
        r.set_exit_code(exec_result.exit_code);
        r.set_duration_ms(exec_result.duration_ms);
        r.set_timed_out(exec_result.timed_out);
        r.set_truncated(exec_result.truncated);
        r.set_request_id(&request_id);

        Ok(())
    }

    async fn exec_stream(
        self: Rc<Self>,
        params: command_agent::ExecStreamParams,
        mut results: command_agent::ExecStreamResults,
    ) -> Result<(), capnp::Error> {
        let _permit = self
            .concurrency_guard
            .try_acquire()
            .ok_or_else(|| capnp::Error::failed("too many concurrent exec requests".to_string()))?;

        let params_reader = params
            .get()
            .map_err(|e| capnp::Error::failed(format!("failed to get params: {e}")))?;

        let request = params_reader
            .get_request()
            .map_err(|e| capnp::Error::failed(format!("failed to get request: {e}")))?;

        let command = request
            .get_command()
            .map_err(|e| capnp::Error::failed(format!("failed to get command: {e}")))?
            .to_str()
            .map_err(|e| capnp::Error::failed(format!("invalid UTF-8 in command: {e}")))?
            .to_owned();

        let work_dir = request
            .get_work_dir()
            .map_err(|e| capnp::Error::failed(format!("failed to get work_dir: {e}")))?
            .to_str()
            .map_err(|e| capnp::Error::failed(format!("invalid UTF-8 in work_dir: {e}")))?
            .to_owned();

        let timeout_secs = request.get_timeout_secs();

        let request_id = request
            .get_request_id()
            .map_err(|e| capnp::Error::failed(format!("failed to get request_id: {e}")))?
            .to_str()
            .map_err(|e| capnp::Error::failed(format!("invalid UTF-8 in request_id: {e}")))?
            .to_owned();

        let extra_env_list = request
            .get_extra_env()
            .map_err(|e| capnp::Error::failed(format!("failed to get extra_env: {e}")))?;

        let mut extra_env: Vec<(String, String)> = Vec::new();
        for env_var in extra_env_list.iter() {
            let key = env_var
                .get_key()
                .map_err(|e| capnp::Error::failed(format!("failed to get env key: {e}")))?
                .to_str()
                .map_err(|e| capnp::Error::failed(format!("invalid UTF-8 in env key: {e}")))?
                .to_owned();
            let value = env_var
                .get_value()
                .map_err(|e| capnp::Error::failed(format!("failed to get env value: {e}")))?
                .to_str()
                .map_err(|e| capnp::Error::failed(format!("invalid UTF-8 in env value: {e}")))?
                .to_owned();
            extra_env.push((key, value));
        }

        let receiver: output_receiver::Client = params_reader
            .get_receiver()
            .map_err(|e| capnp::Error::failed(format!("failed to get receiver: {e}")))?;

        tracing::info!(
            request_id = %request_id,
            command = %command,
            work_dir = %work_dir,
            timeout_secs = timeout_secs,
            "exec_stream request"
        );

        let opts = ExecOpts {
            shell: self.config.shell.clone(),
            max_output_bytes: self.config.max_output_bytes,
            wrapper: self.config.wrapper_binary().map(String::from),
        };

        // Collect JoinHandles for all spawned receive calls so we can await
        // them after execute_stream returns (draining in-flight RPC sends).
        let handles: Rc<RefCell<Vec<tokio::task::JoinHandle<()>>>> =
            Rc::new(RefCell::new(Vec::new()));
        let handles_clone = handles.clone();

        let exec_result = process::execute_stream(
            &command,
            &work_dir,
            timeout_secs,
            &extra_env,
            &opts,
            move |data: &[u8], stream: u8| {
                let mut req = receiver.receive_request();
                {
                    let mut p = req.get();
                    p.set_data(data);
                    p.set_stream(stream);
                }
                let send_promise = req.send().promise;
                let handle = tokio::task::spawn_local(async move {
                    let _ = send_promise.await;
                });
                handles_clone.borrow_mut().push(handle);
            },
        )
        .await
        .map_err(|e| capnp::Error::failed(format!("exec_stream error: {e}")))?;

        // Drain all pending receive calls before returning the result.
        let pending: Vec<_> = handles.borrow_mut().drain(..).collect();
        for handle in pending {
            let _ = handle.await;
        }

        tracing::info!(
            request_id = %request_id,
            exit_code = exec_result.exit_code,
            duration_ms = exec_result.duration_ms,
            timed_out = exec_result.timed_out,
            "exec_stream completed"
        );

        let mut r = results.get().init_result();
        r.set_stdout(&[]);
        r.set_stderr(&[]);
        r.set_exit_code(exec_result.exit_code);
        r.set_duration_ms(exec_result.duration_ms);
        r.set_timed_out(exec_result.timed_out);
        r.set_truncated(false);
        r.set_request_id(&request_id);

        Ok(())
    }

    async fn ping(
        self: Rc<Self>,
        _params: command_agent::PingParams,
        mut results: command_agent::PingResults,
    ) -> Result<(), capnp::Error> {
        let hostname = gethostname::gethostname();
        let hostname_str = hostname.to_string_lossy();
        let uptime_secs = self.agent_start_time.elapsed().as_secs();

        let mut pong = results.get().init_pong();
        pong.set_hostname(&hostname_str);
        pong.set_uptime_secs(uptime_secs);
        pong.set_shell(&self.config.shell);
        pong.set_version(env!("CARGO_PKG_VERSION"));

        Ok(())
    }
}

/// Handle a single incoming connection: set up Cap'n Proto RPC with the authenticator.
pub async fn handle_connection(
    stream: tokio::net::TcpStream,
    peer_ip: IpAddr,
    config: Rc<AgentConfig>,
    rate_limits: Rc<RefCell<HashMap<IpAddr, RateLimitState>>>,
    concurrency_guard: Rc<ConcurrencyGuard>,
    agent_start_time: Instant,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let stream = stream.compat();
    let (reader, writer) = stream.split();

    let network = capnp_rpc::twoparty::VatNetwork::new(
        futures::io::BufReader::new(reader),
        futures::io::BufWriter::new(writer),
        capnp_rpc::rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );

    let authenticator = AuthenticatorImpl::new(
        config,
        peer_ip,
        rate_limits,
        concurrency_guard,
        agent_start_time,
    );
    let auth_client: authenticator::Client = capnp_rpc::new_client(authenticator);

    let rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), Some(auth_client.client));

    rpc_system.await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_guard_acquire_and_release() {
        let guard = Rc::new(ConcurrencyGuard::new(2));
        let p1 = guard.try_acquire();
        assert!(p1.is_some());
        let p2 = guard.try_acquire();
        assert!(p2.is_some());
        // At max — should fail
        assert!(guard.try_acquire().is_none());
        // Drop one permit
        drop(p1);
        // Now should succeed
        assert!(guard.try_acquire().is_some());
    }

    #[test]
    fn concurrency_guard_raii_drop() {
        let guard = Rc::new(ConcurrencyGuard::new(1));
        {
            let _permit = guard.try_acquire().unwrap();
            assert!(guard.try_acquire().is_none());
        }
        // Permit dropped — slot freed
        assert!(guard.try_acquire().is_some());
    }

    #[test]
    fn concurrency_guard_zero_max() {
        let guard = Rc::new(ConcurrencyGuard::new(0));
        assert!(guard.try_acquire().is_none());
    }
}
