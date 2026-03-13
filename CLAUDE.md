# Commando

Zero-escaping command relay for homelab. See `docs/design.md` for full design.

## Build

Requires Rust nightly (edition 2024):

```bash
cargo +nightly build --release
```

For static musl binaries (deployment):
```bash
rustup target add x86_64-unknown-linux-musl --toolchain nightly
cargo +nightly build --release --target x86_64-unknown-linux-musl
```

Requires `capnproto` system package: `sudo apt install capnproto`

## Test

```bash
cargo +nightly test
```

## Architecture

- `crates/commando-common/` — Shared Cap'n Proto schema + HMAC auth helpers
- `crates/commando-agent/` — Agent binary (Cap'n Proto RPC server, runs on each target)
- `crates/commando-gateway/` — Gateway binary (MCP stdio server, routes to agents)
- `schema/commando.capnp` — Cap'n Proto interface definition (single source of truth)

## Before Pushing

Always run these before pushing to remote:

```bash
cargo +nightly fmt -- --check
cargo +nightly clippy -- -D warnings
cargo +nightly test
```

## Conventions

- Single-threaded tokio runtime (`current_thread`) — required for capnp-rpc `!Send` types
- All capnp-rpc code runs within `tokio::task::LocalSet`
- Structured JSON logging via `tracing` (agent: stdout, gateway: stderr)
- Gateway logs to stderr because stdout is reserved for MCP JSON-RPC protocol

## Deployment Operations

There are 4 deploy scripts in `deploy/`. Pick the right one for the task:

### Install agent on a new machine (any Linux host)

SSH into the target machine and run:
```bash
curl -sL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-agent.sh | bash
```
Or pin a version: `COMMANDO_VERSION=v0.3.2 bash` instead of `bash`.

This handles everything: downloads the binary for the correct architecture (x86_64/aarch64), installs the systemd service, generates a PSK, and prints the TOML snippets to add to `gateway.toml`. On subsequent runs it preserves existing config and only replaces the binary + service file.

### First-time deploy agents to all Proxmox LXCs

```bash
./deploy/deploy-agents.sh <proxmox-node> [proxmox-node-2] ...
```
Example: `./deploy/deploy-agents.sh akio-lab akio-garage`

Requires: SSH root access to Proxmox nodes. Uses `pct list/push/exec` to deploy to every running LXC. Generates unique PSKs per agent and prints the `[agent.psk]` entries to add to `gateway.toml`. Only use this for initial setup — it overwrites existing agent configs.

### Update agents on all Proxmox LXCs

```bash
./deploy/update-agents.sh <proxmox-node> [proxmox-node-2] ...
```
Example: `./deploy/update-agents.sh akio-lab akio-garage`

Requires: SSH root access to Proxmox nodes. Downloads the latest release from GitHub (or set `COMMANDO_VERSION=v0.3.2`), pushes binary + service file to every running LXC that already has an agent, restarts the service. Does NOT touch config or PSKs. Skips LXCs without an existing agent.

### Update/deploy gateway

```bash
./deploy/deploy-gateway.sh <proxmox-node> <vmid> [version]
```
Example: `./deploy/deploy-gateway.sh akio-lab 111 v0.3.2`

Pulls the Docker image from `ghcr.io/icyrainz/commando-gateway` and restarts the container on the specified LXC. Defaults to `latest` tag.

### Which script to use

| Task | Script | Scope |
|------|--------|-------|
| Add agent to a new standalone machine | `install-agent.sh` | Single machine (any Linux) |
| First-time setup of all Proxmox LXC agents | `deploy-agents.sh` | All LXCs on specified Proxmox nodes |
| Update agent version on all Proxmox LXCs | `update-agents.sh` | All LXCs on specified Proxmox nodes |
| Update agent on a standalone machine | `install-agent.sh` | Single machine (re-run same script) |
| Update gateway Docker container | `deploy-gateway.sh` | Single gateway LXC |

### Important notes

- Cross-compiling musl binaries on macOS doesn't work (missing `x86_64-linux-musl-gcc`). Tag a GitHub release to get binaries from CI instead.
- `deploy-agents.sh` and `update-agents.sh` are Proxmox-specific (use `pct` commands). For non-Proxmox hosts, use `install-agent.sh` via SSH.
- After `install-agent.sh` or `deploy-agents.sh`, the PSK output must be added to the gateway's `gateway.toml` under `[agent.psk]` and the gateway restarted.
- The agent cannot update itself via `commando_exec` — stopping the agent kills its child processes. Always update agents via SSH using the scripts above.
