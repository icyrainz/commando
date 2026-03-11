use std::cell::RefCell;
use std::collections::HashMap;
use std::net::IpAddr;
use std::rc::Rc;
use std::time::Instant;

use commando_common::auth;
use commando_common::commando_capnp::{authenticator, command_agent};

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
        Some(ConcurrencyPermit { guard: self.clone() })
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
    fn challenge(
        self: Rc<Self>,
        _params: authenticator::ChallengeParams,
        mut results: authenticator::ChallengeResults,
    ) -> impl ::core::future::Future<Output = Result<(), capnp::Error>> + 'static {
        async move {
            let nonce = auth::generate_nonce();
            results.get().set_nonce(&nonce);
            *self.nonce.borrow_mut() = Some(nonce);
            Ok(())
        }
    }

    fn authenticate(
        self: Rc<Self>,
        params: authenticator::AuthenticateParams,
        mut results: authenticator::AuthenticateResults,
    ) -> impl ::core::future::Future<Output = Result<(), capnp::Error>> + 'static {
        async move {
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
                let state = limits.entry(self.peer_ip).or_insert_with(|| RateLimitState {
                    failures: 0,
                    last_failure: Instant::now(),
                });
                state.failures += 1;
                state.last_failure = Instant::now();
                return Err(capnp::Error::failed("authentication failed: invalid HMAC".to_string()));
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

            Ok(())
        }
    }
}

struct CommandAgentImpl {
    config: Rc<AgentConfig>,
    concurrency_guard: Rc<ConcurrencyGuard>,
    agent_start_time: Instant,
}

impl command_agent::Server for CommandAgentImpl {
    fn exec(
        self: Rc<Self>,
        params: command_agent::ExecParams,
        mut results: command_agent::ExecResults,
    ) -> impl ::core::future::Future<Output = Result<(), capnp::Error>> + 'static {
        async move {
            let _permit = self.concurrency_guard.try_acquire().ok_or_else(|| {
                capnp::Error::failed("too many concurrent exec requests".to_string())
            })?;

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
            };

            let exec_result = process::execute(&command, &work_dir, timeout_secs, &extra_env, &opts)
                .await
                .map_err(|e| capnp::Error::failed(format!("exec error: {e}")))?;

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
    }

    fn ping(
        self: Rc<Self>,
        _params: command_agent::PingParams,
        mut results: command_agent::PingResults,
    ) -> impl ::core::future::Future<Output = Result<(), capnp::Error>> + 'static {
        async move {
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
}
