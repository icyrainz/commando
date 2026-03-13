# Streaming Exec Output Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add paginated streaming output to `commando_exec` so long-running commands return partial output in pages, giving humans and LLMs visibility into command progress.

**Architecture:** New `execStream` Cap'n Proto method with `OutputReceiver` callback. Agent forwards output chunks as they arrive. Gateway buffers chunks in a session map, serves pages to MCP clients with `next_page` tokens. Fast commands complete in a single page (same as today).

**Tech Stack:** Rust nightly, capnp/capnp-rpc 0.25, tokio (current_thread + LocalSet), axum 0.8

**Spec:** `docs/superpowers/specs/2026-03-12-streaming-exec-design.md`

---

## File Structure

**New files:**
- `crates/commando-gateway/src/session.rs` — Session struct, SessionMap, page token generation, idle cleanup timer

**Modified files:**
- `schema/commando.capnp` — Add `OutputReceiver` interface + `execStream @2` method
- `crates/commando-agent/src/process.rs` — Add `execute_stream()` that calls a callback per chunk instead of buffering
- `crates/commando-agent/src/rpc.rs` — Add `exec_stream()` handler on `CommandAgentImpl`
- `crates/commando-gateway/src/config.rs` — Add `StreamingConfig` struct
- `crates/commando-gateway/src/rpc.rs` — Add `remote_exec_stream()` that keeps the connection alive on the main `LocalSet`
- `crates/commando-gateway/src/handler.rs` — Modify `handle_exec()` to use streaming, add `handle_output()`, update tool list, add `commando_output` tool schema
- `crates/commando-gateway/src/streamable.rs` — Pass session map into `build_app()`
- `crates/commando-gateway/src/main.rs` — Create session map and pass to streamable server

---

## Chunk 1: Schema + Agent-Side Streaming

### Task 1: Update Cap'n Proto Schema

**Files:**
- Modify: `schema/commando.capnp`

- [ ] **Step 1: Add OutputReceiver interface and execStream method**

Add after the existing `CommandAgent` interface closing brace:

```capnp
interface OutputReceiver {
  receive @0 (data :Data, stream :UInt8) -> ();
  # stream: 0 = stdout, 1 = stderr
  # Agent calls this as output arrives from the child process.
}
```

And add `execStream` to `CommandAgent`:

```capnp
interface CommandAgent {
  exec @0 (request :ExecRequest) -> (result :ExecResult);
  ping @1 () -> (pong :PingResult);
  execStream @2 (request :ExecRequest, receiver :OutputReceiver)
    -> (result :ExecResult);
}
```

- [ ] **Step 2: Verify schema compiles**

Run: `cargo +nightly build -p commando-common`
Expected: Build succeeds (capnpc generates new Rust types)

- [ ] **Step 3: Commit**

```bash
git add schema/commando.capnp
git commit -m "schema: add OutputReceiver and execStream for streaming output"
```

### Task 2: Add Streaming Process Execution on Agent

**Files:**
- Modify: `crates/commando-agent/src/process.rs`

The existing `execute()` buffers all output into `Vec<u8>`. We need `execute_stream()` that calls a callback per chunk.

- [ ] **Step 1: Write tests for execute_stream**

Add tests at the bottom of `process.rs` in the existing `#[cfg(test)]` module:

```rust
#[tokio::test(flavor = "current_thread")]
async fn exec_stream_echo() {
    let chunks = Arc::new(Mutex::new(Vec::<(Vec<u8>, u8)>::new()));
    let chunks_clone = chunks.clone();
    let callback = move |data: &[u8], stream: u8| {
        chunks_clone.lock().unwrap().push((data.to_vec(), stream));
    };
    let result = execute_stream(
        "echo hello",
        "",
        60,
        &[],
        &ExecOpts { shell: "sh".into(), max_output_bytes: 131072 },
        callback,
    )
    .await
    .unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(!result.timed_out);
    // stdout chunks should contain "hello\n"
    let stdout: Vec<u8> = chunks.lock().unwrap()
        .iter()
        .filter(|(_, s)| *s == 0)
        .flat_map(|(d, _)| d.clone())
        .collect();
    assert_eq!(String::from_utf8_lossy(&stdout), "hello\n");
    // ExecResult stdout/stderr should be empty (delivered via callback)
    assert!(result.stdout.is_empty());
    assert!(result.stderr.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn exec_stream_stderr() {
    let chunks = Arc::new(Mutex::new(Vec::<(Vec<u8>, u8)>::new()));
    let chunks_clone = chunks.clone();
    let callback = move |data: &[u8], stream: u8| {
        chunks_clone.lock().unwrap().push((data.to_vec(), stream));
    };
    let result = execute_stream(
        "echo err >&2",
        "",
        60,
        &[],
        &ExecOpts { shell: "sh".into(), max_output_bytes: 131072 },
        callback,
    )
    .await
    .unwrap();
    assert_eq!(result.exit_code, 0);
    let stderr: Vec<u8> = chunks.lock().unwrap()
        .iter()
        .filter(|(_, s)| *s == 1)
        .flat_map(|(d, _)| d.clone())
        .collect();
    assert_eq!(String::from_utf8_lossy(&stderr), "err\n");
}

#[tokio::test(flavor = "current_thread")]
async fn exec_stream_timeout() {
    let chunks = Arc::new(Mutex::new(Vec::<(Vec<u8>, u8)>::new()));
    let chunks_clone = chunks.clone();
    let callback = move |data: &[u8], stream: u8| {
        chunks_clone.lock().unwrap().push((data.to_vec(), stream));
    };
    let result = execute_stream(
        "echo before && sleep 60",
        "",
        1,
        &[],
        &ExecOpts { shell: "sh".into(), max_output_bytes: 131072 },
        callback,
    )
    .await
    .unwrap();
    assert!(result.timed_out);
    // Should have received "before\n" via callback before timeout
    let stdout: Vec<u8> = chunks.lock().unwrap()
        .iter()
        .filter(|(_, s)| *s == 0)
        .flat_map(|(d, _)| d.clone())
        .collect();
    assert_eq!(String::from_utf8_lossy(&stdout), "before\n");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo +nightly test -p commando-agent exec_stream -- --nocapture`
