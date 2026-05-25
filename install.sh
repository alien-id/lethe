#!/usr/bin/env bash
#
# Lethe Rust installer.
# Usage: curl -fsSL https://lethe.gg/install | bash
#
# Downloads the prebuilt `lethe` and (when available) `lethe-migrate`
# binaries for the current platform, then hands off to `lethe init`
# for provider / model / API-key setup. Falls back to a source build
# when no binary asset matches the host (or `LETHE_INSTALL_FROM_SOURCE=1`).
#
# Env knobs:
#   LETHE_HOME                Install root (default: $HOME/.lethe)
#   LETHE_INSTALL_FROM_SOURCE Force a `cargo build --release` even if
#                             a binary release is available.
#   LETHE_SKIP_INIT           Skip the post-install `lethe init` wizard.
#   LETHE_REPO_URL            Override clone URL for the source path.
#   LETHE_RELEASE_BASE_URL    Override binary release base URL.

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

REPO_URL="${LETHE_REPO_URL:-https://github.com/atemerev/lethe.git}"
REPO_OWNER="${LETHE_REPO_OWNER:-atemerev}"
REPO_NAME="${LETHE_REPO_NAME:-lethe}"
RELEASE_BASE_URL="${LETHE_RELEASE_BASE_URL:-https://github.com/$REPO_OWNER/$REPO_NAME/releases/latest/download}"
LETHE_HOME="${LETHE_HOME:-$HOME/.lethe}"
INSTALL_DIR="${LETHE_INSTALL_DIR:-$LETHE_HOME/install}"
CONFIG_DIR="$LETHE_HOME/config"
ENV_FILE="$CONFIG_DIR/.env"
BIN_DIR="$LETHE_HOME/bin"

info()    { echo -e "${BLUE}[INFO]${NC} $1"; }
success() { echo -e "${GREEN}[OK]${NC} $1"; }
warn()    { echo -e "${YELLOW}[WARN]${NC} $1"; }
error()   { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

print_header() {
    echo -e "${BLUE}"
    echo "╔═══════════════════════════════════════════════════════════╗"
    echo "║                     LETHE RUST                            ║"
    echo "║              Local AI assistant runtime                   ║"
    echo "╚═══════════════════════════════════════════════════════════╝"
    echo -e "${NC}"
}

ensure_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        return
    fi

    warn "Rust/Cargo is not installed."
    if command -v curl >/dev/null 2>&1; then
        info "Installing Rust through rustup..."
        curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi

    command -v cargo >/dev/null 2>&1 || error "Install Rust from https://rustup.rs and rerun this installer."
}

ensure_protoc() {
    if command -v protoc >/dev/null 2>&1; then
        return
    fi
    error "protoc is required by the migrator's LanceDB dep at build time. \
Install protobuf-compiler/libprotobuf-dev (Debian/Ubuntu), \
protobuf-compiler/protobuf-devel (Fedora), or protobuf (Homebrew), then rerun."
}

detect_release_target() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os:$arch" in
        Linux:x86_64|Linux:amd64)   echo "x86_64-unknown-linux-gnu" ;;
        Linux:aarch64|Linux:arm64)  echo "aarch64-unknown-linux-gnu" ;;
        Darwin:x86_64|Darwin:amd64) echo "x86_64-apple-darwin" ;;
        Darwin:aarch64|Darwin:arm64) echo "aarch64-apple-darwin" ;;
        *) return 1 ;;
    esac
}

# Download $1 (binary name: "lethe" or "lethe-migrate") for $2 (target
# triple) into $BIN_DIR. Returns non-zero on any failure so callers
# can treat optional binaries gracefully.
download_binary() {
    local name="$1"
    local target="$2"
    local url tmp archive binary
    url="$RELEASE_BASE_URL/${name}-${target}.tar.gz"
    tmp="$(mktemp -d)"
    archive="$tmp/${name}.tar.gz"

    info "Downloading $name: $url"
    if ! curl -fsSL "$url" -o "$archive"; then
        warn "Download failed: $url"
        rm -rf "$tmp"
        return 1
    fi

    if ! tar -xzf "$archive" -C "$tmp"; then
        warn "Could not unpack $url"
        rm -rf "$tmp"
        return 1
    fi

    binary="$(find "$tmp" -type f -name "$name" -perm -111 | head -n 1)"
    if [ -z "$binary" ]; then
        warn "Archive did not contain an executable $name"
        rm -rf "$tmp"
        return 1
    fi

    mkdir -p "$BIN_DIR"
    cp "$binary" "$BIN_DIR/$name"
    chmod +x "$BIN_DIR/$name"
    rm -rf "$tmp"
    success "Installed $BIN_DIR/$name"
    return 0
}

