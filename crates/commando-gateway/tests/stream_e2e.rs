//! End-to-end tests for the production streaming path:
//! `handle_exec_core` -> `start_exec_stream` -> agent `exec_stream`,
//! drained with `build_page`. Also characterizes what happens to a running
//! command when the gateway task, the TCP connection, or the HTTP client goes away.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::compat::TokioAsyncReadCompatExt;

use commando_agent::config::AgentConfig;
use commando_agent::rpc::{self as agent_rpc, ConcurrencyGuard};
use commando_common::auth;
use commando_common::commando_capnp::{authenticator, command_agent, output_receiver};
use commando_gateway::config::{
    AgentConnectionConfig, GatewayConfig, ManualTarget, ServerConfig, StreamingConfig,
};
use commando_gateway::handler::{self, ConcurrencyLimiter, Profiler};
use commando_gateway::registry::{ManualTargetInput, Registry};
use commando_gateway::session::SessionMap;
use commando_gateway::types::ExecPage;

const TEST_PSK: &str = "stream-e2e-secret";
const TARGET: &str = "e2e";

fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, f);
}

// --- agent ---

struct TestAgent {
    port: u16,
    guard: Rc<ConcurrencyGuard>,
    /// Connections whose `handle_connection` has not returned yet.
    open_connections: Rc<Cell<usize>>,
}

/// Start an in-process agent that accepts any number of connections.
async fn start_agent(max_concurrent: usize) -> TestAgent {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let config = Rc::new(AgentConfig {
        bind: "127.0.0.1".to_string(),
        port: 0,
        shell: "sh".to_string(),
        psk: TEST_PSK.to_string(),
        max_output_bytes: 131_072,
        max_concurrent,
    });
    let rate_limits = Rc::new(RefCell::new(HashMap::new()));
    let guard = Rc::new(ConcurrencyGuard::new(max_concurrent));
    let start_time = Instant::now();

    let open_connections = Rc::new(Cell::new(0usize));
    let accept_guard = guard.clone();
    let accept_open = open_connections.clone();
    tokio::task::spawn_local(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            let config = config.clone();
            let rate_limits = rate_limits.clone();
            let guard = accept_guard.clone();
            let open = accept_open.clone();
            open.set(open.get() + 1);
            tokio::task::spawn_local(async move {
                let _ = agent_rpc::handle_connection(
                    stream,
                    peer.ip(),
                    config,
                    rate_limits,
                    guard,
                    start_time,
                )
                .await;
                open.set(open.get() - 1);
            });
        }
    });

    TestAgent {
        port,
        guard,
        open_connections,
    }
}

// --- gateway harness ---

struct Gateway {
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<ConcurrencyLimiter>,
    sessions: Rc<RefCell<SessionMap>>,
}

#[derive(Debug, Default)]
struct Collected {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    timed_out: bool,
    pages: usize,
}

impl Gateway {
    fn new(port: u16) -> Self {
        let config = Arc::new(GatewayConfig {
            server: ServerConfig::default(),
            proxmox: None,
            agent: AgentConnectionConfig {
                default_port: 9876,
                default_timeout_secs: 60,
                connect_timeout_secs: 5,
                max_concurrent_per_target: 4,
                psk: HashMap::from([(TARGET.to_string(), TEST_PSK.to_string())]),
            },
            targets: Vec::<ManualTarget>::new(),
            cache_dir: "/tmp/commando-stream-e2e".to_string(),
            streaming: StreamingConfig {
                page_timeout_secs: 1,
                page_max_bytes: 32_768,
                session_idle_timeout_secs: 60,
                session_buffer_max_bytes: 8 * 1024 * 1024,
            },
        });
        let registry = Registry::from_manual(vec![ManualTargetInput {
            name: TARGET.to_string(),
            host: "127.0.0.1".to_string(),
            port,
            shell: "sh".to_string(),
            tags: vec![],
        }]);
        Self {
            config,
            registry: Arc::new(Mutex::new(registry)),
            limiter: Arc::new(ConcurrencyLimiter::new(4)),
            sessions: Rc::new(RefCell::new(SessionMap::new())),
        }
    }

    fn with_buffer_max(mut self, bytes: usize) -> Self {
        let mut config = (*self.config).clone();
        config.streaming.session_buffer_max_bytes = bytes;
        self.config = Arc::new(config);
        self
    }