Expected: FAIL — `execute_stream` not defined

- [ ] **Step 3: Implement execute_stream**

Add `execute_stream()` in `process.rs`. It reuses the same process spawning logic as `execute()` but reads output in a loop and calls the callback instead of buffering. The `ExecResult` returned has empty `stdout`/`stderr` and `truncated: false`.

```rust
/// Execute a command, forwarding output chunks via callback as they arrive.
/// The ExecResult stdout/stderr fields are empty — all output is delivered via the callback.
pub async fn execute_stream<F>(
    command: &str,
    work_dir: &str,
    timeout_secs: u32,
    extra_env: &[(String, String)],
    opts: &ExecOpts,
    on_chunk: F,
) -> anyhow::Result<ExecResult>
where
    F: Fn(&[u8], u8) + 'static,
{
    let start = std::time::Instant::now();

    let mut child = build_command(command, work_dir, extra_env, opts)?;

    let mut stdout = child.stdout.take().context("failed to capture stdout")?;
    let mut stderr = child.stderr.take().context("failed to capture stderr")?;

    // Arc (not Rc) so this function works on any tokio runtime flavor.
    // The existing execute() tests use multi-threaded runtime, and we want
    // execute_stream tests to work with either flavor.
    let on_chunk = Arc::new(on_chunk);
    let on_chunk_out = on_chunk.clone();
    let on_chunk_err = on_chunk.clone();

    let read_stdout = async move {
        let mut buf = [0u8; 4096];
        loop {
            let n = stdout.read(&mut buf).await.unwrap_or(0);
            if n == 0 { break; }
            on_chunk_out(&buf[..n], 0);
        }
    };

    let read_stderr = async move {
        let mut buf = [0u8; 4096];
        loop {
            let n = stderr.read(&mut buf).await.unwrap_or(0);
            if n == 0 { break; }
            on_chunk_err(&buf[..n], 1);
        }
    };

    let timeout_dur = Duration::from_secs(if timeout_secs == 0 { 60 } else { timeout_secs as u64 });

    // Save PID before moving child into the async block (child.id() returns None after wait)
    let child_pid = child.id();

    let timed_out = match tokio::time::timeout(timeout_dur, async {
        tokio::join!(read_stdout, read_stderr);
        child.wait().await
    }).await {
        Ok(wait_result) => {
            let status = wait_result.context("failed to wait on child")?;
            let duration_ms = start.elapsed().as_millis() as u64;
            return Ok(ExecResult {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: status.code().unwrap_or(-1),
                duration_ms,
                timed_out: false,
                truncated: false,
            });
        }
        Err(_) => true,
    };

    // Timeout path: kill process group using saved PID
    if let Some(pid) = child_pid {
        kill_process_group(pid);
        // Note: child was moved into the timed-out future, so we can't call child.wait().
        // The process group kill + SIGKILL is best-effort. Match execute()'s 5s grace period.
        tokio::time::sleep(Duration::from_secs(5)).await;
        kill_process_group_force(pid);
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    Ok(ExecResult {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_code: -1,
        duration_ms,
        timed_out,
        truncated: false,
    })
}
```

This requires extracting the process-building logic from `execute()` into a shared `build_command()` helper to avoid duplication:

```rust
/// Build the Command with shell, env, work_dir, and setsid. Used by both execute() and execute_stream().
fn build_command(
    command: &str,
    work_dir: &str,
    extra_env: &[(String, String)],
    opts: &ExecOpts,
) -> anyhow::Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new(&opts.shell);
    cmd.arg("-c").arg(command);
    cmd.env_clear();
    // Set minimal base env (HOME, USER, PATH, SHELL, LANG, TERM, NO_COLOR)
    // ... same as current execute() lines 36-46
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    if !work_dir.is_empty() {
        cmd.current_dir(work_dir);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("failed to spawn child process")
}
```

