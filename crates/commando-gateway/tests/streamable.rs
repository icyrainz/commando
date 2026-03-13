//! Streamable HTTP transport tests: verify POST /mcp dispatch,
//! GET/DELETE 405, health endpoint, and error handling.

use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;

use commando_gateway::config::{AgentConnectionConfig, GatewayConfig, ServerConfig};
use commando_gateway::handler::ConcurrencyLimiter;
use commando_gateway::registry::Registry;
use commando_gateway::streamable;

const TEST_API_KEY: &str = "test-secret-key-12345";

fn test_config() -> Arc<GatewayConfig> {
    Arc::new(GatewayConfig {
        server: ServerConfig {
            transport: "streamable-http".to_string(),
            bind: "127.0.0.1".to_string(),
            port: 0,
            api_key: Some(TEST_API_KEY.to_string()),
        },
        proxmox: None,
        agent: AgentConnectionConfig {
            default_port: 9876,
            default_timeout_secs: 60,
            connect_timeout_secs: 5,
            max_concurrent_per_target: 4,
            psk: Default::default(),
        },
        targets: vec![],
        cache_dir: "/tmp/commando-test".to_string(),
        streaming: Default::default(),
    })
}

async fn start_server() -> String {
    let config = test_config();
    let registry = Arc::new(Mutex::new(Registry::new()));
    let limiter = Arc::new(ConcurrencyLimiter::new(4));

    let app = streamable::build_app(config, registry, limiter);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    format!("http://127.0.0.1:{port}")
}

fn auth_header() -> String {
    format!("Bearer {TEST_API_KEY}")
}

fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, f);
}

#[test]
fn health_returns_ok() {
    run_local(async {
        let base = start_server().await;
        let resp = reqwest::get(format!("{base}/health")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "ok");
    });
}

#[test]
fn post_initialize_returns_json() {
    run_local(async {
        let base = start_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/mcp"))
            .header("Content-Type", "application/json")
            .header("Authorization", auth_header())
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["serverInfo"]["name"], "commando");
    });
}

#[test]
fn post_notification_returns_202() {
    run_local(async {
        let base = start_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/mcp"))
            .header("Content-Type", "application/json")
            .header("Authorization", auth_header())
            .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
    });
}

#[test]
fn post_invalid_json_returns_parse_error() {
    run_local(async {
        let base = start_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/mcp"))
            .header("Content-Type", "application/json")
            .header("Authorization", auth_header())
            .body("not json {{{")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], -32700);
    });
}

#[test]
fn post_batch_request_returns_error() {
    run_local(async {
        let base = start_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/mcp"))
            .header("Content-Type", "application/json")
            .header("Authorization", auth_header())
            .body(r#"[{"jsonrpc":"2.0","id":1,"method":"initialize"}]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], -32600);
        assert!(body["error"]["message"].as_str().unwrap().contains("batch"));
    });
}

#[test]
fn get_mcp_returns_405() {
    run_local(async {
        let base = start_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{base}/mcp"))
            .header("Authorization", auth_header())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 405);
    });
}

#[test]
fn delete_mcp_returns_405() {
    run_local(async {
        let base = start_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .delete(format!("{base}/mcp"))
            .header("Authorization", auth_header())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 405);
    });
}

#[test]
fn post_unknown_method_returns_error() {
    run_local(async {
        let base = start_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/mcp"))
            .header("Content-Type", "application/json")
            .header("Authorization", auth_header())
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"nonexistent/method"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], -32601);
    });
}

#[test]
fn mcp_without_auth_returns_401() {
    run_local(async {
        let base = start_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/mcp"))
            .header("Content-Type", "application/json")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    });
}

#[test]
fn mcp_with_wrong_token_returns_401() {
    run_local(async {
        let base = start_server().await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/mcp"))
            .header("Content-Type", "application/json")
            .header("Authorization", "Bearer wrong-key")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    });
}

#[test]
fn health_does_not_require_auth() {
    run_local(async {
        let base = start_server().await;
        let resp = reqwest::get(format!("{base}/health")).await.unwrap();
        assert_eq!(resp.status(), 200);
    });
}
