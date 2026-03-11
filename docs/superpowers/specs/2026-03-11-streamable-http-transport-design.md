# Streamable HTTP Transport — Replace SSE

**Date:** 2026-03-11
**Status:** Approved

## Problem

The gateway's SSE transport implements the deprecated MCP SSE transport spec (two endpoints, persistent event streams, session management). The MCP spec has replaced SSE with Streamable HTTP as the standard HTTP transport. Commando's operations are all stateless request/response, making the SSE session machinery unnecessary overhead.

## Solution

Replace `sse.rs` with `streamable.rs` implementing the MCP Streamable HTTP transport spec. Single `POST /mcp` endpoint, no sessions, no persistent connections.

## Endpoints

| Method | Path | Behavior |
|--------|------|----------|
| `POST` | `/mcp` | Receives JSON-RPC message, returns JSON response (200) or 202 Accepted for notifications |
| `GET` | `/mcp` | 405 Method Not Allowed (no server-to-client push needed) |
| `DELETE` | `/mcp` | 405 Method Not Allowed (no session management) |
| `GET` | `/health` | `{"status": "ok"}` |

### POST /mcp behavior

- **JSON-RPC request** (has `id`) — dispatch via channel bridge to `handler::dispatch_request`, return JSON response with `Content-Type: application/json` (200 OK)
- **JSON-RPC notification** (no `id` or null `id`) — return 202 Accepted with no body
- **Invalid JSON** — return JSON-RPC parse error (`code: -32700`) with `Content-Type: application/json`

### Headers

**Request validation:** The MCP spec requires `Content-Type: application/json` on POST requests and `Accept` including both `application/json` and `text/event-stream`. We do not validate these headers — commando is a private homelab service, not a public API. Invalid requests will fail naturally at the JSON parse step. This is a conscious deviation from the spec to keep the implementation simple.

**Response headers:** The server never sends an `Mcp-Session-Id` header. Since no session ID is issued, clients will not be expected to track one. Requests with an `Mcp-Session-Id` header are accepted and the header is ignored.

### Batch requests

JSON-RPC batch requests (arrays) are not supported. If the POST body is a JSON array, return a JSON-RPC error (`code: -32600`, "batch requests not supported"). This keeps dispatch simple — one request in, one response out.

## Architecture

### Channel bridge (unchanged pattern from SSE)

axum handlers run via `tokio::spawn` (outside LocalSet), but `dispatch_request` calls `remote_exec`/`remote_ping` which use capnp-rpc (`!Send` types requiring LocalSet). The channel bridge pattern solves this:

```
POST /mcp handler (tokio::spawn)
    → WorkItem { request, oneshot::Sender }
    → mpsc channel
    → spawn_local worker (inside LocalSet)
    → dispatch_request(...)
    → oneshot response back to handler
    → HTTP JSON response
```

### No session management

The MCP spec says sessions are optional (`MAY assign a session ID`). Commando's agent communication model is connect-per-request (each `remote_exec`/`remote_ping` opens a fresh TCP connection). No state to track between requests. Omitting sessions simplifies the implementation and removes the `SessionMap` entirely.

### No SSE streaming in responses

The spec allows servers to return SSE streams in POST responses for long-running operations. Commando doesn't need this — `dispatch_request` returns a single JSON-RPC response for every request. All operations block until complete (exec waits for the command to finish, ping is fast).

## Config changes

- `default_transport()` changes from `"sse"` to `"streamable-http"`
- Config value `"sse"` is removed; `"streamable-http"` is the HTTP transport option
- `"stdio"` remains unchanged

### MCP client configuration

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

## Files

| Action | File | Description |
|--------|------|-------------|
| Delete | `src/sse.rs` | Remove SSE transport |
| Create | `src/streamable.rs` | Streamable HTTP transport |
| Modify | `src/main.rs` | Swap `"sse"` match arm for `"streamable-http"`, update CLI help strings |
| Modify | `src/config.rs` | Change `default_transport()` to `"streamable-http"` |
| Modify | `src/lib.rs` | Swap `pub mod sse` for `pub mod streamable` |
| Replace | `tests/sse.rs` → `tests/streamable.rs` | New transport tests |
| Modify | `docs/design.md` | Update transport references and MCP config examples |

## Testing

`streamable.rs` exports a `pub fn build_app(...)` that returns a `Router`, used by both `run_streamable_server` and integration tests (same pattern as the current `sse::build_app`).

Test cases:
- Health endpoint returns `{"status": "ok"}`
- POST valid JSON-RPC request returns 200 with JSON response
- POST notification returns 202 with no body
- POST invalid JSON returns JSON-RPC parse error
- POST batch request (JSON array) returns error
- GET /mcp returns 405
- DELETE /mcp returns 405
- POST unknown method returns JSON-RPC method-not-found error
