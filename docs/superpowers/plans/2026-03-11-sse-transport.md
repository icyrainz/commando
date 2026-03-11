# SSE Transport Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add HTTP/SSE transport to the commando gateway so it runs as a persistent service, replacing the per-session SSH+Docker stdio approach.

**Architecture:** Extract shared MCP handler logic from `mcp.rs` into `handler.rs`. Add `sse.rs` with an axum HTTP server that accepts SSE connections and POST requests. Shared state uses `Arc<Mutex<_>>` for axum `Send` compatibility. Transport selected via CLI flag + config (default: SSE).

**Tech Stack:** Rust, axum 0.8, tokio-stream, tokio (current_thread runtime)

**Spec:** `docs/superpowers/specs/2026-03-11-sse-transport-design.md`

---

## Chunk 1: Dependencies and Config

### Task 1: Add workspace dependencies

**Files:**
- Modify: `Cargo.toml:10-22` (workspace dependencies)
- Modify: `crates/commando-gateway/Cargo.toml:6-22` (gateway dependencies)

- [ ] **Step 1: Add axum and tokio-stream to workspace Cargo.toml**

In `Cargo.toml`, add to `[workspace.dependencies]`:

```toml
axum = "0.8"
tokio-stream = "0.1"
```

- [ ] **Step 2: Add axum and tokio-stream to gateway Cargo.toml**

In `crates/commando-gateway/Cargo.toml`, add to `[dependencies]`:

```toml
axum = { workspace = true }
tokio-stream = { workspace = true }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p commando-gateway`
Expected: compiles with no errors (deps resolve)

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/commando-gateway/Cargo.toml
git commit -m "chore: add axum and tokio-stream dependencies for SSE transport"
```

### Task 2: Add `[server]` config section

**Files:**
- Modify: `crates/commando-gateway/src/config.rs`
- Test: existing tests in `crates/commando-gateway/src/config.rs`

- [ ] **Step 1: Write failing test for server config parsing**

Add to the `tests` module in `config.rs`:

```rust
#[test]
fn parse_config_with_server_section() {
    let toml_str = r#"
[server]
transport = "sse"
bind = "127.0.0.1"
port = 9877

[proxmox]
nodes = []
user = "root@pam"
token_id = "commando"
token_secret = "xxxx"

[agent]

[agent.psk]
"#;
    let config: GatewayConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.server.transport, "sse");
    assert_eq!(config.server.bind, "127.0.0.1");
    assert_eq!(config.server.port, 9877);
}

#[test]
fn server_section_defaults() {
    let toml_str = r#"
[proxmox]
nodes = []
user = "root@pam"
token_id = "commando"
token_secret = "xxxx"

[agent]

[agent.psk]
"#;
    let config: GatewayConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.server.transport, "sse");
    assert_eq!(config.server.bind, "0.0.0.0");
    assert_eq!(config.server.port, 9877);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p commando-gateway -- config::tests::parse_config_with_server_section config::tests::server_section_defaults`
Expected: FAIL — `config.server` field doesn't exist

- [ ] **Step 3: Add ServerConfig struct and field to GatewayConfig**

Add to `config.rs` before the `impl GatewayConfig` block:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_server_port")]
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            bind: default_bind(),
            port: default_server_port(),
        }
    }
}

fn default_transport() -> String { "sse".to_string() }
fn default_bind() -> String { "0.0.0.0".to_string() }
fn default_server_port() -> u16 { 9877 }
```

