# Public Release TODO

Notes from analysis of what needs to change before sharing on r/selfhosted / r/homelab.

Last verified: 2026-03-12

## Still Open

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
- ~~No TLS between gateway and agents~~ — Documented threat model in README: trusted LAN only, recommends reverse proxy (Caddy example) for HTTPS and Tailscale/WireGuard for full encryption
- ~~PSK management is manual and brittle~~ — Documented in README: set-and-forget, rotate via re-running install script, mismatches show clear auth errors in logs
- ~~Dockerfile requires pre-built binary~~ — By design: `FROM scratch` keeps image at ~3MB, CI release workflow handles builds with caching and cross-compilation, users pull from `ghcr.io`

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
