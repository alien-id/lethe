#!/usr/bin/env bash
#
# Lethe Uninstaller
# Removes Lethe service, container, and installation files
#

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

LETHE_HOME="${LETHE_HOME:-$HOME/.lethe}"
INSTALL_DIR="${LETHE_INSTALL_DIR:-$LETHE_HOME/install}"
BIN_DIR="$LETHE_HOME/bin"

info() { echo -e "${BLUE}[INFO]${NC} $1"; }
success() { echo -e "${GREEN}[OK]${NC} $1"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }

prompt_read() {
    local prompt="$1"
    local var_name="$2"
    local value
    printf "%s" "$prompt" > /dev/tty
    IFS= read -r value < /dev/tty
    eval "$var_name=\$value"
}

detect_os() {
    case "$(uname -s)" in
        Linux*)
            if grep -qi microsoft /proc/version 2>/dev/null; then
                echo "wsl"
            else
                echo "linux"
            fi
            ;;
        Darwin*) echo "mac" ;;
        *) echo "unknown" ;;
    esac
}

echo -e "${RED}"
echo "╔═══════════════════════════════════════════════════════════╗"
echo "║                  LETHE UNINSTALLER                        ║"
echo "╚═══════════════════════════════════════════════════════════╝"
echo -e "${NC}"

echo ""
echo "This will remove:"
echo "  - All Lethe services (native and container)"
echo "  - Container rootfs / images"
echo "  - Installed binaries: $BIN_DIR/lethe, $BIN_DIR/lethe-migrate (if present)"
echo "  - Source checkout: $INSTALL_DIR"
echo ""
echo "This will NOT remove (unless you opt in at the end):"
echo "  - Your data: $LETHE_HOME (workspace, config, memory, credentials)"
echo ""
prompt_read "Are you sure you want to uninstall Lethe? [y/N] " REPLY
echo ""

if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "Aborted."
    exit 0
fi

OS=$(detect_os)

# --- Linux services ---

# Podman container service (current)
if [[ -f "$HOME/.config/systemd/user/lethe-container.service" ]]; then
    info "Stopping podman container service..."
    systemctl --user stop lethe-container 2>/dev/null || true
    systemctl --user disable lethe-container 2>/dev/null || true
    rm -f "$HOME/.config/systemd/user/lethe-container.service"
    systemctl --user daemon-reload 2>/dev/null || true
    success "Podman container service removed"
fi

if command -v podman &>/dev/null; then
    if podman container exists lethe 2>/dev/null; then
        info "Removing podman container..."
        podman stop lethe 2>/dev/null || true
        podman rm lethe 2>/dev/null || true
        success "Podman container removed"
    fi
    if podman image exists localhost/lethe:latest 2>/dev/null; then
        info "Removing podman container image..."
        podman rmi localhost/lethe:latest 2>/dev/null || true
        success "Podman container image removed"
    fi
fi

# Old nspawn container service (pre-v0.15)
if [[ -f "/etc/systemd/system/lethe-container.service" ]]; then
    info "Stopping old nspawn container service..."
    sudo systemctl stop lethe-container 2>/dev/null || true
    sudo systemctl disable lethe-container 2>/dev/null || true
    sudo rm -f "/etc/systemd/system/lethe-container.service"
    sudo rm -f "/etc/systemd/nspawn/lethe.nspawn"
    sudo systemctl daemon-reload 2>/dev/null || true
    success "Old nspawn container service removed"
fi

# Old nspawn rootfs
if [[ -d "/var/lib/machines/lethe" ]]; then
    info "Removing old nspawn rootfs..."
    sudo rm -rf "/var/lib/machines/lethe"
    success "Old nspawn rootfs removed"
fi

# Old native systemd user service
if [[ -f "$HOME/.config/systemd/user/lethe.service" ]]; then
    info "Stopping old native service (systemd user)..."
    systemctl --user stop lethe 2>/dev/null || true
    systemctl --user disable lethe 2>/dev/null || true
    rm -f "$HOME/.config/systemd/user/lethe.service"
    rm -rf "$HOME/.config/systemd/user/lethe.service.d" 2>/dev/null || true
    systemctl --user daemon-reload 2>/dev/null || true
    success "Native user service removed"
fi

# Old native systemd system service
if [[ -f "/etc/systemd/system/lethe.service" ]]; then
    info "Stopping old native service (systemd system)..."
    sudo systemctl stop lethe 2>/dev/null || true
    sudo systemctl disable lethe 2>/dev/null || true
    sudo rm -f "/etc/systemd/system/lethe.service"
    sudo systemctl daemon-reload 2>/dev/null || true
    success "Native system service removed"
fi

# --- macOS services ---

# Container launchd service
if [[ -f "$HOME/Library/LaunchAgents/com.lethe.container.plist" ]]; then
    info "Stopping container service (launchd)..."
    launchctl unload "$HOME/Library/LaunchAgents/com.lethe.container.plist" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/com.lethe.container.plist"
    success "Container launchd service removed"
fi

# Container image (apple/container)
if command -v container &>/dev/null; then
    if container image ls 2>/dev/null | grep -q "lethe"; then
        info "Removing apple/container image..."
        container image rm lethe:latest 2>/dev/null || true
        success "Container image removed"
    fi
fi

# Podman container + image (macOS fallback)
if command -v podman &>/dev/null; then
    if podman container exists lethe 2>/dev/null; then
        info "Removing podman container..."
        podman stop lethe 2>/dev/null || true
        podman rm lethe 2>/dev/null || true
        success "Podman container removed"
    fi
    if podman image exists localhost/lethe:latest 2>/dev/null; then
        info "Removing podman image..."
        podman rmi localhost/lethe:latest 2>/dev/null || true
        success "Podman image removed"
    fi
fi

# Old native launchd service
if [[ -f "$HOME/Library/LaunchAgents/com.lethe.agent.plist" ]]; then
    info "Stopping old native service (launchd)..."
    launchctl unload "$HOME/Library/LaunchAgents/com.lethe.agent.plist" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/com.lethe.agent.plist"
    success "Native launchd service removed"
fi

# --- Shared cleanup ---

# Launch script
rm -f "$LETHE_HOME/run-container.sh" 2>/dev/null || true

# Installed binaries
for bin_name in lethe lethe-migrate; do
    if [[ -f "$BIN_DIR/$bin_name" ]]; then
        info "Removing $BIN_DIR/$bin_name..."
        rm -f "$BIN_DIR/$bin_name"
        success "$bin_name binary removed"
    fi
done
# Drop $BIN_DIR if we left it empty, but only if it sits under
# $LETHE_HOME — never touch a user-chosen path with other things in it.
if [[ -d "$BIN_DIR" && "$BIN_DIR" == "$LETHE_HOME/bin" ]]; then
    rmdir "$BIN_DIR" 2>/dev/null || true
fi

# Installation directory (source checkout)
if [[ -d "$INSTALL_DIR" ]]; then
    info "Removing installation directory ($INSTALL_DIR)..."
    rm -rf "$INSTALL_DIR"
    success "Installation directory removed"
fi

# Offer to remove all data
echo ""
prompt_read "Also remove all Lethe data at $LETHE_HOME? [y/N] " remove_data
remove_data=${remove_data:-N}
if [[ "$remove_data" =~ ^[Yy]$ ]]; then
    info "Removing $LETHE_HOME..."
    rm -rf "$LETHE_HOME"
    success "All Lethe data removed"
else
    info "Data preserved at $LETHE_HOME"
fi

echo ""
success "Lethe has been uninstalled."
echo ""
echo "To reinstall:"
echo "  curl -fsSL https://lethe.gg/install | bash"