Add the field to `GatewayConfig`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    pub proxmox: ProxmoxConfig,
    pub agent: AgentConnectionConfig,
    #[serde(default)]
    pub targets: Vec<ManualTarget>,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p commando-gateway -- config::tests`
Expected: all pass, including new and existing tests

- [ ] **Step 5: Update gateway.toml.example**

Add `[server]` section to `config/gateway.toml.example` at the top (before `[proxmox]`):

```toml
# MCP server transport: "sse" (HTTP, default) or "stdio" (stdin/stdout)
# [server]
# transport = "sse"
# bind = "0.0.0.0"
# port = 9877
```

- [ ] **Step 6: Commit**

```bash
git add crates/commando-gateway/src/config.rs config/gateway.toml.example
git commit -m "feat: add [server] config section for transport selection"
```

## Chunk 2: Extract handler.rs from mcp.rs

### Task 3: Extract shared MCP handler logic into handler.rs

The goal is to move all JSON-RPC dispatch logic (tool definitions, request routing, exec/list/ping handlers) out of `mcp.rs` into a new `handler.rs` that both stdio and SSE transports can call.

The key change: handler functions accept `Arc<Mutex<Registry>>`, `Arc<ConcurrencyLimiter>`, and `Arc<GatewayConfig>` instead of `Rc<RefCell<_>>` / `Rc<_>`. This makes them `Send`-compatible for axum.

**Files:**
- Create: `crates/commando-gateway/src/handler.rs`
- Modify: `crates/commando-gateway/src/mcp.rs`
- Modify: `crates/commando-gateway/src/main.rs` (add `mod handler;`, update types)

- [ ] **Step 1: Create handler.rs with the ConcurrencyLimiter (using std::sync::Mutex)**

Create `crates/commando-gateway/src/handler.rs`:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tracing::info;

use crate::config::GatewayConfig;
use crate::registry::Registry;
use crate::rpc;

/// Per-target concurrency semaphore (simple counter-based).
pub struct ConcurrencyLimiter {
    limits: Mutex<HashMap<String, usize>>,
    max_per_target: usize,
}

impl ConcurrencyLimiter {
    pub fn new(max_per_target: usize) -> Self {
        Self {
            limits: Mutex::new(HashMap::new()),
            max_per_target,
        }
    }

    pub fn try_acquire(&self, target: &str) -> bool {
        let mut limits = self.limits.lock().unwrap();
        let count = limits.entry(target.to_string()).or_insert(0);
        if *count >= self.max_per_target {
            return false;
        }
        *count += 1;
        true
    }

    pub fn release(&self, target: &str) {
        let mut limits = self.limits.lock().unwrap();
        if let Some(count) = limits.get_mut(target) {
            *count = count.saturating_sub(1);
        }
    }
}

pub fn process_initialize(request: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "commando",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

pub fn process_tools_list(request: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {
            "tools": [
                {
                    "name": "commando_exec",
                    "description": "Execute a shell command on a target machine. Returns stdout, stderr, exit code, and timing.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "target": {
                                "type": "string",
                                "description": "Fully qualified target name (e.g., 'node-1/my-app', 'my-desktop')"
                            },
                            "command": {
                                "type": "string",
                                "description": "Shell command to execute"
                            },
                            "work_dir": {
                                "type": "string",
                                "description": "Working directory (default: home dir)"
                            },
                            "timeout": {
                                "type": "number",
                                "description": "Timeout in seconds (default: 60)"
                            },
                            "env": {
                                "type": "object",
                                "description": "Additional environment variables",
                                "additionalProperties": { "type": "string" }
                            }
                        },
                        "required": ["target", "command"]
                    }
                },
                {
                    "name": "commando_list",
                    "description": "List all registered targets with their status, shell, tags, and reachability.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "filter": {
                                "type": "string",
                                "description": "Case-insensitive substring match against target name and tags"
                            }
                        }
                    }
                },
                {
                    "name": "commando_ping",
                    "description": "Health check a specific agent. Returns hostname, uptime, shell, and version.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "target": {
                                "type": "string",
                                "description": "Fully qualified target name"
                            }
                        },
                        "required": ["target"]
                    }
                }
            ]
        }
    })
}

pub fn make_error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn make_tool_result(id: &Value, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [
                {
                    "type": "text",
                    "text": text
                }
            ]
        }
    })
}

fn make_tool_error(id: &Value, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [
                {
                    "type": "text",
                    "text": text
                }
            ],
            "isError": true
        }
    })
}

/// Dispatch a single JSON-RPC request and return the response.
/// This is the core handler used by both stdio and SSE transports.
pub async fn dispatch_request(
    request: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
    limiter: &Arc<ConcurrencyLimiter>,
) -> Option<Value> {
    let method = request["method"].as_str().unwrap_or("");
    let id = &request["id"];

    // JSON-RPC 2.0 notifications have no "id" field — never respond
    if request.get("id").is_none() || request["id"].is_null() {
        return None;
    }

    let response = match method {
        "initialize" => process_initialize(request),
        "tools/list" => process_tools_list(request),
        "tools/call" => handle_tools_call(request, config, registry, limiter).await,
        _ => make_error_response(id.clone(), -32601, &format!("Method not found: {method}")),
    };

    Some(response)
}

async fn handle_tools_call(
    request: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
    limiter: &Arc<ConcurrencyLimiter>,
) -> Value {
    let id = &request["id"];
    let tool_name = request["params"]["name"].as_str().unwrap_or("");
    let args = &request["params"]["arguments"];

    match tool_name {
        "commando_exec" => handle_exec(id, args, config, registry, limiter).await,
        "commando_list" => handle_list(id, args, config, registry),
        "commando_ping" => handle_ping(id, args, config, registry).await,
        _ => make_tool_error(id, &format!("Unknown tool: {tool_name}")),
    }
}

async fn handle_exec(
    id: &Value,
    args: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
    limiter: &Arc<ConcurrencyLimiter>,
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
    let timeout_secs = args["timeout"].as_u64().unwrap_or(config.agent.default_timeout_secs as u64) as u32;

    let (host, port) = {
        let reg = registry.lock().unwrap();
        match reg.get(target_name) {
            Some(t) => (t.host.clone(), t.port),
            None => return make_tool_error(id, &format!("unknown target: {target_name}")),
        }
    };

    let psk = match config.agent.psk.get(target_name) {
        Some(p) => p.clone(),
        None => return make_tool_error(id, &format!("no PSK configured for target: {target_name}")),
    };

    if !limiter.try_acquire(target_name) {
        return make_tool_error(id, &format!("concurrency limit reached for target: {target_name}"));
    }

    let extra_env: Vec<(String, String)> = args["env"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let request_id = uuid::Uuid::new_v4().to_string();

    info!(
        target = target_name,
        command = &command[..command.len().min(200)],
        request_id = %request_id,
        "executing command"
    );

    let result = rpc::remote_exec(
        &host, port, &psk, command, work_dir, timeout_secs,
        &extra_env, &request_id, config.agent.connect_timeout_secs,
    ).await;

    limiter.release(target_name);

    match result {
        Ok(r) => {
            let stdout = String::from_utf8_lossy(&r.stdout);
            let stderr = String::from_utf8_lossy(&r.stderr);

            let mut text = String::new();
            if !stdout.is_empty() {
                text.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !text.is_empty() { text.push('\n'); }
                text.push_str("[stderr]\n");
                text.push_str(&stderr);
            }
            if r.timed_out { text.push_str("\n[timed out]"); }
            if r.truncated { text.push_str("\n[output truncated]"); }

            let metadata = format!(
                "\n---\nexit_code: {} | duration: {}ms | request_id: {}",
                r.exit_code, r.duration_ms, r.request_id
            );
            text.push_str(&metadata);

            if r.exit_code != 0 || r.timed_out {
                make_tool_error(id, &text)
            } else {
                make_tool_result(id, &text)
            }
        }
        Err(e) => make_tool_error(id, &format!("exec failed: {e}")),
    }
}

fn handle_list(
    id: &Value,
    args: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
) -> Value {
    let filter = args["filter"].as_str();
    let reg = registry.lock().unwrap();

    let targets: Vec<Value> = reg
        .list(filter)
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "host": t.host,
                "port": t.port,
                "shell": t.shell,
                "tags": t.tags,
                "source": format!("{:?}", t.source),
                "status": t.status,
                "reachable": format!("{:?}", t.reachable),
                "has_psk": config.agent.psk.contains_key(&t.name),
            })
        })
        .collect();

    make_tool_result(id, &serde_json::to_string_pretty(&targets).unwrap_or_default())
}

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

    let (host, port) = {
        let reg = registry.lock().unwrap();
        match reg.get(target_name) {
            Some(t) => (t.host.clone(), t.port),
            None => return make_tool_error(id, &format!("unknown target: {target_name}")),
        }
    };

    let psk = match config.agent.psk.get(target_name) {
        Some(p) => p.clone(),
        None => return make_tool_error(id, &format!("no PSK configured for target: {target_name}")),
    };

    match rpc::remote_ping(&host, port, &psk, config.agent.connect_timeout_secs).await {
        Ok(r) => {
            let text = format!(
                "hostname: {}\nuptime: {}s\nshell: {}\nversion: {}",
                r.hostname, r.uptime_secs, r.shell, r.version
            );
            make_tool_result(id, &text)
        }
        Err(e) => make_tool_error(id, &format!("ping failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_initialize() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            }
        });
        let response = process_initialize(&request);
        assert_eq!(response["id"], 1);
        assert!(response["result"]["capabilities"]["tools"].is_object());
        assert_eq!(response["result"]["serverInfo"]["name"], "commando");
    }

    #[test]
    fn handle_tools_list() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let response = process_tools_list(&request);
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"commando_exec"));
        assert!(names.contains(&"commando_list"));
        assert!(names.contains(&"commando_ping"));
    }

    #[test]
    fn tool_schemas_have_required_fields() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let response = process_tools_list(&request);
        let tools = response["result"]["tools"].as_array().unwrap();

        for tool in tools {
            assert!(tool["name"].is_string(), "tool missing name");
            assert!(tool["description"].is_string(), "tool missing description");
            assert!(tool["inputSchema"].is_object(), "tool missing inputSchema");
        }
    }

    #[test]
    fn error_for_unknown_method() {
        let response = make_error_response(json!(99), -32601, "Method not found");
        assert_eq!(response["id"], 99);
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn concurrency_limiter_acquire_release() {
        let limiter = ConcurrencyLimiter::new(2);
        assert!(limiter.try_acquire("target-1"));
        assert!(limiter.try_acquire("target-1"));
        assert!(!limiter.try_acquire("target-1")); // limit reached
        limiter.release("target-1");
        assert!(limiter.try_acquire("target-1")); // slot freed
    }

    #[test]
    fn concurrency_limiter_independent_targets() {
        let limiter = ConcurrencyLimiter::new(1);
        assert!(limiter.try_acquire("target-1"));
        assert!(limiter.try_acquire("target-2")); // different target, independent
        assert!(!limiter.try_acquire("target-1")); // same target, blocked
    }

    #[test]
    fn dispatch_returns_none_for_notifications() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let config = Arc::new(GatewayConfig {
                server: Default::default(),
                proxmox: crate::config::ProxmoxConfig {
                    nodes: vec![],
                    user: String::new(),
                    token_id: String::new(),
                    token_secret: String::new(),
                    discovery_interval_secs: 60,
                },
                agent: crate::config::AgentConnectionConfig {
                    default_port: 9876,
                    default_timeout_secs: 60,
                    connect_timeout_secs: 5,
                    max_concurrent_per_target: 4,
                    psk: Default::default(),
                },
                targets: vec![],
            });
            let registry = Arc::new(Mutex::new(crate::registry::Registry::new()));
            let limiter = Arc::new(ConcurrencyLimiter::new(4));

            // Notification (no id)
            let notification = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
            let result = dispatch_request(&notification, &config, &registry, &limiter).await;
            assert!(result.is_none());

            // Regular request (has id)
            let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
            let result = dispatch_request(&request, &config, &registry, &limiter).await;
            assert!(result.is_some());
        });
    }
}
```