    /// Bytes buffered in the session behind `token`.
    fn buffered(&self, token: &str) -> usize {
        self.sessions
            .borrow()
            .get_by_token(token)
            .map(|s| s.total_buffered())
            .unwrap_or(0)
    }

    /// Start a command and return its first page.
    async fn start(
        &self,
        command: &str,
        work_dir: &str,
        timeout_secs: Option<u32>,
        extra_env: Vec<(String, String)>,
    ) -> ExecPage {
        let mut profiler = Profiler::new(false);
        handler::handle_exec_core(
            TARGET,
            command,
            work_dir,
            timeout_secs,
            extra_env,
            &self.config,
            &self.registry,
            &self.limiter,
            &self.sessions,
            &mut profiler,
        )
        .await
        .unwrap_or_else(|e| panic!("handle_exec_core failed: {}", e.message))
    }

    /// Poll `build_page` with each `next_page` token until a page has `exit_code`.
    async fn drain(&self, first: ExecPage) -> Collected {
        let mut out = Collected::default();
        let mut page = first;
        loop {
            out.pages += 1;
            out.stdout.push_str(&page.stdout);
            out.stderr.push_str(&page.stderr);
            if let Some(code) = page.exit_code {
                out.exit_code = Some(code);
                out.timed_out = page.timed_out.unwrap_or(false);
                return out;
            }
            assert!(out.pages < 500, "too many pages without exit_code");
            let token = page.next_page.expect("running page has next_page");
            page = handler::build_page(&self.sessions, &token, &self.config.streaming)
                .await
                .unwrap_or_else(|e| panic!("build_page failed: {e}"));
        }
    }

    async fn exec(&self, command: &str) -> Collected {
        self.exec_with(command, "", None, vec![]).await
    }

    async fn exec_with(
        &self,
        command: &str,
        work_dir: &str,
        timeout_secs: Option<u32>,
        extra_env: Vec<(String, String)>,
    ) -> Collected {
        let first = self.start(command, work_dir, timeout_secs, extra_env).await;
        self.drain(first).await
    }
}

// --- process helpers ---

async fn wait_for_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && let Ok(pid) = s.trim().parse::<u32>()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "pid file never appeared");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn kill_process(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status();
}

/// Kills the process in `Drop` so a failed assertion cannot leak it.
struct KillOnDrop(u32);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        kill_process(self.0);
    }
}

fn permit_available(guard: &Rc<ConcurrencyGuard>) -> bool {
    guard.try_acquire().is_some()
}

/// Poll `cond` until it holds or `within` elapses; returns the final value.
async fn eventually(within: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `exec` replaces the shell, so the recorded PID is the long-running process itself.
fn long_running_command(pid_file: &Path) -> String {
    format!("echo $$ > {}; exec sleep 30", pid_file.display())
}

// --- behavior tests ---

#[test]
fn echo_hello() {
    run_local(async {
        let agent = start_agent(4).await;
        let gw = Gateway::new(agent.port);
        let out = gw.exec("echo hello").await;
        assert_eq!(out.stdout, "hello\n");
        assert_eq!(out.stderr, "");
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.timed_out);
    });
}

#[test]
fn exit_code_is_propagated() {
    run_local(async {
        let agent = start_agent(4).await;
        let gw = Gateway::new(agent.port);
        let out = gw.exec("exit 42").await;
        assert_eq!(out.exit_code, Some(42));
        assert_eq!(out.stdout, "");
    });
}

#[test]
fn stderr_arrives_in_stderr() {
    run_local(async {
        let agent = start_agent(4).await;
        let gw = Gateway::new(agent.port);
        let out = gw.exec("echo oops >&2").await;
        assert_eq!(out.stderr, "oops\n");
        assert_eq!(out.stdout, "");
        assert_eq!(out.exit_code, Some(0));
    });
}

#[test]
fn large_output_arrives_complete_across_pages() {
    run_local(async {
        let agent = start_agent(4).await;
        let gw = Gateway::new(agent.port);
        let out = gw.exec("head -c 200000 /dev/zero | tr '\\0' x").await;
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout.len(), 200_000);
        assert!(out.stdout.bytes().all(|b| b == b'x'));
        assert!(out.pages > 1, "expected several pages, got {}", out.pages);
    });
}

#[test]
fn five_megabytes_arrive_complete_and_in_order() {
    run_local(async {
        let agent = start_agent(4).await;
        let gw = Gateway::new(agent.port);
        let out = gw.exec("seq 1 800000").await;
        let expected: String = (1..=800_000).map(|n| format!("{n}\n")).collect();
        assert!(expected.len() > 5_000_000);
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout.len(), expected.len());
        assert!(out.stdout == expected, "output differs or is out of order");
    });
}

