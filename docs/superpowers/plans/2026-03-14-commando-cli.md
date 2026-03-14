# Commando CLI Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a CLI binary and REST API so Claude Code can execute remote commands via Bash tool with native output rendering.

**Architecture:** Refactor gateway handler internals to return structured data, add REST endpoints on the existing axum server, build a thin CLI HTTP client as a new crate.

**Tech Stack:** Rust, axum (REST routes), reqwest (CLI HTTP client), clap (CLI args), serde/serde_json, tokio

**Spec:** `docs/superpowers/specs/2026-03-14-commando-cli-design.md`

---

## Chunk 1: Gateway handler refactoring

Extract core logic from MCP-formatted handlers into shared functions that return structured data. Both MCP and REST formatters will call these.

### Task 1: Extract `build_page` return type into a struct

`build_page` already returns `Result<Value, String>` with a JSON object. Make it return a proper struct so both MCP and REST can format it without reparsing JSON.

**Files:**
- Create: `crates/commando-gateway/src/types.rs`
- Modify: `crates/commando-gateway/src/lib.rs`
- Modify: `crates/commando-gateway/src/handler.rs`

- [ ] **Step 1: Create `types.rs` with shared data types**

```rust
// crates/commando-gateway/src/types.rs
use serde::Serialize;

/// A page of streaming command output.
#[derive(Debug, Clone, Serialize)]
pub struct ExecPage {
    pub stdout: String,
    pub stderr: String,
    /// Present only on the final page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Present only on the final page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Present only when true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
    /// Present when command is still running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page: Option<String>,
}

/// Target info for REST API (minimal fields for CLI display).
#[derive(Debug, Clone, Serialize)]
pub struct TargetInfo {
    pub name: String,
    pub status: String,
    pub host: String,
}

/// Full target info for MCP (preserves all existing fields).
#[derive(Debug, Clone, Serialize)]
pub struct TargetInfoFull {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub shell: String,
    pub tags: Vec<String>,
    pub source: String,
    pub status: String,
    pub reachable: String,
    pub has_psk: bool,
}

/// Result of pinging a target.
#[derive(Debug, Clone, Serialize)]
pub struct PingInfo {
    pub target: String,
    pub hostname: String,
    pub uptime_secs: u64,
    pub shell: String,
    pub latency_ms: u64,
    pub version: String,
}

/// Errors from handler core functions.
#[derive(Debug, Clone)]
pub struct HandlerError {
    pub message: String,
    pub is_gateway_error: bool,
}

impl HandlerError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self { message: msg.into(), is_gateway_error: false }
    }
    pub fn gateway(msg: impl Into<String>) -> Self {
        Self { message: msg.into(), is_gateway_error: true }
    }
}
```

- [ ] **Step 2: Register module in `lib.rs`**

Add `pub mod types;` to `crates/commando-gateway/src/lib.rs`.

- [ ] **Step 3: Verify it compiles**

Run: `cargo +nightly check -p commando-gateway`
Expected: compiles with no errors

- [ ] **Step 4: Commit**

```bash
git add crates/commando-gateway/src/types.rs crates/commando-gateway/src/lib.rs
git commit -m "feat(gateway): add shared types for handler refactoring"
```

### Task 2: Refactor `build_page` to return `ExecPage`

Change `build_page` from returning `Result<Value, String>` to `Result<ExecPage, String>`. Update `format_page_response` to accept `&ExecPage`.

**Files:**
- Modify: `crates/commando-gateway/src/handler.rs`

- [ ] **Step 1: Change `build_page` return type**

In `handler.rs`, change `build_page` signature from:
```rust
async fn build_page(...) -> Result<Value, String> {
```
to:
```rust
async fn build_page(...) -> Result<ExecPage, String> {
```

Add `use crate::types::ExecPage;` at the top.

Replace the two return sites in `build_page`:

The final page return (around line 509):
```rust
Ok(ExecPage {
    stdout,
    stderr,
    exit_code: Some(exit_code),
    duration_ms: Some(duration_ms),
    timed_out: if timed_out { Some(true) } else { None },
    next_page: None,
})
```

The streaming return (around line 523):
```rust
Ok(ExecPage {
    stdout,
    stderr,
    exit_code: None,
    duration_ms: None,
    timed_out: None,
    next_page: Some(new_token),
})
```

