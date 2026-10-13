//! End-to-end cancellation tests: a command is killed unless its client is polling.
//! `streamable::build_app` is served on a local port with an in-process agent.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;

use commando_agent::config::AgentConfig;
use commando_agent::rpc::{self as agent_rpc, ConcurrencyGuard};
use commando_gateway::audit::AuditLogger;
use commando_gateway::config::{
    AgentConnectionConfig, GatewayConfig, ManualTarget, ServerConfig, StreamingConfig,
};
use commando_gateway::handler::ConcurrencyLimiter;
use commando_gateway::registry::{ManualTargetInput, Registry};

const TEST_PSK: &str = "cancel-e2e-secret";
const API_KEY: &str = "cancel-e2e-key";
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

// --- gateway over HTTP ---

struct HttpGateway {
    base: String,
    limiter: Arc<ConcurrencyLimiter>,
}

/// Serve the gateway app; the worker runs on the current `LocalSet`.
async fn start_gateway(agent_port: u16, page_timeout_secs: u64, idle_secs: u64) -> HttpGateway {
    let server = ServerConfig {
        api_key: Some(API_KEY.to_string()),
        ..ServerConfig::default()
    };
    let config = Arc::new(GatewayConfig {
        server,
        proxmox: None,
        agent: AgentConnectionConfig {
            default_port: 9876,
            default_timeout_secs: 60,
            connect_timeout_secs: 5,
            max_concurrent_per_target: 4,
            psk: HashMap::from([(TARGET.to_string(), TEST_PSK.to_string())]),
        },
        targets: Vec::<ManualTarget>::new(),
        cache_dir: "/tmp/commando-cancel-e2e".to_string(),
        streaming: StreamingConfig {
            page_timeout_secs,
            page_max_bytes: 32_768,
            session_idle_timeout_secs: idle_secs,
        },
    });
    let registry = Registry::from_manual(vec![ManualTargetInput {
        name: TARGET.to_string(),
        host: "127.0.0.1".to_string(),
        port: agent_port,
        shell: "sh".to_string(),
        tags: vec![],
    }]);
    let limiter = Arc::new(ConcurrencyLimiter::new(1));
    let audit = Arc::new(AuditLogger::new(
        std::path::PathBuf::from("/dev/null"),
        10 * 1024 * 1024,
    ));
    let app = commando_gateway::streamable::build_app(
        config,
        Arc::new(Mutex::new(registry)),
        limiter.clone(),
        audit,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    HttpGateway {
        base: format!("http://127.0.0.1:{port}"),
        limiter,
    }
}

impl HttpGateway {
    fn post_exec(&self, command: String) -> tokio::task::JoinHandle<serde_json::Value> {
        let url = format!("{}/api/exec", self.base);
        tokio::spawn(async move {
            let resp = reqwest::Client::new()
                .post(url)
                .bearer_auth(API_KEY)
                .json(&serde_json::json!({"target": TARGET, "command": command}))
                .send()
                .await
                .unwrap();
            resp.json().await.unwrap()
        })
    }

    fn get_page(&self, token: &str) -> tokio::task::JoinHandle<serde_json::Value> {
        let url = format!("{}/api/exec?page={token}", self.base);
        tokio::spawn(async move {
            let resp = reqwest::Client::new()
                .get(url)
                .bearer_auth(API_KEY)
                .send()
                .await
                .unwrap();
            resp.json().await.unwrap()
        })
    }

    /// The target slot is free when a fresh acquire succeeds (limit is 1).
    fn slot_free(&self) -> bool {
        if self.limiter.try_acquire(TARGET) {
            self.limiter.release(TARGET);
            true
        } else {
            false
        }
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

/// Kills the process in `Drop` so a failed assertion cannot leak it.
struct KillOnDrop(u32);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill")
            .args(["-9", &self.0.to_string()])
            .stderr(std::process::Stdio::null())
            .status();
    }
}

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

/// Assert the command, permit, agent connection and gateway slot are all released.
async fn assert_cancelled(agent: &TestAgent, gw: &HttpGateway, pid: u32, within: Duration) {
    assert!(
        eventually(within, || !process_alive(pid)).await,
        "remote process still alive"
    );
    assert!(
        eventually(within, || agent.guard.try_acquire().is_some()).await,
        "agent permit not released"
    );
    assert!(
        eventually(within, || agent.open_connections.get() == 0).await,
        "agent connection still open"
    );
    assert!(
        eventually(within, || gw.slot_free()).await,
        "gateway slot still held"
    );
}

// --- tests ---

#[test]
fn disconnect_during_first_post_cancels_command() {
    run_local(async {
        let agent = start_agent(1).await;
        // Page timeout longer than the test: the POST is still waiting when dropped.
        let gw = start_gateway(agent.port, 20, 60).await;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");

        let request = gw.post_exec(long_running_command(&pid_file));
        let pid = wait_for_pid(&pid_file).await;
        let _kill = KillOnDrop(pid);
        assert!(process_alive(pid));

        request.abort();
        assert_cancelled(&agent, &gw, pid, Duration::from_secs(3)).await;
    });
}

#[test]
fn disconnect_during_later_get_cancels_command() {
    run_local(async {
        let agent = start_agent(1).await;
        let gw = start_gateway(agent.port, 2, 60).await;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");

        // First page returns after the 2 s page timeout with a continuation token.
        let first = gw.post_exec(long_running_command(&pid_file)).await.unwrap();
        let pid = wait_for_pid(&pid_file).await;
        let _kill = KillOnDrop(pid);
        let token = first["next_page"]
            .as_str()
            .expect("running page")
            .to_string();
        assert!(process_alive(pid));

        let request = gw.get_page(&token);
        tokio::time::sleep(Duration::from_millis(500)).await;
        request.abort();
        assert_cancelled(&agent, &gw, pid, Duration::from_secs(3)).await;
    });
}

#[test]
fn polling_client_keeps_command_alive_past_idle_timeout() {
    run_local(async {
        let agent = start_agent(1).await;
        let gw = start_gateway(agent.port, 1, 2).await;

        let mut page = gw
            .post_exec("sleep 5; echo done".to_string())
            .await
            .unwrap();
        let started = Instant::now();
        let mut stdout = String::new();
        loop {
            stdout.push_str(page["stdout"].as_str().unwrap_or(""));
            if page["exit_code"].is_number() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "never finished"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
            let token = page["next_page"]
                .as_str()
                .expect("running page")
                .to_string();
            page = gw.get_page(&token).await.unwrap();
        }
        assert_eq!(page["exit_code"], 0, "page: {page}");
        assert_eq!(stdout, "done\n");
        assert!(started.elapsed() >= Duration::from_secs(4));
    });
}

#[test]
fn client_that_stops_polling_is_cancelled_after_idle_timeout() {
    run_local(async {
        let agent = start_agent(1).await;
        let gw = start_gateway(agent.port, 1, 2).await;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");

        // Take the first page, then never poll again.
        let first = gw.post_exec(long_running_command(&pid_file)).await.unwrap();
        let pid = wait_for_pid(&pid_file).await;
        let _kill = KillOnDrop(pid);
        assert!(first["next_page"].is_string());
        assert!(process_alive(pid));

        // Idle timeout 2 s, cleanup runs every second.
        assert_cancelled(&agent, &gw, pid, Duration::from_secs(8)).await;
    });
}

#[test]
fn page_wait_longer_than_idle_timeout_keeps_command_alive() {
    run_local(async {
        let agent = start_agent(1).await;
        let gw = start_gateway(agent.port, 3, 2).await;

        // Quiet command: every page waits the full 3 s, longer than the 2 s idle timeout.
        let mut page = gw
            .post_exec("sleep 5; echo done".to_string())
            .await
            .unwrap();
        let started = Instant::now();
        while !page["exit_code"].is_number() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "never finished"
            );
            let token = page["next_page"]
                .as_str()
                .expect("running page")
                .to_string();
            page = gw.get_page(&token).await.unwrap();
        }
        assert_eq!(page["exit_code"], 0, "page: {page}");
    });
}
