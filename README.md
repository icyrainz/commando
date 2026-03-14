# Commando

Run commands on any Linux machine through MCP tool calls. No SSH escaping, no nested shells, no Ansible playbooks. One tool call, one shell layer.

```
commando_exec(target="web-server", command="docker compose ps --format json")
```

Your AI coding agent gets `commando_list` and `commando_ping` as MCP tools for target discovery, and the `commando` CLI for command execution with full output streaming.

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

- **Gateway** — MCP server + REST API (Docker container or binary). Receives tool calls and CLI requests, routes to agents.
- **Agent** — ~3MB static binary on each target. Executes commands, returns stdout/stderr/exit code.
- **CLI** — Thin HTTP client (`commando exec`). Claude Code calls it via Bash for native output rendering and streaming.
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

### 3. Install the CLI

On the machine where you run Claude Code (macOS or Linux):

```bash
curl -sSL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-cli.sh | bash
```

Set environment variables (add to your shell config):

```bash
export COMMANDO_URL="http://gateway-host:9877"
export COMMANDO_API_KEY="YOUR_API_KEY"
```

Verify: `commando list` should show your targets.

### 4. Connect Claude Code (MCP)

Add the MCP server for target discovery:

```bash
claude mcp add commando --transport http --url "$COMMANDO_URL/mcp" \
  --header "Authorization: Bearer $COMMANDO_API_KEY"
```

Claude Code now has:

| Component | Purpose |
|-----------|---------|
| `commando_list` (MCP tool) | Discover available targets |
| `commando_ping` (MCP tool) | Health check a target |
| `commando exec` (CLI via Bash) | Execute commands with full output streaming |

The MCP tools provide discovery. The CLI provides execution with native Bash rendering — no truncated output, no wasted LLM round-trips.

### 5. Verify

```bash
commando list                                      # see all targets
commando ping web-server                           # health check
commando exec web-server "hostname"                # run a command
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
- **CLI** → GitHub release assets (`commando-cli-x86_64-linux`, `commando-cli-aarch64-linux`, `commando-cli-aarch64-macos`)

**Update CLI:**
```bash
curl -sSL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-cli.sh | bash
```

**Update agents** — re-run the install script on each target:
```bash
curl -sL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-agent.sh | bash
```
It preserves existing config and only replaces the binary + service file.

**Update gateway:**
```bash
cd ~/docker-app && docker compose pull && docker compose up -d
```

## RTK Integration (Token Optimization)

Agents optionally support [RTK](https://github.com/rtk-ai/rtk) — a CLI proxy that reduces token usage by 60-90% on common dev commands (`git`, `docker`, `ls`, etc.). When enabled, commands are wrapped with `rtk` before execution. Unrecognized commands pass through unchanged.

### Setup

1. Install RTK on the target machine:
   ```bash
   curl -fsSL https://raw.githubusercontent.com/rtk-ai/rtk/refs/heads/master/install.sh | sh
   ```

2. Enable in the agent config (`/etc/commando/agent.toml`):
   ```toml
   rtk = true
   ```

3. Restart the agent:
   ```bash
   systemctl restart commando-agent
   ```

RTK is safe to enable globally — it passes through commands it doesn't optimize unchanged.

## Security

- **Bearer token auth** — MCP endpoint requires `Authorization: Bearer <key>` (constant-time comparison). `/health` stays open.
- **HMAC challenge-response** — Agent PSKs never cross the wire
- **Per-agent PSKs** — compromised agent only exposes itself, not the fleet. PSKs are generated during agent install and stored in `gateway.toml`. They're set-and-forget — to rotate, re-run `install-agent.sh` on the target and update the PSK in `gateway.toml`. Mismatches produce a clear auth error in gateway logs.
- **Capability-based access** — Cap'n Proto type system enforces auth before exec
- **Agents run as root** — designed for single-admin environments

### Threat Model

Commando is designed for **trusted LANs only**. It gives root shell access to every machine in your fleet — do not expose it to the public internet.

**What's encrypted:**
- Bearer token auth protects the MCP endpoint from unauthorized access
- HMAC challenge-response ensures PSKs never cross the wire during agent auth

**What's not encrypted:**
- Commands and their output (stdout/stderr) travel as plaintext between gateway and agents (Cap'n Proto RPC) and between your AI agent and the gateway (HTTP)
- The bearer token itself is sent in plaintext HTTP headers

If your AI agent connects from outside your LAN (e.g., cloud-hosted coding agent), you **must** add TLS in front of the gateway. If you're concerned about LAN sniffing, use a network-level overlay.

### Recommended: Reverse Proxy with TLS

Put Caddy, Traefik, or nginx in front of the gateway for HTTPS. Example with Caddy:

```
commando.lan {
    reverse_proxy localhost:9877
}
```

Caddy handles TLS automatically (self-signed for `.lan` domains, or ACME for public domains). Your MCP config then uses `https://`:

```json
{
  "mcpServers": {
    "commando": {
      "type": "http",
      "url": "https://commando.lan/mcp",
      "headers": {
        "Authorization": "Bearer YOUR_API_KEY"
      }
    }
  }
}
```

### Recommended: WireGuard or Tailscale

For encrypting all traffic (including gateway ↔ agent), use a network-level overlay like [Tailscale](https://tailscale.com/) or [WireGuard](https://www.wireguard.com/). This encrypts everything without any code changes:

- Gateway and agents communicate over the encrypted overlay
- Your AI agent connects to the gateway's Tailscale/WireGuard IP
- No certificates to manage, no reverse proxy needed

This is the simplest path to full encryption and is standard practice in homelabs.

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
