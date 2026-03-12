#!/usr/bin/env bash
# Deploy or update the commando-gateway Docker container on a Proxmox LXC.
# Pulls the image from ghcr.io/icyrainz/commando-gateway and restarts via docker compose.
#
# Prerequisites:
#   - SSH root access to the Proxmox node
#   - The target LXC must already have Docker installed and a docker-compose.yml
#     at /root/docker-app/ that references the commando-gateway image
#
# Usage: ./deploy/deploy-gateway.sh <proxmox-node> <vmid> [version]
#
# Arguments:
#   proxmox-node  - Proxmox node hostname (e.g., akio-lab)
#   vmid          - LXC container ID where gateway runs (e.g., 111)
#   version       - Docker image tag (default: "latest")
#
# Examples:
#   ./deploy/deploy-gateway.sh akio-lab 111            # deploy latest
#   ./deploy/deploy-gateway.sh akio-lab 111 v0.3.2     # deploy specific version

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