#[test]
fn five_megabytes_arrive_in_order_with_a_small_buffer_cap() {
    run_local(async {
        let agent = start_agent(4).await;
        let gw = Gateway::new(agent.port).with_buffer_max(1024 * 1024);
        let out = gw.exec("seq 1 800000").await;
        let expected: String = (1..=800_000).map(|n| format!("{n}\n")).collect();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout == expected, "output differs or is out of order");
    });
}

#[test]
fn slow_client_cannot_grow_the_session_past_the_cap() {
    const CAP: usize = 1024 * 1024;
    const TOTAL: usize = 32 * 1024 * 1024;
    run_local(async {
        let agent = start_agent(4).await;
        let gw = Gateway::new(agent.port).with_buffer_max(CAP);
        let first = gw
            .start(
                "head -c 33554432 /dev/zero | tr '\\0' x",
                "",
                Some(60),
                vec![],
            )
            .await;
        assert!(first.exit_code.is_none(), "output should still be coming");
        let mut token = first.next_page.clone().unwrap();
        let mut received = first.stdout.len();

        // The client does not poll for 2 s.
        let mut peak = 0;
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            peak = peak.max(gw.buffered(&token));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            peak <= CAP + 256 * 1024,
            "session buffered {peak} bytes, cap is {CAP}"
        );
        assert!(peak > CAP / 2, "buffer never filled ({peak} bytes)");

        // Then it polls to the end and gets everything.
        let mut exit_code = None;
        for _ in 0..4000 {
            let page = handler::build_page(&gw.sessions, &token, &gw.config.streaming)
                .await
                .unwrap_or_else(|e| panic!("build_page failed: {e}"));
            assert!(page.stdout.bytes().all(|b| b == b'x'));
            received += page.stdout.len();
            if let Some(code) = page.exit_code {
                exit_code = Some(code);
                break;
            }
            token = page.next_page.unwrap();
        }
        assert_eq!(exit_code, Some(0));
        assert_eq!(received, TOTAL);
    });
}

#[test]
fn blocked_receive_calls_do_not_survive_a_session_cleanup() {
    const CAP: usize = 64 * 1024;
    run_local(async {
        let agent = start_agent(1).await;
        let gw = Gateway::new(agent.port).with_buffer_max(CAP);
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let map_refs = Rc::strong_count(&gw.sessions);

        // `yes` never ends: it stops only when the agent stops reading its pipe.
        let command = format!("echo $$ > {}; exec yes", pid_file.display());
        let first = gw.start(&command, "", Some(60), vec![]).await;
        let token = first.next_page.expect("command should still be running");
        let pid = KillOnDrop(wait_for_pid(&pid_file).await);

        let filled = eventually(Duration::from_secs(5), || gw.buffered(&token) > CAP).await;
        assert!(filled, "buffer should fill past the cap");
        // Let the agent run out of window so receive calls are held back.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let expired = gw.sessions.borrow_mut().cleanup_expired(Duration::ZERO);
        assert_eq!(expired.len(), 1);

        let closed = eventually(Duration::from_secs(3), || agent.open_connections.get() == 0).await;
        let gone = eventually(Duration::from_secs(3), || !process_alive(pid.0)).await;
        assert!(closed, "agent connection should close");
        assert!(gone, "remote process should be killed");
        // Held `receive` futures (each owns a clone of the map) died with the task.
        let released = eventually(Duration::from_secs(3), || {
            Rc::strong_count(&gw.sessions) == map_refs
        })
        .await;
        assert!(
            released,
            "session map still has {} references, expected {map_refs}",
            Rc::strong_count(&gw.sessions)
        );
    });
}

