#!/usr/bin/env bash
# Install or update the commando CLI on macOS or Linux.
#
# Downloads the correct binary from GitHub releases, verifies it against the
# release SHA256SUMS (detects corrupted downloads; warns and continues if the
# release has none), and installs to ~/.local/bin.
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

echo "=== Commando CLI Installer ==="
echo "Platform: ${OS} ${ARCH}"
echo "Version: ${VERSION}"
echo ""

# Download binary
echo "Downloading $URL ..."
mkdir -p "$INSTALL_DIR"
TMPBIN=$(mktemp)
trap 'rm -f "$TMPBIN"' EXIT
curl -fSL -o "$TMPBIN" "$URL"
chmod +x "$TMPBIN"
echo "Downloaded $(wc -c < "$TMPBIN" | tr -d ' ') bytes"
verify_checksum "$TMPBIN" "$BINARY_NAME"

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

    # Configure Claude Code MCP if 'claude' is available
    if command -v claude >/dev/null 2>&1; then
        if claude mcp get commando </dev/null >/dev/null 2>&1; then
            echo "Claude Code MCP already configured."
        else
            echo "Configuring Claude Code MCP server..."
            if claude mcp add --scope user --transport http commando "${COMMANDO_URL}/mcp" \
                --header "Authorization: Bearer ${COMMANDO_API_KEY}" </dev/null; then
                echo "Claude Code MCP configured."
            else
                echo "Warning: failed to configure Claude Code MCP. Add manually with:"
                echo "  claude mcp add --scope user --transport http commando \"\$COMMANDO_URL/mcp\" --header \"Authorization: Bearer \$COMMANDO_API_KEY\""
            fi
        fi
    else
        echo "To connect Claude Code, run:"
        echo "  claude mcp add --scope user --transport http commando \"\$COMMANDO_URL/mcp\" --header \"Authorization: Bearer \$COMMANDO_API_KEY\""
    fi

    echo ""
    echo "Verify: commando list"
fi

echo ""
echo "Done."
