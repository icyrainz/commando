# MCP Bearer Token Authentication

**Date:** 2026-03-12
**Status:** Approved

## Problem

The gateway's `/mcp` HTTP endpoint accepts any request with no authentication. Anyone who can reach port 9877 gets arbitrary command execution on all agents as root.

## Solution

Add bearer token authentication to the `/mcp` endpoint. A static API key is configured via environment variable or config file. The gateway checks `Authorization: Bearer <token>` on every MCP request.

## Design

### Configuration

**Environment variable** (preferred for Docker deployments):
```yaml
environment:
  - COMMANDO_API_KEY=your-secret-key-here
```

**Config file** (`gateway.toml`):
```toml
[server]
api_key = "your-secret-key-here"
```

`COMMANDO_API_KEY` env var takes precedence over config file. Required for streamable-http transport — gateway refuses to start without it. Stdio transport does not require it (local process).

### Changes

**`config.rs`** — Add `api_key: Option<String>` to `ServerConfig`.

**`main.rs`** — After loading config, check `COMMANDO_API_KEY` env var. If set, override the config value. Gateway refuses to start in streamable-http mode without an API key configured. Stdio transport does not require it (local process).

**`streamable.rs`** — Add axum middleware that:
- Extracts the `Authorization` header
- Validates it matches `Bearer <configured-key>` using constant-time comparison (`subtle` crate)
- Returns 401 `{"error": "unauthorized"}` on mismatch or missing header
- Only applies to `/mcp` routes — `/health` stays open for monitoring

### Client Configuration

```json
{
  "mcpServers": {
    "commando": {
      "type": "http",
      "url": "http://gateway-host:9877/mcp",
      "headers": {
        "Authorization": "Bearer ${COMMANDO_API_KEY}"
      }
    }
  }
}
```

### Security Notes

- Constant-time string comparison to prevent timing attacks
- API key is never logged (even at debug level)
- Without TLS, the bearer token is visible on the wire — acceptable for trusted LAN, documented as a known limitation
- `/health` endpoint remains unauthenticated for load balancer and monitoring use
