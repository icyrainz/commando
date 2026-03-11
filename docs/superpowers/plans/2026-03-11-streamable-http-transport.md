# Streamable HTTP Transport Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the deprecated SSE transport with MCP Streamable HTTP — single `POST /mcp` endpoint, no sessions, JSON responses.

**Architecture:** Reuse the channel bridge pattern from SSE (axum handler → mpsc → spawn_local worker → dispatch_request → oneshot back). Remove all session management. The new `streamable.rs` is ~80 lines vs SSE's ~140.

**Tech Stack:** Rust, axum, tokio, serde_json. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-03-11-streamable-http-transport-design.md`

---

## Chunk 1: Create streamable.rs and wire it up

### Task 1: Create `streamable.rs` with all endpoints

**Files:**
- Create: `crates/commando-gateway/src/streamable.rs`

- [ ] **Step 1: Write `streamable.rs`**

```rust
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing::info;

use crate::config::GatewayConfig;
use crate::handler;
use crate::registry::Registry;

/// Work item sent from axum handlers (outside LocalSet) to the RPC worker (inside LocalSet).
struct WorkItem {
    request: Value,
    response_tx: tokio::sync::oneshot::Sender<Option<Value>>,
}

type WorkSender = tokio::sync::mpsc::Sender<WorkItem>;

#[derive(Clone)]
struct AppState {
    work_tx: WorkSender,
}

/// Build the Axum router and spawn the RPC worker that bridges axum handlers
/// to the LocalSet where Cap'n Proto RPC lives.
pub fn build_app(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
) -> Router {
    let (work_tx, mut work_rx) = tokio::sync::mpsc::channel::<WorkItem>(64);

    // RPC worker: runs inside LocalSet, processes JSON-RPC requests.
    // axum::serve dispatches handlers via tokio::spawn (outside LocalSet),
    // so handlers send work here via the channel to bridge the !Send gap.
    tokio::task::spawn_local(async move {
        while let Some(item) = work_rx.recv().await {
            let cfg = config.clone();
            let reg = registry.clone();
            let lim = limiter.clone();
            tokio::task::spawn_local(async move {
                let result =
                    handler::dispatch_request(&item.request, &cfg, &reg, &lim).await;
                let _ = item.response_tx.send(result);
            });
        }
    });

    let state = AppState { work_tx };

    Router::new()
        .route("/mcp", post(handle_post).get(handle_get).delete(handle_delete))
        .route("/health", get(handle_health))
        .with_state(state)
}

pub async fn run_streamable_server(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
) -> Result<()> {
    let bind = config.server.bind.clone();
    let port = config.server.port;
    let app = build_app(config, registry, limiter);

    let addr = format!("{bind}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    info!(addr = %addr, "Streamable HTTP server listening");

    let shutdown = async {
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
        info!("shutting down Streamable HTTP server");
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    Ok(())
}

async fn handle_post(
    axum::extract::State(state): axum::extract::State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    let body_str = String::from_utf8_lossy(&body).into_owned();

    let request: Value = match serde_json::from_str(&body_str) {
        Ok(v) => v,
        Err(e) => {
            let error = handler::make_error_response(
                Value::Null,
                -32700,
                &format!("Parse error: {e}"),
            );
            return Json(error).into_response();
        }
    };

    // Reject batch requests (JSON arrays)
    if request.is_array() {
        let error = handler::make_error_response(
            Value::Null,
            -32600,
            "batch requests not supported",
        );
        return Json(error).into_response();
    }

    // Notifications (no id or null id) — accept without dispatching
    if request.get("id").is_none() || request["id"].is_null() {
        return axum::http::StatusCode::ACCEPTED.into_response();
    }

    // Send to the LocalSet worker via channel and await response
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    if state
        .work_tx
        .send(WorkItem {
            request,
            response_tx,
        })
        .await
        .is_err()
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(handler::make_error_response(
                Value::Null,
                -32603,
                "worker unavailable",
            )),
        )
            .into_response();
    }

    match response_rx.await {
        Ok(Some(response)) => Json(response).into_response(),
        Ok(None) => axum::http::StatusCode::ACCEPTED.into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(handler::make_error_response(
                Value::Null,
                -32603,
                "worker dropped response",
            )),
        )
            .into_response(),
    }
}

