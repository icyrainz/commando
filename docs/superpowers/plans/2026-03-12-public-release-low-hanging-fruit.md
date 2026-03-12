# Public Release: Low-Hanging Fruit — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Address 10 quick-win items from `TODO-public-release.md` to reduce adoption friction and improve quality before sharing on r/selfhosted.

**Architecture:** No architectural changes — these are config fixes, CI additions, small code changes, and documentation updates grouped by file locality.

**Tech Stack:** Rust, GitHub Actions, Docker, systemd, TOML

**Spec:** `docs/superpowers/specs/2026-03-11-public-release-low-hanging-fruit-design.md`

---

## Chunk 1: Deploy & Config Files

### Task 1: Parameterize deploy-gateway.sh

**Files:**
- Modify: `deploy/deploy-gateway.sh`

- [ ] **Step 1: Replace hardcoded values with positional args**

Change lines 1-16 of `deploy/deploy-gateway.sh` to:

```bash
#!/usr/bin/env bash
# Deploy commando-gateway to a Proxmox LXC.
# Pulls the latest image from ghcr.io and restarts the container.
#
# Usage: ./deploy-gateway.sh <node> <vmid> [version]
#   node:    Proxmox node hostname (e.g., pve-node-1)
#   vmid:    LXC container ID (e.g., 100)
#   version: tag to deploy (default: latest)
#
# Example:
#   ./deploy-gateway.sh pve-node-1 100          # deploy latest
#   ./deploy-gateway.sh pve-node-1 100 v0.2.0   # deploy specific version

set -euo pipefail

NODE="${1:?Usage: $0 <node> <vmid> [version]}"
VMID="${2:?Usage: $0 <node> <vmid> [version]}"
VERSION="${3:-latest}"
```

The rest of the script (lines 17-26) stays unchanged — it already uses `$NODE`, `$VMID`, `$VERSION`.

- [ ] **Step 2: Verify the script is syntactically valid**

Run: `bash -n deploy/deploy-gateway.sh`
Expected: No output (success)

- [ ] **Step 3: Commit**

```bash
git add deploy/deploy-gateway.sh
git commit -m "fix: parameterize deploy-gateway.sh (remove hardcoded node/vmid)"
```

---

### Task 2: Fix agent.toml.example IP

**Files:**
- Modify: `config/agent.toml.example`

- [ ] **Step 1: Replace hardcoded IP**

Change line 4 of `config/agent.toml.example` from:

```toml
bind = "10.0.0.5"         # Bind to LAN interface (not 0.0.0.0)
```

to:

```toml
bind = "0.0.0.0"           # Bind to all interfaces (or restrict to a LAN IP)
```

- [ ] **Step 2: Commit**

```bash
git add config/agent.toml.example
git commit -m "fix: use 0.0.0.0 placeholder in agent.toml.example"
```

---

### Task 3: Add docker-compose.yml

**Files:**
- Create: `docker-compose.yml`

- [ ] **Step 1: Create docker-compose.yml at repo root**

```yaml
services:
  commando-gateway:
    image: ghcr.io/icyrainz/commando-gateway:latest
    container_name: commando-gateway
    restart: unless-stopped
    network_mode: host
    volumes:
      - /etc/commando:/etc/commando:ro
      - commando-cache:/var/lib/commando  # registry cache persists across restarts

    command: ["--config", "/etc/commando/gateway.toml"]

volumes:
  commando-cache:
```

- [ ] **Step 2: Validate compose syntax**

Run: `docker compose -f docker-compose.yml config > /dev/null`
Expected: No errors (or skip if docker not available locally — the YAML is simple enough)

- [ ] **Step 3: Commit**

```bash
git add docker-compose.yml
git commit -m "feat: add docker-compose.yml for gateway deployment"
```

---

### Task 4: Systemd hardening

**Files:**
- Modify: `deploy/commando-agent.service`

- [ ] **Step 1: Add hardening directives to [Service] section**

