#!/usr/bin/env bash
# Update commando-agent binary on all running LXCs.
# Downloads from GitHub releases and restarts the service.
# Does NOT touch config or PSKs — use deploy-agents.sh for first-time setup.
#
# Usage: ./update-agents.sh <proxmox-node> [proxmox-node-2] ...
#   Downloads the latest release by default.
#   Set COMMANDO_VERSION=v0.2.0 to pin a specific version.
#
# Example:
#   ./update-agents.sh akio-lab
#   COMMANDO_VERSION=v0.1.0 ./update-agents.sh akio-lab akio-garage

set -euo pipefail

REPO="icyrainz/commando"
VERSION="${COMMANDO_VERSION:-latest}"
BINARY_NAME="commando-agent-x86_64-linux"

if [ $# -eq 0 ]; then
    echo "Usage: $0 <proxmox-node-1> [proxmox-node-2] ..."
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
    scp "$TMPBIN" "root@$NODE:$REMOTE_BINARY"

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
        ssh "root@$NODE" "pct push $VMID $REMOTE_BINARY /usr/local/bin/commando-agent --perms 755"
        ssh "root@$NODE" "pct exec $VMID -- systemctl restart commando-agent"
        echo "  [$HOSTNAME] OK"
    done

    ssh "root@$NODE" "rm -f $REMOTE_BINARY"
done

rm -f "$TMPBIN"
echo "Done."
