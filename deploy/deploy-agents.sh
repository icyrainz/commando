#!/usr/bin/env bash
# First-time deploy of commando-agent to ALL running LXCs on Proxmox nodes.
# Pushes binary + service + config to every LXC, generates unique PSKs,
# and prints the [agent.psk] entries to paste into gateway.toml.
#
# WARNING: Overwrites existing agent configs. For updates, use update-agents.sh instead.
#
# Prerequisites:
#   - SSH root access to the specified Proxmox nodes
#   - Agent binary pre-built at target/x86_64-unknown-linux-musl/release/commando-agent
#     (build with: cargo build --release --target x86_64-unknown-linux-musl -p commando-agent)
#
# Usage: ./deploy/deploy-agents.sh <proxmox-node> [proxmox-node-2] ...
#
# Examples:
#   ./deploy/deploy-agents.sh akio-lab akio-garage
#
# After running, copy the printed PSK entries into your gateway.toml [agent.psk] section
# and restart the gateway.
#
# For non-Proxmox hosts, use install-agent.sh instead (SSH in and curl-pipe-bash).

set -euo pipefail

BINARY="target/x86_64-unknown-linux-musl/release/commando-agent"
SERVICE_FILE="deploy/commando-agent.service"
AGENT_PORT=9876
COLLECTED_PSKS=""

if [ $# -eq 0 ]; then
    echo "Usage: $0 <proxmox-node-1> [proxmox-node-2] ..."
    exit 1
fi

if [ ! -f "$BINARY" ]; then
    echo "Error: Agent binary not found at $BINARY"
    echo "Build with: cargo build --release --target x86_64-unknown-linux-musl -p commando-agent"
    exit 1
fi

for NODE in "$@"; do
    echo "=== Deploying to $NODE ==="

    REMOTE_BINARY="/tmp/commando-agent"
    REMOTE_SERVICE="/tmp/commando-agent.service"
    scp "$BINARY" "root@$NODE:$REMOTE_BINARY"
    scp "$SERVICE_FILE" "root@$NODE:$REMOTE_SERVICE"

    VMIDS=$(ssh "root@$NODE" "pct list" | tail -n +2 | awk '{print $1}')

    for VMID in $VMIDS; do
        HOSTNAME=$(ssh "root@$NODE" "pct config $VMID" | grep ^hostname | awk '{print $2}')
        STATUS=$(ssh "root@$NODE" "pct status $VMID" | awk '{print $2}')
        TARGET_NAME="${NODE}/${HOSTNAME}"

        if [ "$STATUS" != "running" ]; then
            echo "  [$TARGET_NAME] SKIP (status: $STATUS)"
            continue
        fi

        echo "  [$TARGET_NAME] Deploying..."

        IP=$(ssh "root@$NODE" "pct exec $VMID -- hostname -I" | awk '{print $1}')

        ssh "root@$NODE" "pct push $VMID $REMOTE_BINARY /usr/local/bin/commando-agent --perms 755"
        ssh "root@$NODE" "pct push $VMID $REMOTE_SERVICE /etc/systemd/system/commando-agent.service"

        PSK=$(openssl rand -hex 32)

        ssh "root@$NODE" "pct exec $VMID -- mkdir -p /etc/commando"
        ssh "root@$NODE" "pct exec $VMID -- bash -c 'cat > /etc/commando/agent.toml << AGENTEOF
bind = \"$IP\"
port = $AGENT_PORT
shell = \"sh\"
psk = \"$PSK\"
max_output_bytes = 131072
max_concurrent = 8
AGENTEOF
chmod 600 /etc/commando/agent.toml'"

        ssh "root@$NODE" "pct exec $VMID -- systemctl daemon-reload"
        ssh "root@$NODE" "pct exec $VMID -- systemctl enable --now commando-agent"

        COLLECTED_PSKS="${COLLECTED_PSKS}\"${TARGET_NAME}\" = \"${PSK}\"\n"
        echo "  [$TARGET_NAME] OK (ip: $IP)"
    done
done

echo ""
echo "=== Add these PSKs to gateway.toml [agent.psk] ==="
echo -e "$COLLECTED_PSKS"
