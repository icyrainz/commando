# Commando

Zero-escaping command relay for homelab. Run commands on any LXC or machine through a single MCP tool call — no SSH escaping, no nested shells.

## The Problem

Remote commands in a Proxmox homelab require triple-nested shell escaping:

```bash
ssh root@pve-node "pct exec 100 -- bash -c 'echo \"hello world\" | grep \"hello\"'"
```

Each layer (local shell → SSH → pct exec → bash -c) interprets quotes, making complex commands fragile and error-prone.

## The Solution

Commando relays commands through MCP (JSON-RPC) and Cap'n Proto (binary serialization) — neither interprets the string as shell. Only one shell on the target ever touches the command.

```
commando_exec(target="akio-ntfy", command="echo \"hello world\" | grep \"hello\"")
```

One shell layer. Done.

## Performance

The gateway runs as a persistent SSE server, so Claude Code maintains a long-lived HTTP connection. Commands execute without SSH handshake overhead.

| Method | Latency | Notes |
|--------|---------|-------|
| **Commando (SSE)** | **~18ms** | HTTP POST on persistent connection |
| **Direct SSH + pct exec** | **~1050ms** | SSH handshake + pct exec per command |

**~58x faster** for command execution. Measured on LAN with `hostname` as the test command.

## Architecture

```
Claude Code (any workstation)
    │
    │ HTTP/SSE (MCP JSON-RPC)
    │
    ▼
┌─────────────────────────────────┐
│  Commando Gateway               │
│  (persistent service on LXC)    │
│                                 │
│  ┌───────────┐  ┌────────────┐  │
│  │ SSE Server│  │  Registry  │  │
│  │ (axum)    │──│            │  │
│  └───────────┘  │ - Proxmox  │  │
│       │         │   auto-disc│  │
│       │         │ - TOML     │  │
│       ▼         │   manual   │  │
│  ┌───────────┐  └────────────┘  │
│  │ Cap'n     │                  │
│  │ Proto RPC │                  │
│  │ Client    │                  │
│  └─────┬─────┘                  │
└────────┼────────────────────────┘
         │
         │ Cap'n Proto RPC (TCP, port 9876)
         │
   ┌─────┼──────┬──────┬──────┐
   ▼     ▼      ▼      ▼      ▼
 Agent  Agent  Agent  Agent  Agent
 LXC    LXC    LXC    LXC    bare
 100    126    128    133    metal
```

**Components:**

- **Gateway** (`commando-gateway`) — MCP server that receives tool calls from Claude Code over HTTP/SSE and routes commands to agents via Cap'n Proto RPC
- **Agent** (`commando-agent`) — Lightweight binary on each target machine that executes commands natively
- **Common** (`commando-common`) — Shared Cap'n Proto schema and HMAC auth helpers

## Transport

The gateway supports two MCP transports:

| Transport | Use Case | Config |
|-----------|----------|--------|
| **SSE** (default) | Persistent remote service | `{"type": "sse", "url": "http://host:9877/sse"}` |
| **stdio** | Local development/testing | `{"type": "stdio", "command": "commando-gateway", ...}` |

SSE is the primary transport. The gateway runs as a persistent service and Claude Code connects over HTTP — no SSH tunnel, no per-session container spawning.

## MCP Tools

### `commando_exec`

Execute a shell command on a target machine.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `target` | string | yes | Target name (e.g., `node-1/my-app`, `my-desktop`) |
| `command` | string | yes | Shell command to execute |
| `work_dir` | string | no | Working directory (default: home dir) |
| `timeout` | number | no | Timeout in seconds (default: 60) |
| `env` | object | no | Additional environment variables |

### `commando_list`

List all registered targets with status, shell, tags, and reachability.

### `commando_ping`

Health check a specific agent. Returns hostname, uptime, shell, and version.

## Releases

CI builds on tagged releases (`v*`) and publishes:
- **Gateway Docker image** → `ghcr.io/icyrainz/commando-gateway:latest`
- **Agent binary** → GitHub release asset (`commando-agent-x86_64-linux`)

### Deploy Gateway

The gateway runs as a Docker container. After a release:

```bash
./deploy/deploy-gateway.sh          # pull latest, restart
./deploy/deploy-gateway.sh v0.2.0   # deploy specific version
```

### Deploy Agents

First-time setup (generates PSKs, installs systemd service):

```bash
./deploy/deploy-agents.sh akio-lab akio-garage
```

Update existing agents to the latest release:

```bash
./deploy/update-agents.sh akio-lab akio-garage
COMMANDO_VERSION=v0.2.0 ./deploy/update-agents.sh akio-lab  # pin version
```

### Building from Source

```bash
cargo build --release --target x86_64-unknown-linux-musl
```

Requires: `sudo apt install capnproto musl-tools`

## Setup

### Claude Code MCP Config

```json
{
  "mcpServers": {
    "commando": {
      "type": "sse",
      "url": "http://gateway-host:9877/sse"
    }
  }
}
```

### Gateway Configuration

```toml
# /etc/commando/gateway.toml

[server]
transport = "sse"    # "sse" (default) or "stdio"
bind = "0.0.0.0"
port = 9877

[proxmox]
nodes = [
    { name = "node-1", host = "192.168.1.10", port = 8006 },
]
user = "root@pam"
token_id = "commando"
token_secret = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

[agent]
default_port = 9876
default_timeout_secs = 60
connect_timeout_secs = 5
max_concurrent_per_target = 4

[agent.psk]
"node-1/my-app" = "output-of-openssl-rand-hex-32"
my-desktop = "output-of-openssl-rand-hex-32"

[[targets]]
name = "my-desktop"
host = "my-desktop"
port = 9876
shell = "fish"
tags = ["gpu", "desktop"]
```

### Agent Configuration

```toml
# /etc/commando/agent.toml

bind = "0.0.0.0"
port = 9876
shell = "bash"
psk = "per-agent-unique-key"
```

Generate PSKs with: `openssl rand -hex 32`

## Security

- **HMAC challenge-response auth** — PSKs never cross the wire
- **Per-agent PSKs** — compromised agent only exposes itself
- **Capability-based access** — Cap'n Proto type system enforces auth before exec
- **Trusted LAN only** — no TLS (commands/output are plaintext on the wire)
- **Agents run as root** — intentional for homelab single-admin use

## Stack

| Component | Technology |
|-----------|-----------|
| Language | Rust |
| Agent RPC | Cap'n Proto |
| Gateway HTTP | axum |
| MCP Transport | SSE (HTTP) / stdio |
| Build target | `x86_64-unknown-linux-musl` (static) |
| Runtime | Single-threaded tokio + LocalSet |

See [docs/design.md](docs/design.md) for the full design document.
