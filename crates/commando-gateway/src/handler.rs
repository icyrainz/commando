use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;
use tracing::info;

use crate::audit::{AuditEntry, AuditLogger};
use crate::config::{GatewayConfig, StreamingConfig};
use crate::registry::Registry;
use crate::rpc;
use crate::session::SessionMap;
use crate::types::*;

/// Helper for collecting stage timings in profiling mode.
pub struct Profiler {
    enabled: bool,
    start: Instant,
    last: Instant,
    stages: BTreeMap<String, f64>,
}

impl Profiler {
    pub fn new(enabled: bool) -> Self {
        let now = Instant::now();
        Self {
            enabled,
            start: now,
            last: now,
            stages: BTreeMap::new(),
        }
    }

    pub fn stage(&mut self, name: &str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last);
        self.stages
            .insert(name.to_string(), elapsed.as_secs_f64() * 1000.0);
        self.last = now;
    }

    pub fn finish(mut self) -> Option<ProfileData> {
        if !self.enabled {
            return None;
        }
        let total = self.start.elapsed().as_secs_f64() * 1000.0;
        // Insert total
        self.stages.insert("_total".to_string(), total);
        Some(ProfileData {
            stages: self.stages,
            total_ms: total,
        })
    }
}

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

pub fn process_tools_list(request: &Value, expose_exec: bool) -> Value {
    let mut list_tool: Value =
        serde_json::from_str(include_str!("tools/commando_list.json")).unwrap();
    list_tool["description"] = if expose_exec {
        "List all registered targets with their status, shell, tags, and reachability.".into()
    } else {
        "List all available commando targets with their status and IP. To execute commands on a target, use the Bash tool: commando exec <target> '<command>'".into()
    };

    let ping_tool: Value = serde_json::from_str(include_str!("tools/commando_ping.json")).unwrap();

    let mut tools = vec![list_tool, ping_tool];

    if expose_exec {
        let exec_tool: Value =
            serde_json::from_str(include_str!("tools/commando_exec.json")).unwrap();
        let output_tool: Value =
            serde_json::from_str(include_str!("tools/commando_output.json")).unwrap();
        tools.push(exec_tool);
        tools.push(output_tool);
    }

    json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {
            "tools": tools
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
    audit: &Arc<AuditLogger>,
) -> Option<Value> {
    // Handle REST API requests (sent via __rest marker)
    if let Some(rest_type) = request["__rest"].as_str() {
        let profile_enabled = request["__profile"].as_bool().unwrap_or(false);
        let result = match rest_type {
            "exec" => {
                let target = request["target"].as_str().unwrap_or("");
                let command = request["command"].as_str().unwrap_or("");
                let work_dir = request["work_dir"].as_str().unwrap_or("");
                let timeout = request["timeout"].as_u64().map(|t| t as u32);
                let mut profiler = Profiler::new(profile_enabled);
                // extra_env intentionally empty — --env flag omitted from CLI v1
                let result = handle_exec_core(
                    target,
                    command,
                    work_dir,
                    timeout,
                    vec![],
                    config,
                    registry,
                    limiter,
                    session_map,
                    &mut profiler,
                )
                .await;
                profiler.stage("audit");
                audit_exec(audit, target, command, &result, "rest");
                profiler.stage("serialize");
                let mut profile_data = profiler.finish();
                match result {
                    Ok(mut page) => {
                        // Merge RPC-internal profile into gateway profile
                        if let Some(ref mut gw) = profile_data
                            && let Some(rpc) = page._profile.take()
                        {
                            gw.stages.extend(rpc.stages);
                        }
                        page._profile = profile_data;
                        serde_json::to_value(&page).unwrap()
                    }
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

    let method = request["method"].as_str().unwrap_or("");
    let id = &request["id"];

    // JSON-RPC 2.0 notifications have no "id" field — never respond to them
    if request.get("id").is_none() || request["id"].is_null() {
        return None;
    }

    let response = match method {
        "initialize" => process_initialize(request),
        "tools/list" => process_tools_list(request, config.server.is_mcp_exec_mode()),
        "tools/call" => {
            handle_tools_call(request, config, registry, limiter, session_map, audit).await
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
    session_map: &Rc<RefCell<SessionMap>>,
    audit: &Arc<AuditLogger>,
) -> Value {
    let id = &request["id"];
    let tool_name = request["params"]["name"].as_str().unwrap_or("");
    let args = &request["params"]["arguments"];

    match tool_name {
        "commando_exec" => {
            handle_exec(id, args, config, registry, limiter, session_map, audit).await
        }
        "commando_list" => handle_list(id, args, config, registry),
        "commando_ping" => handle_ping(id, args, config, registry).await,
        "commando_output" => handle_output(id, args, session_map, &config.streaming).await,
        _ => make_tool_error(id, &format!("Unknown tool: {tool_name}")),
    }
}

fn audit_exec(
    audit: &AuditLogger,
    target: &str,
    command: &str,
    result: &Result<ExecPage, HandlerError>,
    source: &str,
) {
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let cmd = if command.len() > 500 {
        &command[..500]
    } else {
        command
    };
    match result {
        Ok(page) => {
            audit.log(&AuditEntry {
                ts,
                target,
                command: cmd,
                exit_code: page.exit_code,
                duration_ms: page.duration_ms,
                request_id: None,
                source,
                error: None,
            });
        }
        Err(e) => {
            audit.log(&AuditEntry {
                ts,
                target,
                command: cmd,
                exit_code: None,
                duration_ms: None,
                request_id: None,
                source,
                error: Some(&e.message),
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
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
    profiler: &mut Profiler,
) -> Result<ExecPage, HandlerError> {
    let timeout = timeout_secs.unwrap_or(config.agent.default_timeout_secs);

    let (host, port, status) = {
        let reg = registry.lock().unwrap();
        match reg.get(target_name) {
            Some(t) => (t.host.clone(), t.port, t.status.clone()),
            None => {
                return Err(HandlerError::bad_request(format!(
                    "unknown target: {target_name}"
                )));
            }
        }
    };
    profiler.stage("registry_lookup");

    if host.is_empty() {
        return Err(HandlerError::bad_request(format!(
            "target '{}' is {} (no IP available)",
            target_name, status
        )));
    }

    let psk = match config.agent.psk.get(target_name) {
        Some(p) => p.clone(),
        None => {
            return Err(HandlerError::bad_request(format!(
                "no PSK configured for target: {target_name}"
            )));
        }
    };

    if !limiter.try_acquire(target_name) {
        return Err(HandlerError::bad_request(format!(
            "concurrency limit reached for target: {target_name}"
        )));
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    profiler.stage("setup");

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
        timeout,
        &extra_env,
        &request_id,
        config.agent.connect_timeout_secs,
        session_map.clone(),
        session_id.clone(),
        limiter.clone(),
        target_name.to_string(),
    );
    profiler.stage("spawn_rpc");

    // Store the JoinHandle so cleanup can abort it if needed
    {
        let mut map = session_map.borrow_mut();
        if let Some(session) = map.get_by_id_mut(&session_id) {
            session.rpc_task = Some(join_handle);
        } else {
            // Session was unexpectedly removed; abort the spawned task and release slot
            join_handle.abort();
            return Err(HandlerError::bad_request(
                "session lost before execution started",
            ));
        }
    }

    // Build and return the first page
    let page = build_page(session_map, &token, &config.streaming)
        .await
        .map_err(HandlerError::bad_request)?;
    profiler.stage("build_page");
    Ok(page)
}

async fn handle_exec(
    id: &Value,
    args: &Value,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
    limiter: &Arc<ConcurrencyLimiter>,
    session_map: &Rc<RefCell<SessionMap>>,
    audit: &Arc<AuditLogger>,
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

    let mut profiler = Profiler::new(false); // MCP path: no profiling
    let result = handle_exec_core(
        target_name,
        command,
        work_dir,
        timeout_secs,
        extra_env,
        config,
        registry,
        limiter,
        session_map,
        &mut profiler,
    )
    .await;
    audit_exec(audit, target_name, command, &result, "mcp");
    match result {
        Ok(page) => format_page_response(id, &page),
        Err(e) => make_tool_error(id, &e.message),
    }
}

/// Convert a page response into MCP tool result text.
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
        text.push_str(&format!(
            "\n---\nexit_code: {} | duration: {}ms",
            exit_code, duration_ms
        ));
    }
    if let Some(next_page) = &page.next_page {
        text.push_str(&format!("\n[streaming] next_page={next_page}"));
    }
    let is_error = page.exit_code.is_some_and(|c| c != 0) || page.timed_out.unwrap_or(false);
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
pub async fn build_page(
    session_map: &Rc<RefCell<SessionMap>>,
    token: &str,
    config: &StreamingConfig,
) -> Result<ExecPage, String> {
    let page_timeout = Duration::from_secs(config.page_timeout_secs);
    let page_max = config.page_max_bytes;
    let deadline = Instant::now() + page_timeout;

    // Phase 1: Wait until enough data is available, command completes, or timeout.
    // We keep waiting as long as: no data yet, OR data is under page_max and
    // the command hasn't completed (avoids unnecessary pagination for small output).
    loop {
        let (buffered, completed, notify) = {
            let map = session_map.borrow();
            let session = map
                .get_by_token(token)
                .ok_or_else(|| "invalid or expired page token".to_string())?;
            (
                session.total_buffered(),
                session.completed,
                session.notify.clone(),
            )
        };

        if completed || buffered >= page_max {
            break;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        // Wait for notification or timeout
        let _ = tokio::time::timeout(remaining, notify.notified()).await;
        // Re-loop to check state again
    }

    // Phase 2: Drain buffers up to page_max_bytes.
    let (stdout_bytes, stderr_bytes, has_remaining) = {
        let mut map = session_map.borrow_mut();
        let session = map
            .get_by_token_mut(token)
            .ok_or_else(|| "invalid or expired page token".to_string())?;
        session.touch();

        let stdout_bytes = session.drain_stdout_up_to(page_max);
        let remaining_budget = page_max.saturating_sub(stdout_bytes.len());
        let stderr_bytes = session.drain_stderr_up_to(remaining_budget);
        let has_remaining = session.total_buffered() > 0;

        if has_remaining {
            session.notify.notify_one();
        }

        (stdout_bytes, stderr_bytes, has_remaining)
    };
    // Mutable borrow dropped — safe to await below.

    // Check if command already completed.
    let mut exec_result_data = {
        let map = session_map.borrow();
        let session = map
            .get_by_token(token)
            .ok_or_else(|| "session lost during drain".to_string())?;
        if session.completed && !has_remaining {
            session
                .exec_result
                .as_ref()
                .map(|r| (r.exit_code, r.duration_ms, r.timed_out))
        } else {
            None
        }
    };

    // Coalesce: for fast commands, output chunks arrive via the OutputReceiver
    // callback before the exec_stream RPC response sets `completed = true`.
    // Brief wait lets us return data + exit_code in a single page instead of
    // forcing an unnecessary round-trip for just the exit code.
    // Loop because there may be a stale notification from the data arrival
    // that we need to drain before the completion notification arrives.
    if exec_result_data.is_none() && !has_remaining {
        let coalesce_deadline = Instant::now() + Duration::from_millis(10);
        loop {
            let notify = {
                let map = session_map.borrow();
                match map.get_by_token(token) {
                    Some(s) if s.completed && s.total_buffered() == 0 => {
                        exec_result_data = s
                            .exec_result
                            .as_ref()
                            .map(|r| (r.exit_code, r.duration_ms, r.timed_out));
                        break;
                    }
                    Some(s) => s.notify.clone(),
                    None => break,
                }
            };
            let remaining = coalesce_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let _ = tokio::time::timeout(remaining, notify.notified()).await;
        }
    }

    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    if let Some((exit_code, duration_ms, timed_out)) = exec_result_data {
        // Final page: remove session from map, extract rpc_profile if present
        let rpc_profile = session_map
            .borrow_mut()
            .remove_by_token(token)
            .and_then(|s| s.rpc_profile);

        Ok(ExecPage {
            stdout,
            stderr,
            exit_code: Some(exit_code),
            duration_ms: Some(duration_ms),
            timed_out: if timed_out { Some(true) } else { None },
            next_page: None,
            _profile: rpc_profile.map(|rp| {
                let mut stages = BTreeMap::new();
                stages.insert("rpc_tcp_connect".to_string(), rp.tcp_connect_ms);
                stages.insert("rpc_auth".to_string(), rp.auth_ms);
                stages.insert("rpc_exec".to_string(), rp.exec_rpc_ms);
                // Merge agent-side profile if present
                if !rp.agent_profile_json.is_empty()
                    && let Ok(agent) =
                        serde_json::from_str::<serde_json::Value>(&rp.agent_profile_json)
                    && let Some(obj) = agent.as_object()
                {
                    for (k, v) in obj {
                        if let Some(f) = v.as_f64() {
                            stages.insert(format!("agent_{k}"), f);
                        } else if let Some(n) = v.as_u64() {
                            stages.insert(format!("agent_{k}"), n as f64);
                        }
                    }
                }
                ProfileData {
                    stages,
                    total_ms: 0.0, // will be replaced by Profiler::finish()
                }
            }),
        })
    } else {
        // Still running: rotate token
        let new_token = session_map
            .borrow_mut()
            .rotate_token(token)
            .ok_or_else(|| "session disappeared during token rotation".to_string())?;

        Ok(ExecPage {
            stdout,
            stderr,
            exit_code: None,
            duration_ms: None,
            timed_out: None,
            next_page: Some(new_token),
            _profile: None,
        })
    }
}

pub async fn handle_output_core(
    token: &str,
    session_map: &Rc<RefCell<SessionMap>>,
    config: &StreamingConfig,
) -> Result<ExecPage, HandlerError> {
    build_page(session_map, token, config)
        .await
        .map_err(HandlerError::bad_request)
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

    match handle_output_core(token, session_map, config).await {
        Ok(page) => format_page_response(id, &page),
        Err(e) => make_tool_error(id, &e.message),
    }
}

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

pub fn handle_list_core(filter: Option<&str>, registry: &Arc<Mutex<Registry>>) -> Vec<TargetInfo> {
    let reg = registry.lock().unwrap();
    reg.list(filter)
        .iter()
        .map(|t| {
            // For manual targets, status is always "unknown" since there's no
            // Proxmox discovery. Use reachability from ping cycle instead.
            let status = if t.status == "unknown" {
                match t.reachable {
                    crate::registry::Reachability::Reachable => "reachable".to_string(),
                    crate::registry::Reachability::Unreachable => "unreachable".to_string(),
                    crate::registry::Reachability::Unknown => "unknown".to_string(),
                }
            } else {
                t.status.clone()
            };
            TargetInfo {
                name: t.name.clone(),
                status,
                host: t.host.clone(),
            }
        })
        .collect()
}

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

pub async fn handle_ping_core(
    target_name: &str,
    config: &Arc<GatewayConfig>,
    registry: &Arc<Mutex<Registry>>,
) -> Result<PingInfo, HandlerError> {
    let (host, port, status) = {
        let reg = registry.lock().unwrap();
        match reg.get(target_name) {
            Some(t) => (t.host.clone(), t.port, t.status.clone()),
            None => {
                return Err(HandlerError::bad_request(format!(
                    "unknown target: {target_name}"
                )));
            }
        }
    };

    if host.is_empty() {
        return Err(HandlerError::bad_request(format!(
            "target '{}' is {} (no IP available)",
            target_name, status
        )));
    }

    let psk = match config.agent.psk.get(target_name) {
        Some(p) => p.clone(),
        None => {
            return Err(HandlerError::bad_request(format!(
                "no PSK configured for target: {target_name}"
            )));
        }
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session_map() -> Rc<RefCell<SessionMap>> {
        Rc::new(RefCell::new(SessionMap::new()))
    }

    fn test_audit() -> Arc<AuditLogger> {
        Arc::new(AuditLogger::new(
            std::path::PathBuf::from("/dev/null"),
            10 * 1024 * 1024,
        ))
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
    fn handle_tools_list_cli_mode() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let response = process_tools_list(&request, false);
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"commando_list"));
        assert!(names.contains(&"commando_ping"));
        // commando_list description should mention CLI
        let list_desc = tools[0]["description"].as_str().unwrap();
        assert!(list_desc.contains("commando exec"));
    }

    #[test]
    fn handle_tools_list_exec_mode() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let response = process_tools_list(&request, true);
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"commando_list"));
        assert!(names.contains(&"commando_ping"));
        assert!(names.contains(&"commando_exec"));
        assert!(names.contains(&"commando_output"));
    }

    #[test]
    fn tool_schemas_have_required_fields() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let response = process_tools_list(&request, true);
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
        let resp = dispatch_request(
            &request,
            &config,
            &registry,
            &limiter,
            &test_session_map(),
            &test_audit(),
        )
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
                &test_session_map(),
                &test_audit(),
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
            dispatch_request(
                &null_id,
                &config,
                &registry,
                &limiter,
                &test_session_map(),
                &test_audit()
            )
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
            dispatch_request(
                &request,
                &config,
                &registry,
                &limiter,
                &test_session_map(),
                &test_audit()
            )
            .await
            .is_some()
        );
    }

    // ---- Streaming / pagination tests ----

    fn streaming_config(page_max_bytes: usize) -> StreamingConfig {
        StreamingConfig {
            page_timeout_secs: 1, // short timeout for tests
            page_max_bytes,
            session_idle_timeout_secs: 60,
        }
    }

    /// Helper: populate a session with data and optionally mark completed.
    fn populate_session(
        session_map: &Rc<RefCell<SessionMap>>,
        token: &str,
        stdout: &[u8],
        stderr: &[u8],
        completed: bool,
        exit_code: i32,
    ) {
        let mut map = session_map.borrow_mut();
        let session = map.get_by_token_mut(token).unwrap();
        session.stdout_buffer.extend_from_slice(stdout);
        session.stderr_buffer.extend_from_slice(stderr);
        if completed {
            session.completed = true;
            session.exec_result = Some(crate::session::StreamExecResult {
                exit_code,
                duration_ms: 42,
                timed_out: false,
            });
        }
        session.notify.notify_one();
    }

    // -- format_page_response tests --

    #[test]
    fn format_page_stdout_only() {
        let id = json!(1);
        let page = ExecPage {
            stdout: "hello world".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            duration_ms: Some(10),
            timed_out: None,
            next_page: None,
            _profile: None,
        };
        let resp = format_page_response(&id, &page);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hello world"));
        assert!(text.contains("exit_code: 0"));
        assert!(text.contains("duration: 10ms"));
        assert!(!resp["result"]["isError"].as_bool().unwrap_or(false));
    }

    #[test]
    fn format_page_stderr_included() {
        let id = json!(1);
        let page = ExecPage {
            stdout: "out".to_string(),
            stderr: "err msg".to_string(),
            exit_code: Some(0),
            duration_ms: Some(5),
            timed_out: None,
            next_page: None,
            _profile: None,
        };
        let resp = format_page_response(&id, &page);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("out"));
        assert!(text.contains("[stderr]\nerr msg"));
    }

    #[test]
    fn format_page_nonzero_exit_is_error() {
        let id = json!(1);
        let page = ExecPage {
            stdout: String::new(),
            stderr: "fail".to_string(),
            exit_code: Some(1),
            duration_ms: Some(0),
            timed_out: None,
            next_page: None,
            _profile: None,
        };
        let resp = format_page_response(&id, &page);
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
    }

    #[test]
    fn format_page_timed_out() {
        let id = json!(1);
        let page = ExecPage {
            stdout: "partial".to_string(),
            stderr: String::new(),
            exit_code: Some(-1),
            duration_ms: Some(5000),
            timed_out: Some(true),
            next_page: None,
            _profile: None,
        };
        let resp = format_page_response(&id, &page);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("[timed out]"));
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
    }

    #[test]
    fn format_page_streaming_next_page() {
        let id = json!(1);
        let page = ExecPage {
            stdout: "chunk1".to_string(),
            stderr: String::new(),
            exit_code: None,
            duration_ms: None,
            timed_out: None,
            next_page: Some("abc123".to_string()),
            _profile: None,
        };
        let resp = format_page_response(&id, &page);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("[streaming] next_page=abc123"));
        // No exit_code means not an error
        assert!(!resp["result"]["isError"].as_bool().unwrap_or(false));
    }

    #[test]
    fn format_page_empty_output() {
        let id = json!(1);
        let page = ExecPage {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            duration_ms: None,
            timed_out: None,
            next_page: Some("tok123".to_string()),
            _profile: None,
        };
        let resp = format_page_response(&id, &page);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        // Should still have the streaming token
        assert!(text.contains("[streaming] next_page=tok123"));
    }

    // -- build_page tests --

    #[tokio::test(flavor = "current_thread")]
    async fn build_page_completed_session_returns_final() {
        let sm = test_session_map();
        let (token, _) = sm.borrow_mut().create_session();
        populate_session(&sm, &token, b"output data", b"", true, 0);

        let config = streaming_config(32768);
        let page = build_page(&sm, &token, &config).await.unwrap();

        assert_eq!(page.stdout, "output data");
        assert_eq!(page.exit_code.unwrap(), 0);
        assert_eq!(page.duration_ms.unwrap(), 42);
        assert!(page.next_page.is_none()); // final page has no next_page
        // Session should be removed after final page
        assert!(sm.borrow().get_by_token(&token).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_page_running_session_rotates_token() {
        let sm = test_session_map();
        let (token, _) = sm.borrow_mut().create_session();
        populate_session(&sm, &token, b"partial output", b"", false, 0);

        let config = streaming_config(32768);
        let page = build_page(&sm, &token, &config).await.unwrap();

        assert_eq!(page.stdout, "partial output");
        assert!(page.exit_code.is_none()); // not final
        let next_page = page.next_page.as_ref().unwrap();
        assert!(!next_page.is_empty());
        // Old token should be invalid
        assert!(sm.borrow().get_by_token(&token).is_none());
        // New token should work
        assert!(sm.borrow().get_by_token(next_page).is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_page_respects_page_max_bytes() {
        let sm = test_session_map();
        let (token, _) = sm.borrow_mut().create_session();
        // 100 bytes of stdout, page limit of 50
        let big_data = vec![b'A'; 100];
        populate_session(&sm, &token, &big_data, b"", false, 0);

        let config = streaming_config(50);
        let page = build_page(&sm, &token, &config).await.unwrap();

        // Should only get 50 bytes
        assert_eq!(page.stdout.len(), 50);
        // Should have a next_page (still has data)
        assert!(page.next_page.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_page_large_output_completed_not_final_until_drained() {
        let sm = test_session_map();
        let (token, _) = sm.borrow_mut().create_session();
        // 100 bytes of stdout, page limit of 50, command completed
        let big_data = vec![b'X'; 100];
        populate_session(&sm, &token, &big_data, b"", true, 0);

        let config = streaming_config(50);

        // First page: should get 50 bytes but NOT be final (50 bytes remain)
        let page1 = build_page(&sm, &token, &config).await.unwrap();
        assert_eq!(page1.stdout.len(), 50);
        assert!(page1.exit_code.is_none(), "should not be final page yet");
        let token2 = page1.next_page.as_ref().unwrap();

        // Second page: drain remaining 50 bytes, now it's final
        let page2 = build_page(&sm, token2, &config).await.unwrap();
        assert_eq!(page2.stdout.len(), 50);
        assert_eq!(page2.exit_code.unwrap(), 0);
        assert!(page2.next_page.is_none());
        // Session cleaned up
        assert!(sm.borrow().get_by_token(token2).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_page_stdout_and_stderr_share_budget() {
        let sm = test_session_map();
        let (token, _) = sm.borrow_mut().create_session();
        // 40 bytes stdout + 40 bytes stderr, page max 50
        populate_session(&sm, &token, &vec![b'O'; 40], &vec![b'E'; 40], true, 0);

        let config = streaming_config(50);
        let page = build_page(&sm, &token, &config).await.unwrap();

        // stdout gets first 40, stderr gets remaining budget (50-40=10)
        assert_eq!(page.stdout.len(), 40);
        assert_eq!(page.stderr.len(), 10);
        // Still has remaining stderr data, so NOT final even though completed
        assert!(page.exit_code.is_none());
        assert!(page.next_page.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_page_invalid_token_returns_error() {
        let sm = test_session_map();
        let config = streaming_config(32768);
        let result = build_page(&sm, "bogus-token", &config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid or expired"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_page_empty_completed_session() {
        let sm = test_session_map();
        let (token, _) = sm.borrow_mut().create_session();
        // No output, but completed
        populate_session(&sm, &token, b"", b"", true, 0);

        let config = streaming_config(32768);
        let page = build_page(&sm, &token, &config).await.unwrap();

        assert_eq!(page.stdout, "");
        assert_eq!(page.stderr, "");
        assert_eq!(page.exit_code.unwrap(), 0);
        assert!(page.next_page.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_page_timeout_with_no_data() {
        let sm = test_session_map();
        let (token, _) = sm.borrow_mut().create_session();
        // Don't populate any data — session exists but empty and not completed.

        let config = streaming_config(32768);
        let page = build_page(&sm, &token, &config).await.unwrap();

        // After timeout, should return an empty intermediate page
        assert_eq!(page.stdout, "");
        assert_eq!(page.stderr, "");
        assert!(page.next_page.is_some());
    }

    // -- handle_output tests --

    #[tokio::test(flavor = "current_thread")]
    async fn handle_output_missing_page_param() {
        let sm = test_session_map();
        let config = streaming_config(32768);
        let id = json!(1);
        let args = json!({});
        let resp = handle_output(&id, &args, &sm, &config).await;
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("missing required parameter: page")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_output_invalid_token() {
        let sm = test_session_map();
        let config = streaming_config(32768);
        let id = json!(1);
        let args = json!({ "page": "nonexistent-token" });
        let resp = handle_output(&id, &args, &sm, &config).await;
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("invalid or expired")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_output_returns_data_and_next_page() {
        let sm = test_session_map();
        let (token, _) = sm.borrow_mut().create_session();
        populate_session(&sm, &token, b"streamed output", b"", false, 0);

        let config = streaming_config(32768);
        let id = json!(1);
        let args = json!({ "page": token });
        let resp = handle_output(&id, &args, &sm, &config).await;

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("streamed output"));
        assert!(text.contains("[streaming] next_page="));
        assert!(!resp["result"]["isError"].as_bool().unwrap_or(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_output_final_page() {
        let sm = test_session_map();
        let (token, _) = sm.borrow_mut().create_session();
        populate_session(&sm, &token, b"final output", b"", true, 0);

        let config = streaming_config(32768);
        let id = json!(1);
        let args = json!({ "page": token });
        let resp = handle_output(&id, &args, &sm, &config).await;

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("final output"));
        assert!(text.contains("exit_code: 0"));
        assert!(!text.contains("[streaming]"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multi_page_polling_flow() {
        let sm = test_session_map();
        let (token, _sid) = sm.borrow_mut().create_session();
        let config = streaming_config(20); // tiny pages

        // Simulate: 50 bytes of stdout arrives, command still running
        populate_session(
            &sm,
            &token,
            b"AAAAAAAAAABBBBBBBBBBCCCCCCCCCC",
            b"",
            false,
            0,
        );

        let id = json!(1);

        // Page 1: first 20 bytes
        let args1 = json!({ "page": token });
        let resp1 = handle_output(&id, &args1, &sm, &config).await;
        let text1 = resp1["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text1.contains("AAAAAAAAAABBBBBBBBBB")); // first 20
        // Extract next_page token
        let next1 = text1
            .lines()
            .find(|l| l.contains("next_page="))
            .unwrap()
            .split("next_page=")
            .nth(1)
            .unwrap();

        // Page 2: next 10 bytes (remaining) — command still running
        let args2 = json!({ "page": next1 });
        let resp2 = handle_output(&id, &args2, &sm, &config).await;
        let text2 = resp2["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text2.contains("CCCCCCCCCC")); // remaining 10
        let next2 = text2
            .lines()
            .find(|l| l.contains("next_page="))
            .unwrap()
            .split("next_page=")
            .nth(1)
            .unwrap()
            .to_string();

        // Now mark the command as completed
        {
            let mut map = sm.borrow_mut();
            let session = map.get_by_token_mut(&next2).unwrap();
            session.completed = true;
            session.exec_result = Some(crate::session::StreamExecResult {
                exit_code: 0,
                duration_ms: 100,
                timed_out: false,
            });
            session.notify.notify_one();
        }

        // Page 3: final page with exit code
        let args3 = json!({ "page": next2 });
        let resp3 = handle_output(&id, &args3, &sm, &config).await;
        let text3 = resp3["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text3.contains("exit_code: 0"));
        assert!(!text3.contains("[streaming]")); // no more pages
    }

    /// Simulates real-world fast command timing: output arrives via callback
    /// before the RPC completion signal. Without coalesce, this would return
    /// an unnecessary intermediate page followed by a final page with just
    /// the exit code.
    #[test]
    fn build_page_coalesces_fast_command_completion() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let sm = test_session_map();
            let (token, _) = sm.borrow_mut().create_session();
            let session_id = sm
                .borrow()
                .session_id_for_token(&token)
                .unwrap()
                .to_string();

            // Data arrives but command NOT yet completed — simulates the
            // OutputReceiver callback firing before the RPC response.
            {
                let mut map = sm.borrow_mut();
                let session = map.get_by_token_mut(&token).unwrap();
                session.stdout_buffer.extend_from_slice(b"fast output");
                session.notify.notify_one();
            }

            // Spawn task that marks completion after 10ms — simulates the
            // exec_stream RPC response arriving slightly after the data.
            let sm2 = sm.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let notify = {
                    let mut map = sm2.borrow_mut();
                    let session = map.get_by_id_mut(&session_id).unwrap();
                    session.completed = true;
                    session.exec_result = Some(crate::session::StreamExecResult {
                        exit_code: 0,
                        duration_ms: 5,
                        timed_out: false,
                    });
                    session.notify.clone()
                };
                notify.notify_one();
            });

            let config = streaming_config(32768);
            let page = build_page(&sm, &token, &config).await.unwrap();

            // Should be a FINAL page — coalesced data + exit code in one response.
            assert_eq!(page.stdout, "fast output");
            assert_eq!(page.exit_code.unwrap(), 0);
            assert!(
                page.next_page.is_none(),
                "fast command should not require a second page"
            );
            // Session should be cleaned up
            assert!(sm.borrow().get_by_token(&token).is_none());
        });
    }
}
