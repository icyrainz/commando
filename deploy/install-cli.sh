#!/usr/bin/env bash
# Install or update the commando CLI on macOS or Linux.
#
# Downloads the correct binary from GitHub releases and installs to ~/.local/bin.
# No root access required.
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-cli.sh | bash
#   curl -sSL https://raw.githubusercontent.com/icyrainz/commando/main/deploy/install-cli.sh | COMMANDO_VERSION=v0.5.0 bash
#
# Environment:
#   COMMANDO_VERSION  - GitHub release tag (default: "latest")
#   COMMANDO_URL      - Gateway URL (prompted if not set, for shell config)
#   COMMANDO_API_KEY  - Gateway API key (prompted if not set, for shell config)

set -euo pipefail

REPO="icyrainz/commando"
VERSION="${COMMANDO_VERSION:-latest}"
INSTALL_DIR="${HOME}/.local/bin"

# Detect platform and architecture
OS=$(uname -s)
ARCH=$(uname -m)

case "${OS}-${ARCH}" in
    Linux-x86_64)   BINARY_NAME="commando-cli-x86_64-linux" ;;
    Linux-aarch64)  BINARY_NAME="commando-cli-aarch64-linux" ;;
    Darwin-arm64)   BINARY_NAME="commando-cli-aarch64-macos" ;;
    *)              echo "Unsupported platform: ${OS}-${ARCH}"; exit 1 ;;
esac

# Resolve download URL
if [ "$VERSION" = "latest" ]; then
    URL="https://github.com/$REPO/releases/latest/download/$BINARY_NAME"
else
    URL="https://github.com/$REPO/releases/download/$VERSION/$BINARY_NAME"
fi

echo "=== Commando CLI Installer ==="
echo "Platform: ${OS} ${ARCH}"
echo "Version: ${VERSION}"
echo ""

# Download binary
echo "Downloading $URL ..."
mkdir -p "$INSTALL_DIR"
TMPBIN=$(mktemp)
curl -fSL -o "$TMPBIN" "$URL"
chmod +x "$TMPBIN"
echo "Downloaded $(wc -c < "$TMPBIN" | tr -d ' ') bytes"

# Verify it runs
if ! "$TMPBIN" --version >/dev/null 2>&1; then
    echo "Error: downloaded binary failed to execute"
    rm -f "$TMPBIN"
    exit 1
fi

CLI_VERSION=$("$TMPBIN" --version)
echo "$CLI_VERSION"

# Install
mv "$TMPBIN" "$INSTALL_DIR/commando"
chmod 755 "$INSTALL_DIR/commando"
echo "Installed to $INSTALL_DIR/commando"

# Check if ~/.local/bin is in PATH
if ! echo "$PATH" | tr ':' '\n' | grep -q "^${INSTALL_DIR}$"; then
    echo ""
    echo "Warning: $INSTALL_DIR is not in your PATH."
    echo "Add it to your shell config:"
    echo ""
    case "$SHELL" in
        */fish) echo "  fish_add_path $INSTALL_DIR" ;;
        */zsh)  echo "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc" ;;
        *)      echo "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.bashrc" ;;
    esac
fi

# Check env vars
echo ""
if [ -z "${COMMANDO_URL:-}" ] || [ -z "${COMMANDO_API_KEY:-}" ]; then
    echo "=== NEXT STEPS ==="
    echo ""
    echo "Set these environment variables in your shell config:"
    echo ""
    case "$SHELL" in
        */fish)
            echo "  set -gx COMMANDO_URL \"http://your-gateway:9877\""
            echo "  set -gx COMMANDO_API_KEY \"your-api-key\""
            ;;
        *)
            echo "  export COMMANDO_URL=\"http://your-gateway:9877\""
            echo "  export COMMANDO_API_KEY=\"your-api-key\""
            ;;
    esac
    echo ""
    echo "Then verify with:"
    echo "  commando list"
else
    echo "Environment variables already set."
    echo "  COMMANDO_URL=$COMMANDO_URL"
    echo "  COMMANDO_API_KEY=(set)"
    echo ""
    echo "Verify: commando list"
fi

echo ""
echo "Done."
