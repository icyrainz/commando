# Commando — Zero-Escaping Command Relay for Homelab

**Date:** 2026-03-10
**Status:** Design approved, pending implementation

## Problem

Claude Code instances across the homelab manage many LXC containers and machines across multiple Proxmox nodes. Every remote command requires triple-nested shell escaping:

```bash
ssh root@pve-node "pct exec 100 -- bash -c 'cd /root/docker-app && docker compose ps --format json'"
```

Each layer (local bash → SSH → pct exec → bash -c) interprets quotes, making complex commands fragile and error-prone. Pipes, heredocs, and special characters compound the problem.

## Solution

Commando is a command relay system with two components:

1. **Commando Gateway** — an MCP server on a dedicated LXC that receives tool calls from Claude Code and routes commands to target machines
2. **Commando Agent** — a lightweight binary on each LXC/machine that receives commands over Cap'n Proto RPC and executes them natively

The command string travels through MCP (JSON-RPC) and Cap'n Proto (binary serialization) — neither interprets the string as shell. Only one shell (`sh -c` on the target) ever touches the command. Zero escaping layers in transport.

### Non-Goals

- **Not a config management tool** — Commando is not a replacement for Ansible, Terraform, or NixOS. It relays ad-hoc commands; it does not manage desired state.
- **Not for untrusted networks** — unencrypted Cap'n Proto RPC on a trusted LAN. HMAC challenge-response protects PSKs on the wire, but commands and output are plaintext. No TLS, no certificate management (see Future Enhancements for TLS upgrade path).
- **No Windows support** — agents target Linux only (`sh -c` / `bash -c` / `fish -c`).
- **No multi-tenancy** — single admin, single trust domain. No per-user authorization or audit separation.

### Before vs After

**Before (today):**
```bash
ssh root@pve-node "pct exec 100 -- bash -c 'echo \"hello world\" | grep \"hello\"'"
```

**After (Commando):**
```
commando_exec(target="node-1/my-app", command="echo \"hello world\" | grep \"hello\"")
```

One shell layer. Done.

## Stack

| Component | Technology | Why |
|-----------|-----------|-----|
| Language | Rust | Performance, safety, fun |
| RPC | Cap'n Proto | Zero-copy deserialization, typed schemas, built-in RPC |
| MCP | JSON-RPC over Streamable HTTP or stdio | Standard Claude Code MCP protocol |
| Build | Cargo workspace (monorepo) | Two binary targets from one repo |
| Target | `x86_64-unknown-linux-musl` | Static binaries, no runtime deps |

## Architecture

```
Claude Code (any workstation)
    │
    │ Streamable HTTP (MCP JSON-RPC) — or stdio for local dev
    │
    ▼
┌─────────────────────────────────┐
│  Commando Gateway               │
│  (persistent service on LXC)    │
│                                 │
│  ┌───────────┐  ┌────────────┐  │
│  │ Streamable│  │  Registry  │  │
│  │ HTTP(axum)│──│            │  │
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
 101    102    103    104    metal
 node1  node1  node2  node2
```

## Cap'n Proto Schema

The schema file ID must be globally unique. Generate with `capnp id` before first use.

