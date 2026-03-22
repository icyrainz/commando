# Commando — Feature Suggestions

Curated list of next improvements, prioritized by impact. Sourced from real fleet management sessions.

Last updated: 2026-03-22

## 1. Fleet Exec (parallel fan-out)

**Status:** Not started — overlaps with "Batch exec" in design.md Future Enhancements

Run a command across multiple targets in parallel, filtered by name, tag, or node.

```bash
commando fleet exec --tag media 'systemctl status alloy'
commando fleet exec --node akio-lab 'apt-get update'
commando fleet exec --all 'cat /etc/commando/agent.toml'
```

**Why:** Installing/updating across 30+ LXCs one-by-one is painful. A fleet-wide service file fix today took ~90s sequentially; parallel would be ~3s. This is the most common pain point in day-to-day homelab ops.

**Scope:** Gateway-level fan-out with per-target result aggregation. Builds on existing registry (already has tags), concurrency control, and streaming. CLI: `commando fleet exec`. MCP: `commando_exec_batch`.

**Complexity:** Medium — mostly new handler + CLI subcommand. No schema changes needed.

## 2. Elevate (route through Proxmox host)

**Status:** Not started

Route commands through `pct exec` on the parent Proxmox node, bypassing agent-level restrictions (e.g., when the agent is down or the LXC doesn't have one).

```bash
commando exec --elevate akio-lab/akio-ntfy 'apt-get install -y alloy'
```

**Why:** Some operations need to run outside the agent's process tree (the `ProtectSystem` issue that prompted this doc is now fixed, but other cases remain — e.g., installing the agent itself, or recovering a broken agent). Currently requires manual SSH + pct exec.

**Scope:** Gateway needs to:
1. Resolve LXC → parent Proxmox node + VMID (already available from discovery)
2. Route the command to the node's agent as `pct exec <vmid> -- sh -c '<command>'`
3. Requires agents running on Proxmox hosts themselves (already deployed)

**Complexity:** Medium — gateway routing logic + CLI flag. No schema changes.

## 3. Target Add / Remove (runtime config management)

**Status:** Not started

CLI commands to add/remove targets and PSKs without manually editing `gateway.toml`.

```bash
commando target add my-server --host 192.168.0.50 --tags monitoring
commando target remove my-server
```

**Why:** Manually editing TOML with sed broke a target (paperless) and crashed the gateway during a fleet rollout. A CLI command with validation would prevent this.

**Scope:**
- REST API endpoints for target CRUD
- Separate mutable state file (don't rewrite `gateway.toml` — preserves hand-edited comments)
- Hot-reload registry without gateway restart
- Auto-generate PSK on `target add`

**Complexity:** Medium-high — needs mutable config layer, hot-reload, and API endpoints.

## 4. File Push

**Status:** Not started — overlaps with "File transfer" in design.md Future Enhancements

Native file transfer without shell command workarounds.

```bash
commando push akio-lab/akio-obs ./config.alloy /etc/alloy/config.alloy
commando pull akio-lab/akio-obs /var/log/syslog ./syslog.txt
```

**Why:** Pushing files through commando currently requires awkward `tee` + heredoc quoting. A native transfer handles binary encoding, large files, and permissions correctly.

**Scope:** New Cap'n Proto RPC methods (`pushFile`, `pullFile`) + CLI subcommands + MCP tools (`commando_read_file`, `commando_write_file`).

**Complexity:** High — schema changes, streaming binary data, new RPC methods.

## 5. Provision (nice-to-have)

**Status:** Not started

Automate the full LXC lifecycle: clone template → resize → configure → start → install agent → register in gateway.

```bash
commando provision akio-lab --vmid 136 --hostname akio-obs --memory 2048 --cores 2 --disk 16G --tags monitoring
```

**Why:** Creating a new LXC is ~10 manual steps. But this crosses into config management territory (explicitly a non-goal in design.md).

**Scope:** Would use Proxmox API for LXC creation + existing `install-agent.sh` logic. Better kept as a shell script (`deploy/provision.sh`) rather than built into commando proper.

**Complexity:** Medium — but scope creep risk is high. Keep as external script.

---

## Cross-reference

These suggestions complement the existing roadmap in `docs/design.md` § "Future Enhancements":
- **Fleet exec** = "Batch exec (`commando_exec_batch`)" — same feature, this doc adds CLI UX details
- **File push** = "File transfer" — same feature
- **Elevate** and **Target management** are new additions not in the original design
- **Provision** is explicitly out of scope per design.md non-goals, listed here only as a scripting opportunity