- [ ] **Step 2: Update `format_page_response` to accept `&ExecPage`**

Change signature from `fn format_page_response(id: &Value, page: &Value) -> Value` to `fn format_page_response(id: &Value, page: &ExecPage) -> Value`.

```rust
fn format_page_response(id: &Value, page: &ExecPage) -> Value {
    let mut text = String::new();

    if !page.stdout.is_empty() {
        text.push_str(&page.stdout);
    }

    if !page.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&page.stderr);
    }

    if page.timed_out.unwrap_or(false) {
        text.push_str("\n[timed out]");
    }

    if let Some(exit_code) = page.exit_code {
        let duration_ms = page.duration_ms.unwrap_or(0);
        let metadata = format!("\n---\nexit_code: {} | duration: {}ms", exit_code, duration_ms);
        text.push_str(&metadata);
    }

    if let Some(next_page) = &page.next_page {
        text.push_str(&format!("\n[streaming] next_page={next_page}"));
    }

    let is_error = page.exit_code.is_some_and(|c| c != 0)
        || page.timed_out.unwrap_or(false);

    if is_error {
        make_tool_error(id, &text)
    } else {
        make_tool_result(id, &text)
    }
}
```

- [ ] **Step 3: Update callers of `format_page_response`**

In `handle_exec` (line 339):
```rust
Ok(page) => format_page_response(id, &page),
```
No change needed — the variable name stays the same, just the type changed.

Same for `handle_output` (line 543).

- [ ] **Step 4: Run tests**

Run: `cargo +nightly test -p commando-gateway`
Expected: all existing tests pass. Some `format_page_response` tests will need updating since they pass `json!({...})` — update them to pass `ExecPage` structs instead.

- [ ] **Step 5: Fix failing tests**

Two groups of tests need updating:

**A) `format_page_*` tests** (~6 tests, lines 1182-1245): These call `format_page_response` with `json!()` args. Update to pass `ExecPage` structs.

**B) `build_page` tests** (~12 tests, lines 1249-1383): These access the returned value as `page["stdout"]`, `page["exit_code"]`, etc. After refactoring, `build_page` returns `ExecPage`, so these must use struct field access: `page.stdout`, `page.exit_code`, etc.

For `format_page_response` tests, example:

```rust
#[test]
fn format_page_stdout_only() {
    let id = json!(1);
    let page = ExecPage {
        stdout: "hello world".to_string(),
        stderr: String::new(),
        exit_code: Some(0),
        duration_ms: Some(42),
        timed_out: None,
        next_page: None,
    };
    let resp = format_page_response(&id, &page);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("hello world"));
    assert!(text.contains("exit_code: 0"));
}
```

Repeat for all `format_page_*` tests.

- [ ] **Step 6: Run tests again**