- [ ] **Step 2: Add `mod handler;` to main.rs**

In `crates/commando-gateway/src/main.rs`, add `mod handler;` after the existing mod declarations (line 1):

```rust
mod config;
mod handler;
mod mcp;
mod proxmox;
mod registry;
mod rpc;
```

- [ ] **Step 3: Run handler tests to verify they pass**

Run: `cargo test -p commando-gateway -- handler::tests`
Expected: all pass

- [ ] **Step 4: Rewrite mcp.rs to use handler functions**

Replace the entire `mcp.rs` content. The stdio transport becomes a thin wrapper:

```rust
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::GatewayConfig;
use crate::handler;
use crate::registry::Registry;

/// Stdio MCP server loop. Reads JSON-RPC from stdin, writes responses to stdout.
pub async fn run_stdio_loop(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
) -> Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut lines = stdin.lines();

    while let Ok(Some(line)) = lines.next_line().await as tokio::io::Result<Option<String>> {
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = handler::make_error_response(
                    Value::Null,
                    -32700,
                    &format!("Parse error: {e}"),
                );
                write_response(&mut stdout, &err).await?;
                continue;
            }
        };

        if let Some(response) = handler::dispatch_request(&request, &config, &registry, &limiter).await {
            write_response(&mut stdout, &response).await?;
        }
    }

    Ok(())
}

async fn write_response(stdout: &mut tokio::io::Stdout, response: &Value) -> Result<()> {
    let json = serde_json::to_string(response)?;
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}
```

