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

/// Dispatch a JSON-RPC request to the appropriate handler.
/// Returns `None` for notifications (no "id" or null id), `Some(response)` for requests.
pub async fn dispatch_request(
    request: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
    limiter: &Arc<ConcurrencyLimiter>,
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
        "tools/call" => {
            handle_tools_call(request, config, registry, limiter).await
        }
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
        })
    }

    fn test_config_with_target() -> Arc<GatewayConfig> {
        let mut psk = std::collections::HashMap::new();
        psk.insert("my-box".to_string(), "secret123".to_string());
        Arc::new(GatewayConfig {
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
                psk,
            },
            targets: vec![],
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
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("missing required parameter: target"));
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
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("unknown target"));
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
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("no PSK configured"));
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
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("concurrency limit"));
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
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
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
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
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
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("unknown target"));
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
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("no PSK configured"));
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
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("Unknown tool"));
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
        assert!(dispatch_request(&notification, &config, &registry, &limiter).await.is_none());

        // Notification: null id
        let null_id = json!({
            "jsonrpc": "2.0",
            "id": null,
            "method": "notifications/initialized"
        });
        assert!(dispatch_request(&null_id, &config, &registry, &limiter).await.is_none());

        // Request: has id — should return Some
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        assert!(dispatch_request(&request, &config, &registry, &limiter).await.is_some());
    }
}