Refactor `execute()` to use `build_command()` as well (extract, don't duplicate).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo +nightly test -p commando-agent exec_stream -- --nocapture`
Expected: All 3 new tests PASS

- [ ] **Step 5: Run all agent tests to verify no regressions**

Run: `cargo +nightly test -p commando-agent`
Expected: All existing + new tests PASS

- [ ] **Step 6: Commit**

```bash
git add crates/commando-agent/src/process.rs
git commit -m "feat(agent): add execute_stream with per-chunk callback output"
```

### Task 3: Add execStream RPC Handler on Agent

**Files:**
- Modify: `crates/commando-agent/src/rpc.rs`

- [ ] **Step 1: Add exec_stream handler to CommandAgentImpl**

The agent receives an `OutputReceiver` client capability from the gateway. For each output chunk, it calls `receiver.receive()`. The handler is structurally identical to `exec()` but calls `execute_stream()` instead of `execute()`.

In `rpc.rs`, add the `exec_stream` method to the `Server` impl for `CommandAgentImpl`. The capnp-rpc generated server trait for `CommandAgent` will now require an `exec_stream` method since we added it to the schema.

```rust
async fn exec_stream(
    self: Rc<Self>,
    params: command_agent::ExecStreamParams,
    mut results: command_agent::ExecStreamResults,
) -> Result<(), capnp::Error> {
    // Acquire concurrency permit (same as exec)
    let _permit = self.concurrency_guard.try_acquire()
        .ok_or_else(|| capnp::Error::failed("max concurrent commands reached".into()))?;

    // Extract request params (same as exec)
    let request = params.get()?.get_request()?;
    let command = request.get_command()?.to_str()?;
    let work_dir = request.get_work_dir()?.to_str()?;
    let timeout_secs = request.get_timeout_secs();
    let request_id = request.get_request_id()?.to_str()?;
    let extra_env = /* same extraction as exec */;

    // Get the OutputReceiver capability
    let receiver = params.get()?.get_receiver()?;

    // Callback that forwards chunks via Cap'n Proto RPC.
    // We spawn_local each send and collect handles so we can await them
    // before returning. This bounds memory (each send resolves quickly
    // since it's on the same LocalSet as the RpcSystem) while not
    // blocking the synchronous callback.
    let pending_sends: Rc<RefCell<Vec<tokio::task::JoinHandle<()>>>> =
        Rc::new(RefCell::new(Vec::new()));
    let pending_clone = pending_sends.clone();
    let receiver_clone = receiver.clone();
    let on_chunk = move |data: &[u8], stream: u8| {
        let mut req = receiver_clone.receive_request();
        req.get().set_data(data);
        req.get().set_stream(stream);
        let handle = tokio::task::spawn_local(async move {
            let _ = req.send().promise.await;
        });
        pending_clone.borrow_mut().push(handle);
    };

    let opts = ExecOpts {
        shell: self.config.shell.clone(),
        max_output_bytes: self.config.max_output_bytes,
    };

    tracing::info!(
        command = &command[..command.len().min(200)],
        work_dir,
        timeout_secs,
        request_id,
        "exec_stream start"
    );

    let exec_result = process::execute_stream(
        command, work_dir, timeout_secs, &extra_env, &opts, on_chunk,
    ).await;

    // Await all pending receiver sends to ensure output is fully delivered.
    // Collect first to avoid holding RefMut across .await points.
    let handles: Vec<_> = pending_sends.borrow_mut().drain(..).collect();
    for handle in handles {
        let _ = handle.await;
    }

    match exec_result {
        Ok(r) => {
            let mut result_builder = results.get().init_result();
            result_builder.set_stdout(&[]);
            result_builder.set_stderr(&[]);
            result_builder.set_exit_code(r.exit_code);
            result_builder.set_duration_ms(r.duration_ms);
            result_builder.set_timed_out(r.timed_out);
            result_builder.set_truncated(false);
            result_builder.set_request_id(request_id);
            Ok(())
        }
        Err(e) => Err(capnp::Error::failed(format!("exec failed: {e}"))),
    }
}
```

**Important note on the callback:** Each `receive_request().send()` is spawned via `spawn_local` to avoid blocking the synchronous callback, but handles are collected and awaited after `execute_stream` returns. This ensures all output is fully delivered to the gateway before the `ExecResult` is sent, while keeping memory bounded.

- [ ] **Step 2: Verify agent builds**

Run: `cargo +nightly build -p commando-agent`
Expected: Build succeeds

- [ ] **Step 3: Run all agent tests**

Run: `cargo +nightly test -p commando-agent`
Expected: All tests PASS

- [ ] **Step 4: Run clippy**

Run: `cargo +nightly clippy -p commando-agent -- -D warnings`
Expected: No warnings

- [ ] **Step 5: Commit**

```bash
git add crates/commando-agent/src/rpc.rs
git commit -m "feat(agent): add execStream RPC handler with OutputReceiver callback"
```

---

## Chunk 2: Gateway Config + Session Management

### Task 4: Add Streaming Config

**Files:**
- Modify: `crates/commando-gateway/src/config.rs`

- [ ] **Step 1: Add StreamingConfig struct**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct StreamingConfig {
    #[serde(default = "default_page_timeout")]
    pub page_timeout_secs: u64,
    #[serde(default = "default_page_max_bytes")]
    pub page_max_bytes: usize,
    #[serde(default = "default_session_idle_timeout")]
    pub session_idle_timeout_secs: u64,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            page_timeout_secs: default_page_timeout(),
            page_max_bytes: default_page_max_bytes(),
            session_idle_timeout_secs: default_session_idle_timeout(),
        }
    }
}

