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
commando_exec(target="my-server", command="echo \"hello world\" | grep \"hello\"")
```

One shell layer. Done.

## Performance

The gateway runs as a persistent HTTP server. Commands execute without SSH handshake overhead.

| Method | Latency | Notes |
|--------|---------|-------|
| **Commando** | **~18ms** | HTTP POST on persistent connection |
| **Direct SSH + pct exec** | **~1050ms** | SSH handshake + pct exec per command |

**~58x faster** for command execution. Measured on LAN with `hostname` as the test command.

## Architecture

```
Claude Code (any workstation)
    │
    │ HTTP (MCP JSON-RPC)
    │
    ▼
┌─────────────────────────────────┐
│  Commando Gateway               │
│  (persistent service on LXC)    │
│                                 │
│  ┌───────────┐  ┌────────────┐  │
│  │HTTP Server│  │  Registry  │  │
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

- **Gateway** (`commando-gateway`) — MCP server that receives tool calls from Claude Code over HTTP and routes commands to agents via Cap'n Proto RPC
- **Agent** (`commando-agent`) — Lightweight binary on each target machine that executes commands natively
- **Common** (`commando-common`) — Shared Cap'n Proto schema and HMAC auth helpers

## Transport

The gateway supports two MCP transports:

| Transport | Use Case | Config |
|-----------|----------|--------|
| **Streamable HTTP** (default) | Persistent remote service | `{"type": "http", "url": "http://host:9877/mcp"}` |
| **stdio** | Local development/testing | `{"type": "stdio", "command": "commando-gateway", ...}` |

Streamable HTTP is the primary transport. The gateway runs as a persistent service and Claude Code connects over HTTP — no SSH tunnel, no per-session container spawning.

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

## Getting Started

### Prerequisites

- A Linux host for the gateway (LXC, VM, or bare metal) with Docker
- SSH access to target machines (for agent deployment)
- (Optional) Proxmox cluster for auto-discovery

> **Not using Proxmox?** Skip the `[proxmox]` section in `gateway.toml` entirely. Add targets manually with `[[targets]]` entries and their PSKs under `[agent.psk]`. See Step 3 below.

### Step 1: Deploy the Gateway

The gateway runs as a Docker container. On your gateway host:

```bash
# Create config directory
mkdir -p /etc/commando

# Create gateway config (edit values for your setup)
cat > /etc/commando/gateway.toml << 'EOF'
[server]
transport = "streamable-http"
bind = "0.0.0.0"
port = 9877

# Optional: Proxmox auto-discovery (remove if not using Proxmox)
[proxmox]
nodes = []
user = "root@pam"
token_id = "commando"
token_secret = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"

[agent]
default_port = 9876
default_timeout_secs = 60
connect_timeout_secs = 5
max_concurrent_per_target = 4

# PSKs are added here as you deploy agents (step 2)
[agent.psk]
EOF
chmod 600 /etc/commando/gateway.toml

# Copy docker-compose.yml from the repo (or download it)
cp docker-compose.yml ~/docker-app/

# Or if deploying remotely:
curl -fSL -o ~/docker-app/docker-compose.yml \
  https://raw.githubusercontent.com/icyrainz/commando/main/docker-compose.yml

# Start the gateway
cd ~/docker-app && docker compose up -d
```

Verify it's running:

```bash
curl http://localhost:9877/health
# {"status":"ok"}
```

### Step 2: Deploy Agents to Target Machines

Each target machine needs the agent binary, a config with a unique PSK, and a systemd service.

**Option A: Automated (Proxmox LXCs)**

```bash
# From a machine with SSH access to Proxmox nodes
./deploy/deploy-agents.sh pve-node-1 pve-node-2
```

This pushes the agent to all running LXCs, generates PSKs, and prints them for you to add to `gateway.toml`.

**Option B: Manual (any Linux machine)**

