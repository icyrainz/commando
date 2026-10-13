//! Gateway-side RPC hardening: error text reaches the final page, deadlines
//! bound a silent agent, and failed calls close their connection.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;

use commando_agent::config::AgentConfig;
use commando_agent::rpc::{self as agent_rpc, ConcurrencyGuard};
use commando_gateway::config::{
    AgentConnectionConfig, GatewayConfig, ManualTarget, ServerConfig, StreamingConfig,
};
use commando_gateway::handler::{self, ConcurrencyLimiter, Profiler};
use commando_gateway::registry::{ManualTargetInput, Registry};
use commando_gateway::rpc::remote_ping;
use commando_gateway::session::SessionMap;
use commando_gateway::types::ExecPage;

const AGENT_PSK: &str = "hardening-agent-secret";
const TARGET: &str = "hardening";

fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, f);
}

struct TestAgent {
    port: u16,
    /// Connections whose `handle_connection` has not returned yet.
    open_connections: Rc<Cell<usize>>,
}

/// Start an in-process agent that accepts any number of connections.
async fn start_agent() -> TestAgent {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let config = Rc::new(AgentConfig {
        bind: "127.0.0.1".to_string(),
        port: 0,
        shell: "sh".to_string(),
        psk: AGENT_PSK.to_string(),
        max_output_bytes: 131_072,
        max_concurrent: 4,
    });
    let rate_limits = Rc::new(RefCell::new(HashMap::new()));
    let guard = Rc::new(ConcurrencyGuard::new(4));
    let start_time = Instant::now();

    let open_connections = Rc::new(Cell::new(0usize));
    let accept_open = open_connections.clone();
    tokio::task::spawn_local(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            let config = config.clone();
            let rate_limits = rate_limits.clone();
            let guard = guard.clone();
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
        open_connections,
    }
}

/// A "agent" that accepts TCP connections and never answers.
async fn start_silent_agent() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::task::spawn_local(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    port
}

/// A port nothing listens on.
async fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

struct Gateway {
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<ConcurrencyLimiter>,
    sessions: Rc<RefCell<SessionMap>>,
}

