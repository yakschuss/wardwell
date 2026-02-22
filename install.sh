#!/usr/bin/env bash
set -euo pipefail

# Wardwell installer
# Usage: curl -sSf https://raw.githubusercontent.com/youruser/wardwell/main/install.sh | bash

REPO="wardwell"
INSTALL_DIR="${WARDWELL_INSTALL_DIR:-$HOME/.local/bin}"

info() { printf "\033[1;34m→\033[0m %s\n" "$1"; }
ok()   { printf "\033[1;32m✓\033[0m %s\n" "$1"; }
err()  { printf "\033[1;31m✗\033[0m %s\n" "$1" >&2; exit 1; }

# Detect platform
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Darwin) PLATFORM="apple-darwin" ;;
    Linux)  PLATFORM="unknown-linux-gnu" ;;
    *)      err "Unsupported OS: $OS" ;;
esac

case "$ARCH" in
    x86_64)  TARGET_ARCH="x86_64" ;;
    aarch64|arm64) TARGET_ARCH="aarch64" ;;
    *)       err "Unsupported architecture: $ARCH" ;;
esac

TARGET="${TARGET_ARCH}-${PLATFORM}"
info "Detected platform: $TARGET"

# Check for Rust toolchain
if command -v cargo >/dev/null 2>&1; then
    info "Rust toolchain found. Building from source..."

    # Clone or update
    CLONE_DIR="${TMPDIR:-/tmp}/wardwell-install"
    if [ -d "$CLONE_DIR" ]; then
        info "Updating existing clone..."
        cd "$CLONE_DIR" && git pull --quiet
    else
        info "Cloning repository..."
        git clone --quiet --depth 1 https://github.com/youruser/wardwell.git "$CLONE_DIR"
        cd "$CLONE_DIR"
    fi

    # Build
    info "Building release binary..."
    cargo build --release --quiet

    # Install
    mkdir -p "$INSTALL_DIR"
    cp target/release/wardwell "$INSTALL_DIR/wardwell"
    chmod +x "$INSTALL_DIR/wardwell"

    ok "Installed wardwell to $INSTALL_DIR/wardwell"

    # Clean up
    rm -rf "$CLONE_DIR"
else
    err "Rust toolchain not found. Install Rust first: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

# Verify installation
if ! command -v wardwell >/dev/null 2>&1; then
    if [ -x "$INSTALL_DIR/wardwell" ]; then
        info "wardwell installed but not on PATH."
        info "Add this to your shell profile:"
        echo ""
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        echo ""
    fi
else
    ok "wardwell $(wardwell version 2>/dev/null || echo '(version unknown)') is ready"
fi

# Post-install setup
echo ""
info "Quick start:"
echo "  wardwell init              # Scan machine, generate config"
echo "  wardwell hook install --runtime claude-code  # Install hooks"
echo "  wardwell context generate  # Generate CLAUDE.md"
echo "  wardwell ui                # Open the cockpit"
