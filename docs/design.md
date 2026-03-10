# Commando — Zero-Escaping Command Relay for Homelab

**Date:** 2026-03-10
**Status:** Design approved, pending implementation

## Problem

Claude Code running on LXC 133 (akio-paw) manages 30+ LXC containers and machines across two Proxmox nodes. Every remote command requires triple-nested shell escaping:

```bash
ssh root@akio-lab "pct exec 100 -- bash -c 'cd /root/docker-app && docker compose ps --format json'"
```

Each layer (local bash → SSH → pct exec → bash -c) interprets quotes, making complex commands fragile and error-prone. Pipes, heredocs, and special characters compound the problem.

## Solution

Commando is a command relay system with two components:

1. **Commando Gateway** — an MCP server on a dedicated LXC that receives tool calls from Claude Code and routes commands to target machines
2. **Commando Agent** — a lightweight binary on each LXC/machine that receives commands over Cap'n Proto RPC and executes them natively

The command string travels through MCP (JSON-RPC) and Cap'n Proto (binary serialization) — neither interprets the string as shell. Only one shell (`sh -c` on the target) ever touches the command. Zero escaping layers in transport.

### Before vs After

**Before (today):**
```bash
ssh root@akio-lab "pct exec 127 -- bash -c 'echo \"hello world\" | grep \"hello\"'"
```

**After (Commando):**
```
commando_exec(target="akio-nocodb", command="echo \"hello world\" | grep \"hello\"")
```

One shell layer. Done.

## Stack

| Component | Technology | Why |
|-----------|-----------|-----|
| Language | Rust | Performance, safety, fun |
| RPC | Cap'n Proto | Zero-copy deserialization, typed schemas, built-in RPC |
| MCP | JSON-RPC over stdio | Standard Claude Code MCP protocol |
| Build | Cargo workspace (monorepo) | Two binary targets from one repo |
| Target | `x86_64-unknown-linux-musl` | Static binaries, no runtime deps |

## Architecture

```
Claude Code (LXC 133, akio-paw)
    │
    │ stdio (MCP JSON-RPC)
    │
    ▼
┌─────────────────────────────────┐
│  Commando Gateway               │
│  LXC: akio-commando (new)       │
│                                 │
│  ┌───────────┐  ┌────────────┐  │
│  │ MCP Server│  │  Registry  │  │
│  │ (stdio)   │──│            │  │
│  └───────────┘  │ - Proxmox  │  │
│       │         │   auto-disc│  │
│       │         │ - YAML     │  │
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
   ┌─────┼──────┬──────┬──────┬──────────┐
   ▼     ▼      ▼      ▼      ▼          ▼
 Agent  Agent  Agent  Agent  Agent      Agent
 LXC    LXC    LXC    LXC    LXC       akio-
 100    107    127    131    115        fractal
 akio-  akio-  akio-  akio-  akio-
 lab    lab    lab    lab    garage
```

## Cap'n Proto Schema

```capnp
@0xb7c5e6a2d3f41890;

interface CommandAgent {
  # Must be called first on every new connection. Agent disconnects on failure.
  auth @0 (psk :Text) -> (ok :Bool);

  exec @1 (request :ExecRequest) -> (result :ExecResult);
  ping @2 () -> (pong :PingResult);
}

struct ExecRequest {
  command @0 :Text;       # Shell command to execute
  workDir @1 :Text;       # Working directory (empty = home dir)
  timeoutSecs @2 :UInt32; # Timeout in seconds (0 = default 60s)
  env @3 :List(EnvVar);   # Optional environment variable overrides
}

struct EnvVar {
  key @0 :Text;
  value @1 :Text;
}

struct ExecResult {
  stdout @0 :Data;        # Raw stdout bytes
  stderr @1 :Data;        # Raw stderr bytes
  exitCode @2 :Int32;     # Process exit code
  durationMs @3 :UInt64;  # Execution wall time in milliseconds
}

struct PingResult {
  hostname @0 :Text;      # Machine hostname
  uptimeSecs @1 :UInt64;  # Agent uptime in seconds
  shell @2 :Text;         # Default shell (bash, fish, etc.)
}
```

## Registry

### Auto-Discovery (Proxmox API)

On startup and every 60 seconds, the gateway queries both Proxmox nodes:

```
GET https://akio-lab:8006/api2/json/nodes/akio-lab/lxc
GET https://akio-garage:8006/api2/json/nodes/akio-garage/lxc
```

For each running LXC, it extracts:
- VMID, hostname, status (running/stopped)
- IP address (from `lxc/{vmid}/interfaces`)
- Agent port: always `9876`

