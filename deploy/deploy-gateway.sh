#!/usr/bin/env bash
# Deploy commando-gateway to akio-commando (LXC 134).
# Pulls the latest image from ghcr.io and restarts the container.
#
# Usage: ./deploy-gateway.sh [version]
#   version: tag to deploy (default: latest)
#
# Example:
#   ./deploy-gateway.sh          # deploy latest
#   ./deploy-gateway.sh v0.2.0   # deploy specific version

set -euo pipefail

NODE="akio-lab"
VMID=134
VERSION="${1:-latest}"
IMAGE="ghcr.io/icyrainz/commando-gateway:${VERSION}"

echo "Deploying $IMAGE to LXC $VMID..."

ssh "root@$NODE" "pct exec $VMID -- bash -c 'cd /root/docker-app && docker pull $IMAGE && docker compose up -d'"

echo "Done. Verifying..."
sleep 1
ssh "root@$NODE" "pct exec $VMID -- docker ps --filter name=commando-gateway --format 'table {{.Image}}\t{{.Status}}'"
