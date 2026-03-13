# Streaming Exec Output via Pagination

**Date:** 2026-03-12
**Status:** Design approved, pending implementation

## Problem

When Claude Code runs a long command via `commando_exec` (e.g., `apt dist-upgrade`, a build script, a deploy loop), the human supervising the agent has zero visibility until the command completes or times out. If the command fails halfway through a 5-minute run, nobody knows until the timeout expires. The LLM also can't detect problems early and react.

## Solution

Add paginated output to `commando_exec`. The gateway breaks command output into pages based on time (5s) or size (32KB), whichever comes first. Fast commands complete in a single page — same experience as today. Long commands return partial output with a `next_page` token that the LLM polls to get subsequent pages.

### Non-Goals

- **Cancellation** — the LLM cannot cancel a running command mid-stream. Future enhancement.
- **Backward compatibility** — breaking changes to the schema and MCP response format are acceptable.

## Schema Changes

Add `execStream` method and `OutputReceiver` callback interface. Existing `exec` and `ping` unchanged. Adding `execStream @2` is a backward-compatible Cap'n Proto schema evolution — existing clients that only know `exec @0` and `ping @1` continue to work.

```capnp
interface OutputReceiver {
  receive @0 (data :Data, stream :UInt8) -> ();
  # stream: 0 = stdout, 1 = stderr
  # Agent calls this as output arrives from the child process.
}

interface CommandAgent {
  exec @0 (request :ExecRequest) -> (result :ExecResult);
  ping @1 () -> (pong :PingResult);
  execStream @2 (request :ExecRequest, receiver :OutputReceiver)
    -> (result :ExecResult);
}
```

## Agent Changes

`execStream` is a thin variant of `exec`. Same child process spawning, same `setsid`, same clean env, same shell, same timeout/SIGTERM/SIGKILL cleanup.

The only difference is output delivery:

- **`exec`**: buffers stdout/stderr into `Vec<u8>`, returns them in `ExecResult` when command completes. Applies `max_output_bytes` tail truncation.
- **`execStream`**: reads stdout/stderr in a loop with a small buffer (e.g., 4KB). Each chunk is forwarded immediately via `receiver.receive(data, stream_type)`. No buffering, no truncation — the agent's memory usage stays flat regardless of output size. On completion, resolves the promise with `ExecResult` containing exit code, duration, and `timed_out`. The `stdout` and `stderr` fields in `ExecResult` are empty for `execStream` (all output was delivered via the receiver). `truncated` is always false.

The agent remains stateless. It runs the command, forwards output as it arrives, returns the result. No session tracking.

## Gateway Changes

### Long-Lived RPC Connection

The current gateway uses connect-per-request: each `commando_exec` opens a TCP connection, runs the RPC, and disconnects. `execStream` requires the connection to stay alive for the entire command duration because:

- The `OutputReceiver` is a callback capability — the agent calls `receive()` on the gateway over the same RPC connection.
- The `execStream` promise resolves only when the command completes.

For `execStream`, the `RpcSystem` must be spawned as a long-lived task on the gateway's main `LocalSet` (the one in `streamable.rs` where the RPC worker already lives), not in a throwaway per-request `LocalSet`. The connection lifecycle:

1. `commando_exec` arrives → gateway opens TCP connection to agent, authenticates, creates `OutputReceiver` via `capnp_rpc::new_client(receiver_impl)`, sends `execStream` request
2. The `RpcSystem` is `spawn_local`'d on the main `LocalSet` and continues driving the connection — processing incoming `receive()` callbacks from the agent
3. When the command completes (or idle timeout fires), the gateway drops the RPC connection, cleaning up the `RpcSystem` task

The existing `exec` path (used by `commando_ping` and any non-streaming calls) continues using connect-per-request with its own `LocalSet`.

### Session State

The gateway maintains an in-memory session map for running streamed commands. All session access is funneled through the `LocalSet` worker channel — `commando_output` MCP requests are sent as work items to the `LocalSet` worker (same pattern as existing `handle_post` → channel → `dispatch_request`). The `OutputReceiver` callback also runs inside the `LocalSet`, so no cross-thread synchronization is needed for session access.

```
SessionMap: HashMap<String, Session>   // keyed by stable session ID

Session {
    token: String,                    // current valid page token (rotates each poll)
    stdout_buffer: Vec<u8>,           // stdout not yet served to LLM
    stderr_buffer: Vec<u8>,           // stderr accumulated during execution
    stdout_cursor: usize,             // how far the LLM has read stdout
    stderr_cursor: usize,             // how far the LLM has read stderr
    completed: bool,                  // command finished?
    exec_result: Option<ExecResult>,  // set on completion
    last_polled: Instant,             // for idle cleanup
    notify: Notify,                   // wakes waiting handler when data arrives or command completes
}
```

The session map is keyed by a stable internal session ID. The `token` field holds the current one-time-use page token — each poll consumes the current token and generates a new one for the next page. This prevents replay (polling the same token twice) and ensures only the most recent `next_page` value is valid. Token lookup scans the session map matching against `session.token`.

### Stdout/Stderr Handling

The `OutputReceiver.receive()` callback separates stdout and stderr into distinct buffers:

- **`stdout_buffer`** — served incrementally in each page as delta output
- **`stderr_buffer`** — accumulated during execution, interleaved with `[stderr]` markers into mid-stream pages alongside stdout