- [ ] **Step 5: Update main.rs to use Arc<Mutex<_>> types**

Replace `run_gateway` function in `main.rs` to use the new types. Key changes:
- `Rc<GatewayConfig>` → `Arc<GatewayConfig>`
- `Rc<RefCell<Registry>>` → `Arc<Mutex<Registry>>`
- `Rc<ConcurrencyLimiter>` (from mcp) → `Arc<handler::ConcurrencyLimiter>`
- Replace `use std::cell::RefCell; use std::rc::Rc;` with `use std::sync::{Arc, Mutex};`
- Update all `.borrow()` / `.borrow_mut()` calls to `.lock().unwrap()`
- The `run_mcp_loop` call becomes `mcp::run_stdio_loop`

The full updated `main.rs`:

```rust
mod config;
mod handler;
mod mcp;
mod proxmox;
mod registry;
mod rpc;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};

use registry::{DiscoveredTarget, Registry};

#[derive(Parser)]
#[command(name = "commando-gateway", about = "Commando MCP gateway")]
struct Cli {
    #[arg(long, default_value = "/etc/commando/gateway.toml")]
    config: std::path::PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Arc::new(config::GatewayConfig::load(&cli.config)?);

    // Structured JSON logging to stderr (stdout is for MCP protocol)
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("commando_gateway=info".parse().unwrap()),
        )
        .with_target(false)
        .init();

    info!(
        proxmox_nodes = config.proxmox.nodes.len(),
        manual_targets = config.targets.len(),
        "starting commando-gateway v{}",
        env!("CARGO_PKG_VERSION"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, run_gateway(config))
}

async fn run_gateway(config: Arc<config::GatewayConfig>) -> Result<()> {
    // Build initial registry from manual targets
    let manual_inputs: Vec<registry::ManualTargetInput> = config
        .targets
        .iter()
        .map(|t| registry::ManualTargetInput {
            name: t.name.clone(),
            host: t.host.clone(),
            port: t.port,
            shell: t.shell.clone(),
            tags: t.tags.clone(),
        })
        .collect();

    let registry = Arc::new(Mutex::new(Registry::from_manual(manual_inputs)));

    // Try to load cached registry
    let cache_path = std::path::Path::new("/var/lib/commando/registry.json");
    if cache_path.exists() {
        match std::fs::read_to_string(cache_path) {
            Ok(json) => match Registry::from_cache_json(&json) {
                Ok(cached) => {
                    let discovered: Vec<DiscoveredTarget> = cached
                        .targets
                        .values()
                        .filter(|t| t.source == registry::TargetSource::Discovered)
                        .map(|t| DiscoveredTarget {
                            name: t.name.clone(),
                            host: t.host.clone(),
                            port: t.port,
                            status: t.status.clone(),
                        })
                        .collect();
                    registry.lock().unwrap().update_discovered(discovered);
                    info!("loaded cached registry from disk");
                }
                Err(e) => warn!(error = %e, "failed to parse cached registry"),
            },
            Err(e) => warn!(error = %e, "failed to read registry cache"),
        }
    } else if !config.proxmox.nodes.is_empty() {
        info!("no registry cache, running initial discovery");
        run_discovery_cycle(&config, &registry).await;
    }

    let limiter = Arc::new(handler::ConcurrencyLimiter::new(
        config.agent.max_concurrent_per_target,
    ));

    // Spawn background discovery loop
    if !config.proxmox.nodes.is_empty() {
        let config_clone = config.clone();
        let registry_clone = registry.clone();
        tokio::task::spawn_local(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                config_clone.proxmox.discovery_interval_secs,
            ));
            interval.tick().await; // Skip immediate first tick
            loop {
                interval.tick().await;
                run_discovery_cycle(&config_clone, &registry_clone).await;
            }
        });
    }

    // Run MCP server on stdio
    mcp::run_stdio_loop(config, registry, limiter).await
}

async fn run_discovery_cycle(
    config: &config::GatewayConfig,
    registry: &Arc<Mutex<Registry>>,
) {
    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // Proxmox uses self-signed certs
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let mut all_discovered = Vec::new();

    for node in &config.proxmox.nodes {
        match proxmox::discover_node(&http_client, node, &config.proxmox, config.agent.default_port).await {
            Ok(targets) => {
                info!(node = %node.name, count = targets.len(), "discovered LXC targets");
                all_discovered.extend(targets);
            }
            Err(e) => {
                error!(node = %node.name, error = %e, "Proxmox discovery failed");
            }
        }
    }

    registry.lock().unwrap().update_discovered(all_discovered);

    // Ping all targets with PSKs to check reachability
    let targets_to_ping: Vec<(String, String, u16, String)> = {
        let reg = registry.lock().unwrap();
        reg.targets
            .values()
            .filter_map(|t| {
                config.agent.psk.get(&t.name)
                    .map(|psk| (t.name.clone(), t.host.clone(), t.port, psk.clone()))
            })
            .collect()
    };

    let ping_futures: Vec<_> = targets_to_ping
        .iter()
        .map(|(name, host, port, psk)| {
            let name = name.clone();
            let host = host.clone();
            let port = *port;
            let psk = psk.clone();
            let connect_timeout = config.agent.connect_timeout_secs;
            async move {
                let reachable = rpc::remote_ping(&host, port, &psk, connect_timeout).await.is_ok();
                (name, reachable)
            }
        })
        .collect();

    let ping_results = futures::future::join_all(ping_futures).await;
    {
        let mut reg = registry.lock().unwrap();
        for (name, reachable) in ping_results {
            reg.set_reachable(
                &name,
                if reachable { registry::Reachability::Reachable } else { registry::Reachability::Unreachable },
            );
        }
    }

    // Save cache to disk
    let cache_dir = std::path::Path::new("/var/lib/commando");
    if let Err(e) = std::fs::create_dir_all(cache_dir) {
        warn!(error = %e, "failed to create cache directory");
        return;
    }
    let cache_path = cache_dir.join("registry.json");
    match registry.lock().unwrap().to_cache_json() {
        Ok(json) => {
            if let Err(e) = std::fs::write(&cache_path, json) {
                warn!(error = %e, "failed to write registry cache");
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize registry cache"),
    }
}
```