```capnp
@0xb7c5e6a2d3f41890;  # PLACEHOLDER — replace with output of `capnp id`

# Capability-based auth: the bootstrap interface is Authenticator. The client
# requests a challenge nonce, then proves PSK knowledge via HMAC — the PSK
# never crosses the wire. A successful authenticate() returns a CommandAgent
# capability. Without it, the client has no way to call exec or ping —
# enforced by the type system, not runtime state tracking.
interface Authenticator {
  challenge @0 () -> (nonce :Data);
  # Returns a 32-byte random nonce. The client computes HMAC-SHA256(psk, nonce)
  # and passes it to authenticate().

  authenticate @1 (hmac :Data) -> (agent :CommandAgent, agentVersion :Text);
  # Validates the HMAC against the agent's stored PSK and the previously issued
  # nonce. On success, returns a CommandAgent capability. On failure, throws an
  # RPC exception and disconnects. agentVersion (e.g., "0.1.0") enables future
  # version negotiation. Each nonce is single-use — a second authenticate()
  # call on the same connection requires a new challenge().
}

interface CommandAgent {
  exec @0 (request :ExecRequest) -> (result :ExecResult);
  ping @1 () -> (pong :PingResult);
}

struct ExecRequest {
  command @0 :Text;       # Shell command to execute
  workDir @1 :Text;       # Working directory (empty string = agent's default home dir)
  timeoutSecs @2 :UInt32; # Timeout in seconds (0 = default 60s)
  extraEnv @3 :List(EnvVar);  # Additional environment variables (merged with clean base env)
  requestId @4 :Text;     # Optional correlation ID (gateway-generated UUID, echoed in result and logs)
  # No stdin field — intentional. All input must be expressed within the command
  # itself (heredocs, echo pipes, etc.). This keeps the protocol simple and
  # avoids streaming complexity. Covers all Claude Code use cases.
}

struct EnvVar {
  key @0 :Text;
  value @1 :Text;
}

struct ExecResult {
  stdout @0 :Data;        # Raw stdout bytes
  stderr @1 :Data;        # Raw stderr bytes
  exitCode @2 :Int32;     # Process exit code (0-255 normal, 128+signal for signal kills, e.g., SIGTERM=143, SIGKILL=137)
  durationMs @3 :UInt64;  # Execution wall time in milliseconds
  timedOut @4 :Bool;      # True if the process was killed due to timeout
  truncated @5 :Bool;     # True if output was truncated due to size limit
  requestId @6 :Text;     # Echoed from ExecRequest for cross-component log correlation
}

struct PingResult {
  hostname @0 :Text;      # Machine hostname
  uptimeSecs @1 :UInt64;  # Agent uptime in seconds
  shell @2 :Text;         # Default shell (bash, fish, etc.)
  version @3 :Text;       # Agent version (semver, e.g., "0.1.0")
}
```

## Registry

### Auto-Discovery (Proxmox API)

On startup and every 60 seconds, the gateway queries each configured Proxmox node:

```
GET https://<node-host>:8006/api2/json/nodes/<node-name>/lxc
```

For each running LXC, it extracts:
- VMID, hostname, status (running/stopped)
- IP address (from `lxc/{vmid}/interfaces`)
- Agent port: always `9876`

Auto-discovered LXC targets are keyed by `node-name/hostname` (e.g., `node-1/my-app`) to avoid collisions when LXCs on different Proxmox nodes share the same hostname. Manual targets use plain names (e.g., `my-desktop`).

All MCP tools require fully qualified target names. Claude Code must use `node-1/my-app` for auto-discovered LXCs and `my-desktop` for manual targets. `commando_list` returns the fully qualified name for each target, so Claude Code always knows the correct key. No short-name matching or ambiguity resolution — explicit is better.

This builds a near-real-time inventory of all LXC targets (up to 60s lag for newly created LXCs). Stopped LXCs are listed but marked unavailable.

During each discovery cycle, the gateway also pings each agent (TCP connect + `ping()` RPC) and records reachability in the registry. `commando_list` output includes a `reachable` field (true/false/unknown) so Claude Code can see agent health without explicit ping calls. The ping check uses the same `connect_timeout_secs` (default: 5s) and runs concurrently across all targets to avoid slowing the discovery cycle.

### Manual Targets

Non-LXC machines are registered directly in `gateway.toml` under `[[targets]]`:

```toml
[[targets]]
name = "my-desktop"
host = "my-desktop"        # hostname or IP
port = 9876
shell = "fish"             # default shell for this target
tags = ["gpu", "desktop"]
```

### Merged Registry

Auto-discovered LXCs (keyed by `node/hostname`) and manual targets (keyed by plain name) are merged into a single registry. Manual entries can override auto-discovered ones by using the same `node/hostname` key (e.g., to set a custom shell, tags, or override the IP). The registry is queryable via the `commando_list` MCP tool. All PSKs are stored in `gateway.toml` under `[agent.psk]`, keyed by target name — one source of truth for secrets, regardless of how the target was discovered.

The gateway caches the registry to disk (`/var/lib/commando/registry.json`). On startup, if a cached registry exists, it is loaded immediately so targets are available before the first Proxmox poll completes. If no cache exists (first deploy), the gateway runs one synchronous discovery cycle before accepting MCP requests, ensuring targets are available immediately. The cache is updated after each successful discovery cycle.

Each discovery cycle fully replaces the registry with the combination of freshly discovered auto-discovered LXCs and manual targets from `gateway.toml`. LXCs that no longer exist are removed; new ones appear. The cache exists solely for fast startup — it is never merged with live discovery results. If multiple gateway hosts are deployed, each maintains its own independent cache, which is expected and harmless.