#[test]
fn exec_deadline_waits_while_the_gateway_holds_receive_calls() {
    const CAP: usize = 64 * 1024;
    run_local(async {
        // The command writes everything within its 2 s timeout, but the agent
        // returns its result only after the held calls are answered, which takes
        // a slow client past the 2.05 s call deadline.
        commando_gateway::rpc::set_exec_grace(Some(Duration::from_millis(50)));
        let agent = start_agent(4).await;
        let gw = Gateway::new(agent.port).with_buffer_max(CAP);
        let first = gw
            .start("head -c 458752 /dev/zero | tr '\\0' x", "", Some(2), vec![])
            .await;

        let mut page = first;
        let mut received = page.stdout.len();
        let started = Instant::now();
        while page.exit_code.is_none() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let token = page.next_page.clone().expect("running page has next_page");
            page = handler::build_page(&gw.sessions, &token, &gw.config.streaming)
                .await
                .unwrap_or_else(|e| panic!("build_page failed: {e}"));
            received += page.stdout.len();
        }
        assert!(
            started.elapsed() > Duration::from_millis(2300),
            "the call must outlast the deadline for this test to mean anything"
        );
        commando_gateway::rpc::set_exec_grace(None);

        assert_eq!(page.error, None);
        assert_eq!(page.exit_code, Some(0));
        assert_eq!(page.timed_out, None);
        assert!(!page.truncated);
        assert_eq!(received, 458_752);
    });
}

#[test]
fn work_dir_and_extra_env_reach_the_command() {
    run_local(async {
        let agent = start_agent(4).await;
        let gw = Gateway::new(agent.port);
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().canonicalize().unwrap();
        let out = gw
            .exec_with(
                "pwd; echo \"$STREAM_E2E_VAR\"",
                dir_path.to_str().unwrap(),
                None,
                vec![("STREAM_E2E_VAR".to_string(), "from-env".to_string())],
            )
            .await;
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout, format!("{}\nfrom-env\n", dir_path.display()));
    });
}

#[test]
fn timeout_reports_timed_out_and_137() {
    run_local(async {
        let agent = start_agent(4).await;
        let gw = Gateway::new(agent.port);
        let started = Instant::now();
        let out = gw.exec_with("sleep 30", "", Some(1), vec![]).await;
        assert!(out.timed_out);
        assert_eq!(out.exit_code, Some(137));
        assert!(started.elapsed() < Duration::from_secs(4));
    });
}

// --- characterization tests ---

/// (a) Gateway aborts the rpc task of a running command: is the agent's
/// `exec_stream` call future dropped? The abort drops the pending call (capnp
/// Finish) and the rpc_system task with it, so the TCP connection closes.
#[test]
fn finding_a_gateway_task_abort() {
    run_local(async {
        let agent = start_agent(1).await;
        let gw = Gateway::new(agent.port);
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");

        let first = gw
            .start(&long_running_command(&pid_file), "", Some(60), vec![])
            .await;
        assert!(first.exit_code.is_none(), "command should still be running");
        let pid = KillOnDrop(wait_for_pid(&pid_file).await);

        assert!(
            !permit_available(&agent.guard),
            "permit must be held while the command runs"
        );
        assert_eq!(agent.open_connections.get(), 1);

        let expired = gw.sessions.borrow_mut().cleanup_expired(Duration::ZERO);
        assert_eq!(expired.len(), 1);

        let released = eventually(Duration::from_secs(5), || permit_available(&agent.guard)).await;
        let connection_closed =
            eventually(Duration::from_secs(5), || agent.open_connections.get() == 0).await;
        let gone = eventually(Duration::from_secs(5), || !process_alive(pid.0)).await;
        println!("FINDING: (a) gateway task abort: agent permit released = {released}");
        println!("FINDING: (a) gateway task abort: connection closed = {connection_closed}");
        println!("FINDING: (a) gateway task abort: remote process gone = {gone}");

        assert!(released, "agent exec_stream future should be dropped");
        assert!(connection_closed, "connection closes after the abort");
        // The gateway-side concurrency slot is free again.
        let max = gw.config.agent.max_concurrent_per_target;
        for _ in 0..max {
            assert!(
                gw.limiter.try_acquire(TARGET),
                "gateway slot should be free"
            );
        }
        for _ in 0..max {
            gw.limiter.release(TARGET);
        }
        assert!(gone, "remote process is killed by the abort");
    });
}

struct NullReceiver;

impl output_receiver::Server for NullReceiver {
    async fn receive(
        self: Rc<Self>,
        _params: output_receiver::ReceiveParams,
        _results: output_receiver::ReceiveResults,
    ) -> Result<(), capnp::Error> {
        Ok(())
    }
}