```bash
# 1. Download the agent binary
curl -fSL -o /usr/local/bin/commando-agent \
  https://github.com/icyrainz/commando/releases/latest/download/commando-agent-x86_64-linux
chmod +x /usr/local/bin/commando-agent

# 2. Generate a unique PSK for this agent
PSK=$(openssl rand -hex 32)
echo "PSK: $PSK"  # save this — you'll need it for gateway.toml

# 3. Create agent config
mkdir -p /etc/commando
cat > /etc/commando/agent.toml << EOF
bind = "0.0.0.0"
port = 9876
shell = "bash"           # or "fish", "sh"
psk = "$PSK"
max_output_bytes = 131072
max_concurrent = 8
EOF
chmod 600 /etc/commando/agent.toml

# 4. Install systemd service
cat > /etc/systemd/system/commando-agent.service << 'EOF'
[Unit]
Description=Commando Agent
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/commando-agent --config /etc/commando/agent.toml
Restart=always
RestartSec=5
NoNewPrivileges=yes
ProtectSystem=strict
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now commando-agent
```

### Step 3: Register the Agent in the Gateway

Add the agent's PSK and target entry to the gateway config:

```bash
# On the gateway host, edit /etc/commando/gateway.toml:

# Add the PSK (under [agent.psk])
[agent.psk]
my-target = "the-psk-from-step-2"

# Add a manual target entry (under [[targets]])
[[targets]]
name = "my-target"
host = "192.168.1.50"     # IP or hostname of the target machine
port = 9876
shell = "bash"
tags = ["web", "docker"]  # optional, for filtering with commando_list
```

Restart the gateway to pick up the new config:

```bash
cd ~/docker-app && docker compose restart
```

### Step 4: Connect Claude Code

Add the MCP server to your Claude Code config (`~/.claude.json`):

```json
{
  "mcpServers": {
    "commando": {
      "type": "http",
      "url": "http://gateway-host:9877/mcp"
    }
  }
}
```

Restart Claude Code. You should now see `commando_exec`, `commando_list`, and `commando_ping` as available tools.

### Verify

```
commando_list()                                          # see all targets
commando_ping(target="my-target")                        # health check
commando_exec(target="my-target", command="hostname")    # run a command
```

## Updating

CI builds on tagged releases (`v*`) and publishes:
- **Gateway Docker image** → `ghcr.io/icyrainz/commando-gateway:latest`
- **Agent binary** → GitHub release asset (`commando-agent-x86_64-linux`)

### Update Gateway

```bash
./deploy/deploy-gateway.sh pve-node-1 100          # pull latest, restart
./deploy/deploy-gateway.sh pve-node-1 100 v0.2.0   # deploy specific version

# Or manually:
cd ~/docker-app && docker compose pull && docker compose up -d
```

### Update Agents

```bash
./deploy/update-agents.sh pve-node-1 pve-node-2
COMMANDO_VERSION=v0.2.0 ./deploy/update-agents.sh pve-node-1  # pin version

# Or manually on each target:
curl -fSL -o /usr/local/bin/commando-agent \
  https://github.com/icyrainz/commando/releases/latest/download/commando-agent-x86_64-linux
chmod +x /usr/local/bin/commando-agent
systemctl restart commando-agent
```

### Building from Source

Requires system packages:

| Distro | Command |
|--------|---------|
| Debian/Ubuntu | `sudo apt install capnproto musl-tools` |
| Fedora | `sudo dnf install capnproto musl-gcc` |
| Arch | `sudo pacman -S capnproto musl` |
| macOS | `brew install capnp` (musl not needed — native build) |

```bash
cargo build --release --target x86_64-unknown-linux-musl
```

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
| MCP Transport | Streamable HTTP / stdio |
| Build target | `x86_64-unknown-linux-musl` (static) |
| Runtime | Single-threaded tokio + LocalSet |

See [docs/design.md](docs/design.md) for the full design document.
