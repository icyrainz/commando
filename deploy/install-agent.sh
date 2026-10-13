#!/usr/bin/env bash
# Install or update commando-agent on any Linux machine (x86_64 or aarch64).
# Works for both Proxmox LXCs and standalone hosts — architecture-agnostic.
#
# What it does:
#   - Downloads the correct binary from GitHub releases and verifies it against
#     the release SHA256SUMS (detects corrupted downloads; warns and continues if
#     the release has none)
#   - Installs/updates /usr/local/bin/commando-agent and the systemd service
#   - First install: generates a PSK, creates /etc/commando/agent.toml,
#     and prints the TOML snippets to add to gateway.toml
#   - Update: preserves existing config, only replaces binary + service file
#
# Prerequisites:
#   - Root access (run as root or with sudo)
#   - curl, openssl, systemctl
#
# Usage (SSH into the target, then):
#   curl -sL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-agent.sh | bash
#   curl -sL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-agent.sh | COMMANDO_VERSION=v0.3.2 bash
#
# Environment:
#   COMMANDO_VERSION  - GitHub release tag (default: "latest")
#
# After first install, copy the printed [agent.psk] and [[targets]] entries
# into the gateway's gateway.toml and restart the gateway.

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
    BASE_URL="https://github.com/$REPO/releases/latest/download"
else
    BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
fi
URL="$BASE_URL/$BINARY_NAME"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# Verify $1 against the release SHA256SUMS entry for $2. Warn if the release has none.
verify_checksum() {
    local file="$1" name="$2" sums code expected actual
    sums=$(mktemp)
    code=$(curl -sSL -o "$sums" -w '%{http_code}' "$BASE_URL/SHA256SUMS") || {
        rm -f "$sums"
        echo "Error: could not download SHA256SUMS"
        return 1
    }
    case "$code" in
        200) ;;
        404)
            rm -f "$sums"
            echo "Warning: release has no SHA256SUMS, skipping checksum verification"
            return 0
            ;;
        *)
            rm -f "$sums"
            echo "Error: SHA256SUMS download returned HTTP $code"
            return 1
            ;;
    esac
    expected=$(awk -v n="$name" '$2 == n || $2 == "*" n {print $1; exit}' "$sums")
    rm -f "$sums"
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

echo "=== Commando Agent Installer ==="
echo "Architecture: $ARCH"
echo "Version: $VERSION"
echo ""

# Download binary
echo "Downloading $URL ..."
TMPBIN=$(mktemp)
trap 'rm -f "$TMPBIN"' EXIT
curl -fSL -o "$TMPBIN" "$URL"
chmod +x "$TMPBIN"
echo "Downloaded $(wc -c < "$TMPBIN" | tr -d ' ') bytes"
verify_checksum "$TMPBIN" "$BINARY_NAME"

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
shell = "bash"
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