Run: `cargo +nightly test -p commando-gateway`
Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/commando-gateway/src/handler.rs
git commit -m "refactor(gateway): build_page returns ExecPage struct instead of Value"
```

### Task 3: Extract `handle_exec_core`, `handle_list_core`, `handle_ping_core`

Extract the core logic from each MCP handler into public functions that return structured types. The MCP handlers become thin wrappers.

**Files:**
- Modify: `crates/commando-gateway/src/handler.rs`

- [ ] **Step 1: Make `build_page` public**

Change `async fn build_page(` to `pub async fn build_page(`. This allows the REST handler module to call it.

- [ ] **Step 2: Extract `handle_exec_core`**

Create a public function that does everything `handle_exec` does except wrapping in MCP format. It returns `Result<ExecPage, HandlerError>`:

```rust
pub async fn handle_exec_core(
    target_name: &str,
    command: &str,
    work_dir: &str,
    timeout_secs: Option<u32>,
    extra_env: Vec<(String, String)>,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
    limiter: &Arc<ConcurrencyLimiter>,
    session_map: &Rc<RefCell<SessionMap>>,
) -> Result<ExecPage, HandlerError> {
    // Move the body of handle_exec here. Key changes:
    // 1. Replace all `return make_tool_error(id, "msg")` with `return Err(HandlerError::bad_request("msg"))`
    // 2. Apply timeout default: `let timeout = timeout_secs.unwrap_or(config.agent.default_timeout_secs);`
    // 3. Generate request_id: `let request_id = uuid::Uuid::new_v4().to_string();`
    // 4. Keep the info!() log line for command tracing
    // 5. Call build_page and map_err: `build_page(...).await.map_err(HandlerError::bad_request)?`
    // 6. The concurrency limiter, session creation, RPC start, JoinHandle storage all stay the same
}
```

Then slim down `handle_exec` to:

```rust
async fn handle_exec(
    id: &Value,
    args: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
    limiter: &Arc<ConcurrencyLimiter>,
    session_map: &Rc<RefCell<SessionMap>>,
) -> Value {
    let target_name = match args["target"].as_str() {
        Some(t) => t,
        None => return make_tool_error(id, "missing required parameter: target"),
    };
    let command = match args["command"].as_str() {
        Some(c) => c,
        None => return make_tool_error(id, "missing required parameter: command"),
    };
    let work_dir = args["work_dir"].as_str().unwrap_or("");
    let timeout_secs = args["timeout"].as_u64().map(|t| t as u32);
    let extra_env: Vec<(String, String)> = args["env"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();

    match handle_exec_core(target_name, command, work_dir, timeout_secs, extra_env, config, registry, limiter, session_map).await {
        Ok(page) => format_page_response(id, &page),
        Err(e) => make_tool_error(id, &e.message),
    }
}
```

Note: `timeout_secs` becomes `Option<u32>`. The core function applies the default from config:
```rust
let timeout = timeout_secs.unwrap_or(config.agent.default_timeout_secs);
```

- [ ] **Step 3: Extract `handle_output_core`**

```rust
pub async fn handle_output_core(
    token: &str,
    session_map: &Rc<RefCell<SessionMap>>,
    config: &StreamingConfig,
) -> Result<ExecPage, HandlerError> {
    build_page(session_map, token, config)
        .await
        .map_err(HandlerError::bad_request)
}
```

Slim `handle_output`:
```rust
async fn handle_output(
    id: &Value,
    args: &Value,
    session_map: &Rc<RefCell<SessionMap>>,
    config: &StreamingConfig,
) -> Value {
    let token = match args["page"].as_str() {
        Some(t) => t,
        None => return make_tool_error(id, "missing required parameter: page"),
    };
    match handle_output_core(token, session_map, config).await {
        Ok(page) => format_page_response(id, &page),
        Err(e) => make_tool_error(id, &e.message),
    }
}
```

- [ ] **Step 4: Extract `handle_list_core`**

Two functions: `handle_list_core_full` returns all fields (for MCP backward compat), `handle_list_core` returns minimal fields (for REST).

```rust
pub fn handle_list_core_full(
    filter: Option<&str>,
    config: &GatewayConfig,
    registry: &Arc<Mutex<Registry>>,
) -> Vec<TargetInfoFull> {
    let reg = registry.lock().unwrap();
    reg.list(filter)
        .iter()
        .map(|t| TargetInfoFull {
            name: t.name.clone(),
            host: t.host.clone(),
            port: t.port,
            shell: t.shell.clone(),
            tags: t.tags.clone(),
            source: format!("{:?}", t.source),
            status: t.status.clone(),
            reachable: format!("{:?}", t.reachable),
            has_psk: config.agent.psk.contains_key(&t.name),
        })
        .collect()
}

pub fn handle_list_core(
    filter: Option<&str>,
    registry: &Arc<Mutex<Registry>>,
) -> Vec<TargetInfo> {
    let reg = registry.lock().unwrap();
    reg.list(filter)
        .iter()
        .map(|t| TargetInfo {
            name: t.name.clone(),
            status: t.status.clone(),
            host: t.host.clone(),
        })
        .collect()
}
```

Slim `handle_list` — uses the full version to preserve MCP output:
```rust
fn handle_list(
    id: &Value,
    args: &Value,
    config: &GatewayConfig,
    registry: &Arc<Mutex<Registry>>,
) -> Value {
    let filter = args["filter"].as_str();
    let targets = handle_list_core_full(filter, config, registry);
    make_tool_result(
        id,
        &serde_json::to_string_pretty(&targets).unwrap_or_default(),
    )
}
```

- [ ] **Step 5: Extract `handle_ping_core`**

```rust
pub async fn handle_ping_core(
    target_name: &str,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
) -> Result<PingInfo, HandlerError> {
    let (host, port, status) = {
        let reg = registry.lock().unwrap();
        match reg.get(target_name) {
            Some(t) => (t.host.clone(), t.port, t.status.clone()),
            None => return Err(HandlerError::bad_request(format!("unknown target: {target_name}"))),
        }
    };

    if host.is_empty() {
        return Err(HandlerError::bad_request(
            format!("target '{}' is {} (no IP available)", target_name, status),
        ));
    }

    let psk = match config.agent.psk.get(target_name) {
        Some(p) => p.clone(),
        None => {
            return Err(HandlerError::bad_request(
                format!("no PSK configured for target: {target_name}"),
            ));
        }
    };

    match rpc::remote_ping(&host, port, &psk, config.agent.connect_timeout_secs).await {
        Ok(r) => Ok(PingInfo {
            target: target_name.to_string(),
            latency_ms: 0, // remote_ping doesn't return latency; we'll measure it
            version: r.version,
        }),
        Err(e) => Err(HandlerError::gateway(format!("ping failed: {e}"))),
    }
}
```

Measure latency by timing the call, and preserve all fields from `RemotePingResult`:

```rust
pub async fn handle_ping_core(
    target_name: &str,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
) -> Result<PingInfo, HandlerError> {
    // ... validation same as above (host, psk lookups) ...

    let start = std::time::Instant::now();
    match rpc::remote_ping(&host, port, &psk, config.agent.connect_timeout_secs).await {
        Ok(r) => Ok(PingInfo {
            target: target_name.to_string(),
            hostname: r.hostname,
            uptime_secs: r.uptime_secs,
            shell: r.shell,
            latency_ms: start.elapsed().as_millis() as u64,
            version: r.version,
        }),
        Err(e) => Err(HandlerError::gateway(format!("ping failed: {e}"))),
    }
}
```

Slim `handle_ping` — preserves the original MCP output format:
```rust
async fn handle_ping(
    id: &Value,
    args: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
) -> Value {
    let target_name = match args["target"].as_str() {
        Some(t) => t,
        None => return make_tool_error(id, "missing required parameter: target"),
    };
    match handle_ping_core(target_name, config, registry).await {
        Ok(info) => {
            let text = format!(
                "hostname: {}\nuptime: {}s\nshell: {}\nversion: {}",
                info.hostname, info.uptime_secs, info.shell, info.version
            );
            make_tool_result(id, &text)
        }
        Err(e) => make_tool_error(id, &e.message),
    }
}
```

- [ ] **Step 6: Add `use crate::types::*;` to handler.rs**

- [ ] **Step 7: Run tests**

Run: `cargo +nightly test -p commando-gateway`
Expected: all tests pass

- [ ] **Step 8: Commit**

```bash
git add crates/commando-gateway/src/handler.rs
git commit -m "refactor(gateway): extract core handler logic from MCP formatting"
```

---

## Chunk 2: Gateway REST API endpoints

Add REST routes to the existing axum server. REST must be working before removing MCP exec tools.

### Task 5: Add REST routes to `streamable.rs`

Add `/api/exec`, `/api/targets`, `/api/ping/:target` routes with the same bearer auth.

**Files:**
- Create: `crates/commando-gateway/src/rest.rs`
- Modify: `crates/commando-gateway/src/lib.rs`
- Modify: `crates/commando-gateway/src/streamable.rs`

- [ ] **Step 1: Create `rest.rs` with REST handlers**

```rust
// crates/commando-gateway/src/rest.rs
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::HandlerError;

// REST handlers reuse the same AppState type as MCP handlers.
// Both need the work_tx channel to communicate with the LocalSet worker.
// This avoids axum state type conflicts when merging routers.

#[derive(Deserialize)]
pub struct ExecRequest {
    pub target: String,
    pub command: String,
    pub timeout: Option<u32>,
    pub work_dir: Option<String>,
}

#[derive(Deserialize)]
pub struct PageQuery {
    pub page: String,
}

fn error_response(status: axum::http::StatusCode, msg: &str) -> Response {
    (status, Json(json!({"error": msg}))).into_response()
}

fn handler_error_to_response(e: HandlerError) -> Response {
    let status = if e.is_gateway_error {
        axum::http::StatusCode::BAD_GATEWAY
    } else {
        axum::http::StatusCode::BAD_REQUEST
    };
    error_response(status, &e.message)
}

pub async fn handle_exec_post(
    State(state): State<super::streamable::AppState>,
    Json(req): Json<ExecRequest>,
) -> Response {
    // Build an internal request and send through the work channel.
    // The LocalSet worker will call handle_exec_core.
    let internal_req = json!({
        "__rest": "exec",
        "target": req.target,
        "command": req.command,
        "timeout": req.timeout,
        "work_dir": req.work_dir.unwrap_or_default(),
    });

    match send_work(&state, internal_req).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => error_response(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

pub async fn handle_exec_get(
    State(state): State<super::streamable::AppState>,
    Query(query): Query<PageQuery>,
) -> Response {
    let internal_req = json!({
        "__rest": "output",
        "page": query.page,
    });

    match send_work(&state, internal_req).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => error_response(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

pub async fn handle_targets(
    State(state): State<super::streamable::AppState>,
) -> Response {
    let internal_req = json!({
        "__rest": "list",
    });

    match send_work(&state, internal_req).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => error_response(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

pub async fn handle_ping(
    State(state): State<super::streamable::AppState>,
    axum::extract::Path(target): axum::extract::Path<String>,
) -> Response {
    let internal_req = json!({
        "__rest": "ping",
        "target": target,
    });

    match send_work(&state, internal_req).await {
        Ok(resp) => {
            if resp.get("error").is_some() {
                let status = if resp["_gateway"].as_bool().unwrap_or(false) {
                    axum::http::StatusCode::BAD_GATEWAY
                } else {
                    axum::http::StatusCode::BAD_REQUEST
                };
                // Strip internal _gateway field before returning
                let clean = json!({"error": resp["error"]});
                return (status, Json(clean)).into_response();
            }
            Json(resp).into_response()
        }
        Err(e) => error_response(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

async fn send_work(
    state: &super::streamable::AppState,
    request: Value,
) -> Result<Value, String> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    state
        .work_tx
        .send(super::streamable::WorkItem { request, response_tx })
        .await
        .map_err(|_| "worker unavailable".to_string())?;

    response_rx
        .await
        .map_err(|_| "worker dropped response".to_string())?
        .ok_or_else(|| "no response from worker".to_string())
}
```

Note: REST handlers use the same `AppState` as MCP handlers. No separate `RestState` needed — both just need the `work_tx` channel.
```

- [ ] **Step 2: Register module in `lib.rs`**

Add `pub mod rest;` to `crates/commando-gateway/src/lib.rs`.

- [ ] **Step 3: Make `WorkItem` and `AppState` public in `streamable.rs`**

Change `struct WorkItem` to `pub struct WorkItem` and `struct AppState` to `pub struct AppState`. Make fields public:

```rust
pub struct WorkItem {
    pub request: Value,
    pub response_tx: tokio::sync::oneshot::Sender<Option<Value>>,
}

#[derive(Clone)]
pub struct AppState {
    pub work_tx: WorkSender,
}
```

- [ ] **Step 4: Update `dispatch_request` to handle REST work items**

In `handler.rs`, update `dispatch_request` to detect `__rest` requests and route them to core functions:

```rust
pub async fn dispatch_request(
    request: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
    limiter: &Arc<ConcurrencyLimiter>,
    session_map: &Rc<RefCell<SessionMap>>,
) -> Option<Value> {
    // Handle REST API requests (sent via __rest marker)
    if let Some(rest_type) = request["__rest"].as_str() {
        let result = match rest_type {
            "exec" => {
                let target = request["target"].as_str().unwrap_or("");
                let command = request["command"].as_str().unwrap_or("");
                let work_dir = request["work_dir"].as_str().unwrap_or("");
                let timeout = request["timeout"].as_u64().map(|t| t as u32);
                // extra_env intentionally empty — --env flag omitted from CLI v1
                match handle_exec_core(target, command, work_dir, timeout, vec![], config, registry, limiter, session_map).await {
                    Ok(page) => serde_json::to_value(&page).unwrap(),
                    Err(e) => json!({"error": e.message, "_gateway": e.is_gateway_error}),
                }
            }
            "output" => {
                let token = request["page"].as_str().unwrap_or("");
                match handle_output_core(token, session_map, &config.streaming).await {
                    Ok(page) => serde_json::to_value(&page).unwrap(),
                    Err(e) => json!({"error": e.message}),
                }
            }
            "list" => {
                let targets = handle_list_core(None, registry);
                serde_json::to_value(&targets).unwrap()
            }
            "ping" => {
                let target = request["target"].as_str().unwrap_or("");
                match handle_ping_core(target, config, registry).await {
                    Ok(info) => serde_json::to_value(&info).unwrap(),
                    Err(e) => json!({"error": e.message, "_gateway": e.is_gateway_error}),
                }
            }
            _ => json!({"error": format!("unknown REST type: {rest_type}")}),
        };
        return Some(result);
    }

    // Existing MCP JSON-RPC dispatch below...
    let method = request["method"].as_str().unwrap_or("");
    // ... rest unchanged
}
```

- [ ] **Step 5: Add REST routes in `build_app`**

In `streamable.rs`, add the REST routes with the same auth:

```rust
pub fn build_app(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
) -> Router {
    let (work_tx, mut work_rx) = tokio::sync::mpsc::channel::<WorkItem>(64);
    let api_key = Arc::new(config.server.api_key.clone().unwrap_or_default());

    // ... existing LocalSet worker spawn (unchanged) ...

    let state = AppState { work_tx };

    // All routes share the same AppState (which contains the work_tx channel).
    // This avoids axum's state type conflict when merging routers.
    let authed_routes = Router::new()
        .route(
            "/mcp",
            post(handle_post).get(handle_get).delete(handle_delete),
        )
        .route("/api/exec", post(crate::rest::handle_exec_post).get(crate::rest::handle_exec_get))
        .route("/api/targets", get(crate::rest::handle_targets))
        .route("/api/ping/{target}", get(crate::rest::handle_ping))
        .layer(middleware::from_fn_with_state(
            api_key,
            bearer_auth_middleware,
        ))
        .with_state(state);

    Router::new()
        .merge(authed_routes)
        .route("/health", get(handle_health))
}
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo +nightly check -p commando-gateway`
Expected: compiles

- [ ] **Step 7: Run tests**

Run: `cargo +nightly test -p commando-gateway`
Expected: all tests pass

- [ ] **Step 8: Commit**

```bash
git add crates/commando-gateway/src/rest.rs crates/commando-gateway/src/streamable.rs crates/commando-gateway/src/handler.rs crates/commando-gateway/src/lib.rs
git commit -m "feat(gateway): add REST API endpoints for CLI"
```

### Task 6: Add REST endpoint tests

**Files:**
- Modify: `crates/commando-gateway/src/rest.rs` (add `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write tests for REST response format**

Add tests that verify the `ExecPage` serialization produces the expected JSON shape:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ExecPage;

    #[test]
    fn exec_page_completed_serialization() {
        let page = ExecPage {
            stdout: "hello".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            duration_ms: Some(150),
            timed_out: None,
            next_page: None,
        };
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["stdout"], "hello");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["duration_ms"], 150);
        assert!(json.get("timed_out").is_none());
        assert!(json.get("next_page").is_none());
    }

    #[test]
    fn exec_page_streaming_serialization() {
        let page = ExecPage {
            stdout: "partial".to_string(),
            stderr: String::new(),
            exit_code: None,
            duration_ms: None,
            timed_out: None,
            next_page: Some("abc123".to_string()),
        };
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["next_page"], "abc123");
        assert!(json.get("exit_code").is_none());
    }

    #[test]
    fn exec_page_timeout_serialization() {
        let page = ExecPage {
            stdout: "partial".to_string(),
            stderr: String::new(),
            exit_code: Some(124),
            duration_ms: Some(60000),
            timed_out: Some(true),
            next_page: None,
        };
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["exit_code"], 124);
        assert_eq!(json["timed_out"], true);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo +nightly test -p commando-gateway`
Expected: all tests pass

- [ ] **Step 3: Commit**

```bash
git add crates/commando-gateway/src/rest.rs
git commit -m "test(gateway): add REST response format tests"
```

---

## Chunk 3: CLI binary

Build the `commando` CLI as a thin HTTP client.

### Task 7: Scaffold the CLI crate

**Files:**
- Create: `crates/commando-cli/Cargo.toml`
- Create: `crates/commando-cli/src/main.rs`

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "commando-cli"
version.workspace = true
edition.workspace = true

[[bin]]
name = "commando"
path = "src/main.rs"

[dependencies]
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
clap = { version = "4", features = ["derive"] }
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { version = "1", features = ["rt", "macros"] }
```

- [ ] **Step 2: Create `main.rs` with CLI arg parsing**

```rust
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "commando", version, about = "Commando CLI — transparent remote execution")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Execute a command on a remote target
    Exec {
        /// Target machine name
        target: String,
        /// Command to execute
        command: String,
        /// Timeout in seconds
        #[arg(long)]
        timeout: Option<u32>,
        /// Working directory on target
        #[arg(long)]
        workdir: Option<String>,
    },
    /// List available targets
    List,
    /// Ping a target
    Ping {
        /// Target machine name
        target: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let exit_code = rt.block_on(run(cli));
    std::process::exit(exit_code);
}

async fn run(cli: Cli) -> i32 {
    let url = match std::env::var("COMMANDO_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("error: COMMANDO_URL environment variable not set");
            return 1;
        }
    };
    let api_key = match std::env::var("COMMANDO_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            eprintln!("error: COMMANDO_API_KEY environment variable not set");
            return 1;
        }
    };

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client");

    match cli.command {
        Commands::Exec { target, command, timeout, workdir } => {
            cmd_exec(&client, &url, &api_key, &target, &command, timeout, workdir.as_deref()).await
        }
        Commands::List => cmd_list(&client, &url, &api_key).await,
        Commands::Ping { target } => cmd_ping(&client, &url, &api_key, &target).await,
    }
}

async fn cmd_exec(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    target: &str,
    command: &str,
    timeout: Option<u32>,
    workdir: Option<&str>,
) -> i32 {
    use std::io::Write;

    let url = format!("{base_url}/api/exec");
    let mut body = serde_json::json!({
        "target": target,
        "command": command,
    });
    if let Some(t) = timeout {
        body["timeout"] = serde_json::json!(t);
    }
    if let Some(w) = workdir {
        body["work_dir"] = serde_json::json!(w);
    }

    let resp = match client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = body["error"].as_str().unwrap_or("unknown error");
        eprintln!("error: {msg} (HTTP {status})");
        return 1;
    }

    let mut json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: failed to parse response: {e}");
            return 1;
        }
    };

    loop {
        // Print stdout
        if let Some(stdout) = json["stdout"].as_str() {
            if !stdout.is_empty() {
                print!("{stdout}");
                let _ = std::io::stdout().flush();
            }
        }
        // Print stderr
        if let Some(stderr) = json["stderr"].as_str() {
            if !stderr.is_empty() {
                eprint!("{stderr}");
                let _ = std::io::stderr().flush();
            }
        }

        // Check if done
        if let Some(exit_code) = json["exit_code"].as_i64() {
            return exit_code as i32;
        }

        // Follow next_page
        let next_page = match json["next_page"].as_str() {
            Some(p) => p.to_string(),
            None => {
                eprintln!("error: no exit_code and no next_page in response");
                return 1;
            }
        };

        let page_url = format!("{base_url}/api/exec?page={next_page}");
        let resp = match client
            .get(&page_url)
            .bearer_auth(api_key)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let msg = body["error"].as_str().unwrap_or("unknown error");
            eprintln!("error: {msg} (HTTP {status})");
            return 1;
        }

        json = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                eprintln!("error: failed to parse response: {e}");
                return 1;
            }
        };
    }
}

async fn cmd_list(client: &reqwest::Client, base_url: &str, api_key: &str) -> i32 {
    let url = format!("{base_url}/api/targets");
    let resp = match client.get(&url).bearer_auth(api_key).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    if !resp.status().is_success() {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = body["error"].as_str().unwrap_or("unknown error");
        eprintln!("error: {msg}");
        return 1;
    }

    let targets: Vec<serde_json::Value> = match resp.json().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    for t in &targets {
        let name = t["name"].as_str().unwrap_or("?");
        let status = t["status"].as_str().unwrap_or("?");
        let host = t["host"].as_str().unwrap_or("");
        println!("{name}\t{status}\t{host}");
    }

    0
}

async fn cmd_ping(client: &reqwest::Client, base_url: &str, api_key: &str, target: &str) -> i32 {
    let url = format!("{base_url}/api/ping/{target}");
    let resp = match client.get(&url).bearer_auth(api_key).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    if !resp.status().is_success() {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = body["error"].as_str().unwrap_or("unknown error");
        eprintln!("error: {msg}");
        return 1;
    }

    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let target = json["target"].as_str().unwrap_or("?");
    let latency = json["latency_ms"].as_u64().unwrap_or(0);
    let version = json["version"].as_str().unwrap_or("?");
    println!("pong from {target} in {latency}ms (v{version})");

    0
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo +nightly check -p commando-cli`
Expected: compiles

- [ ] **Step 4: Commit**

```bash
git add crates/commando-cli/
git commit -m "feat(cli): scaffold commando CLI binary with exec, list, ping"
```

### Task 8: Test the CLI against a live gateway

This is a manual integration test.

- [ ] **Step 1: Build the CLI**

Run: `cargo +nightly build -p commando-cli`

- [ ] **Step 2: Set env vars and test list**

```bash
export COMMANDO_URL="http://akio-commando:9877"
export COMMANDO_API_KEY="your-key"
./target/debug/commando list
```

Expected: prints targets with status

- [ ] **Step 3: Test exec**

```bash
./target/debug/commando exec <some-target> "echo hello"
```

Expected: prints `hello`, exits 0

- [ ] **Step 4: Test exit code passthrough**

```bash
./target/debug/commando exec <some-target> "exit 42"
echo $?
```

Expected: exits 42

- [ ] **Step 5: Test stderr**

```bash
./target/debug/commando exec <some-target> "echo err >&2"
```

Expected: `err` printed to stderr

- [ ] **Step 6: Test ping**

```bash
./target/debug/commando ping <some-target>
```

Expected: `pong from <target> in Nms (vX.Y.Z)`

---

## Chunk 4: MCP cleanup and final checks

### Task 9: Update MCP tool descriptions

Only do this after REST endpoints and CLI are proven working.

**Files:**
- Modify: `crates/commando-gateway/src/handler.rs`

- [ ] **Step 1: Update `process_tools_list`**

Remove the `commando_exec` and `commando_output` tool entries from the `tools` array. Update `commando_list` description:

```rust
{
    "name": "commando_list",
    "description": "List all available commando targets with their status and IP. To execute commands on a target, use the Bash tool: commando exec <target> '<command>'",
    // ... inputSchema stays the same
}
```

Keep `commando_ping` unchanged.

- [ ] **Step 2: Run tests and fix any that assert on tool count**

Run: `cargo +nightly test -p commando-gateway`
Update any test expecting 4 tools to expect 2.

- [ ] **Step 3: Commit**

```bash
git add crates/commando-gateway/src/handler.rs
git commit -m "feat(gateway): remove exec/output from MCP tools list, update list description"
```

### Task 10: Run full test suite and lint

- [ ] **Step 1: Format check**

Run: `cargo +nightly fmt -- --check`

- [ ] **Step 2: Clippy**

Run: `cargo +nightly clippy -- -D warnings`

- [ ] **Step 3: All tests**

Run: `cargo +nightly test`

- [ ] **Step 4: Fix any issues and commit**

### Task 11: Update CLAUDE.md

Add CLI usage instructions to the project CLAUDE.md.

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add CLI section**

Add a section about the CLI binary and how it relates to the MCP server:

```markdown
## CLI

The `commando` CLI is a thin HTTP client that talks to the gateway's REST API.
Claude Code should use this via Bash for command execution instead of the MCP
`commando_exec` tool directly.

```bash
# Set env vars (shared with MCP config)
export COMMANDO_URL="http://akio-commando:9877"
export COMMANDO_API_KEY="your-key"

# Execute commands
commando exec <target> '<command>'
commando list
commando ping <target>
```
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: add CLI usage to CLAUDE.md"
```
