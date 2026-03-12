# Public Release TODO

Notes from analysis of what needs to change before sharing on r/selfhosted / r/homelab.

Last verified: 2026-03-12

## Still Open

### No TLS between gateway and agents
Commands and stdout/stderr are plaintext on the wire. HMAC protects the PSK during auth, but command output (which may contain secrets) is visible to anyone sniffing the LAN. Consider optional TLS or document the threat model clearly.

### PSK management is manual and brittle
PSKs are generated in deploy scripts and printed to stdout for copy-paste into `gateway.toml`. No tooling to verify the PSK table matches deployed agents, rotate keys, or diagnose mismatches. Consider a `commando-gateway verify-psks` subcommand.

### Dockerfile requires pre-built binary
`Dockerfile.gateway` is `FROM scratch` + `COPY` pre-built binaries. `docker build` fails without pre-built artifacts. The release workflow handles this, but local `docker build` won't work. Consider a multi-stage Dockerfile that builds from source.

## Fixed

- ~~Agents always run as root, no systemd hardening~~ — `NoNewPrivileges=yes`, `ProtectSystem=true`, `PrivateTmp=yes` added to service file
- ~~Hardcoded personal values in deploy scripts~~ — `deploy-gateway.sh` parameterized, `agent.toml.example` uses `0.0.0.0`
- ~~No docker-compose.yml in the repo~~ — added at repo root
- ~~x86_64 only, no ARM builds~~ — aarch64 added to release workflow via zigbuild
- ~~Cap'n Proto build dependency not obvious~~ — documented in README with per-distro install commands
- ~~Proxmox-only auto-discovery~~ — README presents manual targets as first-class, Proxmox as optional
- ~~Discovered targets hardcode shell to "bash"~~ — defaults to `"sh"` now
- ~~Stopped LXCs vanish from commando_list~~ — stopped LXCs show in list with status, exec returns clear error
- ~~Output truncation is silent~~ — `[output truncated]` message appended when truncation occurs
- ~~Registry cache path hardcoded~~ — `--cache-dir` CLI flag added, configurable in gateway.toml
- ~~Gateway has zero auth~~ — Bearer token auth on `/mcp` endpoint via `COMMANDO_API_KEY` env var, constant-time comparison, `/health` stays open

## What's Already Good (keep these)

- HMAC auth with constant-time comparison in `commando-common/src/auth.rs`
- Process management in `process.rs` (setsid, SIGTERM + grace + SIGKILL, partial output on timeout)
- Static musl binaries + `FROM scratch` Docker image
- `docs/design.md` is thorough and well-written
- Manual-override-wins-over-discovered registry logic
- Per-IP rate limiting on agent auth failures
- Integration tests covering the full auth+exec flow
- Universal `install-agent.sh` (curl | bash) for any Linux machine
- AI agent efficiency documented (zero escaping, token savings)
- Comprehensive CLAUDE.md with deployment operations guide
