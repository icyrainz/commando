#!/usr/bin/env bash
# Update commando-agent binary + service file on all running Proxmox LXCs.
# Downloads from GitHub releases, pushes to every LXC that already has an agent,
# and restarts the service. Does NOT touch config or PSKs.
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
#
# Examples:
#   ./deploy/update-agents.sh akio-lab akio-garage
#   COMMANDO_VERSION=v0.3.2 ./deploy/update-agents.sh akio-lab
#
# For non-Proxmox hosts, use install-agent.sh instead (SSH in and curl-pipe-bash).

set -euo pipefail

REPO="icyrainz/commando"
VERSION="${COMMANDO_VERSION:-latest}"
BINARY_NAME="commando-agent-x86_64-linux"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SERVICE_FILE="$SCRIPT_DIR/commando-agent.service"

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
    URL="https://github.com/$REPO/releases/latest/download/$BINARY_NAME"
else
    URL="https://github.com/$REPO/releases/download/$VERSION/$BINARY_NAME"
fi

# Download binary once
TMPBIN=$(mktemp)
echo "Downloading $URL ..."
curl -fSL -o "$TMPBIN" "$URL"
chmod +x "$TMPBIN"
echo "Downloaded $(wc -c < "$TMPBIN") bytes"

for NODE in "$@"; do
    echo "=== Updating agents on $NODE ==="

    REMOTE_BINARY="/tmp/commando-agent"
    REMOTE_SERVICE="/tmp/commando-agent.service"
    scp "$TMPBIN" "root@$NODE:$REMOTE_BINARY"
    scp "$SERVICE_FILE" "root@$NODE:$REMOTE_SERVICE"

    VMIDS=$(ssh "root@$NODE" "pct list" | tail -n +2 | awk '{print $1}')

    for VMID in $VMIDS; do
        HOSTNAME=$(ssh "root@$NODE" "pct config $VMID" | grep ^hostname | awk '{print $2}')
        STATUS=$(ssh "root@$NODE" "pct status $VMID" | awk '{print $2}')

        if [ "$STATUS" != "running" ]; then
            echo "  [$HOSTNAME] SKIP (stopped)"
            continue
        fi

        # Only update LXCs that already have the agent installed
        HAS_AGENT=$(ssh "root@$NODE" "pct exec $VMID -- test -f /usr/local/bin/commando-agent && echo yes || echo no")
        if [ "$HAS_AGENT" != "yes" ]; then
            echo "  [$HOSTNAME] SKIP (no agent installed)"
            continue
        fi

        echo "  [$HOSTNAME] Updating..."
        ssh "root@$NODE" "pct exec $VMID -- systemctl stop commando-agent"
        ssh "root@$NODE" "pct push $VMID $REMOTE_BINARY /usr/local/bin/commando-agent --perms 755"
        ssh "root@$NODE" "pct push $VMID $REMOTE_SERVICE /etc/systemd/system/commando-agent.service"
        ssh "root@$NODE" "pct exec $VMID -- systemctl daemon-reload"
        ssh "root@$NODE" "pct exec $VMID -- systemctl start commando-agent"
        echo "  [$HOSTNAME] OK"
    done

    ssh "root@$NODE" "rm -f $REMOTE_BINARY $REMOTE_SERVICE"
done

rm -f "$TMPBIN"
echo "Done."
