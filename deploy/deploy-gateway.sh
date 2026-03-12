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
IMAGE="ghcr.io/icyrainz/commando-gateway:${VERSION}"

echo "Deploying $IMAGE to LXC $VMID..."

ssh "root@$NODE" "pct exec $VMID -- bash -c 'cd /root/docker-app && docker pull $IMAGE && docker compose up -d'"

echo "Done. Verifying..."
sleep 1
ssh "root@$NODE" "pct exec $VMID -- docker ps --filter name=commando-gateway --format 'table {{.Image}}\t{{.Status}}'"