- [ ] **Step 6: Run all existing tests**

Run: `cargo test -p commando-gateway`
Expected: all tests pass (handler, config, registry, mcp)

- [ ] **Step 7: Verify full build**

Run: `cargo build -p commando-gateway`
Expected: compiles with no errors

- [ ] **Step 8: Commit**

```bash
git add crates/commando-gateway/src/handler.rs crates/commando-gateway/src/mcp.rs crates/commando-gateway/src/main.rs
git commit -m "refactor: extract MCP handler logic into handler.rs, switch to Arc<Mutex>"
```

## Chunk 3: Make RPC calls context-agnostic

### Task 4: Wrap RPC functions in LocalSet for spawn_local compatibility

**Critical:** `rpc.rs` uses `tokio::task::spawn_local` for Cap'n Proto RpcSystem (which is `!Send`). This works when called from within a `LocalSet` (stdio path), but `axum::serve` dispatches handlers via `tokio::spawn` (outside any `LocalSet`). Calling `spawn_local` from outside a `LocalSet` **panics at runtime**.

**Fix:** Wrap each RPC function body in its own `LocalSet::run_until()`. This creates a temporary `LocalSet` context for the `spawn_local` calls, making the functions callable from any context (both `tokio::spawn` and `spawn_local`).

**Files:**
- Modify: `crates/commando-gateway/src/rpc.rs`