impl Gateway {
    fn new(port: u16, psk: &str, connect_timeout_secs: u64) -> Self {
        let config = Arc::new(GatewayConfig {
            server: ServerConfig::default(),
            proxmox: None,
            agent: AgentConnectionConfig {
                default_port: 9876,
                default_timeout_secs: 60,
                connect_timeout_secs,
                max_concurrent_per_target: 4,
                psk: HashMap::from([(TARGET.to_string(), psk.to_string())]),
            },
            targets: Vec::<ManualTarget>::new(),
            cache_dir: "/tmp/commando-rpc-hardening".to_string(),
            streaming: StreamingConfig {
                page_timeout_secs: 1,
                page_max_bytes: 32_768,
                session_idle_timeout_secs: 60,
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

    /// Run `command` and poll pages until the final one (the one with `exit_code`).
    async fn exec_final_page(&self, command: &str) -> ExecPage {
        let mut profiler = Profiler::new(false);
        let mut page = handler::handle_exec_core(
            TARGET,
            command,
            "",
            Some(10),
            vec![],
            &self.config,
            &self.registry,
            &self.limiter,
            &self.sessions,
            &mut profiler,
        )
        .await
        .unwrap_or_else(|e| panic!("handle_exec_core failed: {}", e.message));
        for _ in 0..50 {
            if page.exit_code.is_some() {
                return page;
            }
            let token = page.next_page.take().expect("running page has next_page");
            page = handler::build_page(&self.sessions, &token, &self.config.streaming)
                .await
                .unwrap_or_else(|e| panic!("build_page failed: {e}"));
        }
        panic!("no final page");
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

#[test]
fn wrong_psk_reports_authentication_error_and_closes_connection() {
    run_local(async {
        let agent = start_agent().await;
        let gw = Gateway::new(agent.port, "not-the-agent-psk", 5);

        let page = gw.exec_final_page("echo hi").await;
        println!("error: {:?}", page.error);
        assert_eq!(page.exit_code, Some(-1));
        let error = page.error.expect("final page carries the rpc error");
        assert!(error.contains("authentication"), "error was: {error}");

        let closed = eventually(Duration::from_secs(5), || agent.open_connections.get() == 0).await;
        assert!(closed, "agent connection handler should return");
        assert!(gw.limiter.try_acquire(TARGET));
    });
}

#[test]
fn nothing_listening_reports_connect_failure() {
    run_local(async {
        let port = closed_port().await;
        let gw = Gateway::new(port, AGENT_PSK, 5);

        let page = gw.exec_final_page("echo hi").await;
        println!("error: {:?}", page.error);
        assert_eq!(page.exit_code, Some(-1));
        let error = page.error.expect("final page carries the rpc error");
        assert!(error.contains("connect"), "error was: {error}");
    });
}

#[test]
fn silent_agent_exec_fails_within_auth_deadline() {
    run_local(async {
        let port = start_silent_agent().await;
        let gw = Gateway::new(port, AGENT_PSK, 1);

        let started = Instant::now();
        let page = gw.exec_final_page("echo hi").await;
        let elapsed = started.elapsed();
        println!("error: {:?} after {elapsed:?}", page.error);
        assert_eq!(page.exit_code, Some(-1));
        let error = page.error.expect("final page carries the rpc error");
        assert!(error.contains("timed out"), "error was: {error}");
        assert!(error.contains("authentication"), "error was: {error}");
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    });
}

#[test]
fn silent_agent_ping_fails_within_deadline() {
    run_local(async {
        let port = start_silent_agent().await;

        let started = Instant::now();
        let result = remote_ping("127.0.0.1", port, AGENT_PSK, 1).await;
        let elapsed = started.elapsed();
        let error = result.err().expect("ping must fail").to_string();
        println!("error: {error} after {elapsed:?}");
        assert!(error.contains("timed out"), "error was: {error}");
        // 2 x connect_timeout_secs, plus scheduling slack.
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    });
}

#[test]
fn error_key_is_absent_without_error() {
    let page = ExecPage {
        stdout: "hi".to_string(),
        stderr: String::new(),
        exit_code: Some(0),
        duration_ms: Some(1),
        timed_out: None,
        next_page: None,
        error: None,
        _profile: None,
    };
    let json = serde_json::to_value(&page).unwrap();
    assert!(json.get("error").is_none(), "json was: {json}");

    let failed = ExecPage {
        error: Some("boom".to_string()),
        exit_code: Some(-1),
        ..page
    };
    let json = serde_json::to_value(&failed).unwrap();
    assert_eq!(json["error"], "boom");
    assert_eq!(json["exit_code"], -1);
}

// --- REST layer ---

const API_KEY: &str = "hardening-api-key";

/// Serve the REST API for a gateway whose single target points at `port`.
async fn start_rest(port: u16, psk: &str) -> String {
    let gw = Gateway::new(port, psk, 5);
    let mut config = (*gw.config).clone();
    config.server.api_key = Some(API_KEY.to_string());
    config.server.audit_log_path = Some("/dev/null".to_string());
    let audit = Arc::new(commando_gateway::audit::AuditLogger::new(
        std::path::PathBuf::from("/dev/null"),
        10 * 1024 * 1024,
    ));
    let app = commando_gateway::streamable::build_app(
        Arc::new(config),
        gw.registry.clone(),
        gw.limiter.clone(),
        audit,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{rest_port}")
}

async fn post_exec(base: &str, target: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/exec"))
        .bearer_auth(API_KEY)
        .json(&serde_json::json!({"target": target, "command": "echo hi"}))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

#[test]
fn rest_exec_rpc_failure_is_a_full_page() {
    run_local(async {
        let agent = start_agent().await;
        let base = start_rest(agent.port, "not-the-agent-psk").await;

        let (mut status, mut body) = post_exec(&base, TARGET).await;
        // The first response may be a running page; follow next_page to the final one.
        for _ in 0..20 {
            let Some(token) = body["next_page"].as_str().map(str::to_string) else {
                break;
            };
            let resp = reqwest::Client::new()
                .get(format!("{base}/api/exec?page={token}"))
                .bearer_auth(API_KEY)
                .send()
                .await
                .unwrap();
            status = resp.status().as_u16();
            body = resp.json().await.unwrap();
        }
        println!("{status} {body}");
        assert_eq!(status, 200);
        assert_eq!(body["exit_code"], -1);
        assert!(body["error"].as_str().unwrap().contains("authentication"));
        assert!(body.get("stdout").is_some());
    });
}

#[test]
fn rest_exec_unknown_target_is_400_with_only_error() {
    run_local(async {
        let agent = start_agent().await;
        let base = start_rest(agent.port, AGENT_PSK).await;

        let (status, body) = post_exec(&base, "no-such-target").await;
        assert_eq!(status, 400);
        assert!(body["error"].is_string());
        assert!(body.get("stdout").is_none());
    });
}

#[test]
fn rest_exec_invalid_page_token_is_400() {
    run_local(async {
        let agent = start_agent().await;
        let base = start_rest(agent.port, AGENT_PSK).await;

        let resp = reqwest::Client::new()
            .get(format!("{base}/api/exec?page=bogus"))
            .bearer_auth(API_KEY)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);
    });
}