Both streams are served in every page that has content. On each page, the gateway emits stdout delta, then any stderr delta prefixed with `[stderr]\n`. This matches the existing `handle_exec` format and gives the LLM (and the human) early visibility into errors without waiting for completion.

### Page Boundary Logic

Each MCP response (both `commando_exec` and `commando_output`) waits up to **5 seconds** for output or completion before responding. This avoids empty responses and tight polling loops. The handler `await`s the session's `Notify` primitive with a 5-second `tokio::time::timeout` — the `OutputReceiver` callback and the `execStream` completion handler both call `notify.notify_one()` to wake the waiting handler immediately when data arrives or the command finishes.

A page is emitted when any of these conditions is met:
- **5 seconds elapsed** since the page started
- **32KB of new output** accumulated since the page started
- **Command completed** (regardless of time or size)

### Flow

1. `commando_exec` arrives → gateway generates a cryptographically random session token (128-bit hex), stores `Session`, starts `execStream` RPC as a `spawn_local` task on the main `LocalSet`
2. `OutputReceiver.receive()` appends chunks to the appropriate session buffer (stdout or stderr)
3. Gateway waits up to 5s for output/completion
4. Returns first page:
   - Command completed within 5s → full output + `exit_code`, no `next_page` (same as today)
   - Still running or hit 32KB → delta output + `next_page` token
5. `commando_output(page: "token")` → arrives as work item via channel → `LocalSet` worker reads session, returns delta since last poll, waits up to 5s, resets idle timer
6. Last page: command completed, remaining output + `exit_code` + `duration_ms`, no `next_page`

### Page Token Security

Page tokens are cryptographically random 128-bit hex strings. Since the gateway may serve multiple Claude Code instances over Streamable HTTP, predictable tokens would allow one session to read another's output.

### Idle Cleanup

A periodic `spawn_local` task runs inside the `LocalSet` every 10 seconds, scanning sessions for idle timeout. If no `commando_output` poll has arrived within **60 seconds**:
- Drop the agent RPC connection (triggers SIGTERM → SIGKILL on child process via existing agent `SO_KEEPALIVE` + disconnect cleanup)
- Discard the session and invalidate the page token

Sessions are also cleaned up when the final page (no `next_page`) is served.

Stale page tokens return an MCP error: `"session expired — command was terminated after 60s idle"`.

### Concurrency Limiter

The concurrency slot is held for the entire command duration — from `execStream` start to command completion (or idle timeout cleanup). This is correct: the agent is still running the command. The existing `max_concurrent_per_target=4` limit may be exhausted by long-running commands. This is expected behavior — the limit protects the agent host regardless of whether output is streaming.

### Gateway Restart

If the gateway restarts while streaming sessions are active, all in-memory sessions are lost. The LLM will poll with a stale token and get "session expired." The agent's child process will be cleaned up when the TCP connection drop is detected via `SO_KEEPALIVE`. No session persistence — this is accepted behavior.

## MCP Tool Interface

### `commando_exec` (modified response)

Parameters unchanged: `target`, `command`, `work_dir`, `timeout`, `env`.

Response when command completes within first page window:
```json
{
  "stdout": "complete output...",
  "stderr": "if any...",
  "exit_code": 0,
  "duration_ms": 42
}
```

Response when command is still running:
```json
{
  "stdout": "partial stdout...",
  "stderr": "partial stderr if any...",
  "next_page": "tok_a1b2c3d4e5f6..."
}
```

### `commando_output` (new tool)

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `page` | string | yes | Page token from previous response |

Mid-execution response:
```json
{
  "stdout": "delta since last poll...",
  "stderr": "stderr delta if any...",
  "next_page": "tok_f6e5d4c3b2a1..."
}
```

Final response:
```json
{
  "stdout": "remaining output...",
  "stderr": "remaining stderr if any...",
  "exit_code": 0,
  "duration_ms": 15230
}
```

- **Delta only** — each page contains only new output since the last poll
- **`stdout` and `stderr`** — served in every page that has content. Stderr is prefixed with `[stderr]\n` in the text response.
- **`exit_code`, `duration_ms`** — only on the final page (don't exist until command completes)
- **`timed_out`** — included on the final page if the command hit its timeout

### Tool Description Guidance

The `commando_exec` tool description should include:

> "If the response includes a `next_page` field, the command is still running. Call `commando_output` with the page token to get more output. If there is no `next_page`, the command has completed."

## Configuration

New gateway config fields under `[streaming]`:

```toml
[streaming]
page_timeout_secs = 5         # max wait per page before returning
page_max_bytes = 32768        # 32KB max output per page
session_idle_timeout_secs = 60 # kill command if LLM stops polling
```

## Key Invariants

- Fast commands (< 5s) produce a single response with no `next_page` — indistinguishable from current behavior for the LLM
- The agent is stateless — it forwards output chunks and returns results, no session tracking
- All session state (buffers, page tokens, idle timers) lives in the gateway, accessed exclusively within the `LocalSet` (no cross-thread sync)
- Existing `exec` RPC method is unchanged — `execStream` is a separate method
- The 5s wait per poll prevents tight polling loops that waste LLM inference tokens
- Page tokens are cryptographically random to prevent cross-session leakage
- Concurrency slots are held for the full command duration
