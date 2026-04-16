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
log_msg() {
    local level="$1"
    local msg="$2"
    printf "[%s] %-5s: %s\n" "$(date +'%Y-%m-%dT%H:%M:%S%z')" "$level" "$msg"
}

log()    { log_msg "INFO" "$1"; }
warn()   { log_msg "WARN" "$1"; }
error()  { log_msg "ERROR" "$1" >&2; exit 1; }

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

get_debian_codename() {
    # Pull codename directly from os-release to avoid dependency on lsb-release
    if [[ -f /etc/os-release ]]; then
        . /etc/os-release
        echo "$VERSION_CODENAME"
    else
        error "Could not determine Debian codename from /etc/os-release"
    fi
}

# --- Main Logic ---

ensure_pkg() {
    local pkg="$1"
    if ! is_pkg_installed "$pkg"; then
        log "Installing package: $pkg"
        apt-get install -y -qq "$pkg"
    else
        log "Package already present: $pkg"
    fi
}

main() {
    log "Starting Nomad bootstrap process..."

    # 1. Privilege Check & Self-Sudo
    if ! is_root; then
        log "Not running as root. Attempting to escalate via sudo..."
        exec sudo "$0" "$@"
    fi

    # 2. Bootstrap core dependencies
    # We install these first to ensure we can handle keys and repo management
    log "Checking core system dependencies..."
    apt-get update -qq
    for pkg in curl gnupg ca-certificates; do
        ensure_pkg "$pkg"
    done

    # 3. Setup HashiCorp GPG Keyring
    if [[ ! -f "$HASHICORP_KEYRING" ]]; then
        log "Adding HashiCorp GPG keyring..."
        # Ensure directory exists
        mkdir -p "$(dirname "$HASHICORP_KEYRING")"
        curl -fsSL https://apt.releases.hashicorp.com/gpg | gpg --dearmor -o "$HASHICORP_KEYRING"
        chmod 644 "$HASHICORP_KEYRING"
    else
        log "HashiCorp keyring already exists."
    fi

    # 4. Setup HashiCorp Repository
    if [[ ! -f "$HASHICORP_SOURCES" ]]; then
        local codename
        codename=$(get_debian_codename)
        log "Adding HashiCorp repository ($codename)..."
        echo "deb [signed-by=$HASHICORP_KEYRING] https://apt.releases.hashicorp.com $codename main" > "$HASHICORP_SOURCES"
    else
        log "HashiCorp repository source already exists."
    fi

    # 5. Install Nomad
    # We must update again to pick up the new HashiCorp repo
    log "Updating package lists with HashiCorp repository..."
    apt-get update -qq

    if [[ "$NOMAD_VERSION" == "latest" ]]; then
        log "Installing latest Nomad..."
        apt-get install -y -qq nomad
    else
        log "Installing Nomad version: $NOMAD_VERSION..."
        apt-get install -y -qq "nomad=$NOMAD_VERSION"
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
