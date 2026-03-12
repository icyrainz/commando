#!/usr/bin/env bash
# Install or update commando-agent on any Linux machine.
# Detects architecture, downloads the binary from GitHub releases,
# installs the systemd service, and starts/restarts the agent.
#
# Usage:
#   curl -sL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-agent.sh | bash
#   curl -sL ... | COMMANDO_VERSION=v0.3.0 bash
#
# First-time install will prompt for PSK and bind address.
# Updates preserve the existing config.

set -euo pipefail

REPO="icyrainz/commando"
VERSION="${COMMANDO_VERSION:-latest}"
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc/commando"
CONFIG_FILE="$CONFIG_DIR/agent.toml"
SERVICE_FILE="/etc/systemd/system/commando-agent.service"

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  BINARY_NAME="commando-agent-x86_64-linux" ;;
    aarch64) BINARY_NAME="commando-agent-aarch64-linux" ;;
    *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

# Resolve download URL
if [ "$VERSION" = "latest" ]; then
    URL="https://github.com/$REPO/releases/latest/download/$BINARY_NAME"
else
    URL="https://github.com/$REPO/releases/download/$VERSION/$BINARY_NAME"
fi

echo "=== Commando Agent Installer ==="
echo "Architecture: $ARCH"
echo "Version: $VERSION"
echo ""

# Download binary
echo "Downloading $URL ..."
TMPBIN=$(mktemp)
curl -fSL -o "$TMPBIN" "$URL"
chmod +x "$TMPBIN"
echo "Downloaded $(wc -c < "$TMPBIN" | tr -d ' ') bytes"

# Stop existing agent if running
if systemctl is-active --quiet commando-agent 2>/dev/null; then
    echo "Stopping existing agent..."
    systemctl stop commando-agent
fi

# Install binary
echo "Installing to $INSTALL_DIR/commando-agent ..."
mv "$TMPBIN" "$INSTALL_DIR/commando-agent"
chmod 755 "$INSTALL_DIR/commando-agent"

# Install systemd service
cat > "$SERVICE_FILE" <<'EOF'
[Unit]
Description=Commando Agent
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/commando-agent --config /etc/commando/agent.toml
Restart=always
RestartSec=5
NoNewPrivileges=yes
ProtectSystem=true
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload

# Create config if first-time install
if [ ! -f "$CONFIG_FILE" ]; then
    echo ""
    echo "First-time setup — creating config."

    # Generate PSK
    PSK=$(openssl rand -hex 32)

    # Detect default bind address
    DEFAULT_BIND=$(hostname -I 2>/dev/null | awk '{print $1}')
    DEFAULT_BIND="${DEFAULT_BIND:-0.0.0.0}"

    mkdir -p "$CONFIG_DIR"
    cat > "$CONFIG_FILE" <<TOML
bind = "$DEFAULT_BIND"
port = 9876
shell = "sh"
psk = "$PSK"
TOML
    chmod 600 "$CONFIG_FILE"

    HOSTNAME=$(hostname)

    echo ""
    echo "Config written to $CONFIG_FILE"
    echo ""
    echo "=== NEXT STEPS ==="
    echo ""
    echo "1. Add the PSK to your gateway config (/etc/commando/gateway.toml):"
    echo ""
    echo "   [agent.psk]"
    echo "   \"$HOSTNAME\" = \"$PSK\""
    echo ""
    echo "2. Add this machine as a target in the same file:"
    echo ""
    echo "   [[targets]]"
    echo "   name = \"$HOSTNAME\""
    echo "   host = \"$DEFAULT_BIND\""
    echo "   shell = \"sh\""
    echo "   tags = []"
    echo ""
    echo "3. Restart the gateway to pick up the changes."
fi

# Enable and start
systemctl enable --now commando-agent

echo ""
echo "Done. Agent status:"
systemctl status commando-agent --no-pager --lines=3