async fn handle_get() -> Response {
    axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response()
}

async fn handle_delete() -> Response {
    axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response()
}

async fn handle_health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p commando-gateway 2>&1`
Expected: success (new module not yet wired in, but file should parse)

Note: The module won't compile standalone until wired into lib.rs (step in Task 2). Save this file and proceed.

---

### Task 2: Wire up the new transport, remove SSE

**Files:**
- Modify: `crates/commando-gateway/src/lib.rs`
- Modify: `crates/commando-gateway/src/main.rs`
- Modify: `crates/commando-gateway/src/config.rs`
- Delete: `crates/commando-gateway/src/sse.rs`

- [ ] **Step 1: Update `lib.rs` — swap `sse` for `streamable`**

Change:
```rust
pub mod sse;
```
To:
```rust
pub mod streamable;
```

- [ ] **Step 2: Update `config.rs` — change default transport**

In `crates/commando-gateway/src/config.rs`, change:
```rust
fn default_transport() -> String { "sse".to_string() }
```
To:
```rust
fn default_transport() -> String { "streamable-http".to_string() }
```

- [ ] **Step 3: Update `main.rs` — swap transport match arm and imports**

Replace:
```rust
use commando_gateway::sse;
```
With:
```rust
use commando_gateway::streamable;
```

Replace the CLI help string:
```rust
    /// MCP transport: "sse" or "stdio"
```
With:
```rust
    /// MCP transport: "streamable-http" or "stdio"
```

Replace the two SSE-related CLI arg comments:
```rust
    /// HTTP bind address (SSE only)
```
With:
```rust
    /// HTTP bind address (streamable-http only)
```

```rust
    /// HTTP port (SSE only)
```
With:
```rust
    /// HTTP port (streamable-http only)
```

Replace the match arm in `run_gateway`:
```rust
        "sse" => sse::run_sse_server(config, registry, limiter).await,
```
With:
```rust
        "streamable-http" => streamable::run_streamable_server(config, registry, limiter).await,
```

Replace the error message in the fallback arm:
```rust
        other => anyhow::bail!("unknown transport: {other} (expected 'stdio' or 'sse')"),
```
With:
```rust
        other => anyhow::bail!("unknown transport: {other} (expected 'stdio' or 'streamable-http')"),
```

- [ ] **Step 4: Delete `sse.rs`**

Run: `rm crates/commando-gateway/src/sse.rs`

- [ ] **Step 5: Build to verify everything compiles**

Run: `cargo build -p commando-gateway 2>&1`
Expected: success, no errors

- [ ] **Step 6: Run existing unit tests (handler, config, registry)**

Run: `cargo test -p commando-gateway --lib 2>&1`
Expected: all 29 unit tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/commando-gateway/src/streamable.rs \
       crates/commando-gateway/src/lib.rs \
       crates/commando-gateway/src/main.rs \
       crates/commando-gateway/src/config.rs
git rm crates/commando-gateway/src/sse.rs
git commit -m "feat: replace SSE transport with Streamable HTTP

Single POST /mcp endpoint, no sessions, no persistent connections.
Implements MCP Streamable HTTP transport spec."
```

---

### Task 3: Replace SSE tests with Streamable HTTP tests

**Files:**
- Delete: `crates/commando-gateway/tests/sse.rs`
- Create: `crates/commando-gateway/tests/streamable.rs`

- [ ] **Step 1: Delete old SSE tests**

Run: `rm crates/commando-gateway/tests/sse.rs`

- [ ] **Step 2: Write Streamable HTTP transport tests**

```rust
//! Streamable HTTP transport tests: verify POST /mcp dispatch,
//! GET/DELETE 405, health endpoint, and error handling.

use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;

use commando_gateway::config::{
    AgentConnectionConfig, GatewayConfig, ProxmoxConfig, ServerConfig,
};
use commando_gateway::handler::ConcurrencyLimiter;
use commando_gateway::registry::Registry;
use commando_gateway::streamable;

fn test_config() -> Arc<GatewayConfig> {
    Arc::new(GatewayConfig {
        server: ServerConfig {
            transport: "streamable-http".to_string(),
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
        let resp = reqwest::get(format!("{base}/mcp")).await.unwrap();
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
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"nonexistent/method"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], -32601);
    });
}
```