## MCP Tools

The gateway exposes three tools to Claude Code:

### `commando_exec`

Execute a command on a target machine.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `target` | string | yes | Fully qualified target name (e.g., `node-1/my-app`, `my-desktop`) |
| `command` | string | yes | Shell command to execute |
| `work_dir` | string | no | Working directory (default: home dir) |
| `timeout` | number | no | Timeout in seconds (default: 60) |
| `env` | object | no | Additional environment variables (e.g., `{"PGPASSWORD": "xxx"}`) merged with the clean base env |

**Returns:** stdout, stderr, exit code, duration, timed_out, truncated

**Output encoding:** The gateway performs lossy UTF-8 conversion for stdout/stderr — invalid bytes are replaced with U+FFFD. MCP tool results are passed directly to the LLM, so output must always be readable text. No base64 fallback. This is a permanent design choice, not a v1 shortcut — binary output is not a use case for this system.

### `commando_list`

List all registered targets with their status.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `filter` | string | no | Case-insensitive substring match against target name and tags |

**Returns:** Array of targets with name, host, type, status, shell, tags, has_psk, reachable

The `has_psk` field indicates whether the gateway has a PSK configured for this target. Auto-discovered LXCs without a PSK entry will show `has_psk: false` — a signal that the agent needs to be deployed and its PSK added to `gateway.toml`.

### `commando_ping`

Health check a specific agent.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `target` | string | yes | Fully qualified target name |

**Returns:** hostname, uptime, shell, version, reachability

## Components

### Commando Agent (`commando-agent`)

A single static Rust binary (~2-4MB) that runs on every target machine.

**Responsibilities:**
- Listen on configured bind address and TCP port `9876` for Cap'n Proto RPC connections. Enable `SO_KEEPALIVE` on accepted sockets so the agent detects dead connections promptly during long-running commands — this ensures process group cleanup (step 8) fires even if the gateway crashes without a clean TCP close
- Authenticate incoming connections via pre-shared key (PSK) using the `Authenticator` capability interface. On success, returns a `CommandAgent` capability — the client cannot call `exec` or `ping` without it. Simple rate limiting for v1: after 3 consecutive auth failures from the same peer IP, delay responses by 1 second. Per-IP failure counters are held in an in-memory map and cleared on successful auth from that IP
- Enforce a local concurrency limit (`max_concurrent`, default: 8) using an RAII guard tied to connection lifetime — the guard is acquired when an `exec` call starts and released when the connection closes (whether the exec completes normally, times out, or the connection drops). This ensures the counter never leaks. The agent rejects `exec` calls with an RPC error when the limit is reached. This is the true backstop: it protects the host regardless of how many gateways connect. The gateway's `max_concurrent_per_target` (default: 4) is a courtesy limit per gateway instance — multiple Claude Code sessions can collectively reach the agent's hard limit
- Execute commands using the configured shell (`sh -c`, `bash -c`, `fish -c`). Any shell configured in `agent.toml` must support the `-c <command>` interface
- Return stdout, stderr, exit code, and timing
- Cap buffered stdout/stderr at `max_output_bytes` (default 128KB). If either stream exceeds the limit, keep the **last** `max_output_bytes` (tail truncation) and set `truncated = true` in the response. Tail truncation is intentional: errors and final output are at the end, which is what matters for debugging. 128KB is chosen because MCP tool results feed directly into the LLM context window — 1MB+ of text would far exceed useful context
- On timeout: send SIGTERM, wait 5s grace period, then SIGKILL if still alive. Return buffered stdout/stderr with `timedOut = true`
- Respond to ping requests with hostname, uptime, shell, and agent version

**Configuration** (`/etc/commando/agent.toml`, must be `chmod 600 root:root` — contains the agent PSK):
```toml
bind = "10.0.0.5"   # bind to LAN interface only (not 0.0.0.0)
port = 9876
shell = "sh"        # or "bash", "fish", etc.
psk = "per-agent-unique-key"  # unique to this agent, gateway must know it
max_output_bytes = 131_072    # 128KB, tail-truncate stdout/stderr beyond this
max_concurrent = 8            # max simultaneous exec calls, rejects beyond this
# rtk = true                  # wrap commands with rtk for token-optimized output (requires rtk binary on PATH)
```

