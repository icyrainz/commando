# Commando

Zero-escaping command relay for homelab. See `docs/design.md` for full design.

## Build

```bash
cargo build --release
```

For static musl binaries (deployment):
```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

Requires `capnproto` system package: `sudo apt install capnproto`

## Test

```bash
cargo test
```

## Architecture

- `crates/commando-common/` — Shared Cap'n Proto schema + HMAC auth helpers
- `crates/commando-agent/` — Agent binary (Cap'n Proto RPC server, runs on each target)
- `crates/commando-gateway/` — Gateway binary (MCP stdio server, routes to agents)
- `schema/commando.capnp` — Cap'n Proto interface definition (single source of truth)

## Conventions

- Single-threaded tokio runtime (`current_thread`) — required for capnp-rpc `!Send` types
- All capnp-rpc code runs within `tokio::task::LocalSet`
- Structured JSON logging via `tracing` (agent: stdout, gateway: stderr)
- Gateway logs to stderr because stdout is reserved for MCP JSON-RPC protocol
