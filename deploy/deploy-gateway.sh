#!/usr/bin/env bash
# Deploy or update the commando-gateway Docker container on a Proxmox LXC.
# Pulls the image from ghcr.io/icyrainz/commando-gateway and restarts via docker compose.
# The compose file pins the :latest tag, so a specific version is re-tagged as
# :latest after the pull. Otherwise "docker compose up -d --pull never" would ignore it.
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
REPO_IMAGE="ghcr.io/icyrainz/commando-gateway"
IMAGE="${REPO_IMAGE}:${VERSION}"

echo "Deploying $IMAGE to LXC $VMID..."

if [ "$VERSION" = "latest" ]; then
    REMOTE_CMD="cd /root/docker-app && docker pull $IMAGE && docker compose up -d --pull never"
else
    REMOTE_CMD="cd /root/docker-app && docker pull $IMAGE && docker tag $IMAGE ${REPO_IMAGE}:latest && docker compose up -d --pull never"
fi
ssh "root@$NODE" "pct exec $VMID -- bash -c '$REMOTE_CMD'"

echo "Done. Verifying..."
sleep 1
ssh "root@$NODE" "pct exec $VMID -- docker ps --filter name=commando-gateway --format 'table {{.Image}}\t{{.Status}}'"

PULLED_ID=$(ssh "root@$NODE" "pct exec $VMID -- docker image inspect -f '{{.Id}}' $IMAGE") \
    || { echo "Error: could not inspect pulled image $IMAGE"; exit 1; }
RUNNING_ID=$(ssh "root@$NODE" "pct exec $VMID -- docker inspect --type container -f '{{.Image}}' commando-gateway") \
    || { echo "Error: could not inspect container commando-gateway"; exit 1; }
echo "Pulled image ID:  $PULLED_ID"
echo "Running image ID: $RUNNING_ID"
if [ "$PULLED_ID" = "$RUNNING_ID" ]; then
    echo "MATCH"
else
    echo "MISMATCH"
    exit 1
fi