**Config reload:** The agent reads `agent.toml` once at startup. Changes to PSK, shell, bind address, or limits require a restart (`systemctl restart commando-agent`). There is no hot-reload mechanism — this is intentional to keep the agent simple. PSK rotation is a restart.

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
1. Accept TCP connection (with `SO_KEEPALIVE` enabled)
2. Serve the `Authenticator` interface. On `challenge()`: generate and return a 32-byte random nonce. On `authenticate(hmac)`: validate `HMAC-SHA256(psk, nonce)`, return a `CommandAgent` capability on success, throw RPC exception and disconnect on failure
3. Receive `ExecRequest` via `CommandAgent.exec()` (only reachable after successful auth)
4. Spawn child process using `Command::new(&shell).arg("-c").arg(&command)` with `pre_exec(|| { libc::setsid(); })` to place each command in its own process group. This is the core zero-escaping guarantee: the command is passed as a single OS argument (argv), never interpolated into a string. The command travels from Claude Code through MCP JSON and Cap'n Proto binary serialization without any layer interpreting it as shell. Only the target shell receives it, as-is. No login shell (`-l`) flag — this avoids profile scripts polluting stdout with motd or greeting messages
5. Build a clean environment: start from a minimal base, then apply `extraEnv` overrides. Do not inherit the agent's process environment. No restrictions on env var names — `extraEnv` can override `PATH`, `LD_PRELOAD`, etc. This is accepted risk: the gateway already has full exec-as-root access, so env var restrictions would be security theater. Base env:
   - `HOME=/root`, `USER=root` (agent runs as root)
   - `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`
   - `SHELL` = configured shell from `agent.toml`
   - `LANG=C.UTF-8`, `TERM=dumb`, `NO_COLOR=1` (`TERM=dumb` and `NO_COLOR` are intentional for non-interactive exec: disables pagers, color escape sequences, and curses output that would pollute captured stdout)
6. Wait for completion (with timeout)
7. If timeout expires: SIGTERM the entire process group (`kill(-pgid, SIGTERM)`), wait 5s, SIGKILL the group if still alive, set `timedOut = true`
8. If the RPC connection drops while a child process is running: SIGTERM the entire process group (same 5s grace + SIGKILL). The `setsid()` process group ensures all child processes (e.g., subprocesses spawned by `docker compose`) are cleaned up — no orphans. The agent tracks active child PIDs per connection and cleans up on disconnect
9. Return `ExecResult` with captured stdout/stderr, exit code, timing, and timeout flag

### Commando Gateway (`commando-gateway`)

An MCP server that bridges Claude Code to the agent network.

**Responsibilities:**
- Serve MCP protocol over stdio (launched by Claude Code)
- Maintain the target registry (auto-discovery + manual targets from `gateway.toml`)
- Route `commando_exec` calls to the correct agent via Cap'n Proto RPC — converts the MCP `env` object (`{"KEY": "val"}`) to Cap'n Proto `List(EnvVar)` format
- Enforce per-target concurrency limits via a configurable semaphore (default: 4 concurrent execs per agent)
- Provide `commando_list` and `commando_ping` tools

**Connection model:** Connect-per-request. Each `commando_exec` or `commando_ping` call opens a fresh TCP connection to the target agent, authenticates, executes, and closes. This is simple and avoids stale connection bugs. The latency overhead (~1-5ms on LAN for TCP handshake + auth) is negligible for human-initiated commands. TCP connect timeout is `connect_timeout_secs` (default: 5s) — if the agent host is unreachable, the call fails fast rather than hanging for the OS default (~2 min).

**Error handling:** Before connecting, the gateway validates that the target exists in the registry and has a PSK configured in `[agent.psk]`. Missing targets return an MCP error: `"unknown target: <name>"`. Targets discovered via Proxmox but without a PSK return: `"no PSK configured for target: <name>"`. This makes unconfigured agents visible without silent failures.

**Runtime:** The gateway uses a single-threaded tokio runtime (`tokio::runtime::Builder::new_current_thread()`). This sidesteps `capnp-rpc`'s `!Send` constraint entirely — `RpcSystem` uses `Rc` internally, but on a single-threaded runtime no `Send` bound is required. Concurrent MCP requests (e.g., two `commando_exec` calls) are handled via async I/O on the single thread, which is sufficient since all work is I/O-bound (waiting on agent TCP responses). Each Claude Code instance launches its own gateway process via SSH, so there is no cross-instance contention.