After the `RestartSec=5` line in `deploy/commando-agent.service`, add:

```ini
NoNewPrivileges=yes
ProtectSystem=strict
PrivateTmp=yes
```

The full `[Service]` section should be:

```ini
[Service]
Type=simple
ExecStart=/usr/local/bin/commando-agent --config /etc/commando/agent.toml
Restart=always
RestartSec=5
NoNewPrivileges=yes
ProtectSystem=strict
PrivateTmp=yes
```

- [ ] **Step 2: Commit**

```bash
git add deploy/commando-agent.service
git commit -m "fix: add systemd hardening to agent service file"
```

---

## Chunk 2: CI (ARM Builds)

### Task 5: Add aarch64 to release workflow and Dockerfile

**Files:**
- Modify: `.github/workflows/release.yml`
- Modify: `Dockerfile.gateway`

- [ ] **Step 1: Update Dockerfile.gateway with build arg**

Replace the current `Dockerfile.gateway` with:

```dockerfile
FROM scratch
ARG TARGETARCH=amd64
COPY --chmod=755 binaries-${TARGETARCH}/commando-gateway /commando-gateway
ENTRYPOINT ["/commando-gateway"]
```

Note: `TARGETARCH` is automatically set by `docker buildx` when using `--platform`. The artifact directories will be named `binaries-amd64` and `binaries-arm64` matching Docker's architecture names.

- [ ] **Step 2: Rewrite release.yml with build matrix**

Replace `.github/workflows/release.yml` with:

```yaml
name: Release

on:
  push:
    tags:
      - "v*"

permissions:
  contents: write
  packages: write

jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        include:
          - target: x86_64-unknown-linux-musl
            arch: amd64
            linker: ""
            packages: "musl-tools"
          - target: aarch64-unknown-linux-musl
            arch: arm64
            linker: aarch64-linux-gnu-gcc
            packages: "gcc-aarch64-linux-gnu"
            use_zigbuild: true
    steps:
      - uses: actions/checkout@v4

      - name: Install toolchain
        run: |
          sudo apt-get update
          sudo apt-get install -y capnproto ${{ matrix.packages }}
          rustup target add ${{ matrix.target }}

      - name: Install cargo-zigbuild
        if: matrix.use_zigbuild
        run: pip3 install ziglang && cargo install cargo-zigbuild

      - uses: Swatinem/rust-cache@v2
        with:
          key: ${{ matrix.target }}-release

      - name: Build release binaries (native)
        if: ${{ !matrix.use_zigbuild }}
        run: cargo build --release --target ${{ matrix.target }}

      - name: Build release binaries (zigbuild)
        if: matrix.use_zigbuild
        run: cargo zigbuild --release --target ${{ matrix.target }}

      - name: Upload binaries
        uses: actions/upload-artifact@v4
        with:
          name: binaries-${{ matrix.arch }}
          path: |
            target/${{ matrix.target }}/release/commando-gateway
            target/${{ matrix.target }}/release/commando-agent

  gateway-image:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: actions/download-artifact@v4
        with:
          path: artifacts/

      - name: Prepare Docker context
        run: |
          mkdir -p binaries-amd64 binaries-arm64
          cp artifacts/binaries-amd64/commando-gateway binaries-amd64/
          cp artifacts/binaries-arm64/commando-gateway binaries-arm64/
          chmod +x binaries-amd64/commando-gateway binaries-arm64/commando-gateway

      - name: Set up QEMU
        uses: docker/setup-qemu-action@v3

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Log in to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Extract version tag
        id: version
        run: echo "tag=${GITHUB_REF#refs/tags/}" >> "$GITHUB_OUTPUT"

      - name: Build and push Docker image
        uses: docker/build-push-action@v6
        with:
          context: .
          file: Dockerfile.gateway
          push: true
          platforms: linux/amd64,linux/arm64
          tags: |
            ghcr.io/icyrainz/commando-gateway:${{ steps.version.outputs.tag }}
            ghcr.io/icyrainz/commando-gateway:latest

  github-release:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: artifacts/

      - name: Rename binaries
        run: |
          mv artifacts/binaries-amd64/commando-gateway commando-gateway-x86_64-linux
          mv artifacts/binaries-amd64/commando-agent commando-agent-x86_64-linux
          mv artifacts/binaries-arm64/commando-gateway commando-gateway-aarch64-linux
          mv artifacts/binaries-arm64/commando-agent commando-agent-aarch64-linux
          chmod +x commando-*-linux

      - name: Create GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          generate_release_notes: true
          files: |
            commando-gateway-x86_64-linux
            commando-agent-x86_64-linux
            commando-gateway-aarch64-linux
            commando-agent-aarch64-linux
```