This builds a live inventory of all LXC targets. Stopped LXCs are listed but marked unavailable.

### Manual Targets (`/etc/commando/targets.yaml`)

Non-LXC machines are registered via YAML config:

```yaml
targets:
  - name: akio-fractal
    host: akio-fractal        # hostname or IP
    port: 9876
    shell: fish               # default shell for this target
    type: machine             # "machine" (not LXC)
    tags:
      - gpu
      - desktop
```

### Merged Registry

Auto-discovered LXCs and manual targets are merged into a single registry. Manual entries can override auto-discovered ones (e.g., to set a custom shell or tags). The registry is queryable via the `commando_list` MCP tool.

## MCP Tools

The gateway exposes three tools to Claude Code:

### `commando_exec`

Execute a command on a target machine.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `target` | string | yes | Target hostname (e.g., `akio-arr`, `akio-fractal`) |
| `command` | string | yes | Shell command to execute |
| `work_dir` | string | no | Working directory (default: home dir) |
| `timeout` | number | no | Timeout in seconds (default: 60) |

**Returns:** stdout, stderr, exit code, duration

### `commando_list`

List all registered targets with their status.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `filter` | string | no | Filter by name, tag, or status |

**Returns:** Array of targets with name, host, type, status, shell, tags

### `commando_ping`

Health check a specific agent.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `target` | string | yes | Target hostname |

**Returns:** hostname, uptime, shell, reachability

## Components

### Commando Agent (`commando-agent`)

A single static Rust binary (~2-4MB) that runs on every target machine.

**Responsibilities:**
- Listen on TCP port `9876` for Cap'n Proto RPC connections
- Authenticate incoming connections via pre-shared key (PSK)
- Execute commands using the machine's default shell (`sh -c` or `fish -c`)
- Return stdout, stderr, exit code, and timing
- Respond to ping requests with hostname, uptime, and shell info

**Configuration** (`/etc/commando/agent.toml`):
```toml
port = 9876
shell = "sh"        # or "fish" for akio-fractal
psk = "shared-secret-key-here"
```

**Systemd unit** (`/etc/systemd/system/commando-agent.service`):
```ini
[Unit]
Description=Commando Agent
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/commando-agent --config /etc/commando/agent.toml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

**Process execution flow:**
1. Accept TCP connection
2. Wait for `auth(psk)` call — disconnect if invalid
3. Receive `ExecRequest` via Cap'n Proto RPC
4. Spawn child process: `sh -c "<command>"` (or configured shell)
5. Apply `env` overrides and working directory if specified
6. Wait for completion (with timeout)
7. Capture stdout/stderr, exit code
8. Return `ExecResult` with timing data

### Commando Gateway (`commando-gateway`)

An MCP server that bridges Claude Code to the agent network.

**Responsibilities:**
- Serve MCP protocol over stdio (launched by Claude Code)
- Maintain the target registry (auto-discovery + YAML)
- Route `commando_exec` calls to the correct agent via Cap'n Proto RPC
- Handle connections to agents (see concurrency note below)
- Provide `commando_list` and `commando_ping` tools

**Concurrency note:** `capnp-rpc`'s `RpcSystem` uses `Rc` internally and is `!Send`. Each agent connection must be driven on a `tokio::task::spawn_local()` within a `LocalSet`. The gateway should use a single `LocalSet` driving all agent connections, or one `LocalSet` per connection on a dedicated thread. This is a known constraint of the crate — do not attempt `tokio::spawn()` with `RpcSystem`.

**Configuration** (`/etc/commando/gateway.toml`):
```toml
[proxmox]
nodes = [
  { name = "akio-lab", host = "192.168.0.227", port = 8006 },
  { name = "akio-garage", host = "192.168.0.197", port = 8006 },
]
user = "root@pam"
token_id = "commando"
token_secret = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
discovery_interval_secs = 60

[agent]
default_port = 9876
psk = "shared-secret-key-here"
default_timeout_secs = 60