fn default_page_timeout() -> u64 { 5 }
fn default_page_max_bytes() -> usize { 32_768 } // 32KB
fn default_session_idle_timeout() -> u64 { 60 }
```

- [ ] **Step 2: Add streaming field to GatewayConfig**

```rust
pub struct GatewayConfig {
    // ... existing fields ...
    #[serde(default)]
    pub streaming: StreamingConfig,
}
```

- [ ] **Step 3: Verify build**

Run: `cargo +nightly build -p commando-gateway`
Expected: Build succeeds

- [ ] **Step 4: Commit**

```bash
git add crates/commando-gateway/src/config.rs
git commit -m "feat(gateway): add streaming config section"
```

### Task 5: Implement Session Management

**Files:**
- Create: `crates/commando-gateway/src/session.rs`
- Modify: `crates/commando-gateway/src/main.rs` (add `mod session;`)

- [ ] **Step 1: Write tests for session management**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_map_create_and_lookup() {
        let mut map = SessionMap::new();
        let (token, _session_id) = map.create_session();
        assert!(map.get_by_token(&token).is_some());
        assert!(map.get_by_token("bogus").is_none());
    }

    #[test]
    fn session_token_rotation() {
        let mut map = SessionMap::new();
        let token1 = map.create_session();
        let token2 = map.rotate_token(&token1).unwrap();
        assert_ne!(token1, token2);
        // Old token no longer valid
        assert!(map.get_by_token(&token1).is_none());
        // New token works
        assert!(map.get_by_token(&token2).is_some());
    }

    #[test]
    fn session_drain_stdout() {
        let mut map = SessionMap::new();
        let (token, _session_id) = map.create_session();
        {
            let session = map.get_by_token_mut(&token).unwrap();
            session.stdout_buffer.extend_from_slice(b"hello world");
        }
        let session = map.get_by_token_mut(&token).unwrap();
        let drained = session.drain_stdout();
        assert_eq!(drained, b"hello world");
        assert!(session.stdout_buffer.is_empty());
    }

    #[test]
    fn session_drain_stderr() {
        let mut map = SessionMap::new();
        let (token, _session_id) = map.create_session();
        {
            let session = map.get_by_token_mut(&token).unwrap();
            session.stderr_buffer.extend_from_slice(b"err msg");
        }
        let session = map.get_by_token_mut(&token).unwrap();
        let drained = session.drain_stderr();
        assert_eq!(drained, b"err msg");
        assert!(session.stderr_buffer.is_empty());
    }

    #[test]
    fn session_cleanup_expired() {
        let mut map = SessionMap::new();
        let (token, _session_id) = map.create_session();
        // Manually set last_polled far in the past
        let session = map.get_by_token_mut(&token).unwrap();
        session.last_polled = Instant::now() - Duration::from_secs(120);
        let expired = map.cleanup_expired(Duration::from_secs(60));
        assert_eq!(expired.len(), 1);
        assert!(map.get_by_token(&token).is_none());
    }

    #[test]
    fn generate_token_is_unique() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert_ne!(t1, t2);
        assert_eq!(t1.len(), 32); // 128-bit = 16 bytes = 32 hex chars
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo +nightly test -p commando-gateway session -- --nocapture`
Expected: FAIL — module not found

- [ ] **Step 3: Implement session.rs**

```rust
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rand::Rng;
use tokio::sync::Notify;

/// Generate a cryptographically random 128-bit hex token.
pub fn generate_token() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    hex::encode(bytes)
}

pub struct Session {
    pub stdout_buffer: Vec<u8>,
    pub stderr_buffer: Vec<u8>,
    pub completed: bool,
    pub exec_result: Option<StreamExecResult>,
    pub last_polled: Instant,
    pub notify: Rc<Notify>,  // Rc so we can clone before awaiting (avoids holding RefCell borrow across .await)
    pub rpc_task: Option<tokio::task::JoinHandle<()>>,  // background RPC task — aborted on idle cleanup
}

/// Minimal result from execStream — only fields available after completion.
pub struct StreamExecResult {
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
}

impl Session {
    fn new() -> Self {
        Self {
            stdout_buffer: Vec::new(),
            stderr_buffer: Vec::new(),
            completed: false,
            exec_result: None,
            last_polled: Instant::now(),
            notify: Rc::new(Notify::new()),
            rpc_task: None,
        }
    }

    pub fn drain_stdout(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.stdout_buffer)
    }

    pub fn drain_stderr(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.stderr_buffer)
    }

    /// Drain at most `max_bytes` from stdout buffer.
    pub fn drain_stdout_up_to(&mut self, max_bytes: usize) -> Vec<u8> {
        if self.stdout_buffer.len() <= max_bytes {
            std::mem::take(&mut self.stdout_buffer)
        } else {
            let rest = self.stdout_buffer.split_off(max_bytes);
            std::mem::replace(&mut self.stdout_buffer, rest)
        }
    }

    /// Drain at most `max_bytes` from stderr buffer.
    pub fn drain_stderr_up_to(&mut self, max_bytes: usize) -> Vec<u8> {
        if self.stderr_buffer.len() <= max_bytes {
            std::mem::take(&mut self.stderr_buffer)
        } else {
            let rest = self.stderr_buffer.split_off(max_bytes);
            std::mem::replace(&mut self.stderr_buffer, rest)
        }
    }

    pub fn touch(&mut self) {
        self.last_polled = Instant::now();
    }

    pub fn total_buffered(&self) -> usize {
        self.stdout_buffer.len() + self.stderr_buffer.len()
    }
}

/// Session map keyed by internal session ID. Tokens are one-time-use and rotate per poll.
pub struct SessionMap {
    sessions: HashMap<String, Session>,      // session_id -> Session
    token_to_session: HashMap<String, String>, // token -> session_id
}

impl SessionMap {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            token_to_session: HashMap::new(),
        }
    }

    /// Create a new session. Returns (page_token, session_id).
    pub fn create_session(&mut self) -> (String, String) {
        let session_id = generate_token();
        let token = generate_token();
        self.sessions.insert(session_id.clone(), Session::new());
        self.token_to_session.insert(token.clone(), session_id.clone());
        (token, session_id)
    }

    /// Look up a session by its current page token (immutable).
    pub fn get_by_token(&self, token: &str) -> Option<&Session> {
        let session_id = self.token_to_session.get(token)?;
        self.sessions.get(session_id)
    }

    /// Look up a session by its current page token (mutable).
    pub fn get_by_token_mut(&mut self, token: &str) -> Option<&mut Session> {
        let session_id = self.token_to_session.get(token)?.clone();
        self.sessions.get_mut(&session_id)
    }

    /// Rotate the page token: invalidate old, generate new. Returns new token.
    pub fn rotate_token(&mut self, old_token: &str) -> Option<String> {
        let session_id = self.token_to_session.remove(old_token)?;
        let new_token = generate_token();
        self.token_to_session.insert(new_token.clone(), session_id);
        Some(new_token)
    }

    /// Remove a session by its current page token.
    pub fn remove_by_token(&mut self, token: &str) -> Option<Session> {
        let session_id = self.token_to_session.remove(token)?;
        self.sessions.remove(&session_id)
    }

    /// Get a session by internal session ID (used by OutputReceiver callback).
    pub fn get_by_id_mut(&mut self, session_id: &str) -> Option<&mut Session> {
        self.sessions.get_mut(session_id)
    }

    /// Get session ID for a given token.
    pub fn session_id_for_token(&self, token: &str) -> Option<&str> {
        self.token_to_session.get(token).map(|s| s.as_str())
    }

    /// Remove expired sessions. Returns list of expired session IDs (for RPC cleanup).
    pub fn cleanup_expired(&mut self, idle_timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        let expired_ids: Vec<String> = self.sessions
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_polled) > idle_timeout)
            .map(|(id, _)| id.clone())
            .collect();

        // Remove expired tokens
        self.token_to_session.retain(|_, sid| !expired_ids.contains(sid));
        // Remove expired sessions and abort their background RPC tasks
        for id in &expired_ids {
            if let Some(mut session) = self.sessions.remove(id) {
                if let Some(handle) = session.rpc_task.take() {
                    handle.abort(); // kills the RPC connection, triggering agent cleanup
                }
            }
        }

        expired_ids
    }
}
```