install_release_binaries() {
    local target

    if ! command -v curl >/dev/null 2>&1 || ! command -v tar >/dev/null 2>&1; then
        warn "curl and tar are required for binary install."
        return 1
    fi

    if ! target="$(detect_release_target)"; then
        warn "No binary release target for $(uname -s)/$(uname -m)."
        return 1
    fi

    # `lethe` is required — failure here means we fall back to source.
    download_binary lethe "$target" || return 1

    # `lethe-migrate` is optional — only useful for v0.18→v0.19
    # migration. A missing asset (e.g. older release tag) shouldn't
    # block the install.
    if ! download_binary lethe-migrate "$target"; then
        warn "lethe-migrate not available for this release — only required \
to migrate data from v0.18 or earlier."
    fi
    return 0
}

checkout_repo() {
    local script_dir
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
    if [ -f "$script_dir/Cargo.toml" ] && grep -q 'name = "lethe"' "$script_dir/Cargo.toml" 2>/dev/null; then
        INSTALL_DIR="$script_dir"
        info "Using local checkout: $INSTALL_DIR"
        return
    fi

    if [ -d "$INSTALL_DIR/.git" ]; then
        info "Updating existing checkout: $INSTALL_DIR"
        git -C "$INSTALL_DIR" pull --ff-only
    else
        info "Cloning Lethe into $INSTALL_DIR"
        mkdir -p "$(dirname "$INSTALL_DIR")"
        git clone "$REPO_URL" "$INSTALL_DIR"
    fi
}

build_from_source() {
    ensure_cargo
    checkout_repo

    info "Building lethe with Cargo..."
    cargo build --release --manifest-path "$INSTALL_DIR/Cargo.toml"
    mkdir -p "$BIN_DIR"
    cp "$INSTALL_DIR/target/release/lethe" "$BIN_DIR/lethe"
    chmod +x "$BIN_DIR/lethe"
    success "Installed $BIN_DIR/lethe"

    # The migrator is one-shot; only build it if explicitly requested.
    if [ "${LETHE_BUILD_MIGRATOR:-0}" = "1" ]; then
        ensure_protoc
        info "Building lethe-migrate with Cargo..."
        cargo build --release --manifest-path "$INSTALL_DIR/migrator/Cargo.toml"
        cp "$INSTALL_DIR/migrator/target/release/lethe-migrate" "$BIN_DIR/lethe-migrate"
        chmod +x "$BIN_DIR/lethe-migrate"
        success "Installed $BIN_DIR/lethe-migrate"
    fi
}

run_init_wizard() {
    if [ "${LETHE_SKIP_INIT:-0}" = "1" ]; then
        info "LETHE_SKIP_INIT=1 — skipping setup wizard."
        return
    fi
    if [ -f "$ENV_FILE" ]; then
        info "Existing config at $ENV_FILE — skipping setup wizard."
        info "Rerun '$BIN_DIR/lethe init' anytime to reconfigure."
        return
    fi
    # `lethe init` reads from stdin; under `curl | bash` our stdin is
    # the curl pipe, so redirect explicitly from the controlling TTY.
    if [ ! -e /dev/tty ]; then
        warn "No /dev/tty available — skipping setup wizard."
        warn "Run '$BIN_DIR/lethe init' manually to configure."
        return
    fi
    echo ""
    info "Launching setup wizard: $BIN_DIR/lethe init"
    echo ""
    if ! "$BIN_DIR/lethe" init < /dev/tty; then
        warn "Setup wizard exited with an error."
        warn "You can rerun it anytime: $BIN_DIR/lethe init"
    fi
}

main() {
    print_header

    if [ "${LETHE_INSTALL_FROM_SOURCE:-0}" != "1" ] && install_release_binaries; then
        :
    else
        warn "Falling back to source build."
        build_from_source
    fi

    run_init_wizard

    echo ""
    success "Lethe installed."
    echo "  Binary:    $BIN_DIR/lethe"
    if [ -x "$BIN_DIR/lethe-migrate" ]; then
        echo "  Migrator:  $BIN_DIR/lethe-migrate  (run only when moving data from v0.18)"
    fi
    echo "  Config:    $ENV_FILE"
    echo ""
    echo "Next:  $BIN_DIR/lethe check"
}

main "$@"