- [ ] **Step 1: Wrap `remote_exec` body in LocalSet::run_until**

In `rpc.rs`, change `remote_exec` to wrap its entire body:

```rust
pub async fn remote_exec(
    host: &str,
    port: u16,
    psk: &str,
    command: &str,
    work_dir: &str,
    timeout_secs: u32,
    extra_env: &[(String, String)],
    request_id: &str,
    connect_timeout_secs: u64,
) -> Result<RemoteExecResult> {
    let local = tokio::task::LocalSet::new();
    local.run_until(async {
        // ... entire existing body unchanged ...
    }).await
}
```

The body inside `run_until` stays exactly the same — only the outer wrapping changes.

- [ ] **Step 2: Wrap `remote_ping` body in LocalSet::run_until**

Same pattern for `remote_ping`:

```rust
pub async fn remote_ping(
    host: &str,
    port: u16,
    psk: &str,
    connect_timeout_secs: u64,
) -> Result<RemotePingResult> {
    let local = tokio::task::LocalSet::new();
    local.run_until(async {
        // ... entire existing body unchanged ...
    }).await
}
```

- [ ] **Step 3: Verify tests still pass**

Run: `cargo test -p commando-gateway`
Expected: all pass (no functional change, just wrapping)

- [ ] **Step 4: Verify full build**

Run: `cargo build -p commando-gateway`
Expected: compiles with no errors

- [ ] **Step 5: Commit**

```bash
git add crates/commando-gateway/src/rpc.rs
git commit -m "fix: wrap RPC calls in LocalSet for spawn_local context independence"
```

## Chunk 4: SSE Transport Module

### Task 5: Implement sse.rs

**Files:**
- Create: `crates/commando-gateway/src/sse.rs`
- Modify: `crates/commando-gateway/src/main.rs` (add `mod sse;`)

- [ ] **Step 1: Create sse.rs with the SSE server**

Create `crates/commando-gateway/src/sse.rs`:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn};

use crate::config::GatewayConfig;
use crate::handler;
use crate::registry::Registry;

type SessionMap = Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<String>>>>;

#[derive(Clone)]
struct AppState {
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
    sessions: SessionMap,
}

#[derive(Deserialize)]
struct MessageQuery {
    session_id: String,
}