- [ ] **Step 3: Verify YAML syntax**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))"`
Expected: No errors

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/release.yml Dockerfile.gateway
git commit -m "feat: add aarch64 ARM builds to release workflow"
```

---

## Chunk 3: Gateway Crate Changes

### Task 6: Default discovered shell to "sh"

**Files:**
- Modify: `crates/commando-gateway/src/config.rs` (make `default_shell` pub)
- Modify: `crates/commando-gateway/src/registry.rs` (use shared default)

- [ ] **Step 1: Make `default_shell()` public in config.rs**

In `crates/commando-gateway/src/config.rs`, change line 88 from:

```rust
fn default_shell() -> String { "sh".to_string() }
```

to:

```rust
pub fn default_shell() -> String { "sh".to_string() }
```

- [ ] **Step 2: Use shared default in registry.rs**

In `crates/commando-gateway/src/registry.rs`, add at the top (after the existing `use` statements):

```rust
use crate::config;
```

Then change line 90 from:

```rust
                    shell: "bash".to_string(),
```

to:

```rust
                    shell: config::default_shell(),
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p commando-gateway`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/commando-gateway/src/config.rs crates/commando-gateway/src/registry.rs
git commit -m "fix: default discovered targets to sh instead of bash"
```

---

### Task 7: Configurable cache path

**Files:**
- Modify: `crates/commando-gateway/src/config.rs`
- Modify: `crates/commando-gateway/src/main.rs`

- [ ] **Step 1: Add `cache_dir` to GatewayConfig**

In `crates/commando-gateway/src/config.rs`, add a default function alongside the others (after line 88):

```rust
pub fn default_cache_dir() -> String { "/var/lib/commando".to_string() }
```

Add the `cache_dir` field to `GatewayConfig` (after the `targets` field):

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    pub proxmox: ProxmoxConfig,
    pub agent: AgentConnectionConfig,
    #[serde(default)]
    pub targets: Vec<ManualTarget>,
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
}
```

- [ ] **Step 2: Add `--cache-dir` CLI arg**

In `crates/commando-gateway/src/main.rs`, add to the `Cli` struct:

```rust
    /// Registry cache directory
    #[arg(long)]
    cache_dir: Option<String>,
```

Add the CLI override in `main()`, after the existing CLI overrides (after line 47):

```rust
    if let Some(cache_dir) = &cli.cache_dir {
        config.cache_dir = cache_dir.clone();
    }
```

- [ ] **Step 3: Replace hardcoded paths in run_gateway()**

In `crates/commando-gateway/src/main.rs`, replace line 95:

```rust
    let cache_path = std::path::Path::new("/var/lib/commando/registry.json");
```

with:

```rust
    let cache_path = std::path::Path::new(&config.cache_dir).join("registry.json");
