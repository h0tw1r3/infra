#!/usr/bin/env bash

set -euo pipefail
IFS=$'\n\t'

IMAGE="${IMAGE:-rust:1-trixie}"
WORKDIR="/workspace"

log() {
    printf '[integration] %s\n' "$1"
}

if ! command -v docker >/dev/null 2>&1; then
    printf 'docker is required but not installed\n' >&2
    exit 1
fi

log "Running Debian smoke suite in container image: ${IMAGE}"

docker run --rm \
    -v "$(pwd)":"${WORKDIR}" \
    -w "${WORKDIR}" \
    "${IMAGE}" \
    bash -lc '
set -euo pipefail
IFS=$"\n\t"

log() {
    printf "[container] %s\n" "$1"
}

log "Installing container prerequisites"
apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl gnupg openssh-client

export PATH="/usr/local/cargo/bin:${PATH}"
if ! command -v cargo >/dev/null 2>&1; then
    printf "cargo is required but not found in container PATH\n" >&2
    exit 1
fi

log "Running unit and integration tests"
cargo test

log "Running clippy"
cargo clippy -- -D warnings

cat > /tmp/inventory.toml <<EOF
[[nodes]]
name = "server-1"
host = "server-1.example.com"
role = "server"
bootstrap_expect = 1
nomad_version = "latest"
EOF

log "Running dry-run controller smoke test"
./target/debug/nomad-bootstrapper --inventory /tmp/inventory.toml --dry-run --log-level info

log "Debian smoke suite passed"
'

log "Integration suite completed successfully"
