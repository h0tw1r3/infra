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

log "Running Debian integration suite in container image: ${IMAGE}"

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

log "Installing host prerequisites for integration run"
apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl gnupg

# Official rust images may place cargo under /usr/local/cargo/bin without exporting it in login shells.
export PATH="/usr/local/cargo/bin:${PATH}"
if ! command -v cargo >/dev/null 2>&1; then
    printf "cargo is required but not found in container PATH\n" >&2
    exit 1
fi

log "Building release binary"
cargo build --release

log "Verifying role-less phase execution works (--phase ensure-deps)"
./target/release/nomad-bootstrapper --phase ensure-deps --log-level info

log "Running full converge flow up to configure (server mode)"
./target/release/nomad-bootstrapper \
  --nomad-version latest \
  --role server \
  --bootstrap-expect 1 \
  --high-latency \
  --up-to configure \
  --log-level info

if [[ ! -f /etc/nomad.d/nomad.hcl ]]; then
    printf "nomad configuration was not created\n" >&2
    exit 1
fi

if ! grep -q "raft_multiplier = 5" /etc/nomad.d/nomad.hcl; then
    printf "expected high-latency config (raft_multiplier = 5) not found\n" >&2
    exit 1
fi

first_hash=$(sha256sum /etc/nomad.d/nomad.hcl | awk "{print \$1}")

log "Running converge flow a second time for idempotency"
./target/release/nomad-bootstrapper \
  --nomad-version latest \
  --role server \
  --bootstrap-expect 1 \
  --high-latency \
  --up-to configure \
  --log-level info

second_hash=$(sha256sum /etc/nomad.d/nomad.hcl | awk "{print \$1}")

if [[ "${first_hash}" != "${second_hash}" ]]; then
    printf "configuration changed between identical runs (idempotency failure)\n" >&2
    exit 1
fi

log "Asserting configure phase rejects missing role context"
set +e
./target/release/nomad-bootstrapper --phase configure >/tmp/configure_no_role.out 2>&1
rc=$?
set -e
if [[ $rc -eq 0 ]]; then
    printf "configure phase unexpectedly succeeded without role\n" >&2
    exit 1
fi
if ! grep -q -- "--role must be specified" /tmp/configure_no_role.out; then
    printf "expected validation message not found for configure-without-role\n" >&2
    cat /tmp/configure_no_role.out >&2
    exit 1
fi

log "Running client mode provisioning with server addresses"
./target/release/nomad-bootstrapper \
  --nomad-version latest \
  --role client \
  --server-address 10.0.1.1:4647 \
  --server-address 10.0.1.2:4647 \
  --up-to configure \
  --log-level info

if ! grep -q "client {" /etc/nomad.d/nomad.hcl; then
    printf "client block not found in configuration\n" >&2
    exit 1
fi

if ! grep -q "servers = " /etc/nomad.d/nomad.hcl; then
    printf "servers list not found in configuration\n" >&2
    exit 1
fi

log "Verifying client mode configuration idempotency"
client_first=$(sha256sum /etc/nomad.d/nomad.hcl | awk "{print \$1}")
./target/release/nomad-bootstrapper \
  --nomad-version latest \
  --role client \
  --server-address 10.0.1.1:4647 \
  --server-address 10.0.1.2:4647 \
  --up-to configure \
  --log-level info
client_second=$(sha256sum /etc/nomad.d/nomad.hcl | awk "{print \$1}")

if [[ "${client_first}" != "${client_second}" ]]; then
    printf "client configuration changed between identical runs\n" >&2
    exit 1
fi

log "Debian integration checks passed"
'

log "Integration suite completed successfully"