**Dependencies:** Add to `crates/commando-gateway/Cargo.toml`:
```toml
hex = "0.4"
rand = "0.9"
```
Both are needed for token generation in `session.rs`.

- [ ] **Step 4: Add `mod session;` to gateway lib/main**

Add `mod session;` in the appropriate gateway module file.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo +nightly test -p commando-gateway session -- --nocapture`
Expected: All 6 tests PASS

- [ ] **Step 6: Run all gateway tests**

Run: `cargo +nightly test -p commando-gateway`
Expected: All tests PASS

- [ ] **Step 7: Commit**

```bash
git add crates/commando-gateway/src/session.rs crates/commando-gateway/src/main.rs crates/commando-gateway/Cargo.toml
git commit -m "feat(gateway): add session management for streaming exec"
```

---

## Chunk 3: Gateway RPC Streaming Client

### Task 6: Add remote_exec_stream to Gateway RPC

**Files:**
- Modify: `crates/commando-gateway/src/rpc.rs`

This is the most complex task. Unlike `remote_exec()` which creates a throwaway `LocalSet`, `remote_exec_stream()` runs on the caller's `LocalSet` (the main one from `streamable.rs`). It spawns the RPC connection as a long-lived background task and writes output into the session via the `OutputReceiver` callback.

- [ ] **Step 1: Add OutputReceiver implementation**

The gateway implements `OutputReceiver` to receive chunks from the agent and write them into a `Session`. Since both the `OutputReceiver` callback and session access run inside the same `LocalSet`, use `Rc<RefCell<SessionMap>>` — no `Arc`/`Mutex` needed.

```rust
use std::cell::RefCell;
use std::rc::Rc;

use crate::session::{SessionMap, StreamExecResult};

struct OutputReceiverImpl {
    session_map: Rc<RefCell<SessionMap>>,
    session_id: String,
}

