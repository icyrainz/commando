# Public Release: Low-Hanging Fruit

Addresses the quick-win items from `TODO-public-release.md` before tackling larger security and architecture changes (bearer token auth, TLS, etc.).

## Scope

10 items organized into 4 groups by file locality.

## Group 1: Deploy & Config Files

### 1a. Parameterize deploy-gateway.sh

**File:** `deploy/deploy-gateway.sh`

Replace hardcoded `NODE="akio-lab"` / `VMID=134` with required positional args:

```bash
NODE="${1:?Usage: $0 <node> <vmid> [version]}"
VMID="${2:?Usage: $0 <node> <vmid> [version]}"
VERSION="${3:-latest}"
```

Update the header comment to match the new usage.

### 1b. Fix agent.toml.example IP

**File:** `config/agent.toml.example`

Change `bind = "10.0.0.5"` → `bind = "0.0.0.0"` with a comment to restrict to a LAN interface if desired. Aligns with the README's manual setup guide.

### 1c. Add docker-compose.yml

**File:** `docker-compose.yml` (repo root)

Extract the inline compose file from the README into a standalone file. Include a cache volume mount for the registry. Update README Step 1 to reference this file instead of inlining it.

```yaml
services:
  commando-gateway:
    image: ghcr.io/icyrainz/commando-gateway:latest
    container_name: commando-gateway
    restart: unless-stopped
    network_mode: host
    volumes:
      - /etc/commando:/etc/commando:ro
      - commando-cache:/var/lib/commando
    command: ["--config", "/etc/commando/gateway.toml"]

volumes:
  commando-cache:
```

### 1d. Systemd hardening

**File:** `deploy/commando-agent.service`

Add hardening directives. The agent needs root for command execution, so no `User=` change, but restrict everything else:

```ini
NoNewPrivileges=yes
ProtectSystem=strict
PrivateTmp=yes
```

Note: `ProtectHome` is intentionally omitted — the agent executes arbitrary commands as child processes, and those commands may legitimately write to home directories. The agent itself logs to stdout (not `/var/log`), so no `ReadWritePaths` needed beyond what `ProtectSystem=strict` already allows.

## Group 2: CI (ARM Builds)

**File:** `.github/workflows/release.yml`

### Build matrix

Add `aarch64-unknown-linux-musl` target alongside existing `x86_64-unknown-linux-musl` using a matrix strategy. The aarch64 target requires cross-compilation tooling:

- Install `gcc-aarch64-linux-gnu` as the cross-linker
- Set `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc`
- For musl: Ubuntu's `musl-tools` only provides x86_64. Use `musl-cross` from `musl.cc` or the `cross-rs` tool for aarch64 musl support. Alternatively, use `cargo-zigbuild` which bundles musl for all targets.
- Upload artifacts with target name in the key (e.g., `binaries-x86_64`, `binaries-aarch64`) to avoid collisions.

### Multi-arch Docker image

Use `docker/build-push-action` with `platforms: linux/amd64,linux/arm64`. Update `Dockerfile.gateway` to accept a build arg or use `--platform` selectors for the COPY.

### GitHub release binaries

Download both artifact sets, rename with arch suffix:
- `commando-gateway-x86_64-linux`
- `commando-gateway-aarch64-linux`
- `commando-agent-x86_64-linux`
- `commando-agent-aarch64-linux`

No macOS builds — agents and gateway deploy on Linux. macOS users build from source.

## Group 3: Gateway Crate

### 3a. Default discovered shell to "sh"

**File:** `crates/commando-gateway/src/registry.rs` line 90

Change `shell: "bash".to_string()` → `shell: "sh".to_string()`. Use `config::default_shell()` (make it `pub`) instead of a string literal, so the default is shared between manual and discovered targets and cannot drift.

### 3b. Configurable cache path

**Files:** `crates/commando-gateway/src/config.rs`, `crates/commando-gateway/src/main.rs`

1. Add `cache_dir` field to `GatewayConfig` with default `"/var/lib/commando"`
2. Add `--cache-dir` CLI arg to `Cli` struct
3. Replace hardcoded `/var/lib/commando` paths in `main.rs:95` and `main.rs:216` with the config/CLI value

