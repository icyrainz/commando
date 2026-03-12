# Commando

Run commands on any Linux machine through MCP tool calls. No SSH escaping, no nested shells, no Ansible playbooks. One tool call, one shell layer.

```
commando_exec(target="web-server", command="docker compose ps --format json")
```

Your AI coding agent gets `commando_exec`, `commando_list`, and `commando_ping` as MCP tools — it can manage your entire fleet without you writing a single SSH command.

## Why

Every remote command through SSH requires shell escaping. Add a container runtime and it gets worse:

```bash
# Without Commando: escape hell
ssh root@server "bash -c 'echo \"hello world\" | grep \"hello\"'"

# With Proxmox LXCs: triple-nested escaping
ssh root@pve-node "pct exec 100 -- bash -c 'cd /app && docker compose ps --format json'"
```

Each layer (local shell → SSH → container exec → bash -c) interprets quotes. Pipes, heredocs, and special characters break constantly.

Commando transports commands through MCP (JSON-RPC) and Cap'n Proto (binary serialization) — neither interprets the string as shell. The command arrives at the target machine untouched. Only one `sh -c` ever runs it.

## AI Agent Efficiency

SSH is expensive for AI coding agents — not just in latency, but in tokens and context window.

| | SSH | Commando |
|--|-----|----------|
| Command | `ssh root@node "pct exec 100 -- bash -c 'cmd'"` | `command="cmd"` |
| Escaping | Agent reasons about nested quotes every call | Zero — command passed as-is |
| Target lookup | `ssh + pct list`, parse output, map hostname→VMID | `commando_list()` — one call |
| Escaping failures | Common → retry loop burns tokens and context | Doesn't happen |

Every failed SSH command with broken quoting costs 3-4 rounds of agent reasoning to fix. With Commando, that entire class of errors is eliminated.

## Performance

The gateway is a persistent HTTP server. No SSH handshake per command.

| Method | Latency | Notes |
|--------|---------|-------|
| **Commando** | **~18ms** | HTTP POST → Cap'n Proto RPC |
| **SSH** | **~1050ms** | SSH handshake + command per invocation |

**~58x faster.** Measured on LAN with `hostname` as the test command.

For an AI agent executing dozens of commands per task, this is the difference between a responsive workflow and waiting.

## How It Works

```
AI Coding Agent (Claude Code, etc.)
    │
    │ HTTP (MCP JSON-RPC)
    ▼
┌──────────────────────────────┐
│  Commando Gateway            │
│  (one persistent service)    │
│                              │
│  HTTP Server ── Registry     │
│       │        - auto-disc   │
│       ▼        - manual TOML │
│  Cap'n Proto                 │
│  RPC Client                  │
└───────┬──────────────────────┘
        │ Cap'n Proto RPC (TCP 9876)
  ┌─────┼──────┬──────┬──────┐
  ▼     ▼      ▼      ▼      ▼
Agent  Agent  Agent  Agent  Agent
(any Linux machine — LXC, VM, bare metal, cloud)
```

- **Gateway** — MCP server (Docker container or binary). Receives tool calls, routes to agents.
- **Agent** — ~3MB static binary on each target. Executes commands, returns stdout/stderr/exit code.
- Commands travel through binary serialization, never through a shell, until the target machine runs `sh -c`.

## Quick Start

### 1. Deploy the Gateway

On any Linux machine with Docker:

```bash
mkdir -p /etc/commando

cat > /etc/commando/gateway.toml << 'EOF'
[server]
transport = "streamable-http"
bind = "0.0.0.0"
port = 9877

[agent]
default_port = 9876
default_timeout_secs = 60
connect_timeout_secs = 5

# PSKs added here as you deploy agents
[agent.psk]
EOF
chmod 600 /etc/commando/gateway.toml

# Generate an API key for MCP endpoint authentication
API_KEY=$(openssl rand -hex 32)
echo "Your API key: $API_KEY"

# Download and start (set COMMANDO_API_KEY in docker-compose.yml or env)
curl -fSL -o docker-compose.yml \
  https://raw.githubusercontent.com/icyrainz/commando/main/docker-compose.yml
COMMANDO_API_KEY=$API_KEY docker compose up -d
```

The `COMMANDO_API_KEY` environment variable is **required** — the gateway refuses to start without it. Save this key; you'll need it to configure your AI agent.

Verify: `curl http://localhost:9877/health` → `{"status":"ok"}`