- [ ] **Step 3: Run transport tests**

Run: `cargo test -p commando-gateway --test streamable 2>&1`
Expected: all 8 tests pass

- [ ] **Step 4: Run full test suite**

Run: `cargo test 2>&1`
Expected: all tests pass (unit + integration + streamable transport)

- [ ] **Step 5: Commit**

```bash
git rm crates/commando-gateway/tests/sse.rs
git add crates/commando-gateway/tests/streamable.rs
git commit -m "test: replace SSE tests with Streamable HTTP transport tests"
```

---

### Task 4: Update documentation

**Files:**
- Modify: `docs/design.md`

- [ ] **Step 1: Update design.md transport references**

In `docs/design.md`, make these changes:

1. Stack table (line ~53): Change `MCP | JSON-RPC over SSE (HTTP) or stdio` to `MCP | JSON-RPC over Streamable HTTP or stdio`

2. Architecture diagram text (line ~61): Change `HTTP/SSE (MCP JSON-RPC)` to `HTTP (MCP JSON-RPC)`

3. Architecture diagram box (lines ~69-70): Change `SSE Server` to `HTTP Server`

4. MCP server configuration section (lines ~356-366): Replace SSE config example:
```json
{
  "mcpServers": {
    "commando": {
      "type": "streamable-http",
      "url": "http://gateway-host:9877/mcp"
    }
  }
}
```
Update the label above it from "SSE transport (recommended — persistent service, no SSH):" to "Streamable HTTP transport (recommended — persistent service, no SSH):".

5. Gateway lifecycle section (line ~379): Change "Claude Code connects via HTTP/SSE" to "Claude Code connects via HTTP".

6. Repo structure section (line ~441): Change `handler.rs        # MCP dispatch logic (shared by stdio + SSE)` to `handler.rs        # MCP dispatch logic (shared by stdio + HTTP)`

7. Repo structure section (line ~443): Change `sse.rs            # SSE transport (HTTP server via axum)` to `streamable.rs     # Streamable HTTP transport (axum)`

8. Dependencies table (line ~472): Change `axum | HTTP server for SSE transport` to `axum | HTTP server for Streamable HTTP transport`

9. Dependencies table (line ~473): Change `tokio-stream | Stream adapter for SSE events` to remove or change to `tokio-stream | Stream utilities` (still a workspace dep, keep for now).

- [ ] **Step 2: Verify no remaining SSE references**

Run: `grep -ri "sse" docs/design.md` — should return nothing (or only unrelated matches like "addresses").

- [ ] **Step 3: Commit**

```bash
git add docs/design.md
git commit -m "docs: update design.md for Streamable HTTP transport"
```

---

### Task 5: Update config test for new default

**Files:**
- Modify: `crates/commando-gateway/src/config.rs`

- [ ] **Step 1: Update config tests that reference the default transport**

In `crates/commando-gateway/src/config.rs`, in `server_section_defaults` test and `parse_config_with_server_section` test, update any assertions about the default transport value from `"sse"` to `"streamable-http"`.

The `server_section_defaults` test (line ~168) has:
```rust
assert_eq!(config.server.transport, "sse");
```
Change to:
```rust
assert_eq!(config.server.transport, "streamable-http");
```

The `parse_config_with_server_section` test (line ~162) explicitly sets `transport = "sse"` in the TOML. Update the TOML string and assertion:
```rust
transport = "streamable-http"
```
```rust
assert_eq!(config.server.transport, "streamable-http");
```

- [ ] **Step 2: Run config tests**

Run: `cargo test -p commando-gateway config::tests 2>&1`
Expected: all 4 config tests pass

- [ ] **Step 3: Run full test suite one final time**

Run: `cargo test 2>&1`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/commando-gateway/src/config.rs
git commit -m "test: update config tests for streamable-http default"
```