```

Note: `cache_path` changes from `&Path` to `PathBuf`, which is fine since it's used with `std::fs::read_to_string()` which accepts `AsRef<Path>`.

- [ ] **Step 4: Replace hardcoded paths in run_discovery_cycle()**

In `crates/commando-gateway/src/main.rs`, the `run_discovery_cycle` function needs access to `cache_dir`. Add it as a parameter.

Change the function signature from:

```rust
async fn run_discovery_cycle(
    config: &config::GatewayConfig,
    registry: &Arc<Mutex<Registry>>,
) {
```

to:

```rust
async fn run_discovery_cycle(
    config: &config::GatewayConfig,
    registry: &Arc<Mutex<Registry>>,
) {
```

(Signature stays the same — `config` already has `cache_dir`.)

Replace lines 216-221:

```rust
    let cache_dir = std::path::Path::new("/var/lib/commando");
    if let Err(e) = std::fs::create_dir_all(cache_dir) {
```

with:

```rust
    let cache_dir = std::path::Path::new(&config.cache_dir);
    if let Err(e) = std::fs::create_dir_all(cache_dir) {
```

- [ ] **Step 5: Update handler.rs test helpers to include cache_dir**

In `crates/commando-gateway/src/handler.rs`, update the `test_config()` and `test_config_with_target()` functions. Add `cache_dir: "/tmp/commando-test".to_string(),` to each `GatewayConfig` construction. For example in `test_config()`:

```rust
    fn test_config() -> Arc<GatewayConfig> {
        Arc::new(GatewayConfig {
            server: Default::default(),
            proxmox: crate::config::ProxmoxConfig {
                nodes: vec![],
                user: String::new(),
                token_id: String::new(),
                token_secret: String::new(),
                discovery_interval_secs: 60,
            },
            agent: crate::config::AgentConnectionConfig {
                default_port: 9876,
                default_timeout_secs: 60,
                connect_timeout_secs: 5,
                max_concurrent_per_target: 4,
                psk: Default::default(),
            },
            targets: vec![],
            cache_dir: "/tmp/commando-test".to_string(),
        })
    }
```

Do the same for `test_config_with_target()`.

- [ ] **Step 6: Add config test for cache_dir**

In `crates/commando-gateway/src/config.rs`, add a test:

```rust
    #[test]
    fn cache_dir_defaults() {
        let toml_str = r#"
[proxmox]
nodes = []
user = "root@pam"
token_id = "commando"
token_secret = "xxxx"

[agent]

[agent.psk]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.cache_dir, "/var/lib/commando");
    }
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p commando-gateway`
Expected: All tests pass

- [ ] **Step 8: Commit**

```bash
git add crates/commando-gateway/src/config.rs crates/commando-gateway/src/main.rs crates/commando-gateway/src/handler.rs
git commit -m "feat: add --cache-dir config option for registry cache path"
```

---

### Task 8: Show stopped LXCs

**Files:**
- Modify: `crates/commando-gateway/src/proxmox.rs`
- Modify: `crates/commando-gateway/src/handler.rs`

- [ ] **Step 1: Write test for stopped LXC visibility in registry**

Add to the `#[cfg(test)]` module in `crates/commando-gateway/src/registry.rs`:

```rust
    #[test]
    fn stopped_target_visible_in_list() {
        let mut registry = Registry::new();
        registry.update_discovered(vec![
            DiscoveredTarget {
                name: "node-1/running-app".to_string(),
                host: "10.0.0.1".to_string(),
                port: 9876,
                status: "running".to_string(),
            },
            DiscoveredTarget {
                name: "node-1/stopped-app".to_string(),
                host: "".to_string(),
                port: 9876,
                status: "stopped".to_string(),
            },
        ]);
        let all = registry.list(None);
        assert_eq!(all.len(), 2);
        let stopped = all.iter().find(|t| t.name == "node-1/stopped-app").unwrap();
        assert!(stopped.host.is_empty());
        assert_eq!(stopped.status, "stopped");
    }
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p commando-gateway stopped_target_visible`
Expected: PASS (the registry already handles empty hosts — this verifies they're visible in list)

- [ ] **Step 3: Modify discover_node to include stopped LXCs**

In `crates/commando-gateway/src/proxmox.rs`, replace the loop body (lines 63-99) with:

```rust
    for lxc in &lxc_list.data {
        if lxc.status != "running" {
            // Stopped/paused LXCs have no guest agent — skip the interface lookup
            targets.push(DiscoveredTarget {
                name: format!("{}/{}", node.name, lxc.name),
                host: "".to_string(),
                port: default_port,
                status: lxc.status.clone(),
            });
            continue;
        }

        // Get interfaces for IP discovery (running LXCs only)
        let iface_url = format!(
            "{}/nodes/{}/lxc/{}/interfaces",
            base_url, node.name, lxc.vmid
        );
        let ip = match client
            .get(&iface_url)
            .header("Authorization", &auth_header)
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(iface_resp) = resp.json::<ProxmoxResponse<Vec<InterfaceEntry>>>().await {
                    extract_ip(&iface_resp.data)
                } else {
                    None
                }
            }
            Err(_) => None,
        };

        if let Some(host) = ip {
            targets.push(DiscoveredTarget {
                name: format!("{}/{}", node.name, lxc.name),
                host,
                port: default_port,
                status: lxc.status.clone(),
            });
        } else {
            tracing::warn!(
                node = %node.name,
                vmid = lxc.vmid,
                name = %lxc.name,
                "could not determine IP for LXC, skipping"
            );
        }
    }
```

- [ ] **Step 4: Write test for empty-host guard in handler.rs**

Add to the `#[cfg(test)]` module in `crates/commando-gateway/src/handler.rs`:

```rust
    #[tokio::test]
    async fn exec_stopped_target_returns_clear_error() {
        let config = test_config_with_target();
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        // Create a registry with a stopped target (empty host)
        let mut registry = Registry::new();
        registry.update_discovered(vec![crate::registry::DiscoveredTarget {
            name: "node-1/stopped-app".to_string(),
            host: "".to_string(),
            port: 9876,
            status: "stopped".to_string(),
        }]);
        let registry = Arc::new(Mutex::new(registry));

        // Add PSK for the stopped target
        let mut psk = std::collections::HashMap::new();
        psk.insert("node-1/stopped-app".to_string(), "secret123".to_string());
        let config = Arc::new(GatewayConfig {
            agent: crate::config::AgentConnectionConfig {
                psk,
                ..config.agent.clone()
            },
            ..(*config).clone()
        });

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_exec",
                "arguments": { "target": "node-1/stopped-app", "command": "echo hi" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("stopped"), "error should mention target status, got: {text}");
    }
```

- [ ] **Step 5: Write test for ping handler empty-host guard**

Add to the `#[cfg(test)]` module in `crates/commando-gateway/src/handler.rs`:

```rust
    #[tokio::test]
    async fn ping_stopped_target_returns_clear_error() {
        let limiter = Arc::new(ConcurrencyLimiter::new(4));

        let mut registry = Registry::new();
        registry.update_discovered(vec![crate::registry::DiscoveredTarget {
            name: "node-1/stopped-app".to_string(),
            host: "".to_string(),
            port: 9876,
            status: "stopped".to_string(),
        }]);
        let registry = Arc::new(Mutex::new(registry));

        let mut psk = std::collections::HashMap::new();
        psk.insert("node-1/stopped-app".to_string(), "secret123".to_string());
        let config = Arc::new(GatewayConfig {
            server: Default::default(),
            proxmox: None,
            agent: crate::config::AgentConnectionConfig {
                default_port: 9876,
                default_timeout_secs: 60,
                connect_timeout_secs: 5,
                max_concurrent_per_target: 4,
                psk,
            },
            targets: vec![],
            cache_dir: "/tmp/commando-test".to_string(),
        });

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "commando_ping",
                "arguments": { "target": "node-1/stopped-app" }
            }
        });
        let resp = dispatch_request(&request, &config, &registry, &limiter).await.unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap_or(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("stopped"), "error should mention target status, got: {text}");
    }
```

- [ ] **Step 6: Run tests to verify they fail**

Run: `cargo test -p commando-gateway stopped_target_returns_clear_error`
Expected: FAIL — the handlers do not check for empty host yet

- [ ] **Step 7: Add empty-host guard in handle_exec and handle_ping**

In `crates/commando-gateway/src/handler.rs`, in the `handle_exec` function, after retrieving `(host, port)` from the registry (after line 241), add:

```rust
    if host.is_empty() {
        let status = {
            let reg = registry.lock().unwrap();
            reg.get(target_name).map(|t| t.status.clone()).unwrap_or_default()
        };
        return make_tool_error(id, &format!("target '{}' is {} (no IP available)", target_name, status));
    }
```

Also add the same guard in `handle_ping`, after retrieving `(host, port)` (after line 357):

```rust
    if host.is_empty() {
        let status = {
            let reg = registry.lock().unwrap();
            reg.get(target_name).map(|t| t.status.clone()).unwrap_or_default()
        };
        return make_tool_error(id, &format!("target '{}' is {} (no IP available)", target_name, status));
    }
```

- [ ] **Step 8: Run tests**

Run: `cargo test -p commando-gateway`
Expected: All tests pass

- [ ] **Step 9: Commit**

```bash
git add crates/commando-gateway/src/proxmox.rs crates/commando-gateway/src/handler.rs crates/commando-gateway/src/registry.rs
git commit -m "feat: show stopped LXCs in target list with clear error on exec"
```

---

## Chunk 4: Make Proxmox Config Optional

### Task 9: Make proxmox config optional

**Files:**
- Modify: `crates/commando-gateway/src/config.rs`
- Modify: `crates/commando-gateway/src/main.rs`
- Modify: `crates/commando-gateway/src/handler.rs` (test helpers)

- [ ] **Step 1: Write test for config without proxmox section**

In `crates/commando-gateway/src/config.rs`, add a test:

```rust
    #[test]
    fn parse_config_without_proxmox() {
        let toml_str = r#"
[agent]
default_port = 9876

[agent.psk]
my-target = "secret"

[[targets]]
name = "my-target"
host = "192.168.1.50"
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert!(config.proxmox.is_none());
        assert_eq!(config.targets.len(), 1);
        assert_eq!(config.agent.psk["my-target"], "secret");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p commando-gateway parse_config_without_proxmox`
Expected: FAIL — `proxmox` is currently required

- [ ] **Step 3: Make proxmox optional in GatewayConfig**

In `crates/commando-gateway/src/config.rs`, change the `proxmox` field in `GatewayConfig`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub proxmox: Option<ProxmoxConfig>,
    pub agent: AgentConnectionConfig,
    #[serde(default)]
    pub targets: Vec<ManualTarget>,
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
}
```

- [ ] **Step 4: Update main.rs access points**

In `crates/commando-gateway/src/main.rs`, update the 3 places that access `config.proxmox`:

**Startup log (line 63):** Change:
```rust
        proxmox_nodes = config.proxmox.nodes.len(),
```
to:
```rust
        proxmox_nodes = config.proxmox.as_ref().map(|p| p.nodes.len()).unwrap_or(0),
```

**Cache loading gate (around line 118):** Change:
```rust
    } else if !config.proxmox.nodes.is_empty() {
```
to:
```rust
    } else if config.proxmox.as_ref().is_some_and(|p| !p.nodes.is_empty()) {
```

**Discovery loop (around line 128):** Change:
```rust
    if !config.proxmox.nodes.is_empty() {
```
to:
```rust
    if let Some(proxmox) = &config.proxmox {
        if !proxmox.nodes.is_empty() {
```

Inside the block, change `config_clone.proxmox.discovery_interval_secs` (around line 133) to `proxmox_clone.discovery_interval_secs`. Specifically, clone `proxmox` for the move into the async block:

```rust
    if let Some(proxmox) = &config.proxmox {
        if !proxmox.nodes.is_empty() {
            let config_clone = config.clone();
            let registry_clone = registry.clone();
            let discovery_interval = proxmox.discovery_interval_secs;
            tokio::task::spawn_local(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                    discovery_interval,
                ));
                interval.tick().await;
                loop {
                    interval.tick().await;
                    run_discovery_cycle(&config_clone, &registry_clone).await;
                }
            });
        }
    }
```

The `run_discovery_cycle` call stays the same since it takes `&config` and accesses `config.proxmox` internally (guarded by the early return added in the `run_discovery_cycle` change).

**run_discovery_cycle (around line 163):** Change:
```rust
    for node in &config.proxmox.nodes {
        match proxmox::discover_node(&http_client, node, &config.proxmox, config.agent.default_port).await {
```
to:
```rust
    let proxmox = match &config.proxmox {
        Some(p) => p,
        None => return,
    };
    for node in &proxmox.nodes {
        match proxmox::discover_node(&http_client, node, proxmox, config.agent.default_port).await {
```

- [ ] **Step 5: Update handler.rs test helpers**

In `crates/commando-gateway/src/handler.rs`, update both `test_config()` and `test_config_with_target()`:

Change:
```rust
            proxmox: crate::config::ProxmoxConfig {
                nodes: vec![],
                user: String::new(),
                token_id: String::new(),
                token_secret: String::new(),
                discovery_interval_secs: 60,
            },
```

to:
```rust
            proxmox: None,
```

- [ ] **Step 6: Update existing config tests**

In `crates/commando-gateway/src/config.rs`, update the existing tests. The tests that include `[proxmox]` sections should now assert `config.proxmox.is_some()` and use `.unwrap()` to access fields. For example, in `parse_full_config`:

Change:
```rust
        assert_eq!(config.proxmox.nodes.len(), 2);
        assert_eq!(config.proxmox.nodes[0].name, "node-1");
```
to:
```rust
        let proxmox = config.proxmox.unwrap();
        assert_eq!(proxmox.nodes.len(), 2);
        assert_eq!(proxmox.nodes[0].name, "node-1");
```

Apply similar changes to `parse_config_with_server_section`, `server_section_defaults`, and `parse_minimal_config`.

- [ ] **Step 7: Run tests**

Run: `cargo test -p commando-gateway`
Expected: All tests pass

- [ ] **Step 8: Update gateway.toml.example**

In `config/gateway.toml.example`, add a comment showing proxmox is optional and add a manual-only example:

```toml
# Commando Gateway Configuration
# Copy to /etc/commando/gateway.toml and set chmod 600 root:root

# MCP server transport: "streamable-http" (HTTP, default) or "stdio" (stdin/stdout)
# [server]
# transport = "streamable-http"
# bind = "0.0.0.0"
# port = 9877

# Optional: Proxmox auto-discovery (omit this section for manual-only setup)
# [proxmox]
# nodes = [
#     { name = "node-1", host = "192.168.1.10", port = 8006 },
# ]
# user = "root@pam"
# token_id = "commando"
# token_secret = "REPLACE_WITH_PROXMOX_API_TOKEN"
# discovery_interval_secs = 60

[agent]
default_port = 9876
default_timeout_secs = 60
connect_timeout_secs = 5
max_concurrent_per_target = 4

[agent.psk]
# my-target = "output-of-openssl-rand-hex-32"

[[targets]]
name = "my-target"
host = "192.168.1.50"
port = 9876
shell = "sh"
tags = ["web"]
```

- [ ] **Step 9: Commit**

```bash
git add crates/commando-gateway/src/config.rs crates/commando-gateway/src/main.rs crates/commando-gateway/src/handler.rs config/gateway.toml.example
git commit -m "feat: make proxmox config optional for manual-only setups"
```

---

## Chunk 5: Documentation Updates

### Task 10: Fix SSE references and update README

**Files:**
- Modify: `README.md`
- Modify: `config/gateway.toml.example` (if not already done in Task 9)

- [ ] **Step 1: Replace all SSE references in README.md**

Apply these replacements throughout `README.md`:

1. Line 27: `"persistent SSE server"` → `"persistent HTTP server"`
2. Line 28: `"The gateway runs as a persistent SSE server, so Claude Code maintains a long-lived HTTP connection."` → `"The gateway runs as a persistent HTTP server. Commands execute without SSH handshake overhead."`
3. Line 31: `"Commando (SSE)"` → `"Commando"`
4. Line 33: `"HTTP POST on persistent connection"` stays (already accurate)
5. Line 41: `"HTTP/SSE (MCP JSON-RPC)"` → `"HTTP (MCP JSON-RPC)"`
6. Line 49: `"SSE Server"` → `"HTTP Server"`
7. Line 50: `"(axum)    │──│"` stays
8. Line 73: `"HTTP/SSE"` → `"HTTP"`
9. Lines 81-86: Replace the transport table:

```markdown
| Transport | Use Case | Config |
|-----------|----------|--------|
| **Streamable HTTP** (default) | Persistent remote service | `{"type": "http", "url": "http://host:9877/mcp"}` |
| **stdio** | Local development/testing | `{"type": "stdio", "command": "commando-gateway", ...}` |

Streamable HTTP is the primary transport. The gateway runs as a persistent service and Claude Code connects over HTTP — no SSH tunnel, no per-session container spawning.
```

10. Line 129: `transport = "sse"` → `transport = "streamable-http"`
11. Lines 267-268: Claude Code config:

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

12. Line 336: `"SSE (HTTP) / stdio"` → `"Streamable HTTP / stdio"`

- [ ] **Step 2: Enhance "Building from Source" section**

Replace the current "Building from Source" section (lines 315-319) with:

```markdown
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
```

- [ ] **Step 3: Update README Step 1 to reference docker-compose.yml**

In the "Step 1: Deploy the Gateway" section (around lines 151-167), replace the inline `docker-compose.yml` creation with a reference to the repo file:

```markdown
# Copy docker-compose.yml from the repo
curl -fSL -o ~/docker-app/docker-compose.yml \
  https://raw.githubusercontent.com/icyrainz/commando/main/docker-compose.yml

# Start the gateway
cd ~/docker-app && docker compose up -d
```

- [ ] **Step 4: Update deploy-gateway.sh usage examples in README**

Lines 293-294 of README.md show the old usage without `<node>` and `<vmid>` args. Update to:

```markdown
./deploy/deploy-gateway.sh pve-node-1 100          # pull latest, restart
./deploy/deploy-gateway.sh pve-node-1 100 v0.2.0   # deploy specific version
```

- [ ] **Step 5: Update inline systemd service in README**

Lines 214-227 of README.md inline the systemd service file. Add the hardening directives to match `deploy/commando-agent.service`:

```ini
NoNewPrivileges=yes
ProtectSystem=strict
PrivateTmp=yes
```

- [ ] **Step 6: Add manual-only setup documentation**

In the "Getting Started" section, after the prerequisites, add a note:

```markdown
> **Not using Proxmox?** Skip the `[proxmox]` section in `gateway.toml` entirely. Add targets manually with `[[targets]]` entries and their PSKs under `[agent.psk]`. See Step 3 below.
```

- [ ] **Step 7: Run a link/syntax check**

Run: `grep -n "sse\|SSE" README.md` to verify no stale references remain.
Expected: No matches (or only false positives like substrings in URLs)

- [ ] **Step 8: Commit**

```bash
git add README.md
git commit -m "docs: update README for streamable HTTP, add build deps table, document manual-only setup"
```

---

## Final Verification

- [ ] **Run full test suite**

Run: `cargo test`
Expected: All tests pass

- [ ] **Build check**

Run: `cargo build`
Expected: Compiles without warnings
