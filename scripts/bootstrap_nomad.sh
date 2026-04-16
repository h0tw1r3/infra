#!/usr/bin/env bash

# --- Configuration ---
# You can override these by passing environment variables
NOMAD_VERSION="${NOMAD_VERSION:-latest}"
HASHICORP_KEYRING="/usr/share/keyrings/hashicorp-archive-keyring.gpg"
HASHICORP_SOURCES="/etc/apt/sources.list.d/hashicorp.list"

# --- Safety & Robustness ---
set -euo pipefail
IFS=$'\n\t'

# --- Logging Helpers ---
log() {
    printf "[%s] INFO: %s\n" "$(date +'%Y-%m-%dT%H:%M:%S%z')" "$1"
}

warn() {
    printf "[%s] WARN: %s\n" "$(date +'%Y-%m-%dT%H:%M:%S%z')" "$1"
}

error() {
    printf "[%s] ERROR: %s\n" "$(date +'%Y-%m-%dT%H:%M:%S%z')" "$1" >&2
    exit 1
}

# --- Verification Functions ---
is_root() {
    [[ $EUID -eq 0 ]]
}

is_pkg_installed() {
    dpkg -s "$1" >/dev/null 2>&1
}

is_cmd_available() {
    command -v "$1" >/dev/null 2>&1
}

# --- Main Logic ---

main() {
    log "Starting Nomad bootstrap process..."

    # 1. Privilege Check
    if ! is_root; then
        error "This script must be run as root (or via sudo)."
    fi

    # 2. Ensure fundamental system dependencies are present
    # We need curl for downloading and gnupg for handling keys
    for pkg in curl gnupg ca-certificates; do
        if ! is_pkg_installed "$pkg"; then
            log "Installing system dependency: $pkg"
            apt-get update -qq
            apt-get install -y -qq "$pkg"
        else
            log "System dependency present: $pkg"
        fi
    done

    # 3. Setup HashiCorp GPG Keyring
    if [[ ! -f "$HASHICORP_KEYRING" ]]; then
        log "Adding HashiCorp GPG keyring..."
        curl -fsSL https://apt.releases.hashicorp.com/gpg | gpg --dearmor -o "$HASHICORP_KEYRING"
        chmod 644 "$HASHICORP_KEYRING"
    else
        log "HashiCorp keyring already exists. Skipping."
    fi

    # 4. Setup HashiCorp Repository
    if [[ ! -f "$HASHICORP_SOURCES" ]]; then
        log "Adding HashiCorp repository to sources.list.d..."
        echo "deb [signed-by=$HASHICORP_KEYRING] https://apt.releases.hashicorp.com $(lsb_release -cs) main" >"$HASHICORP_SOURCES"
    else
        log "HashiCorp repository source already exists. Skipping."
    fi

    # 5. Install Nomad
    if ! is_cmd_available nomad; then
        log "Installing Nomad via apt..."
        apt-get update -qq
        apt-get install -y -qq nomad
    else
        log "Nomad is already installed. Skipping installation."
    fi

    # 6. Final Verification
    if is_cmd_available nomad; then
        local version
        version=$(nomad version | head -n 1)
        log "Bootstrap successful! Installed: $version"
    else
        error "Nomad installation failed verification."
    fi
}

main "$@"
