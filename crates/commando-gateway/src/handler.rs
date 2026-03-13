use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;
use tracing::info;

use crate::config::{GatewayConfig, StreamingConfig};
use crate::registry::Registry;
use crate::rpc;
use crate::session::SessionMap;

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
                    "description": "Execute a shell command on a target machine. If the response includes a next_page field, the command is still running — call commando_output with the page token to get more output.",
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
                },
                {
                    "name": "commando_output",
                    "description": "Get the next page of output from a streaming command. Use when commando_exec returns a next_page token.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["page"],
                        "properties": {
                            "page": {
                                "type": "string",
                                "description": "Page token from previous commando_exec or commando_output response"
                            }
                        }
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

/// Dispatch a JSON-RPC request to the appropriate handler.
/// Returns `None` for notifications (no "id" or null id), `Some(response)` for requests.
pub async fn dispatch_request(
    request: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
    limiter: &Arc<ConcurrencyLimiter>,
    session_map: &Rc<RefCell<SessionMap>>,
) -> Option<Value> {
    let method = request["method"].as_str().unwrap_or("");
    let id = &request["id"];

    // JSON-RPC 2.0 notifications have no "id" field — never respond to them
    if request.get("id").is_none() || request["id"].is_null() {
        return None;
    }

    let response = match method {
        "initialize" => process_initialize(request),
        "tools/list" => process_tools_list(request),
        "tools/call" => handle_tools_call(request, config, registry, limiter, session_map).await,
        _ => make_error_response(id.clone(), -32601, &format!("Method not found: {method}")),
    };

    Some(response)
}

async fn handle_tools_call(
    request: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
    limiter: &Arc<ConcurrencyLimiter>,
    session_map: &Rc<RefCell<SessionMap>>,
) -> Value {
    let id = &request["id"];
    let tool_name = request["params"]["name"].as_str().unwrap_or("");
    let args = &request["params"]["arguments"];

    match tool_name {
        "commando_exec" => handle_exec(id, args, config, registry, limiter, session_map).await,
        "commando_list" => handle_list(id, args, config, registry),
        "commando_ping" => handle_ping(id, args, config, registry).await,
        "commando_output" => handle_output(id, args, session_map, &config.streaming).await,
        _ => make_tool_error(id, &format!("Unknown tool: {tool_name}")),
    }
}

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
    let timeout_secs = args["timeout"]
        .as_u64()
        .unwrap_or(config.agent.default_timeout_secs as u64) as u32;

    let (host, port, status) = {
        let reg = registry.lock().unwrap();
        match reg.get(target_name) {
            Some(t) => (t.host.clone(), t.port, t.status.clone()),
            None => return make_tool_error(id, &format!("unknown target: {target_name}")),
        }
    };

    if host.is_empty() {
        return make_tool_error(
            id,
            &format!("target '{}' is {} (no IP available)", target_name, status),
        );
    }

    let psk = match config.agent.psk.get(target_name) {
        Some(p) => p.clone(),
        None => {
            return make_tool_error(id, &format!("no PSK configured for target: {target_name}"));
        }
    };

    if !limiter.try_acquire(target_name) {
        return make_tool_error(
            id,
            &format!("concurrency limit reached for target: {target_name}"),
        );
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

    // Create a streaming session
    let (token, session_id) = session_map.borrow_mut().create_session();

    // Start the streaming RPC (the spawned task releases the concurrency slot via RAII guard)
    let join_handle = rpc::start_remote_exec_stream(
        &host,
        port,
        &psk,
        command,
        work_dir,
        timeout_secs,
        &extra_env,
        &request_id,
        config.agent.connect_timeout_secs,
        session_map.clone(),
        session_id.clone(),
        limiter.clone(),
        target_name.to_string(),
    );

    // Store the JoinHandle so cleanup can abort it if needed
    {
        let mut map = session_map.borrow_mut();
        if let Some(session) = map.get_by_id_mut(&session_id) {
            session.rpc_task = Some(join_handle);
        } else {
            // Session was unexpectedly removed; abort the spawned task and release slot
            join_handle.abort();
            return make_tool_error(id, "session lost before execution started");
        }
    }

    // Build and return the first page
    match build_page(session_map, &token, &config.streaming).await {
        Ok(page) => format_page_response(id, &page),
        Err(e) => make_tool_error(id, &format!("exec failed: {e}")),
    }
}

/// Convert a page response JSON into MCP tool result text.
fn format_page_response(id: &Value, page: &Value) -> Value {
    let mut text = String::new();

    if let Some(stdout) = page["stdout"].as_str()
        && !stdout.is_empty()
    {
        text.push_str(stdout);
    }

    if let Some(stderr) = page["stderr"].as_str()
        && !stderr.is_empty()
    {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(stderr);
    }

    if page["timed_out"].as_bool().unwrap_or(false) {
        text.push_str("\n[timed out]");
    }

    // Final page: include metadata footer with exit code and duration
    if let Some(exit_code) = page["exit_code"].as_i64() {
        let duration_ms = page["duration_ms"].as_u64().unwrap_or(0);
        let metadata = format!(
            "\n---\nexit_code: {} | duration: {}ms",
            exit_code, duration_ms
        );
        text.push_str(&metadata);
    }

    // Streaming: include next_page token if still running
    if let Some(next_page) = page["next_page"].as_str() {
        text.push_str(&format!("\n[streaming] next_page={next_page}"));
    }

    let is_error = page["exit_code"].as_i64().is_some_and(|c| c != 0)
        || page["timed_out"].as_bool().unwrap_or(false);

    if is_error {
        make_tool_error(id, &text)
    } else {
        make_tool_result(id, &text)
    }
}

/// Build a page of streaming output from a session.
///
/// Phase 1: Wait for data to become available (or completion/timeout).
/// Phase 2: Drain buffers up to page_max_bytes and build the response.
async fn build_page(
    session_map: &Rc<RefCell<SessionMap>>,
    token: &str,
    config: &StreamingConfig,
) -> Result<Value, String> {
    let page_timeout = Duration::from_secs(config.page_timeout_secs);
    let page_max = config.page_max_bytes;
    let deadline = Instant::now() + page_timeout;

    // Phase 1: Wait until data is available, completed, or timeout expires.
    loop {
        let (has_data, completed, notify) = {
            let map = session_map.borrow();
            let session = map
                .get_by_token(token)
                .ok_or_else(|| "invalid or expired page token".to_string())?;
            (
                session.total_buffered() > 0,
                session.completed,
                session.notify.clone(),
            )
        };

        if has_data || completed {
            break;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        // Wait for notification or timeout
        let _ = tokio::time::timeout(remaining, notify.notified()).await;
        // Re-loop to check if data is now available
    }

    // Phase 2: Drain buffers up to page_max_bytes.
    let mut map = session_map.borrow_mut();
    let session = map
        .get_by_token_mut(token)
        .ok_or_else(|| "invalid or expired page token".to_string())?;
    session.touch();

    let stdout_bytes = session.drain_stdout_up_to(page_max);
    let remaining_budget = page_max.saturating_sub(stdout_bytes.len());
    let stderr_bytes = session.drain_stderr_up_to(remaining_budget);

    let has_remaining = session.total_buffered() > 0;
    let completed = session.completed;
    // Only treat as final page when command is done AND all buffered data has been drained.
    // Otherwise a command completing with >page_max_bytes output would lose the tail.
    let exec_result_data = if completed && !has_remaining {
        session
            .exec_result
            .as_ref()
            .map(|r| (r.exit_code, r.duration_ms, r.timed_out))
    } else {
        None
    };

    // If there's remaining buffered data, re-notify so next poll returns immediately
    if has_remaining {
        session.notify.notify_one();
    }

    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    // Need to drop the mutable borrow before potentially removing the session
    drop(map);

    if let Some((exit_code, duration_ms, timed_out)) = exec_result_data {
        // Final page: remove session from map
        session_map.borrow_mut().remove_by_token(token);

        Ok(json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
            "duration_ms": duration_ms,
            "timed_out": timed_out,
        }))
    } else {
        // Still running: rotate token
        let new_token = session_map
            .borrow_mut()
            .rotate_token(token)
            .ok_or_else(|| "session disappeared during token rotation".to_string())?;

        Ok(json!({
            "stdout": stdout,
            "stderr": stderr,
            "next_page": new_token,
        }))
    }
}

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

    match build_page(session_map, token, config).await {
        Ok(page) => format_page_response(id, &page),
        Err(e) => make_tool_error(id, &e),
    }
}