### 3c. Show stopped LXCs

**File:** `crates/commando-gateway/src/proxmox.rs`

Two changes:

1. **Skip the `/interfaces` API call for non-running LXCs** — stopped containers don't have a guest agent, so the call always fails. Check `lxc.status` before making the HTTP request.
2. **Include stopped LXCs with `host: ""`** — when `lxc.status != "running"`, push a `DiscoveredTarget` with `host: ""` (empty string) and the actual status (e.g., "stopped").

The registry already has `status` and `reachability` fields, so `commando_list` surfaces them as stopped/unreachable.

Additionally, add a guard in the exec handler (`handler.rs`): if the target's host is empty, return an error like `"target '{}' is {} (no IP available)"` using the target's status field, rather than letting it fall through to a confusing DNS resolution failure on `""`.

## Group 4: Documentation

### 4a. Document capnproto build dependency

**File:** `README.md`

Enhance the "Building from Source" section with a per-distro table:

| Distro | Command |
|--------|---------|
| Debian/Ubuntu | `sudo apt install capnproto musl-tools` |
| Fedora | `sudo dnf install capnproto musl-gcc` |
| Arch | `sudo pacman -S capnproto musl` |
| macOS | `brew install capnp` (musl not needed — native build) |

### 4b. Document non-Proxmox setup as first-class path

**Files:** `crates/commando-gateway/src/config.rs`, `crates/commando-gateway/src/main.rs`, `README.md`

Code change: Make `proxmox` field optional in `GatewayConfig` (`pub proxmox: Option<ProxmoxConfig>`) with corresponding adjustments in `main.rs` at these access points:

1. `main.rs:63` — startup log `proxmox_nodes` count (use `0` when `None`)
2. `main.rs:118` — initial discovery gate (skip when `None`)
3. `main.rs:128` — background discovery loop gate (skip when `None`)
4. `main.rs:163` — discovery iteration (guarded by the above)
5. `main.rs:164` — passed to `discover_node()` (guarded by the above)

Update `config.rs` tests: add a test for config without `[proxmox]` section. Update `gateway.toml.example` to show `[proxmox]` as optional with a comment.

README changes:
- Add a "Manual-Only Setup" example with a minimal `gateway.toml` that omits `[proxmox]` entirely
- Present manual setup first, Proxmox auto-discovery second (since manual is simpler and universal)

### 4c. Fix stale SSE references in README

**File:** `README.md`

The README references SSE transport in 10+ locations but the code migrated to Streamable HTTP in `feb249d`. Replace all SSE references throughout `README.md`:

- Line 27: "persistent SSE server" → "persistent HTTP server"
- Line 31: "Commando (SSE)" → "Commando (Streamable HTTP)"
- Line 41: "HTTP/SSE (MCP JSON-RPC)" → "HTTP (MCP JSON-RPC)"
- Line 49: "SSE Server" → "HTTP Server"
- Line 73: "HTTP/SSE" → "HTTP"
- Line 83: transport table `"type": "sse"` → `"type": "http"`, URL `/sse` → `/mcp`
- Line 86: "SSE is the primary transport" → "Streamable HTTP is the primary transport"
- Line 129: `transport = "sse"` → `transport = "streamable-http"` in getting-started config
- Lines 267-268: Claude Code config `"type": "sse"` → `"type": "http"`, URL `/sse` → `/mcp`
- Line 336: "SSE (HTTP) / stdio" → "Streamable HTTP / stdio"

Also update `config/gateway.toml.example` line 4: `"sse"` → `"streamable-http"`.

## Out of Scope (Next Iteration)

These items from `TODO-public-release.md` are deferred:
- Bearer token / API key auth on `/mcp` endpoint
- TLS between gateway and agents
- Multi-stage Dockerfile (build inside container)
- Shell auto-detection (probe target for available shell)
- Output streaming / large output recovery
- PSK management tooling (`commando-gateway verify-psks`)
