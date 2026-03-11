//! SSE transport integration tests: verify HTTP endpoints, session management,
//! and message routing through the SSE server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpListener;

use commando_gateway::config::{
    AgentConnectionConfig, GatewayConfig, ProxmoxConfig, ServerConfig,
};
use commando_gateway::handler::ConcurrencyLimiter;
use commando_gateway::registry::Registry;
use commando_gateway::sse;

fn test_config() -> Arc<GatewayConfig> {
    Arc::new(GatewayConfig {
        server: ServerConfig {
            transport: "sse".to_string(),
            bind: "127.0.0.1".to_string(),
            port: 0,
        },
        proxmox: ProxmoxConfig {
            nodes: vec![],
            user: String::new(),
            token_id: String::new(),
            token_secret: String::new(),
            discovery_interval_secs: 60,
        },
        agent: AgentConnectionConfig {
            default_port: 9876,
            default_timeout_secs: 60,
            connect_timeout_secs: 5,
            max_concurrent_per_target: 4,
            psk: Default::default(),
        },
        targets: vec![],
    })
}

/// Start the SSE server on a random port, returning the base URL.
/// Must be called from within a LocalSet (for the RPC worker spawn_local).
async fn start_sse_server() -> String {
    let config = test_config();
    let registry = Arc::new(Mutex::new(Registry::new()));
    let limiter = Arc::new(ConcurrencyLimiter::new(4));

    let app = sse::build_app(config, registry, limiter);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    format!("http://127.0.0.1:{port}")
}

fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, f);
}

/// Read SSE chunks until we find the endpoint event, return session_id.
/// Uses a timeout to avoid hanging if the endpoint event never arrives.
async fn read_session_id(base: &str) -> String {
    let client = reqwest::Client::new();
    let mut resp = client
        .get(format!("{base}/sse"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let mut buf = String::new();
    // Read chunks until we find the session_id (with timeout)
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = resp.chunk().await.unwrap() {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            if let Some(sid) = extract_session_id(&buf) {
                return sid;
            }
        }
        panic!("SSE stream ended without endpoint event");
    })
    .await
    .expect("timed out waiting for SSE endpoint event");

    result
}

fn extract_session_id(buf: &str) -> Option<String> {
    for line in buf.lines() {
        if let Some(rest) = line.strip_prefix("data: /messages?session_id=") {
            return Some(rest.to_string());
        }
    }
    None
}

// ─── Health endpoint ────────────────────────────────────────────────

#[test]
fn health_returns_ok() {
    run_local(async {
        let base = start_sse_server().await;
        let resp = reqwest::get(format!("{base}/health")).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "ok");
    });
}

// ─── Invalid session returns 404 ────────────────────────────────────

#[test]
fn message_to_invalid_session_returns_404() {
    run_local(async {
        let base = start_sse_server().await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{base}/messages?session_id=nonexistent"))
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 404);
    });
}

// ─── SSE session establishment ──────────────────────────────────────

#[test]
fn sse_endpoint_returns_event_stream() {
    run_local(async {
        let base = start_sse_server().await;
        let resp = reqwest::get(format!("{base}/sse")).await.unwrap();
        assert_eq!(resp.status(), 200);

        let content_type = resp.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(
            content_type.contains("text/event-stream"),
            "expected text/event-stream, got {content_type}"
        );
    });
}

// ─── Full SSE flow: open session, send message, get response ────────

#[test]
fn sse_full_flow_initialize() {
    run_local(async {
        let base = start_sse_server().await;
        let client = reqwest::Client::new();

        // 1. Open SSE connection and get session_id
        let session_id = read_session_id(&base).await;
        assert!(!session_id.is_empty());

        // 2. Send an initialize request to the session
        let resp = client
            .post(format!("{base}/messages?session_id={session_id}"))
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .send()
            .await
            .unwrap();

        // Message endpoint returns 202 Accepted
        assert_eq!(resp.status(), 202);
    });
}

// ─── Notification returns 202 without dispatching ───────────────────

#[test]
fn sse_notification_returns_accepted() {
    run_local(async {
        let base = start_sse_server().await;
        let client = reqwest::Client::new();

        let session_id = read_session_id(&base).await;

        // Send a notification (no id field)
        let resp = client
            .post(format!("{base}/messages?session_id={session_id}"))
            .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 202);
    });
}
