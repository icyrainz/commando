#!/usr/bin/env bash
# Update commando-agent binary + service file on all running Proxmox LXCs.
# Downloads from GitHub releases, pushes to every LXC that already has an agent,
# and restarts the service. Does NOT touch config or PSKs.
#
# The binary is checked against the release SHA256SUMS. That detects corrupted
# downloads. It does not prove authenticity: both files come from the same release.
#
# A failure on one LXC does not stop the run. The agent on that LXC is started
# again, the LXC is listed as FAILED, and the script exits non-zero at the end.
#
# Prerequisites:
#   - SSH root access to the specified Proxmox nodes
#   - deploy/commando-agent.service must exist alongside this script
#   - Agents must already be installed (skips LXCs without /usr/local/bin/commando-agent)
#
# Usage: ./deploy/update-agents.sh <proxmox-node> [proxmox-node-2] ...
#
# Environment:
#   COMMANDO_VERSION  - GitHub release tag (default: "latest")
#   COMMANDO_VMIDS    - Space-separated VMIDs; when set, only these are updated
#
# Examples:
#   ./deploy/update-agents.sh akio-lab akio-garage
#   COMMANDO_VERSION=v0.3.2 ./deploy/update-agents.sh akio-lab
#   COMMANDO_VMIDS="101 102" ./deploy/update-agents.sh akio-lab
#
# For non-Proxmox hosts, use install-agent.sh instead (SSH in and curl-pipe-bash).

# Remote commands expand variables on the client side on purpose.
# shellcheck disable=SC2029
set -euo pipefail

REPO="icyrainz/commando"
VERSION="${COMMANDO_VERSION:-latest}"
VMID_FILTER="${COMMANDO_VMIDS:-}"
BINARY_NAME="commando-agent-x86_64-linux"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SERVICE_FILE="$SCRIPT_DIR/commando-agent.service"
REMOTE_BINARY="/tmp/commando-agent"
REMOTE_SERVICE="/tmp/commando-agent.service"

if [ $# -eq 0 ]; then
    echo "Usage: $0 <proxmox-node-1> [proxmox-node-2] ..."
    exit 1
fi

if [ ! -f "$SERVICE_FILE" ]; then
    echo "Error: Service file not found at $SERVICE_FILE"
    exit 1
fi

# Resolve download URL
if [ "$VERSION" = "latest" ]; then
    BASE_URL="https://github.com/$REPO/releases/latest/download"
else
    BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
fi
URL="$BASE_URL/$BINARY_NAME"

TMPBIN=$(mktemp)
TMPSUMS=$(mktemp)

# LXC whose agent is stopped and not yet confirmed started
PENDING_NODE=""
PENDING_VMID=""

cleanup() {
    trap '' INT TERM
    if [ -n "$PENDING_NODE" ]; then
        echo "Interrupted: starting agent on $PENDING_NODE vmid $PENDING_VMID"
        ssh -o ConnectTimeout=10 "root@$PENDING_NODE" "pct exec $PENDING_VMID -- systemctl start commando-agent" || true
    fi
    rm -f "$TMPBIN" "$TMPSUMS"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# Verify $1 against the release SHA256SUMS entry for $2. Warn if the release has none.
verify_checksum() {
    local file="$1" name="$2" code expected actual
    code=$(curl -sSL -o "$TMPSUMS" -w '%{http_code}' "$BASE_URL/SHA256SUMS") || {
        echo "Error: could not download SHA256SUMS"
        return 1
    }
    case "$code" in
        200) ;;
        404)
            echo "Warning: release has no SHA256SUMS, skipping checksum verification"
            return 0
            ;;
        *)
            echo "Error: SHA256SUMS download returned HTTP $code"
            return 1
            ;;
    esac
    expected=$(awk -v n="$name" '$2 == n || $2 == "*" n {print $1; exit}' "$TMPSUMS")
    if [ -z "$expected" ]; then
        echo "Error: SHA256SUMS has no entry for $name"
        return 1
    fi
    actual=$(sha256_of "$file")
    if [ "$expected" != "$actual" ]; then
        echo "Error: checksum mismatch for $name"
        echo "  expected $expected"
        echo "  actual   $actual"
        return 1
    fi
    echo "Checksum OK ($name)"
}

# Wait briefly, then check the agent is running. Type=simple units report
# "started" even when the binary exits at once.
agent_active() {
    sleep 2
    ssh "root@$1" "pct exec $2 -- systemctl is-active --quiet commando-agent"
}