### 2. Install Agents

SSH into each target machine and run:

```bash
curl -sL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-agent.sh | bash
```

That's it. The script downloads the correct binary (x86_64 or aarch64), installs the systemd service, generates a unique PSK, and prints what to add to your gateway config:

```
=== NEXT STEPS ===

1. Add the PSK to your gateway config (/etc/commando/gateway.toml):

   [agent.psk]
   "web-server" = "a1b2c3d4..."

2. Add this machine as a target in the same file:

   [[targets]]
   name = "web-server"
   host = "192.168.1.50"
   shell = "sh"
   tags = []

3. Restart the gateway to pick up the changes.
```

Copy those snippets into your `gateway.toml`, restart the gateway (`docker compose restart`), and the target is live.

To pin a version: `curl -sL ... | COMMANDO_VERSION=v0.3.2 bash`

### 3. Connect Your AI Agent

Add the MCP server to Claude Code (`~/.claude.json`), using the API key from step 1:

```json
{
  "mcpServers": {
    "commando": {
      "type": "http",
      "url": "http://gateway-host:9877/mcp",
      "headers": {
        "Authorization": "Bearer YOUR_API_KEY"
      }
    }
  }
}
```

Or via CLI:
```bash
claude mcp add commando --transport http --url http://gateway-host:9877/mcp \
  --header "Authorization: Bearer YOUR_API_KEY"
```

Your agent now has three tools:

| Tool | Purpose |
|------|---------|
| `commando_exec(target, command, ...)` | Run a command on any target |
| `commando_list(filter?)` | List all targets with status and reachability |
| `commando_ping(target)` | Health check — hostname, uptime, shell, version |

### 4. Verify

```
commando_list()                                          # see all targets
commando_ping(target="web-server")                       # health check
commando_exec(target="web-server", command="hostname")   # run a command
```

## Proxmox Auto-Discovery

If you run Proxmox, the gateway can automatically discover all your LXC containers. Add to `gateway.toml`:

```toml
[proxmox]
nodes = [
  { name = "pve-1", host = "192.168.1.10", port = 8006 },
]
user = "root@pam"
token_id = "commando"
token_secret = "your-api-token"
discovery_interval_secs = 60
```

The gateway polls Proxmox every 60 seconds, discovers running LXCs, and merges them into the target registry. New LXCs appear automatically — just deploy the agent and add a PSK.

For bulk deployment to all LXCs on a Proxmox node:
```bash
./deploy/deploy-agents.sh pve-1 pve-2    # first-time setup (generates PSKs)
./deploy/update-agents.sh pve-1 pve-2    # update existing agents
```

## Updating

CI builds on tagged releases and publishes:
- **Gateway** → `ghcr.io/icyrainz/commando-gateway` (Docker image)
- **Agent** → GitHub release assets (`commando-agent-x86_64-linux`, `commando-agent-aarch64-linux`)

**Update agents** — re-run the install script on each target:
```bash
curl -sL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-agent.sh | bash
```
It preserves existing config and only replaces the binary + service file.

**Update gateway:**
```bash
cd ~/docker-app && docker compose pull && docker compose up -d
```

## Security

- **Bearer token auth** — MCP endpoint requires `Authorization: Bearer <key>` (constant-time comparison). `/health` stays open.
- **HMAC challenge-response** — Agent PSKs never cross the wire
- **Per-agent PSKs** — compromised agent only exposes itself, not the fleet
- **Capability-based access** — Cap'n Proto type system enforces auth before exec
- **Trusted LAN only** — no TLS (commands/output are plaintext on the wire)
- **Agents run as root** — designed for single-admin environments

## Building from Source

Requires `capnproto` system package:

| Distro | Command |
|--------|---------|
| Debian/Ubuntu | `sudo apt install capnproto musl-tools` |
| Fedora | `sudo dnf install capnproto musl-gcc` |
| Arch | `sudo pacman -S capnproto musl` |
| macOS | `brew install capnp` |

```bash
cargo build --release --target x86_64-unknown-linux-musl
```

## Stack

| Component | Technology |
|-----------|-----------|
| Language | Rust |
| Agent RPC | Cap'n Proto (zero-copy serialization) |
| Gateway HTTP | axum |
| MCP Transport | Streamable HTTP / stdio |
| Build target | `x86_64-unknown-linux-musl` (static, ~3MB) |

See [docs/design.md](docs/design.md) for the full design document.