[manual_targets]
config_path = "/etc/commando/targets.yaml"
```

**MCP server configuration** (added to Claude Code on LXC 133):
```json
{
  "mcpServers": {
    "commando": {
      "command": "ssh",
      "args": [
        "-o", "ServerAliveInterval=15",
        "-o", "ServerAliveCountMax=3",
        "root@akio-commando",
        "/usr/local/bin/commando-gateway",
        "--config", "/etc/commando/gateway.toml"
      ]
    }
  }
}
```

**Gateway lifecycle:** The gateway runs as a persistent systemd service on akio-commando. Claude Code connects to it via SSH stdio, which is a single long-lived SSH connection (not per-command). If the SSH connection drops (network blip, Claude restart), Claude Code reconnects and gets a warm gateway with a pre-populated registry — no re-discovery delay. The gateway process itself survives SSH disconnects because it's managed by systemd, not by the SSH session.

## Authentication

**Pre-Shared Key (PSK):**
- A shared secret configured on both gateway and all agents
- Transmitted via the `auth()` RPC method — the mandatory first call on every new connection
- Agent disconnects immediately if `auth(psk)` returns `ok = false`
- **Note:** Cap'n Proto RPC traffic is unencrypted over TCP. On a trusted LAN this is acceptable (same machines, same admin), but it is NOT equivalent to SSH which encrypts the transport. If this matters later, wrap connections in TLS via `tokio-rustls`.

**Proxmox API Token:**
- A dedicated API token (`root@pam!commando`) for Proxmox auto-discovery
- Scoped to read-only via `PVEAuditor` role — can only list LXCs and query interfaces
- Created via:
  ```bash
  pveum user token add root@pam commando --privsep 1
  pveum acl modify / --token 'root@pam!commando' --roles PVEAuditor
  ```

## Repo Structure

```
commando/
├── Cargo.toml                    # Workspace root
├── schema/
│   └── commando.capnp            # Cap'n Proto schema (shared)
├── crates/
│   ├── commando-agent/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── main.rs           # Agent binary
│   ├── commando-gateway/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs           # Gateway binary entry point
│   │       ├── mcp.rs            # MCP server (stdio JSON-RPC)
│   │       ├── registry.rs       # Target registry (discovery + YAML)
│   │       ├── proxmox.rs        # Proxmox API client
│   │       └── rpc.rs            # Cap'n Proto RPC client to agents
│   └── commando-common/
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs            # Shared types, config parsing, auth
├── deploy/
│   ├── agent.service             # Systemd unit for agent
│   ├── gateway.service           # Systemd unit for gateway
│   └── deploy-agents.sh          # Script to push agent binary to all LXCs
├── config/
│   ├── gateway.toml.example
│   ├── agent.toml.example
│   └── targets.yaml.example
└── README.md
```

## Key Rust Dependencies

| Crate | Purpose |
|-------|---------|
| `capnp`, `capnpc` | Cap'n Proto serialization + compiler |
| `capnp-rpc` | Cap'n Proto RPC (async, tokio-based) |
| `tokio` | Async runtime |
| `serde`, `serde_json` | JSON for MCP protocol |
| `toml` | Config file parsing |
| `serde_yaml` | Manual targets YAML |
| `reqwest` | HTTP client for Proxmox API |

## Deployment Plan

### Phase 1: Build

1. Create the `commando` Cargo workspace
2. Define the Cap'n Proto schema
3. Implement `commando-agent` (exec + ping over Cap'n Proto RPC)
4. Implement `commando-gateway` (MCP server + registry + RPC client)
5. Cross-compile both binaries for `x86_64-unknown-linux-musl`

### Phase 2: Infrastructure

1. Create `akio-commando` LXC from template 1000 on akio-lab
2. Create Proxmox API token with scoped permissions:
   ```bash
   pveum user token add root@pam commando --privsep 1
   pveum acl modify / --token 'root@pam!commando' --roles PVEAuditor
   ```
3. Generate PSK: `openssl rand -hex 32`
4. Deploy `commando-gateway` to akio-commando with gateway.toml + targets.yaml
5. Enable gateway systemd service
6. Add MCP config to Claude Code settings on LXC 133

### Phase 3: Agent Rollout

`deploy/deploy-agents.sh` automates the rollout:
1. Query Proxmox API for all running LXCs on both nodes
2. For each LXC: `pct push <VMID> commando-agent /usr/local/bin/commando-agent`
3. Push agent.toml config (with PSK) and systemd unit
4. Enable and start `commando-agent.service` via `pct exec`
5. Deploy to akio-fractal separately via `scp` (with `shell = "fish"` config)
6. Report success/failure per target
7. Verify all agents with `commando_list` and `commando_ping`

### Phase 4: Template Update

1. Bake `commando-agent` binary into LXC template 1000
2. Add systemd unit to template
3. Update homelab skill documentation with Commando usage

## Future Enhancements (Not in Scope)

- **File transfer:** Read/write files on targets without shell commands
- **Streaming output:** Cap'n Proto streaming for long-running commands
- **Service helpers:** Typed tools like `docker_compose(action, service)`, `systemctl(action, unit)`
- **Web UI:** Dashboard showing all agents, status, recent commands
- **Audit log:** Record all commands executed through Commando
- **Agent auto-update:** Gateway pushes new agent binaries to targets
