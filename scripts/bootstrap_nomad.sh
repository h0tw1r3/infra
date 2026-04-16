#!/usr/bin/env bash

# --- Configuration ---
# You can override these by passing environment variables
NOMAD_VERSION="${NOMAD_VERSION:-latest}"
HASHICORP_KEYRING="/usr/share/keyrings/hashicorp-archive-keyring.gpg"
HASHICORP_SOURCES_FILE="sources.list.d/hashicorp.list"

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

pkg_version() {
    dpkg-query -W -f='${source:Upstream-Version}\n' "$1"
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

ensure_pkg_need_apt_update=1
ensure_pkg() {
    local pkg="$1"
    local do_apt_update="${2:-$ensure_pkg_need_apt_update}"

    if ! is_pkg_installed "$pkg"; then
        if [[ "$do_apt_update" -eq 1 ]]; then
            log "Updating package lists before installing $pkg..."
            apt-get update -qq
            ensure_pkg_need_apt_update=0
        fi
        log "Installing package: $pkg"
        apt-get install -y -qq "$pkg"
    fi
}

main() {
    # 1. Privilege Check & Self-Sudo
    if ! is_root; then
        log "Not running as root. Attempting to escalate via sudo..."
        exec sudo -E "$0" "$@"
    fi

    log "Starting Nomad bootstrap process..."

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
    fi

    # 4. Setup HashiCorp Repository
    if [[ ! -f "$HASHICORP_SOURCES_FILE" ]]; then
        local codename
        codename=$(get_debian_codename)
        log "Adding HashiCorp repository ($codename)..."
        echo "deb [signed-by=$HASHICORP_KEYRING] https://apt.releases.hashicorp.com $codename main" > "/etc/apt/$HASHICORP_SOURCES_FILE"
    fi

    # 5. Install Nomad
    # We must update again to pick up the new HashiCorp repo
    log "Updating package lists with HashiCorp repository..."
    apt-get update -qq -o Dir::Etc::sourcelist="$HASHICORP_SOURCES_FILE" -o Dir::Etc::sourceparts="-" -o APT::Get::List-Cleanup="0"

    local NOMAD_INSTALL
    NOMAD_INSTALL=0
    if [ "$NOMAD_VERSION" = "latest" ]; then
	if apt list --upgradable nomad 2>&1 | grep 'upgradable from' ; then
	    NOMAD_INSTALL=1
	fi
    else
	if [ "$NOMAD_VERSION" != "$(pkg_version nomad)" ] ; then
            NOMAD_INSTALL=1
	fi
    fi
    if [ "$NOMAD_INSTALL" = 1 ] ; then
        log "Installing Nomad version: $NOMAD_VERSION..."
        apt-get install -y -qq "nomad=${NOMAD_VERSION##latest}*"
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
