# Public Release TODO

Notes from analysis of what needs to change before sharing on r/selfhosted / r/homelab.

## Security (Red Flags)

### Gateway has zero auth
The `/mcp` HTTP endpoint accepts any request with no token, API key, or IP allowlist. Anyone who can reach port 9877 gets arbitrary command execution on all agents as root. At minimum, add a bearer token / API key check.

### No TLS between gateway and agents
Commands and stdout/stderr are plaintext on the wire. HMAC protects the PSK during auth, but command output (which may contain secrets) is visible to anyone sniffing the LAN. Consider optional TLS or document the threat model clearly.

### Agents always run as root, no systemd hardening
`deploy/commando-agent.service` has no `User=`, `CapabilityBoundingSet=`, `NoNewPrivileges=`, or any hardening directives. Add systemd sandboxing options and consider supporting non-root operation for use cases that don't need it.

## Adoption Friction

### Hardcoded personal values in deploy scripts and examples
- `deploy/deploy-gateway.sh` hardcodes `NODE="akio-lab"`, `VMID=134`
- `config/agent.toml.example` hardcodes `bind = "10.0.0.5"`
- Replace with parameterized values or clear placeholders

### No docker-compose.yml in the repo
`deploy/deploy-gateway.sh` references `docker compose up -d` but no compose file exists in the repo. Add a working `docker-compose.yml` example.

### Dockerfile requires pre-built binary
`Dockerfile.gateway` is `FROM scratch` + `COPY target/x86_64.../commando-gateway`. `docker build` fails without a local Rust toolchain + musl target. Add a multi-stage Dockerfile that builds inside the container.

### x86_64 only, no ARM builds
`.github/workflows/release.yml` only builds `x86_64-unknown-linux-musl`. Many homelabs run Raspberry Pi or ARM NAS devices. Add `aarch64-unknown-linux-musl` to the release matrix.

### Cap'n Proto build dependency not obvious
`commando-common/build.rs` requires system `capnproto` package. First-time `cargo build` gives a cryptic error. Document this prominently or vendor the generated code.

### Proxmox-only auto-discovery
The killer feature (auto-discovering LXCs) only works with Proxmox. Non-Proxmox users fall back to manual `[[targets]]` config. Consider documenting manual-only setup as a first-class path, or adding other discovery backends later.

## Quality of Life

### Discovered targets hardcode shell to "bash"
`registry.rs` `update_discovered()` sets `shell: "bash"` for every auto-discovered LXC. Alpine containers with only `sh` or machines running fish will fail. Add shell auto-detection (check `/etc/passwd` or probe the target) or at least default to `sh`.

### Stopped LXCs vanish from commando_list
Stopped LXCs are silently skipped during discovery and don't appear in the target list at all. They should show up with a "stopped" or "unreachable" status so operators know the target exists.

### PSK management is manual and brittle
PSKs are generated in `deploy/deploy-agents.sh` and printed to stdout for copy-paste into `gateway.toml`. No tooling to verify the PSK table matches deployed agents, rotate keys, or diagnose mismatches. Consider a `commando-gateway verify-psks` subcommand.

### Output truncation is silent
Agent truncates stdout to 128KB (tail). If a command produces large JSON output, the result is silently a malformed fragment. The `truncated` flag is set but there's no way to recover the dropped data. Consider logging a warning or providing a way to stream large output.

### Registry cache path hardcoded
`/var/lib/commando/registry.json` is hardcoded with no `--cache-dir` flag and no fallback to `~/.local/share/commando/` for non-root deployments.

## What's Already Good (keep these)

- HMAC auth with constant-time comparison in `commando-common/src/auth.rs`
- Process management in `process.rs` (setsid, SIGTERM + grace + SIGKILL, partial output on timeout)
- Static musl binaries + `FROM scratch` Docker image
- `docs/design.md` is thorough and well-written
- Manual-override-wins-over-discovered registry logic
- Per-IP rate limiting on agent auth failures
- Integration tests covering the full auth+exec flow
