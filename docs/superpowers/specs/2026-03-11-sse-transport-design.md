# SSE Transport for Commando Gateway

## Problem

The gateway currently only supports stdio MCP transport, requiring Claude Code to SSH into the gateway host and spawn a Docker container per session. This is:

- Complex to configure (SSH key paths, Docker args, keepalive settings in MCP config)
- Fragile (SSH disconnects kill the gateway, no reconnection)
- Wasteful (new container + gateway process per Claude Code session)

## Solution

Add HTTP/SSE transport to the gateway so it runs as a persistent service. Claude Code connects directly via `"type": "sse"` — no SSH, no per-session containers.

### Before (stdio via SSH)

```json
"commando": {
  "type": "stdio",
  "command": "ssh",
  "args": ["-i", "/home/akio/.ssh/id_ed25519", "-o", "IdentitiesOnly=yes",
           "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3",
           "root@akio-commando", "docker", "run", "-i", "--rm",
           "--network", "host", "-v", "/etc/commando:/etc/commando:ro",
           "-v", "/var/lib/commando:/var/lib/commando",
           "commando-gateway", "--config", "/etc/commando/gateway.toml"]
}
```

### After (SSE)

```json
"commando": {
  "type": "sse",
  "url": "http://akio-commando:9877/sse"
}
```

## Architecture

### Module Split

The existing `mcp.rs` gets split into three files:

```
mcp.rs (today)  →  handler.rs   (shared JSON-RPC dispatch: initialize, tools/list, tools/call)
                    mcp.rs       (stdio transport loop, calls handler)
                    sse.rs       (axum HTTP server with SSE, calls handler)
```

`handler.rs` contains all the MCP protocol logic (tool definitions, request routing, exec/list/ping handlers). Both transports call the same handler functions and get back JSON-RPC response values.

### Transport Selection

CLI flag `--transport` with values `stdio` or `sse` (default: `sse`). CLI flag overrides `[server].transport` from the config file.

`main.rs` branches after registry/config setup:
- `stdio` → calls `mcp::run_stdio_loop()` (existing behavior)
- `sse` → calls `sse::run_sse_server()` (new)

### SSE Protocol Flow

```
Claude Code                          Gateway (axum)
    |                                     |
    |--GET /sse ------------------------>|  (SSE stream opens)
    |<-- event: endpoint                  |  (sends POST URL)
    |    data: /messages?session_id=xxx   |
    |                                     |
    |--POST /messages?session_id=xxx --->|  (JSON-RPC request)
    |   body: {"method":"initialize"...}  |
    |<-- 202 Accepted                     |
    |<-- event: message                   |  (JSON-RPC response via SSE)
    |    data: {"result":...}             |
    |                                     |
    |--POST /messages?session_id=xxx --->|  (tool call)
    |   body: {"method":"tools/call"...}  |
    |<-- 202 Accepted                     |
    |<-- event: message                   |  (tool result via SSE)
    |    data: {"result":...}             |
    |                                     |
```

### Session Management

- Each `GET /sse` connection creates a session with a UUID and a `tokio::sync::mpsc::Sender<String>`
- Sessions stored in `Arc<Mutex<HashMap<String, mpsc::Sender<String>>>>` (required because axum handlers must be `Send`)
- POST requests look up the session by `session_id` query param, dispatch to handler, push response through the sender
- SSE receiver side uses `tokio_stream::wrappers::ReceiverStream` to convert the channel into an SSE event stream
- When SSE connection drops, the receiver is dropped, causing subsequent `send()` to fail — POST handler detects this, removes the session, and returns 404
- Notifications (e.g. `notifications/initialized`) are acknowledged with 202 but produce no SSE event (same as stdio path which silently drops them)

### Config Addition

```toml
[server]
transport = "sse"    # "stdio" or "sse", default: "sse"
bind = "0.0.0.0"     # HTTP listen address (SSE only)
port = 9877          # HTTP listen port (SSE only)
```

Port 9877 chosen to avoid collision with agent port 9876. CLI flags `--transport`, `--bind`, `--port` override config values.

### Dependencies

Add to workspace `Cargo.toml`:
- `axum = "0.8"` — HTTP server + SSE support (via `axum::response::sse`)
- `tokio-stream = "0.1"` — stream utilities for SSE

Both are lightweight. `axum` reuses `hyper`/`tower` already in the dep tree via `reqwest`.

### Runtime Compatibility

The gateway uses `tokio::runtime::Builder::new_current_thread()` with `LocalSet` for `!Send` cap'n proto types.

**Important**: Axum handlers require `Send + 'static` bounds at compile time, regardless of runtime flavor. This means `Rc<RefCell<_>>` cannot be used directly in axum handlers. The solution:

- **SSE layer state** uses `Arc<Mutex<_>>` for `Registry`, `ConcurrencyLimiter`, `GatewayConfig`, and the session map. No actual contention since everything runs on one thread.
- **Cap'n proto RPC code** (agent connections within `spawn_local` tasks) continues to use the existing `!Send` types freely.
- The `ConcurrencyLimiter` is refactored to use `std::sync::Mutex` instead of `RefCell` so it works in both transports.
- **Stdio transport** continues to work as before — `Arc<Mutex<_>>` is a superset of `Rc<RefCell<_>>` in capability.

### Concurrency

Multiple Claude Code sessions can connect simultaneously via separate SSE streams. Each session's tool calls go through the existing `ConcurrencyLimiter` which already tracks per-target limits. No changes needed.

### Error Handling

- POST with unknown `session_id` → 404
- POST with invalid JSON → JSON-RPC parse error pushed via SSE
- SSE connection drops mid-request → in-flight agent RPC completes, response discarded, session cleaned up
- Agent unreachable → tool error response (existing behavior)

### Health Check

`GET /health` returns 200 with a simple JSON body (`{"status":"ok"}`). Useful for Docker `HEALTHCHECK` and monitoring.

### Graceful Shutdown

On SIGTERM: stop accepting new SSE connections, let in-flight requests complete (with a timeout), then exit. Axum's `graceful_shutdown` with a signal listener handles this.

### Security

No auth on the HTTP endpoint. The gateway runs on a trusted LAN — consistent with the existing design doc's non-goal: "Not for untrusted networks."

### Deployment

The gateway runs as a persistent Docker container (or systemd service) on akio-commando instead of being spawned per-session:

```bash
docker run -d --name commando-gateway \
  --restart unless-stopped \
  --network host \
  -v /etc/commando:/etc/commando:ro \
  -v /var/lib/commando:/var/lib/commando \
  commando-gateway --config /etc/commando/gateway.toml
```

## Files Changed

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Add `axum`, `tokio-stream` deps |
| `crates/commando-gateway/Cargo.toml` | Add `axum`, `tokio-stream` deps |
| `crates/commando-gateway/src/handler.rs` | **New** — extracted MCP dispatch logic |
| `crates/commando-gateway/src/sse.rs` | **New** — axum SSE server |
| `crates/commando-gateway/src/mcp.rs` | Refactor to call handler functions |
| `crates/commando-gateway/src/config.rs` | Add `[server]` section |
| `crates/commando-gateway/src/main.rs` | Add `--transport` CLI arg, branch on transport |
| `config/gateway.toml.example` | Add `[server]` section |

## Non-Goals

- TLS/HTTPS (trusted LAN)
- Authentication on HTTP endpoint
- Streamable HTTP transport (future extension)
- Client-side code (Claude Code handles SSE natively)
- CORS headers (LAN only, no browser clients)