impl commando_common::commando_capnp::output_receiver::Server for OutputReceiverImpl {
    async fn receive(
        self: Rc<Self>,
        params: commando_common::commando_capnp::output_receiver::ReceiveParams,
        _results: commando_common::commando_capnp::output_receiver::ReceiveResults,
    ) -> Result<(), capnp::Error> {
        let data = params.get()?.get_data()?;
        let stream = params.get()?.get_stream();

        let mut map = self.session_map.borrow_mut();
        if let Some(session) = map.get_by_id_mut(&self.session_id) {
            if stream == 0 {
                session.stdout_buffer.extend_from_slice(data);
            } else {
                session.stderr_buffer.extend_from_slice(data);
            }
            session.notify.notify_one();
        }
        Ok(())
    }
}
```

- [ ] **Step 2: Add remote_exec_stream function**

Unlike `remote_exec` which blocks until completion, this spawns the RPC as a background `spawn_local` task and returns the session ID immediately. The caller (handler) then waits on the session's `Notify` for the first page.

```rust
/// Start a streaming exec on a remote agent. Spawns the RPC connection as a
/// long-lived task on the current LocalSet. Output chunks are written into the
/// session via OutputReceiver callback. Returns the internal session ID.
///
/// The caller must be running inside the main LocalSet.
/// The caller must acquire a concurrency slot via `limiter.try_acquire(target)` BEFORE calling
/// this function. The spawned task calls `limiter.release(target)` on completion/error.
pub fn start_remote_exec_stream(
    host: &str,
    port: u16,
    psk: &str,
    command: &str,
    work_dir: &str,
    timeout_secs: u32,
    extra_env: &[(String, String)],
    request_id: &str,
    connect_timeout_secs: u64,
    session_map: Rc<RefCell<SessionMap>>,
    session_id: String,
    limiter: Arc<ConcurrencyLimiter>,
    target_name: String,
) -> tokio::task::JoinHandle<()> {
    let host = host.to_string();
    let psk = psk.to_string();
    let command = command.to_string();
    let work_dir = work_dir.to_string();
    let extra_env = extra_env.to_vec();
    let request_id = request_id.to_string();

    tokio::task::spawn_local(async move {
        // Release concurrency slot when this task exits (success or error)
        struct LimiterGuard { limiter: Arc<ConcurrencyLimiter>, target: String }
        impl Drop for LimiterGuard {
            fn drop(&mut self) { self.limiter.release(&self.target); }
        }
        let _guard = LimiterGuard { limiter: limiter.clone(), target: target_name.clone() };

        let result: anyhow::Result<()> = async {
            let addr = format!("{host}:{port}");

            // Connect with timeout
            let stream = timeout(
                Duration::from_secs(connect_timeout_secs),
                TcpStream::connect(&addr),
            )
            .await
            .context("connect timeout")?
            .context("TCP connect failed")?;

            stream.set_nodelay(true)?;
            let stream = stream.compat();
            let (reader, writer) = stream.split();

            let network = capnp_rpc::twoparty::VatNetwork::new(
                futures::io::BufReader::new(reader),
                futures::io::BufWriter::new(writer),
                capnp_rpc::rpc_twoparty_capnp::Side::Client,
                Default::default(),
            );

            let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), None);
            let disconnector = rpc_system.get_disconnector();
            let auth_client: authenticator::Client =
                rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);

            tokio::task::spawn_local(rpc_system);

            // Authenticate
            let agent_client = authenticate(&auth_client, psk.as_bytes()).await?;

            // Create OutputReceiver capability
            let receiver_impl = OutputReceiverImpl {
                session_map: session_map.clone(),
                session_id: session_id.clone(),
            };
            let receiver_client: commando_common::commando_capnp::output_receiver::Client =
                capnp_rpc::new_client(receiver_impl);

            // Build execStream request
            let mut request = agent_client.exec_stream_request();
            {
                let mut req_builder = request.get().init_request();
                req_builder.set_command(&command);
                req_builder.set_work_dir(&work_dir);
                req_builder.set_timeout_secs(timeout_secs);
                req_builder.set_request_id(&request_id);

                if !extra_env.is_empty() {
                    let mut env_list = req_builder.init_extra_env(extra_env.len() as u32);
                    for (i, (key, value)) in extra_env.iter().enumerate() {
                        let mut entry = env_list.reborrow().get(i as u32);
                        entry.set_key(key);
                        entry.set_value(value);
                    }
                }
            }
            request.get().set_receiver(receiver_client);

            // Await completion — this blocks until the command finishes
            let response = request.send().promise.await?;
            let result = response.get()?.get_result()?;

            // Mark session as completed
            {
                let mut map = session_map.borrow_mut();
                if let Some(session) = map.get_by_id_mut(&session_id) {
                    session.exec_result = Some(StreamExecResult {
                        exit_code: result.get_exit_code(),
                        duration_ms: result.get_duration_ms(),
                        timed_out: result.get_timed_out(),
                    });
                    session.completed = true;
                    session.notify.notify_one();
                }
            }

            // Disconnect the RPC connection
            disconnector.await?;
            Ok(())
        }.await;

        if let Err(e) = result {
            tracing::error!(session_id, error = %e, "streaming exec failed");
            // Mark session as completed with error
            let mut map = session_map.borrow_mut();
            if let Some(session) = map.get_by_id_mut(&session_id) {
                session.exec_result = Some(StreamExecResult {
                    exit_code: -1,
                    duration_ms: 0,
                    timed_out: false,
                });
                session.completed = true;
                session.notify.notify_one();
            }
        }
        // _permit dropped here — concurrency slot released
    })
}
```

- [ ] **Step 3: Verify build**

Run: `cargo +nightly build -p commando-gateway`
Expected: Build succeeds

- [ ] **Step 4: Run clippy**

Run: `cargo +nightly clippy -p commando-gateway -- -D warnings`
Expected: No warnings

- [ ] **Step 5: Commit**

```bash
git add crates/commando-gateway/src/rpc.rs
git commit -m "feat(gateway): add remote_exec_stream with OutputReceiver and long-lived RPC"
```

---

## Chunk 4: Gateway Handler + Integration

### Task 7: Update Gateway Handler for Streaming Exec

**Files:**
- Modify: `crates/commando-gateway/src/handler.rs`
- Modify: `crates/commando-gateway/src/streamable.rs`
- Modify: `crates/commando-gateway/src/main.rs`

- [ ] **Step 1: Thread session map through the gateway**

In `main.rs`, create the `Rc<RefCell<SessionMap>>` and pass it to `build_app()`. In `streamable.rs`, add `session_map` to the state passed to the `LocalSet` worker. In `handler.rs`, add `session_map` parameter to `dispatch_request()`.

Update `streamable.rs` `build_app()` signature:

```rust
pub fn build_app(
    config: Arc<GatewayConfig>,
    registry: Arc<Mutex<Registry>>,
    limiter: Arc<handler::ConcurrencyLimiter>,
    session_map: Rc<RefCell<session::SessionMap>>,  // new
) -> Router
```

The `session_map` is cloned into the `spawn_local` worker closure. The `dispatch_request` signature gains the same parameter.

- [ ] **Step 2: Add commando_output tool to tool list**

In `handler.rs` `process_tools_list()`, add the new tool schema:

```rust
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
```

Also update the `commando_exec` tool description to mention pagination:

> "Execute a command on a target machine. If the response includes a next_page field, the command is still running — call commando_output with the page token to get more output. If there is no next_page, the command has completed."

- [ ] **Step 3: Modify handle_exec to use streaming**

Replace the current `rpc::remote_exec()` call with `rpc::start_remote_exec_stream()`. After starting the stream, store the returned `JoinHandle` in `session.rpc_task`, then wait on the session's `Notify` via `build_page()` for the first page response.

Key flow in `handle_exec`:
```rust
// 1. Acquire concurrency slot (same as before)
if !limiter.try_acquire(&target_name) {
    return make_tool_error(id, "max concurrent commands reached");
}