**Configuration** (`/etc/commando/gateway.toml`, must be `chmod 600 root:root` — contains all agent PSKs and the Proxmox API token):
```toml
[proxmox]
nodes = [
  { name = "node-1", host = "192.168.1.10", port = 8006 },
  { name = "node-2", host = "192.168.1.11", port = 8006 },
]
user = "root@pam"
token_id = "commando"
token_secret = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
discovery_interval_secs = 60

[agent]
default_port = 9876
default_timeout_secs = 60
connect_timeout_secs = 5       # TCP connect timeout to agents
max_concurrent_per_target = 4  # semaphore limit per agent

# Per-agent PSKs use fully qualified target names as keys.
# Auto-discovered LXCs: "node-name/hostname", manual targets: plain name.
# Each agent has a unique key, limiting blast radius if compromised.
[agent.psk]
"node-1/my-app" = "aaaa..."
"node-1/my-db" = "bbbb..."
my-desktop = "cccc..."
# ... one entry per target

[[targets]]
name = "my-desktop"
host = "my-desktop"
port = 9876
shell = "fish"
tags = ["gpu", "desktop"]
```

**MCP server configuration** (added to any Claude Code instance that needs homelab access):

Streamable HTTP transport (recommended — persistent service, no SSH):
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

Stdio transport (local development/testing):
```json
{
  "mcpServers": {
    "commando": {
      "command": "commando-gateway",
      "args": ["--config", "/etc/commando/gateway.toml"]
    }
  }
}
```

**Gateway lifecycle:** The gateway runs as a persistent service (Docker container or systemd unit) on a dedicated LXC. Claude Code connects via Streamable HTTP — no SSH tunnel required. The gateway survives Claude Code restarts and maintains a warm registry. Multiple Claude Code sessions connect to the same gateway instance. The only requirements are network access to agents (TCP 9876) and Proxmox API (HTTPS 8006).

For stdio transport, Claude Code launches the gateway on-demand. The process lives for the duration of the session — useful for local development but not recommended for production.

## Authentication

**Per-Agent Pre-Shared Keys (PSK):**
- Each agent has its own unique PSK — a compromised agent only exposes itself, not the fleet
- The gateway stores all PSKs in `[agent.psk]` keyed by target name, and uses the correct one to compute HMAC responses when connecting to each agent
- Each agent only knows its own PSK (configured in its local `agent.toml`)
- Authenticated via HMAC challenge-response over the `Authenticator` capability interface: the gateway calls `challenge()` to receive a 32-byte random nonce, computes `HMAC-SHA256(psk, nonce)`, and passes the result to `authenticate()`. The PSK never crosses the wire. On success, returns a `CommandAgent` capability; on failure, throws an RPC exception and disconnects. Auth is enforced by the Cap'n Proto capability model: without a successful `authenticate()`, the client has no capability to call `exec` or `ping`
- PSKs are generated per-agent during deployment: `openssl rand -hex 32`
- **PSK rotation:** To rotate a PSK: (1) generate a new key, (2) update the gateway's `[agent.psk]` entry (picked up on next Claude Code reconnect), (3) update the agent's `agent.toml`, (4) restart the agent. Updating the gateway first avoids an auth mismatch window — the agent is only briefly unreachable during its restart in step 4. For fleet-wide rotation, use the deploy script to batch the update
- **Security assumptions (trusted LAN):**
  - Cap'n Proto RPC traffic is unencrypted over TCP — commands and output are visible on the wire. Not equivalent to SSH.
  - HMAC challenge-response prevents passive PSK capture, but an attacker on the network can still observe command strings and output. Without TLS, active MITM remains possible.
  - Gateway holds all agent PSKs — compromising the gateway exposes the full fleet. Per-agent PSKs only limit blast radius for individual agent compromise.
  - Agents run as root (no `User=` in systemd unit) — all commands execute with root privileges. This is intentional: LXC containers in this homelab are single-purpose and root-only, and Claude Code needs full access for package management, Docker, and service control.
  - All of this is acceptable for a single-admin homelab on a trusted LAN. If the threat model changes, wrap connections in TLS via `tokio-rustls`.

