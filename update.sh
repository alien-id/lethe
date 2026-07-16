#!/usr/bin/env bash
#
# Lethe Rust updater.
# Usage: curl -fsSL https://lethe.gg/update | bash

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

LETHE_HOME="${LETHE_HOME:-$HOME/.lethe}"
INSTALL_DIR="${LETHE_INSTALL_DIR:-$LETHE_HOME/install}"
BIN_DIR="$LETHE_HOME/bin"
REPO_OWNER="${LETHE_REPO_OWNER:-alien-id}"
REPO_NAME="${LETHE_REPO_NAME:-lethe}"
RELEASE_BASE_URL="${LETHE_RELEASE_BASE_URL:-https://github.com/$REPO_OWNER/$REPO_NAME/releases/latest/download}"

info() { echo -e "${BLUE}[INFO]${NC} $1"; }
success() { echo -e "${GREEN}[OK]${NC} $1"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

# Container-first deployments run their own copy of the binary baked into
# the image, so updating the host binary doesn't touch the running
# container. If one is present, tell the user how to roll the new version in.
post_update_notice() {
    local has_container=0
    if [ -f "$HOME/.config/systemd/user/lethe-container.service" ] \
       || [ -f "$HOME/Library/LaunchAgents/com.lethe.container.plist" ]; then
        has_container=1
    elif command -v podman >/dev/null 2>&1 && podman container exists lethe 2>/dev/null; then
        has_container=1
    fi
    if [ "$has_container" = "1" ]; then
        echo ""
        warn "A container deployment is still running the previous version."
        warn "Roll the update into it with:  $BIN_DIR/lethe container up --rebuild"
    fi
}

detect_release_target() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os:$arch" in
        Linux:x86_64|Linux:amd64) echo "x86_64-unknown-linux-gnu" ;;
        Linux:aarch64|Linux:arm64) echo "aarch64-unknown-linux-gnu" ;;
        Darwin:x86_64|Darwin:amd64) echo "x86_64-apple-darwin" ;;
        Darwin:aarch64|Darwin:arm64) echo "aarch64-apple-darwin" ;;
        *) return 1 ;;
    esac
}

install_release_binary() {
    local target url tmp archive binary

    if ! command -v curl >/dev/null 2>&1 || ! command -v tar >/dev/null 2>&1; then
        warn "curl and tar are required for binary update."
        return 1
    fi

    if ! target="$(detect_release_target)"; then
        warn "No binary release target for $(uname -s)/$(uname -m)."
        return 1
    fi

    url="$RELEASE_BASE_URL/lethe-$target.tar.gz"
    tmp="$(mktemp -d)"
    archive="$tmp/lethe.tar.gz"

    info "Downloading binary release: $url"
    if ! curl -fsSL "$url" -o "$archive"; then
        warn "Binary release download failed."
        rm -rf "$tmp"
        return 1
    fi

    if ! tar -xzf "$archive" -C "$tmp"; then
        warn "Binary release archive could not be unpacked."
        rm -rf "$tmp"
        return 1
    fi

    binary="$(find "$tmp" -type f -name lethe -perm -111 | head -n 1)"
    if [ -z "$binary" ]; then
        warn "Binary release archive did not contain an executable lethe binary."
        rm -rf "$tmp"
        return 1
    fi

    mkdir -p "$BIN_DIR"
    cp "$binary" "$BIN_DIR/lethe"
    chmod +x "$BIN_DIR/lethe"
    rm -rf "$tmp"
    success "Updated $BIN_DIR/lethe from binary release"
}

if [ "${LETHE_UPDATE_FROM_SOURCE:-0}" != "1" ] && install_release_binary; then
    post_update_notice
    exit 0
fi

warn "Falling back to source update."

if ! command -v cargo >/dev/null 2>&1; then
    error "Cargo is required for source update. Install Rust from https://rustup.rs."
fi

if ! command -v protoc >/dev/null 2>&1; then
    error "protoc is required by LanceDB source builds. Install protobuf-compiler/libprotobuf-dev (Debian/Ubuntu), protobuf-compiler/protobuf-devel (Fedora), or protobuf (Homebrew)."
fi

if [ ! -d "$INSTALL_DIR/.git" ]; then
    error "No Lethe checkout found at $INSTALL_DIR. Set LETHE_INSTALL_DIR or rerun install.sh."
fi

info "Updating checkout: $INSTALL_DIR"
git -C "$INSTALL_DIR" pull --ff-only

info "Building release binary..."
cargo build --release --manifest-path "$INSTALL_DIR/Cargo.toml"

mkdir -p "$BIN_DIR"
cp "$INSTALL_DIR/target/release/lethe" "$BIN_DIR/lethe"
chmod +x "$BIN_DIR/lethe"

success "Updated $BIN_DIR/lethe"
post_update_notice