// 2. Create session
let (token, session_id) = session_map.borrow_mut().create_session();

// 3. Start streaming RPC (limiter slot is released by the spawned task on completion)
let join_handle = rpc::start_remote_exec_stream(
    &host, port, &psk, &command, &work_dir, timeout_secs,
    &extra_env, &request_id, connect_timeout_secs,
    session_map.clone(), session_id.clone(),
    limiter.clone(), target_name,
);

// 4. Store the JoinHandle so idle cleanup can abort it
{
    let mut map = session_map.borrow_mut();
    if let Some(session) = map.get_by_id_mut(&session_id) {
        session.rpc_task = Some(join_handle);
    }
}

// 5. Build and return first page
let resp = build_page(session_map, &token, &config.streaming).await;
```

The core page-building logic (shared between `handle_exec` first page and `handle_output` subsequent pages):

```rust
/// Build a page response from the current session state.
/// Waits up to page_timeout for output or completion.
/// If the command is completed, removes the session from the map.
/// Returns the response JSON.
async fn build_page(
    session_map: &Rc<RefCell<SessionMap>>,
    token: &str,
    config: &StreamingConfig,
) -> Result<Value, String> {
    let page_timeout = Duration::from_secs(config.page_timeout_secs);

    // Phase 1: Wait loop — keeps waiting until we have data, completion, or timeout.
    // Clone the Rc<Notify> so we can drop the RefCell borrow before awaiting.
    let deadline = tokio::time::Instant::now() + page_timeout;
    loop {
        let (has_data, maybe_notify) = {
            let map = session_map.borrow();
            let session = map.get_by_token(token).ok_or("session expired")?;
            let has_data = session.completed
                || session.total_buffered() > 0
                || session.total_buffered() >= page_max;
            if has_data {
                (true, None)
            } else {
                (false, Some(session.notify.clone())) // Rc<Notify> clone — cheap
            }
        }; // borrow dropped here

        if has_data {
            break;
        }

        // Wait for data or remaining timeout (no borrow held)
        if let Some(notify) = maybe_notify {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break; // timeout expired
            }
            let _ = tokio::time::timeout(remaining, notify.notified()).await;
        }
    }

    // Phase 2: Drain buffers and build response (fresh borrow).
    // Drain at most page_max_bytes to enforce per-page size limits.
    let mut map = session_map.borrow_mut();
    let session = map.get_by_token_mut(token).ok_or("session expired")?;
    session.touch();

    let stdout_bytes = session.drain_stdout_up_to(page_max);
    let stderr_bytes = session.drain_stderr_up_to(page_max.saturating_sub(stdout_bytes.len()));
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    // If we drained partial data and there's more, re-notify so next poll returns immediately
    if session.total_buffered() > 0 {
        session.notify.notify_one();
    }

    if session.completed {
        let r = session.exec_result.as_ref().unwrap();
        let mut resp = serde_json::json!({});
        if !stdout.is_empty() { resp["stdout"] = stdout.into(); }
        if !stderr.is_empty() { resp["stderr"] = stderr.into(); }
        resp["exit_code"] = r.exit_code.into();
        resp["duration_ms"] = r.duration_ms.into();
        if r.timed_out { resp["timed_out"] = true.into(); }
        // Remove completed session (drop mutable borrow first, re-borrow)
        drop(map);
        session_map.borrow_mut().remove_by_token(token);
        Ok(resp)
    } else {
        let new_token = map.rotate_token(token).ok_or("session expired")?;
        let mut resp = serde_json::json!({});
        if !stdout.is_empty() { resp["stdout"] = stdout.into(); }
        if !stderr.is_empty() { resp["stderr"] = stderr.into(); }
        resp["next_page"] = new_token.into();
        Ok(resp)
    }
}
```

- [ ] **Step 4: Add format_page_response helper**

This converts the JSON page response into the MCP text format, matching the existing `handle_exec` output style:

```rust
/// Format a page response JSON into MCP tool result text.
/// Matches the existing handle_exec format: stdout, then [stderr], then metadata footer.
fn format_page_response(resp: &Value) -> String {
    let mut parts = Vec::new();

    if let Some(stdout) = resp.get("stdout").and_then(|v| v.as_str()) {
        if !stdout.is_empty() {
            parts.push(stdout.to_string());
        }
    }
    if let Some(stderr) = resp.get("stderr").and_then(|v| v.as_str()) {
        if !stderr.is_empty() {
            parts.push(format!("[stderr]\n{stderr}"));
        }
    }
    if let Some(timed_out) = resp.get("timed_out").and_then(|v| v.as_bool()) {
        if timed_out {
            parts.push("[timed out]".to_string());
        }
    }

    // Metadata footer (only on final page when exit_code is present)
    if let Some(exit_code) = resp.get("exit_code").and_then(|v| v.as_i64()) {
        let duration = resp.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        parts.push(format!("\n---\nexit={exit_code} duration={duration}ms"));
    }

    // Streaming indicator (when there's a next page)
    if let Some(next_page) = resp.get("next_page").and_then(|v| v.as_str()) {
        parts.push(format!("\n---\n[streaming] next_page={next_page}"));
    }

    parts.join("\n")
}
```

- [ ] **Step 5: Add handle_output function (routes `commando_output` calls)**

```rust
async fn handle_output(
    id: &Value,
    args: &Value,
    session_map: &Rc<RefCell<SessionMap>>,
    config: &StreamingConfig,
) -> Value {
    let page = match args.get("page").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return make_tool_error(id, "missing required parameter: page"),
    };

    match build_page(session_map, page, config).await {
        Ok(resp) => {
            // build_page handles session removal internally when completed
            let text = format_page_response(&resp);
            let is_error = resp.get("exit_code")
                .and_then(|v| v.as_i64())
                .is_some_and(|c| c != 0)
                || resp.get("timed_out").and_then(|v| v.as_bool()).unwrap_or(false);
            if is_error {
                make_tool_error(id, &text)
            } else {
                make_tool_result(id, &text)
            }
        }
        Err(e) => make_tool_error(id, &e),
    }
}
```

- [ ] **Step 6: Add commando_output to handle_tools_call dispatch**

```rust
"commando_output" => handle_output(id, args, session_map, &config.streaming).await,
```

- [ ] **Step 7: Spawn idle cleanup timer**

In `streamable.rs` `build_app()`, spawn a periodic cleanup task:

```rust
let cleanup_map = session_map.clone();
let idle_timeout = Duration::from_secs(config.streaming.session_idle_timeout_secs);
tokio::task::spawn_local(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;
        let expired = cleanup_map.borrow_mut().cleanup_expired(idle_timeout);
        if !expired.is_empty() {
            tracing::info!(count = expired.len(), "cleaned up expired streaming sessions");
        }
    }
});
```

- [ ] **Step 8: Verify build**

Run: `cargo +nightly build -p commando-gateway`
Expected: Build succeeds

- [ ] **Step 9: Run all tests**

Run: `cargo +nightly test -p commando-gateway`
Expected: All tests PASS (existing handler tests may need minor adjustments for new `session_map` parameter in `dispatch_request`)

- [ ] **Step 10: Fix any test signature changes**

Existing handler tests call `dispatch_request` — update them to pass a dummy `Rc<RefCell<SessionMap>>`. Also update any test config builders/helpers to include `streaming: StreamingConfig::default()` so existing tests compile. Search for any `GatewayConfig` construction in tests and add the new field.

- [ ] **Step 11: Run full test suite + clippy + fmt**

Run: `cargo +nightly fmt -- --check && cargo +nightly clippy -- -D warnings && cargo +nightly test`
Expected: All pass

- [ ] **Step 12: Commit**

```bash
git add crates/commando-gateway/
git commit -m "feat(gateway): add streaming exec with pagination and commando_output tool"
```

### Task 8: Integration Test

**Files:**
- No new files — manual testing against a running agent

- [ ] **Step 1: Build both binaries**

Run: `cargo +nightly build --release`
Expected: Both `commando-agent` and `commando-gateway` build successfully

- [ ] **Step 2: Test fast command (single page)**

Using the MCP tools via Claude Code or curl, run a fast command like `echo hello`. Verify:
- Response has `stdout`, `exit_code`, `duration_ms` — no `next_page`
- Same behavior as before streaming was added

- [ ] **Step 3: Test slow command (multi-page)**

Run a slow command like `for i in $(seq 1 10); do echo "line $i"; sleep 2; done`. Verify:
- First response has partial stdout + `next_page` token
- Calling `commando_output` with the token returns more output
- Final page has `exit_code` and no `next_page`

- [ ] **Step 4: Test idle timeout**

Start a slow command, get the first page with `next_page`, then wait > 60s without polling. Verify:
- Polling with the stale token returns "session expired" error
- Agent process was killed

- [ ] **Step 5: Commit any fixes**

```bash
git add -A
git commit -m "fix: integration test fixes for streaming exec"
```

### Task 9: Final Cleanup

- [ ] **Step 1: Run full pre-push checks**

```bash
cargo +nightly fmt -- --check
cargo +nightly clippy -- -D warnings
cargo +nightly test
```

- [ ] **Step 2: Update design.md future enhancements**

Remove or update the streaming output bullet in `docs/design.md` Future Enhancements to reference the new spec.

- [ ] **Step 3: Commit and push**

```bash
git add docs/design.md
git commit -m "docs: update future enhancements — streaming exec implemented"
git push
```
