//! End-to-end tests for the audit log: one `start` and one `end` event per
//! command, one `rejected` line per refused request, driven through the real
//! streaming path against an in-process agent.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::net::TcpListener;

use commando_agent::config::AgentConfig;
use commando_agent::rpc::{self as agent_rpc, ConcurrencyGuard};
use commando_gateway::audit::AuditLogger;
use commando_gateway::config::{
    AgentConnectionConfig, GatewayConfig, ManualTarget, ServerConfig, StreamingConfig,
};
use commando_gateway::handler::{self, ConcurrencyLimiter, Profiler};
use commando_gateway::registry::{ManualTargetInput, Registry};
use commando_gateway::session::SessionMap;
use commando_gateway::types::ExecPage;

const AGENT_PSK: &str = "audit-e2e-secret";
const TARGET: &str = "audited";

fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, f);
}

/// Start an in-process agent that accepts any number of connections.
async fn start_agent() -> u16 {
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
    let open = Rc::new(Cell::new(0usize));

    tokio::task::spawn_local(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            let config = config.clone();
            let rate_limits = rate_limits.clone();
            let guard = guard.clone();
            let open = open.clone();
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
    port
}

struct Gateway {
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<ConcurrencyLimiter>,
    sessions: Rc<RefCell<SessionMap>>,
    audit: Arc<AuditLogger>,
    log_path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Gateway {
    fn new(port: u16, psk: &str, page_timeout_secs: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("audit.log");
        let config = Arc::new(GatewayConfig {
            server: ServerConfig::default(),
            proxmox: None,
            agent: AgentConnectionConfig {
                default_port: 9876,
                default_timeout_secs: 60,
                connect_timeout_secs: 5,
                max_concurrent_per_target: 4,
                psk: HashMap::from([(TARGET.to_string(), psk.to_string())]),
            },
            targets: Vec::<ManualTarget>::new(),
            cache_dir: dir.path().to_string_lossy().into_owned(),
            streaming: StreamingConfig {
                page_timeout_secs,
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
            audit: Arc::new(AuditLogger::new(log_path.clone(), 1024 * 1024)),
            log_path,
            _dir: dir,
        }
    }

    /// Start a command through the audited entry point and return its first page.
    async fn start(
        &self,
        target: &str,
        command: &str,
        timeout_secs: Option<u32>,
    ) -> Result<ExecPage, String> {
        let mut profiler = Profiler::new(false);
        handler::handle_exec_audited(
            target,
            command,
            "",
            timeout_secs,
            vec![],
            &self.config,
            &self.registry,
            &self.limiter,
            &self.sessions,
            &mut profiler,
            &self.audit,
            "rest",
        )
        .await
        .map_err(|e| e.message)
    }

    /// Poll pages until one carries `exit_code`.
    async fn drain(&self, first: ExecPage) -> ExecPage {
        let mut page = first;
        for _ in 0..100 {
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

    /// Every audit line, each parsed as JSON.
    fn lines(&self) -> Vec<Value> {
        let Ok(text) = std::fs::read_to_string(&self.log_path) else {
            return Vec::new();
        };
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad line {l:?}: {e}")))
            .collect()
    }

    /// Wait until the log holds an `end` event.
    async fn wait_for_end(&self) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let lines = self.lines();
            if lines.iter().any(|l| l["event"] == "end") {
                return lines;
            }
            assert!(Instant::now() < deadline, "no end event; log: {lines:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Assert `lines` is exactly one start followed by one end with the same request_id.
fn start_and_end(lines: &[Value]) -> (&Value, &Value) {
    assert_eq!(lines.len(), 2, "log: {lines:?}");
    let (start, end) = (&lines[0], &lines[1]);
    assert_eq!(start["event"], "start");
    assert_eq!(end["event"], "end");
    let id = start["request_id"].as_str().expect("start has request_id");
    assert!(!id.is_empty());
    assert_eq!(end["request_id"], id);
    assert_eq!(start["target"], TARGET);
    assert_eq!(end["target"], TARGET);
    assert_eq!(start["source"], "rest");
    assert_eq!(end["source"], "rest");
    (start, end)
}

#[test]
fn short_command_writes_start_then_completed_end() {
    run_local(async {
        let port = start_agent().await;
        let gw = Gateway::new(port, AGENT_PSK, 5);

        let first = gw.start(TARGET, "echo hi", Some(10)).await.unwrap();
        let page = gw.drain(first).await;
        assert_eq!(page.exit_code, Some(0));

        let lines = gw.lines();
        let (start, end) = start_and_end(&lines);
        assert_eq!(start["command"], "echo hi");
        assert!(end.get("command").is_none());
        assert_eq!(end["outcome"], "completed");
        assert_eq!(end["exit_code"], 0);
        assert!(end["duration_ms"].is_u64(), "end: {end}");
        assert!(end.get("error").is_none());
    });
}

#[test]
fn command_outliving_first_page_is_closed_without_polling() {
    run_local(async {
        let port = start_agent().await;
        let gw = Gateway::new(port, AGENT_PSK, 1);

        let first = gw.start(TARGET, "sleep 2", Some(10)).await.unwrap();
        assert!(first.exit_code.is_none(), "first page should be running");
        assert_eq!(gw.lines().len(), 1, "only the start event so far");

        // No further poll: the rpc task alone must write the end event.
        let lines = gw.wait_for_end().await;
        let (_, end) = start_and_end(&lines);
        assert_eq!(end["outcome"], "completed");
        assert_eq!(end["exit_code"], 0);
        assert!(end["duration_ms"].as_u64().unwrap() >= 1000, "end: {end}");
    });
}

#[test]
fn command_that_times_out_ends_with_timed_out() {
    run_local(async {
        let port = start_agent().await;
        let gw = Gateway::new(port, AGENT_PSK, 1);

        let first = gw.start(TARGET, "sleep 10", Some(1)).await.unwrap();
        let page = gw.drain(first).await;
        assert_eq!(page.timed_out, Some(true));

        let lines = gw.wait_for_end().await;
        let (_, end) = start_and_end(&lines);
        assert_eq!(end["outcome"], "timed_out");
    });
}

#[test]
fn wrong_psk_ends_with_rpc_error() {
    run_local(async {
        let port = start_agent().await;
        let gw = Gateway::new(port, "not-the-agent-psk", 5);

        let first = gw.start(TARGET, "echo hi", Some(10)).await.unwrap();
        gw.drain(first).await;

        let lines = gw.wait_for_end().await;
        let (_, end) = start_and_end(&lines);
        assert_eq!(end["outcome"], "rpc_error");
        assert_eq!(end["error"], "authentication failed: invalid HMAC");
        assert!(end.get("exit_code").is_none());
    });
}

#[test]
fn expired_session_ends_with_aborted() {
    run_local(async {
        let port = start_agent().await;
        let gw = Gateway::new(port, AGENT_PSK, 1);

        let first = gw.start(TARGET, "sleep 5", Some(10)).await.unwrap();
        assert!(first.exit_code.is_none(), "first page should be running");

        let expired = gw.sessions.borrow_mut().cleanup_expired(Duration::ZERO);
        assert_eq!(expired.len(), 1);

        let lines = gw.wait_for_end().await;
        let (_, end) = start_and_end(&lines);
        assert_eq!(end["outcome"], "aborted");
    });
}

#[test]
fn unknown_target_writes_one_rejected_line() {
    run_local(async {
        let port = start_agent().await;
        let gw = Gateway::new(port, AGENT_PSK, 1);

        let err = gw.start("nope", "echo hi", None).await.unwrap_err();
        assert_eq!(err, "unknown target: nope");

        let lines = gw.lines();
        assert_eq!(lines.len(), 1, "log: {lines:?}");
        assert_eq!(lines[0]["event"], "rejected");
        assert_eq!(lines[0]["target"], "nope");
        assert_eq!(lines[0]["command"], "echo hi");
        assert_eq!(lines[0]["error"], "unknown target: nope");
        assert!(lines[0].get("request_id").is_none());
        assert!(lines[0].get("outcome").is_none());
    });
}