# Update one LXC. Sets LXC_NAME. Returns 0 = updated, 2 = skipped, 1 = failed.
update_lxc() {
    local node="$1" vmid="$2" name status probe_rc=0
    LXC_NAME="vmid-$vmid"

    name=$(ssh "root@$node" "pct config $vmid" | awk '/^hostname/ {print $2}') || {
        echo "  [$LXC_NAME] FAILED (hostname lookup)"
        return 1
    }
    [ -n "$name" ] && LXC_NAME="$name"
    status=$(ssh "root@$node" "pct status $vmid" | awk '{print $2}') || {
        echo "  [$LXC_NAME] FAILED (status lookup)"
        return 1
    }

    if [ "$status" != "running" ]; then
        echo "  [$LXC_NAME] SKIP (stopped)"
        return 2
    fi

    # Only update LXCs that already have the agent installed: 0 = yes, 1 = no, else error
    ssh "root@$node" "pct exec $vmid -- test -f /usr/local/bin/commando-agent" || probe_rc=$?
    case "$probe_rc" in
        0) ;;
        1)
            echo "  [$LXC_NAME] SKIP (no agent installed)"
            return 2
            ;;
        *)
            echo "  [$LXC_NAME] FAILED (agent probe, exit $probe_rc)"
            return 1
            ;;
    esac

    echo "  [$LXC_NAME] Updating..."
    PENDING_NODE="$node"
    PENDING_VMID="$vmid"
    if ssh "root@$node" "pct exec $vmid -- systemctl stop commando-agent" \
        && ssh "root@$node" "pct push $vmid $REMOTE_BINARY /usr/local/bin/commando-agent --perms 755" \
        && ssh "root@$node" "pct push $vmid $REMOTE_SERVICE /etc/systemd/system/commando-agent.service" \
        && ssh "root@$node" "pct exec $vmid -- systemctl daemon-reload" \
        && ssh "root@$node" "pct exec $vmid -- systemctl start commando-agent" \
        && agent_active "$node" "$vmid"; then
        PENDING_NODE=""
        PENDING_VMID=""
        echo "  [$LXC_NAME] OK"
        return 0
    fi

    echo "  [$LXC_NAME] FAILED (update step), trying to start the agent again"
    if ssh "root@$node" "pct exec $vmid -- systemctl start commando-agent" \
        && agent_active "$node" "$vmid"; then
        echo "  [$LXC_NAME] agent is running again"
    else
        echo "  [$LXC_NAME] WARNING: agent could not be started, it may be down"
    fi
    PENDING_NODE=""
    PENDING_VMID=""
    return 1
}

# Download binary once
echo "Downloading $URL ..."
curl -fSL -o "$TMPBIN" "$URL"
chmod +x "$TMPBIN"
echo "Downloaded $(wc -c < "$TMPBIN" | tr -d ' ') bytes"
verify_checksum "$TMPBIN" "$BINARY_NAME"

UPDATED=""
SKIPPED=""
FAILED=""
MATCHED=" "

for NODE in "$@"; do
    echo "=== Updating agents on $NODE ==="

    if ! scp "$TMPBIN" "root@$NODE:$REMOTE_BINARY" \
        || ! scp "$SERVICE_FILE" "root@$NODE:$REMOTE_SERVICE"; then
        echo "  FAILED to copy files to $NODE"
        FAILED="$FAILED $NODE(scp)"
        continue
    fi

    if ! VMIDS=$(ssh "root@$NODE" "pct list" | tail -n +2 | awk '{print $1}'); then
        echo "  FAILED to list LXCs on $NODE"
        FAILED="$FAILED $NODE(pct-list)"
        continue
    fi

    for VMID in $VMIDS; do
        if [ -n "$VMID_FILTER" ]; then
            case " $VMID_FILTER " in
                *" $VMID "*) MATCHED="$MATCHED$VMID " ;;
                *) continue ;;
            esac
        fi

        rc=0
        update_lxc "$NODE" "$VMID" || rc=$?
        case "$rc" in
            0) UPDATED="$UPDATED $NODE/$LXC_NAME" ;;
            2) SKIPPED="$SKIPPED $NODE/$LXC_NAME" ;;
            *) FAILED="$FAILED $NODE/$LXC_NAME" ;;
        esac
    done

    ssh "root@$NODE" "rm -f $REMOTE_BINARY $REMOTE_SERVICE" || true
done

for REQ in $VMID_FILTER; do
    case "$MATCHED" in
        *" $REQ "*) ;;
        *)
            echo "WARNING: requested VMID $REQ matched no LXC on any node"
            FAILED="$FAILED vmid-$REQ(not-found)"
            ;;
    esac
done

echo ""
echo "=== Summary ==="
echo "Updated:${UPDATED:- none}"
echo "Skipped:${SKIPPED:- none}"
echo "Failed: ${FAILED:- none}"

if [ -n "$FAILED" ]; then
    exit 1
fi
echo "Done."