fn handle_list(
    id: &Value,
    args: &Value,
    config: &GatewayConfig,
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

    make_tool_result(
        id,
        &serde_json::to_string_pretty(&targets).unwrap_or_default(),
    )
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

    let (host, port, status) = {
        let reg = registry.lock().unwrap();
        match reg.get(target_name) {
            Some(t) => (t.host.clone(), t.port, t.status.clone()),
            None => return make_tool_error(id, &format!("unknown target: {target_name}")),
        }
    };

    if host.is_empty() {
        return make_tool_error(
            id,
            &format!("target '{}' is {} (no IP available)", target_name, status),
        );
    }

    let psk = match config.agent.psk.get(target_name) {
        Some(p) => p.clone(),
        None => {
            return make_tool_error(id, &format!("no PSK configured for target: {target_name}"));
        }
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

    fn test_session_map() -> Rc<RefCell<SessionMap>> {
        Rc::new(RefCell::new(SessionMap::new()))
    }

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
        assert_eq!(tools.len(), 4);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"commando_exec"));
        assert!(names.contains(&"commando_list"));
        assert!(names.contains(&"commando_ping"));
        assert!(names.contains(&"commando_output"));
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
        assert!(limiter.try_acquire("host-a"));
        assert!(limiter.try_acquire("host-a"));
        // At limit — should fail
        assert!(!limiter.try_acquire("host-a"));
        // Release one slot
        limiter.release("host-a");
        // Now should succeed again
        assert!(limiter.try_acquire("host-a"));
    }

    #[test]
    fn concurrency_limiter_independent_targets() {
        let limiter = ConcurrencyLimiter::new(1);
        assert!(limiter.try_acquire("host-a"));
        // Different target should be independent
        assert!(limiter.try_acquire("host-b"));
        // Same target at limit
        assert!(!limiter.try_acquire("host-a"));
        assert!(!limiter.try_acquire("host-b"));
    }

    fn test_config() -> Arc<GatewayConfig> {
        Arc::new(GatewayConfig {
            server: Default::default(),
            proxmox: None,
            agent: crate::config::AgentConnectionConfig {
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

    fn test_config_with_target() -> Arc<GatewayConfig> {
        let mut psk = std::collections::HashMap::new();
        psk.insert("my-box".to_string(), "secret123".to_string());
        Arc::new(GatewayConfig {
            server: Default::default(),
            proxmox: None,
            agent: crate::config::AgentConnectionConfig {
                default_port: 9876,
                default_timeout_secs: 60,
                connect_timeout_secs: 5,
                max_concurrent_per_target: 4,
                psk,
            },
            targets: vec![],
            cache_dir: "/tmp/commando-test".to_string(),
            streaming: Default::default(),
        })
    }

    fn registry_with_target() -> Arc<Mutex<Registry>> {
        let targets = vec![crate::registry::ManualTargetInput {
            name: "my-box".to_string(),
            host: "10.0.0.5".to_string(),
            port: 9876,
            shell: "bash".to_string(),
            tags: vec!["test".to_string()],
        }];
        Arc::new(Mutex::new(Registry::from_manual(targets)))
    }

    #[tokio::test]
    async fn exec_missing_target_param() {
        let config = test_config();
        let registry = Arc::new(Mutex::new(Registry::new()));
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_exec",
                "arguments": { "command": "echo hi" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("missing required parameter: target")
        );
    }

    #[tokio::test]
    async fn exec_unknown_target() {
        let config = test_config();
        let registry = Arc::new(Mutex::new(Registry::new()));
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_exec",
                "arguments": { "target": "nonexistent", "command": "echo hi" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unknown target")
        );
    }

    #[tokio::test]
    async fn exec_no_psk_configured() {
        // Target exists in registry but no PSK in config
        let config = test_config(); // no PSKs
        let registry = registry_with_target();
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_exec",
                "arguments": { "target": "my-box", "command": "echo hi" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no PSK configured")
        );
    }

    #[tokio::test]
    async fn exec_concurrency_limit_reached() {
        let config = test_config_with_target();
        let registry = registry_with_target();
        let limiter = Arc::new(ConcurrencyLimiter::new(1));

        // Exhaust the limiter
        assert!(limiter.try_acquire("my-box"));

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_exec",
                "arguments": { "target": "my-box", "command": "echo hi" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("concurrency limit")
        );
    }

    #[tokio::test]
    async fn list_with_targets() {
        let config = test_config();
        let registry = registry_with_target();
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_list",
                "arguments": {}
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("my-box"));
        assert!(text.contains("10.0.0.5"));
    }

    #[tokio::test]
    async fn list_with_filter() {
        let config = test_config();
        let registry = registry_with_target();
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_list",
                "arguments": { "filter": "nonexistent" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains("my-box"));
    }

    #[tokio::test]
    async fn ping_missing_target() {
        let config = test_config();
        let registry = Arc::new(Mutex::new(Registry::new()));
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_ping",
                "arguments": { "target": "nonexistent" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unknown target")
        );
    }

    #[tokio::test]
    async fn ping_no_psk() {
        let config = test_config(); // no PSKs
        let registry = registry_with_target();
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_ping",
                "arguments": { "target": "my-box" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no PSK configured")
        );
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let config = test_config();
        let registry = Arc::new(Mutex::new(Registry::new()));
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "nonexistent_tool",
                "arguments": {}
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Unknown tool")
        );
    }

    #[tokio::test]
    async fn exec_stopped_target_returns_clear_error() {
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let mut registry = Registry::new();
        registry.update_discovered(vec![crate::registry::DiscoveredTarget {
            name: "node-1/stopped-app".to_string(),
            host: "".to_string(),
            port: 9876,
            status: "stopped".to_string(),
        }]);
        let registry = Arc::new(Mutex::new(registry));

        let mut psk = std::collections::HashMap::new();
        psk.insert("node-1/stopped-app".to_string(), "secret123".to_string());
        let config = Arc::new(GatewayConfig {
            server: Default::default(),
            proxmox: None,
            agent: crate::config::AgentConnectionConfig {
                default_port: 9876,
                default_timeout_secs: 60,
                connect_timeout_secs: 5,
                max_concurrent_per_target: 4,
                psk,
            },
            targets: vec![],
            cache_dir: "/tmp/commando-test".to_string(),
            streaming: Default::default(),
        });

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_exec",
                "arguments": { "target": "node-1/stopped-app", "command": "echo hi" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("stopped"),
            "error should mention target status, got: {text}"
        );
    }

    #[tokio::test]
    async fn ping_stopped_target_returns_clear_error() {
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let mut registry = Registry::new();
        registry.update_discovered(vec![crate::registry::DiscoveredTarget {
            name: "node-1/stopped-app".to_string(),
            host: "".to_string(),
            port: 9876,
            status: "stopped".to_string(),
        }]);
        let registry = Arc::new(Mutex::new(registry));

        let mut psk = std::collections::HashMap::new();
        psk.insert("node-1/stopped-app".to_string(), "secret123".to_string());
        let config = Arc::new(GatewayConfig {
            server: Default::default(),
            proxmox: None,
            agent: crate::config::AgentConnectionConfig {
                default_port: 9876,
                default_timeout_secs: 60,
                connect_timeout_secs: 5,
                max_concurrent_per_target: 4,
                psk,
            },
            targets: vec![],
            cache_dir: "/tmp/commando-test".to_string(),
            streaming: Default::default(),
        });

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_ping",
                "arguments": { "target": "node-1/stopped-app" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
            .await
            .unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("stopped"),
            "error should mention target status, got: {text}"
        );
    }

    #[tokio::test]
    async fn dispatch_returns_none_for_notifications() {
        let config = test_config();
        let registry = Arc::new(Mutex::new(Registry::new()));
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        // Notification: no "id" field
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        assert!(
            dispatch_request(
                &notification,
                &config,
                &registry,
                &limiter,
                &test_session_map()
            )
            .await
            .is_none()
        );

        // Notification: null id
        let null_id = json!({
            "jsonrpc": "2.0",
            "id": null,
            "method": "notifications/initialized"
        });
        assert!(
            dispatch_request(&null_id, &config, &registry, &limiter, &test_session_map())
                .await
                .is_none()
        );

        // Request: has id — should return Some
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        assert!(
            dispatch_request(&request, &config, &registry, &limiter, &test_session_map())
                .await
                .is_some()
        );
    }
}