/// Connect, authenticate and enqueue `exec_stream` on tasks owned by the
/// current `LocalSet`. `send()` only enqueues the request; callers must wait
/// for the command's side effect (`wait_for_pid`) to know the agent started it.
async fn send_raw_exec_stream(port: u16, command: &str) {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.set_nodelay(true).unwrap();
    let (reader, writer) = stream.compat().split();
    let network = capnp_rpc::twoparty::VatNetwork::new(
        futures::io::BufReader::new(reader),
        futures::io::BufWriter::new(writer),
        capnp_rpc::rpc_twoparty_capnp::Side::Client,
        Default::default(),
    );
    let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), None);
    let auth_client: authenticator::Client =
        rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
    tokio::task::spawn_local(rpc_system);

    let challenge = auth_client
        .challenge_request()
        .send()
        .promise
        .await
        .unwrap();
    let nonce = challenge.get().unwrap().get_nonce().unwrap();
    let hmac = auth::compute_hmac(TEST_PSK.as_bytes(), nonce);
    let mut auth_request = auth_client.authenticate_request();
    auth_request.get().set_hmac(&hmac);
    let auth_response = auth_request.send().promise.await.unwrap();
    let agent_client: command_agent::Client = auth_response.get().unwrap().get_agent().unwrap();

    let receiver: output_receiver::Client = capnp_rpc::new_client(NullReceiver);
    let mut request = agent_client.exec_stream_request();
    {
        let mut params = request.get();
        let mut req = params.reborrow().init_request();
        req.set_command(command);
        req.set_work_dir("");
        req.set_timeout_secs(60);
        req.set_request_id("stream-e2e-raw");
        params.set_receiver(receiver);
    }
    let call = request.send().promise;
    tokio::task::spawn_local(async move {
        let _keep = (auth_client, agent_client);
        let _ = call.await;
    });
}

/// (b) TCP connection to the agent closes while `exec_stream` runs: is the
/// call future dropped?
#[test]
fn finding_b_connection_close() {
    run_local(async {
        let agent = start_agent(1).await;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");

        let nested = tokio::task::LocalSet::new();
        let command = long_running_command(&pid_file);
        let pid = nested
            .run_until(async {
                send_raw_exec_stream(agent.port, &command).await;
                wait_for_pid(&pid_file).await
            })
            .await;
        let pid = KillOnDrop(pid);

        assert!(
            !permit_available(&agent.guard),
            "permit must be held while the command runs"
        );
        assert_eq!(agent.open_connections.get(), 1);

        // Dropping the LocalSet drops the rpc system, and with it the TCP stream.
        drop(nested);

        let released = eventually(Duration::from_secs(5), || permit_available(&agent.guard)).await;
        let closed = eventually(Duration::from_secs(5), || agent.open_connections.get() == 0).await;
        let gone = eventually(Duration::from_secs(5), || !process_alive(pid.0)).await;
        println!("FINDING: (b) connection close: agent permit released = {released}");
        println!("FINDING: (b) connection close: connection handler returned = {closed}");
        println!("FINDING: (b) connection close: remote process gone = {gone}");

        assert!(released, "agent exec_stream future should be dropped");
        assert!(closed, "agent connection handler should return");
        assert!(gone, "remote process is killed by the disconnect");
    });
}

/// (c) Does axum/hyper drop a handler future when the HTTP client disconnects
/// mid-request? Proves only that: the handler future is dropped, so the
/// receiving end it holds closes. It does NOT show that a disconnect cancels
/// the command: in production the handler only awaits a oneshot receiver and
/// the work runs in a separate `spawn_local` task (streamable.rs). A later row
/// adds that cancellation.
#[test]
fn finding_c_http_client_disconnect() {
    use axum::Router;
    use axum::extract::State;
    use axum::routing::get;
    use tokio::sync::{mpsc, oneshot};

    type Work = oneshot::Sender<()>;

    async fn handler(State(tx): State<mpsc::Sender<Work>>) -> &'static str {
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(resp_tx).await.unwrap();
        let _ = resp_rx.await;
        "done"
    }

    run_local(async {
        let (work_tx, mut work_rx) = mpsc::channel::<Work>(4);
        let app = Router::new().route("/", get(handler)).with_state(work_tx);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://127.0.0.1:{port}/");
        let request = tokio::spawn(reqwest::Client::new().get(url).send());

        // Wait until the handler is running, then disconnect the client.
        let mut sender = tokio::time::timeout(Duration::from_secs(5), work_rx.recv())
            .await
            .expect("handler was not called within 5s")
            .expect("work channel closed");
        request.abort();
        let _ = request.await;

        let dropped = tokio::time::timeout(Duration::from_secs(2), sender.closed())
            .await
            .is_ok();
        println!("FINDING: (c) http client disconnect: handler future dropped = {dropped}");
        server.abort();

        assert!(dropped, "axum drops the handler future on disconnect");
    });
}