pub async fn run_sse_server(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
) -> Result<()> {
    let state = AppState {
        config: config.clone(),
        registry,
        limiter,
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/sse", get(handle_sse))
        .route("/messages", post(handle_message))
        .route("/health", get(handle_health))
        .with_state(state);

    let addr = format!("{}:{}", config.server.bind, config.server.port);
    let listener = TcpListener::bind(&addr).await?;
    info!(addr = %addr, "SSE server listening");

    let shutdown = async {
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ).expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
        info!("shutting down SSE server");
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    Ok(())
}

async fn handle_sse(
    State(state): State<AppState>,
) -> Sse<ReceiverStream<Result<Event, std::convert::Infallible>>> {
    let session_id = uuid::Uuid::new_v4().to_string().replace("-", "");
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(32);

    // Store the session sender (for messages, we need a separate channel)
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<String>(32);
    state.sessions.lock().unwrap().insert(session_id.clone(), msg_tx);

    let endpoint_url = format!("/messages?session_id={session_id}");
    let session_id_clone = session_id.clone();
    let sessions = state.sessions.clone();

    // Spawn task to forward message channel to SSE events
    // Use spawn_local to stay within the LocalSet context (required for Cap'n Proto RPC)
    tokio::task::spawn_local(async move {
        // Send the endpoint event first
        let endpoint_event = Event::default()
            .event("endpoint")
            .data(&endpoint_url);
        if tx.send(Ok(endpoint_event)).await.is_err() {
            return;
        }

        // Forward JSON-RPC responses as SSE message events
        while let Some(data) = msg_rx.recv().await {
            let event = Event::default()
                .event("message")
                .data(data);
            if tx.send(Ok(event)).await.is_err() {
                break;
            }
        }

        // Cleanup session on disconnect
        sessions.lock().unwrap().remove(&session_id_clone);
        info!(session_id = %session_id_clone, "SSE session closed");
    });

    info!(session_id = %session_id, "SSE session opened");

    Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
}

async fn handle_message(
    State(state): State<AppState>,
    Query(query): Query<MessageQuery>,
    body: String,
) -> Response {
    let session_id = &query.session_id;

    // Look up the session sender
    let sender = {
        let sessions = state.sessions.lock().unwrap();
        sessions.get(session_id).cloned()
    };

    let sender = match sender {
        Some(s) => s,
        None => {
            warn!(session_id = %session_id, "unknown session");
            return (
                axum::http::StatusCode::NOT_FOUND,
                "Could not find session",
            ).into_response();
        }
    };

    // Parse JSON-RPC request
    let request: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            let err = handler::make_error_response(
                Value::Null,
                -32700,
                &format!("Parse error: {e}"),
            );
            let json_str = serde_json::to_string(&err).unwrap_or_default();
            if sender.send(json_str).await.is_err() {
                // Session disconnected
                state.sessions.lock().unwrap().remove(session_id);
            }
            return axum::http::StatusCode::ACCEPTED.into_response();
        }
    };

    // Dispatch to handler
    if let Some(response) = handler::dispatch_request(
        &request,
        &state.config,
        &state.registry,
        &state.limiter,
    ).await {
        let json_str = serde_json::to_string(&response).unwrap_or_default();
        if sender.send(json_str).await.is_err() {
            // Session disconnected during processing
            state.sessions.lock().unwrap().remove(session_id);
            return (
                axum::http::StatusCode::NOT_FOUND,
                "Session disconnected",
            ).into_response();
        }
    }

    axum::http::StatusCode::ACCEPTED.into_response()
}

async fn handle_health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}
```

- [ ] **Step 2: Add `mod sse;` to main.rs**

In `main.rs`, add after `mod mcp;`:

```rust
mod config;
mod handler;
mod mcp;
mod proxmox;
mod registry;
mod rpc;
mod sse;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p commando-gateway`
Expected: compiles (may have unused warnings for sse module — that's fine)

- [ ] **Step 4: Commit**

```bash
git add crates/commando-gateway/src/sse.rs crates/commando-gateway/src/main.rs
git commit -m "feat: add SSE transport module with axum HTTP server"
```

### Task 6: Wire up transport selection in main.rs

**Files:**
- Modify: `crates/commando-gateway/src/main.rs`

- [ ] **Step 1: Add --transport, --bind, --port CLI args**

Update the `Cli` struct:

```rust
#[derive(Parser)]
#[command(name = "commando-gateway", about = "Commando MCP gateway")]
struct Cli {
    #[arg(long, default_value = "/etc/commando/gateway.toml")]
    config: std::path::PathBuf,

    /// MCP transport: "sse" or "stdio"
    #[arg(long)]
    transport: Option<String>,

    /// HTTP bind address (SSE only)
    #[arg(long)]
    bind: Option<String>,

    /// HTTP port (SSE only)
    #[arg(long)]
    port: Option<u16>,
}
```

- [ ] **Step 2: Apply CLI overrides to config and branch on transport**

After loading config in `main()`, apply CLI overrides:

```rust
fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = config::GatewayConfig::load(&cli.config)?;

    // CLI overrides for server settings
    if let Some(transport) = &cli.transport {
        config.server.transport = transport.clone();
    }
    if let Some(bind) = &cli.bind {
        config.server.bind = bind.clone();
    }
    if let Some(port) = cli.port {
        config.server.port = port;
    }

    let config = Arc::new(config);

    // ... (logging setup unchanged) ...
```

- [ ] **Step 3: Branch on transport in run_gateway**

Replace the last line of `run_gateway` (currently `mcp::run_stdio_loop(...)`) with:

```rust
    // Run MCP server on selected transport
    match config.server.transport.as_str() {
        "stdio" => mcp::run_stdio_loop(config, registry, limiter).await,
        "sse" => sse::run_sse_server(config, registry, limiter).await,
        other => anyhow::bail!("unknown transport: {other} (expected 'stdio' or 'sse')"),
    }
