#!/bin/sh
set -e

# cica installer script

CICA_VERSION="${CICA_VERSION:-latest}"
CICA_BASE_URL="https://github.com/oxideai/cica/releases"

# Colors for output
if [ -t 1 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    RESET='\033[0m'
else
    RED=''
    GREEN=''
    YELLOW=''
    BLUE=''
    RESET=''
fi

info() {
    printf "${BLUE}>${RESET} %s\n" "$1"
}

success() {
    printf "${GREEN}>${RESET} %s\n" "$1"
}

error() {
    printf "${RED}>${RESET} %s\n" "$1" >&2
}

warning() {
    printf "${YELLOW}>${RESET} %s\n" "$1"
}

# Detect OS and architecture
detect_platform() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    ARCH=$(uname -m)

    # Normalize OS names
    case "$OS" in
        linux*) OS="linux" ;;
        darwin*) OS="macos" ;;
        *) error "Unsupported operating system: $OS"; exit 1 ;;
    esac

    # Normalize architecture names
    case "$ARCH" in
        x86_64|amd64) ARCH="x86_64" ;;
        arm64|aarch64) ARCH="aarch64" ;;
        *) error "Unsupported architecture: $ARCH"; exit 1 ;;
    esac

    info "Detected platform: $OS/$ARCH"
}

# Check if cica should be installed/upgraded
should_install() {
    if ! command -v cica >/dev/null 2>&1; then
        return 0  # Not installed, should install
    fi

    # Already installed, check if upgrade needed when installing latest
    if [ "$CICA_VERSION" = "latest" ]; then
        CURRENT_VERSION=$(cica --version 2>/dev/null | grep -o '[0-9]\+\.[0-9]\+\.[0-9]\+' || echo "0.0.0")

        # Get latest version from GitHub API
        if command -v curl >/dev/null 2>&1; then
            LATEST_VERSION=$(curl -fsSL https://api.github.com/repos/oxideai/cica/releases/latest | grep -o '"tag_name": *"v[^"]*"' | sed 's/"tag_name": *"v\([^"]*\)"/\1/' 2>/dev/null || echo "$CURRENT_VERSION")
        else
            LATEST_VERSION="$CURRENT_VERSION"
        fi

        if [ "$CURRENT_VERSION" != "$LATEST_VERSION" ]; then
            info "Upgrading cica from $CURRENT_VERSION to $LATEST_VERSION"
            return 0  # Should upgrade
        else
            info "cica $CURRENT_VERSION is already up to date at $(command -v cica)"
            exit 0
        fi
    else
        # Installing specific version, allow reinstall
        CURRENT_VERSION=$(cica --version 2>/dev/null | grep -o '[0-9]\+\.[0-9]\+\.[0-9]\+' || echo "unknown")
        info "Reinstalling cica $CICA_VERSION (current: $CURRENT_VERSION)"
        return 0
    fi
}

# Determine installation directory
get_install_dir() {
    # Check common directories in order of preference
    if [ -w "/usr/local/bin" ] && [ -z "$CI" ]; then
        INSTALL_DIR="/usr/local/bin"
    elif [ -d "$HOME/.local/bin" ]; then
        INSTALL_DIR="$HOME/.local/bin"
    else
        INSTALL_DIR="$HOME/.local/bin"
        mkdir -p "$INSTALL_DIR"
    fi

    info "Installing to: $INSTALL_DIR"
}

# Download cica binary
download_binary() {
    if [ "$CICA_VERSION" = "latest" ]; then
        DOWNLOAD_URL="$CICA_BASE_URL/latest/download/cica-$OS-$ARCH"
    else
        DOWNLOAD_URL="$CICA_BASE_URL/download/v$CICA_VERSION/cica-$OS-$ARCH"
    fi

    BINARY_NAME="cica"

    info "Downloading cica from $DOWNLOAD_URL"

    # Create temporary directory
    TEMP_DIR=$(mktemp -d)
    trap 'rm -rf "$TEMP_DIR"' EXIT

    # Download with curl or wget
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --progress-bar "$DOWNLOAD_URL" -o "$TEMP_DIR/$BINARY_NAME"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --show-progress "$DOWNLOAD_URL" -O "$TEMP_DIR/$BINARY_NAME"
    else
        error "Neither curl nor wget found. Please install one of them."
        exit 1
    fi

    # Make executable
    chmod +x "$TEMP_DIR/$BINARY_NAME"

    # Verify download
    if [ ! -f "$TEMP_DIR/$BINARY_NAME" ]; then
        error "Download failed"
        exit 1
    fi

    success "Downloaded successfully"
}

# Install the binary
install_binary() {
    info "Installing cica..."

    # Move to installation directory
    if [ -w "$INSTALL_DIR" ]; then
        mv "$TEMP_DIR/$BINARY_NAME" "$INSTALL_DIR/"
    else
        # Need sudo for system directories
        warning "Installation to $INSTALL_DIR requires sudo privileges"
        sudo mv "$TEMP_DIR/$BINARY_NAME" "$INSTALL_DIR/"
    fi

    success "cica installed successfully!"
}

# Setup PATH if needed
setup_path() {
    # Check if install directory is in PATH
    case ":$PATH:" in
        *":$INSTALL_DIR:"*)
            return
            ;;
    esac

    warning "$INSTALL_DIR is not in your PATH"

    # Detect shell and provide instructions
    SHELL_NAME=$(basename "$SHELL")
    case "$SHELL_NAME" in
        bash)
            PROFILE="$HOME/.bashrc"
            ;;
        zsh)
            PROFILE="$HOME/.zshrc"
            ;;
        fish)
            info "For fish shell, run:"
            printf "  ${GREEN}fish_add_path %s${RESET}\n" "\"$INSTALL_DIR\""
            return
            ;;
        *)
            PROFILE="$HOME/.profile"
            ;;
    esac

    info "Add this line to your $PROFILE:"
    printf "  ${GREEN}export PATH=\"%s:\$PATH\"${RESET}\n" "$INSTALL_DIR"
    info "Then restart your shell or run:"
    printf "  ${GREEN}source %s${RESET}\n" "$PROFILE"
}

# Main installation flow
main() {
    printf "${BLUE}%s${RESET}\n" "
   ┌─────────────────────────────────┐
   │  cica - agentic personal        │
   │         assistant               │
   │  https://github.com/oxideai/cica │
   └─────────────────────────────────┘
    "

    detect_platform

    if should_install; then
        get_install_dir
        download_binary
        install_binary
        setup_path

        echo
        success "Installation complete!"
        info "Run 'cica init' to get started"
    fi
}

# Run main function
main "$@"