**Proxmox API Token:**
- A dedicated API token (`root@pam!commando`) for Proxmox auto-discovery
- Scoped to read-only via `PVEAuditor` role — can only list LXCs and query interfaces
- Created via:
  ```bash
  pveum user token add root@pam commando --privsep 1
  pveum acl modify / --token 'root@pam!commando' --roles PVEAuditor
  ```

## Logging

Both components use `tracing` with structured JSON output (for future Loki/Grafana ingestion).

**Agent logs:**
- Connection accepted/rejected (peer IP, auth success/failure)
- Command execution: target, command (truncated at 200 chars), working dir, exit code, duration, truncated, timed out
- Process lifecycle: startup, shutdown, config loaded

**Gateway logs:**
- MCP tool calls: tool name, target, command (truncated), duration
- Registry updates: targets added/removed/changed, discovery cycle duration, Proxmox API errors
- Agent connections: target, connect/disconnect, RPC errors
- Cache operations: load/save, staleness

Log level controlled via `RUST_LOG` env var (default: `info`). Set `debug` for development, `trace` for deep troubleshooting.

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
│   │       ├── handler.rs        # MCP dispatch logic (shared by stdio + streamable-http)
│   │       ├── mcp.rs            # Stdio transport (JSON-RPC over stdin/stdout)
│   │       ├── streamable.rs     # Streamable HTTP transport (HTTP server via axum)
│   │       ├── registry.rs       # Target registry (discovery + manual targets)
│   │       ├── proxmox.rs        # Proxmox API client
│   │       └── rpc.rs            # Cap'n Proto RPC client to agents
│   └── commando-common/
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs            # Shared types, config parsing, auth
├── deploy/
│   ├── agent.service             # Systemd unit for agent
│   └── deploy-agents.sh          # Script to push agent binary to all LXCs
├── config/
│   ├── gateway.toml.example
│   └── agent.toml.example
└── README.md
```

## Key Rust Dependencies

| Crate | Purpose |
|-------|---------|
| `capnp`, `capnpc` | Cap'n Proto serialization + compiler |
| `capnp-rpc` | Cap'n Proto RPC (async, tokio-based) |
| `tokio` | Async runtime |
| `serde`, `serde_json` | JSON for MCP protocol |
| `toml` | Config file parsing (gateway.toml, agent.toml) |
| `reqwest` | HTTP client for Proxmox API |
| `hmac`, `sha2` | HMAC-SHA256 for challenge-response auth |
| `rand` | Nonce generation for auth challenges |
| `axum` | HTTP server for Streamable HTTP transport |
| `tracing`, `tracing-subscriber` | Structured logging (JSON output for Loki ingestion) |

## Deployment Plan

### Phase 1: Build

1. Create the `commando` Cargo workspace
2. Define the Cap'n Proto schema
3. Implement `commando-agent` (exec + ping over Cap'n Proto RPC)
4. Implement `commando-gateway` (MCP server + registry + RPC client)
5. Cross-compile both binaries for `x86_64-unknown-linux-musl`

### Phase 2: Infrastructure

1. Choose a host for the gateway (dedicated LXC, Proxmox host, or existing machine)
2. Create Proxmox API token with scoped permissions:
   ```bash
   pveum user token add root@pam commando --privsep 1
   pveum acl modify / --token 'root@pam!commando' --roles PVEAuditor
   ```
3. Generate per-agent PSKs: `openssl rand -hex 32` (one per target)
4. Deploy `commando-gateway` to the gateway host with gateway.toml
5. Add MCP config to Claude Code instances that need homelab access

### Phase 3: Agent Rollout

`deploy/deploy-agents.sh` automates the rollout and remains the permanent bootstrap and fallback mechanism — it uses SSH + `pct` directly, so it works even when Commando itself is down or buggy. The script takes a list of Proxmox node hostnames as arguments (e.g., `./deploy-agents.sh node-1 node-2`). For each node, it SSHes in and runs `pct` commands — so it can be executed from any machine with SSH access to the Proxmox nodes (including the gateway host). Steps:
1. For each Proxmox node: SSH in and query running LXCs via `pct list`
2. For each LXC: `ssh root@<node> pct push <VMID> commando-agent /usr/local/bin/commando-agent`
3. Generate a unique PSK per agent (`openssl rand -hex 32`), push agent.toml config + systemd unit
4. Collect all generated PSKs and append them to the gateway's `[agent.psk]` table
5. Enable and start `commando-agent.service` via `pct exec`
6. Report success/failure per target
7. Verify all agents with `commando_list` and `commando_ping`

For non-LXC machines (e.g., desktops, bare-metal servers), deploy manually via `scp` with the appropriate `shell` and `bind` config.

### Phase 4: Template Update

1. Bake `commando-agent` binary into the LXC template
2. Add systemd unit to template
3. New LXCs automatically have the agent pre-installed

## Future Enhancements (Not in Scope)

Last updated: 2026-03-14. Feedback sources: homelab user review, DevOps/SRE review, AI tooling developer review.

### Implemented

- ~~**Streaming output:**~~ Paginated output via `execStream` RPC + `commando_output` MCP tool. See `docs/superpowers/specs/2026-03-12-streaming-exec-design.md`.
- ~~**CLI + REST API:**~~ `commando exec/list/ping` CLI with REST endpoints on gateway. See `docs/superpowers/specs/2026-03-14-commando-cli-design.md`.

### High Priority

- **Audit log:** Record all commands executed through Commando — who, what target, when, stdout/stderr, exit code. Written to structured log or append-only store. Table stakes for any tool that gives an LLM root access. *(Requested by: DevOps, homelab reviewers)*
- **File transfer:** `commando_read_file` and `commando_write_file` MCP tools + CLI commands for reading/writing files on targets without shell commands. Handles binary encoding, partial reads. Completes Commando as a remote management toolkit. *(Requested by: AI tooling reviewer)*
- **Batch exec (`commando_exec_batch`):** Fan-out a command to multiple targets in parallel (by name or tag), returning results per target. Single tool call instead of N sequential ones — saves LLM context tokens. CLI: `commando exec --tag web "apt update"`. *(Requested by: homelab, AI tooling reviewers)*
- **LLM-optimized truncation guidance:** When output is truncated, append actionable hints (e.g., "pipe through `tail`/`head`/`grep` to narrow output") so the LLM knows how to explore the rest instead of blindly retrying. *(Requested by: AI tooling reviewer)*
- **CLI `--json` flag:** Structured JSON output from `commando exec/list/ping` for machine parsing. Lets the LLM opt into structured parsing when it needs to reason about output. *(Requested by: AI tooling reviewer)*

### Medium Priority

- **TLS transport:** Wrap agent connections in TLS via `tokio-rustls` for encrypted-on-the-wire security. Eliminates plaintext command visibility concerns and enables deployment outside trusted LANs. *(Requested by: DevOps, AI tooling reviewers)*
- **Non-root execution mode:** Configurable `user` field per target in `gateway.toml`. Agent runs as root but `su`s to the specified user before executing commands. Root should require explicit config. *(Requested by: homelab, DevOps reviewers)*
- **Per-caller authorization:** Replace single API key with per-user credentials (JWT, mTLS, or OIDC). Add target-level ACLs. Required for team use. *(Requested by: DevOps reviewer)*
- **Target-scoped environment presets:** Per-target `env` in `gateway.toml` (e.g., `KUBECONFIG`, `DOCKER_HOST`) injected into every command. Saves tokens by eliminating repetitive `--env` flags. *(Requested by: AI tooling reviewer)*
- **CLI read_timeout bug:** The CLI's 30s `read_timeout` can timeout on commands that take >30s before producing any output. Should be configurable or set to `command_timeout + margin`. *(Found by: AI tooling reviewer)*

### Low Priority / Nice to Have

- **Connection pooling:** Persistent connections with multiplexing for lower latency on high-frequency operations
- **MCP resources:** Expose target list as an MCP resource (not just a tool) for clients that support resource subscriptions
- **Webhook/callback on completion:** Gateway POSTs to a callback URL when a long-running command completes — enables fire-and-forget patterns
- **`commando init` subcommand:** One command that configures Claude Code MCP by editing `~/.claude.json` with `${COMMANDO_URL}` env var references
- **Web UI:** Dashboard showing all agents, status, recent commands
- **Agent auto-update:** Gateway pushes new agent binaries to targets
- **Graceful agent shutdown:** On SIGTERM, SIGTERM all active child process groups (5s grace + SIGKILL) before exiting
- **Version negotiation:** `agentVersion` compatibility checks in gateway to detect incompatible agents
- **Rename `expose_exec_tool`:** Consider `mcp_exec_mode` or similar for clarity
- **Binary checksum verification:** Install scripts should verify checksums or signatures of downloaded binaries