```

- [ ] **Step 4: Update the info log to include transport**

Update the startup log:

```rust
    info!(
        proxmox_nodes = config.proxmox.nodes.len(),
        manual_targets = config.targets.len(),
        transport = %config.server.transport,
        "starting commando-gateway v{}",
        env!("CARGO_PKG_VERSION"),
    );
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo build -p commando-gateway`
Expected: compiles with no errors

- [ ] **Step 6: Run all tests**

Run: `cargo test -p commando-gateway`
Expected: all pass

- [ ] **Step 7: Commit**

```bash
git add crates/commando-gateway/src/main.rs
git commit -m "feat: wire up transport selection with CLI overrides"
```

## Chunk 5: Integration Testing and Deployment

### Task 7: Manual integration test

This task verifies the SSE transport works end-to-end using curl before deploying.

- [ ] **Step 1: Build the gateway**

Run: `cargo build -p commando-gateway`

- [ ] **Step 2: Create a minimal test config**

Create a temporary config file at `/tmp/commando-test.toml`:

```toml
[server]
transport = "sse"
bind = "127.0.0.1"
port = 9877

[proxmox]
nodes = []
user = "test"
token_id = "test"
token_secret = "test"

[agent]

[agent.psk]
```

- [ ] **Step 3: Start the gateway in SSE mode**

Run in one terminal:
```bash
RUST_LOG=commando_gateway=debug cargo run -p commando-gateway -- --config /tmp/commando-test.toml
```

Expected: logs "SSE server listening" on stderr

- [ ] **Step 4: Test SSE connection with curl**

In another terminal:
```bash
curl -N http://127.0.0.1:9877/sse
```

Expected: receives an SSE event like:
```
event: endpoint
data: /messages?session_id=<hex>
```

- [ ] **Step 5: Test POST initialize**

Using the session_id from the previous step:
```bash
curl -X POST "http://127.0.0.1:9877/messages?session_id=<hex>" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}'
```

Expected: 202 response, and the SSE stream in the first terminal shows:
```
event: message
data: {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05",...}}
```

- [ ] **Step 6: Test health endpoint**

```bash
curl http://127.0.0.1:9877/health
```

Expected: `{"status":"ok"}`

- [ ] **Step 7: Test tools/list**

```bash
curl -X POST "http://127.0.0.1:9877/messages?session_id=<hex>" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
```

Expected: SSE stream receives message event with 3 tools listed

- [ ] **Step 8: Test invalid session**

```bash
curl -X POST "http://127.0.0.1:9877/messages?session_id=bogus" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize"}'
```

Expected: 404 "Could not find session"

### Task 8: Build and deploy to akio-commando

**Files:**
- Modify: config on akio-commando (`/etc/commando/gateway.toml`)

- [ ] **Step 1: Build static musl binary**

```bash
cargo build --release --target x86_64-unknown-linux-musl
```

- [ ] **Step 2: Build Docker image**

```bash
docker build -f Dockerfile.gateway -t commando-gateway .
```

- [ ] **Step 3: Export and deploy to akio-commando**

```bash
docker save commando-gateway | ssh root@akio-commando docker load
```

- [ ] **Step 4: Update gateway.toml on akio-commando**

Add `[server]` section to `/etc/commando/gateway.toml`:

```toml
[server]
transport = "sse"
bind = "0.0.0.0"
port = 9877
```

- [ ] **Step 5: Restart the gateway container**

```bash
ssh root@akio-commando "docker stop commando-gateway 2>/dev/null; docker rm commando-gateway 2>/dev/null; docker run -d --name commando-gateway --restart unless-stopped --network host -v /etc/commando:/etc/commando:ro -v /var/lib/commando:/var/lib/commando commando-gateway --config /etc/commando/gateway.toml"
```

- [ ] **Step 6: Verify it's running**

```bash
ssh root@akio-commando "docker logs commando-gateway --tail 5"
curl http://akio-commando:9877/health
```

Expected: logs show "SSE server listening", health returns `{"status":"ok"}`

- [ ] **Step 7: Update Claude Code MCP config**

In `~/.claude.json`, replace the commando MCP server config:

```json
"commando": {
  "type": "sse",
  "url": "http://akio-commando:9877/sse"
}
```

- [ ] **Step 8: Test with Claude Code**

Start a new Claude Code session and try:
- `commando_list` — should show targets
- `commando_ping` on a target — should return hostname/uptime
- `commando_exec` on a target — should execute command

- [ ] **Step 9: Commit any final tweaks**

```bash
git add -A
git commit -m "feat: SSE transport complete and deployed"
```
