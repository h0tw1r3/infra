#!/usr/bin/env bash

set -euo pipefail
IFS=$'\n\t'

BUILD_IMAGE="${BUILD_IMAGE:-rust:1-trixie}"
NODE_BASE_IMAGE="${NODE_BASE_IMAGE:-debian:trixie}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "${REPO_ROOT}/.tmp"
SMOKE_ROOT="$(mktemp -d "${REPO_ROOT}/.tmp/nomad-smoke.XXXXXX")"
SSH_KEY_PATH="${SMOKE_ROOT}/smoke_id"
INVENTORY_PATH="${SMOKE_ROOT}/inventory.toml"
NODE_IMAGE_TAG="nomad-smoke-node:$(date +%s)-$$"
NETWORK_NAME="nomad-smoke-net-$$"
SERVER_ONE_CONTAINER="nomad-smoke-server-1-$$"
SERVER_TWO_CONTAINER="nomad-smoke-server-2-$$"
SERVER_ONE_NAME="server-1"
SERVER_TWO_NAME="server-2"
DATACENTER_NAME="smoke"

log() {
    printf '[integration] %s\n' "$1"
}

cleanup() {
    if docker ps -a --format '{{.Names}}' | grep -Fxq "${SERVER_ONE_CONTAINER}"; then
        docker rm -f "${SERVER_ONE_CONTAINER}" >/dev/null 2>&1 || true
    fi
    if docker ps -a --format '{{.Names}}' | grep -Fxq "${SERVER_TWO_CONTAINER}"; then
        docker rm -f "${SERVER_TWO_CONTAINER}" >/dev/null 2>&1 || true
    fi
    if docker network inspect "${NETWORK_NAME}" >/dev/null 2>&1; then
        docker network rm "${NETWORK_NAME}" >/dev/null 2>&1 || true
    fi
    if docker image inspect "${NODE_IMAGE_TAG}" >/dev/null 2>&1; then
        docker image rm -f "${NODE_IMAGE_TAG}" >/dev/null 2>&1 || true
    fi
    rm -rf "${SMOKE_ROOT}"
}

trap cleanup EXIT

if ! command -v docker >/dev/null 2>&1; then
    printf 'docker is required but not installed\n' >&2
    exit 1
fi

if ! command -v ssh-keygen >/dev/null 2>&1; then
    printf 'ssh-keygen is required but not installed\n' >&2
    exit 1
fi

log "Creating isolated smoke-test workspace at ${SMOKE_ROOT}"
ssh-keygen -q -t ed25519 -N '' -f "${SSH_KEY_PATH}" >/dev/null

cat > "${SMOKE_ROOT}/systemctl" <<'EOF'
#!/usr/bin/env bash

set -euo pipefail
IFS=$'\n\t'

COMMAND="${1:-}"
UNIT="${2:-}"
PID_FILE="/var/run/nomad-smoke/nomad.pid"
LOG_DIR="/var/log/nomad-smoke"
LOG_FILE="${LOG_DIR}/nomad.log"

ensure_supported_unit() {
    case "${UNIT}" in
        nomad|nomad.service) ;;
        *)
            printf 'smoke systemctl only supports the nomad service, got %s\n' "${UNIT:-<empty>}" >&2
            exit 1
            ;;
    esac
}

running_pid() {
    if [[ ! -f "${PID_FILE}" ]]; then
        return 1
    fi

    local pid
    pid="$(cat "${PID_FILE}")"
    if [[ -z "${pid}" ]]; then
        return 1
    fi

    if kill -0 "${pid}" 2>/dev/null; then
        printf '%s\n' "${pid}"
        return 0
    fi

    rm -f "${PID_FILE}"
    return 1
}

start_nomad() {
    mkdir -p /var/run/nomad-smoke "${LOG_DIR}" /opt/nomad

    if running_pid >/dev/null; then
        return 0
    fi

    if ! command -v nomad >/dev/null 2>&1; then
        printf 'nomad binary is not installed\n' >&2
        exit 1
    fi

    nohup nomad agent -config=/etc/nomad.d >"${LOG_FILE}" 2>&1 &
    local pid=$!
    printf '%s\n' "${pid}" > "${PID_FILE}"

    for _ in $(seq 1 30); do
        if ! kill -0 "${pid}" 2>/dev/null; then
            printf 'nomad exited during startup\n' >&2
            cat "${LOG_FILE}" >&2 || true
            rm -f "${PID_FILE}"
            exit 1
        fi

        if curl -fsS http://127.0.0.1:4646/v1/agent/self >/dev/null 2>&1; then
            return 0
        fi

        sleep 1
    done

    if ! kill -0 "${pid}" 2>/dev/null; then
        printf 'nomad exited during startup\n' >&2
        cat "${LOG_FILE}" >&2 || true
        rm -f "${PID_FILE}"
        exit 1
    fi
}

stop_nomad() {
    local pid
    if ! pid="$(running_pid)"; then
        return 0
    fi

    kill "${pid}" 2>/dev/null || true
    for _ in $(seq 1 30); do
        if ! kill -0 "${pid}" 2>/dev/null; then
            rm -f "${PID_FILE}"
            return 0
        fi
        sleep 1
    done

    kill -9 "${pid}" 2>/dev/null || true
    rm -f "${PID_FILE}"
}

print_status() {
    if running_pid >/dev/null; then
        printf 'nomad.service - Nomad smoke service\n'
        printf '   Active: active (running)\n'
        return 0
    fi

    printf 'nomad.service - Nomad smoke service\n'
    printf '   Active: inactive (dead)\n'
    return 3
}

case "${COMMAND}" in
    start)
        ensure_supported_unit
        start_nomad
        ;;
    stop)
        ensure_supported_unit
        stop_nomad
        ;;
    restart)
        ensure_supported_unit
        stop_nomad
        start_nomad
        ;;
    status)
        ensure_supported_unit
        print_status
        ;;
    is-active)
        ensure_supported_unit
        if running_pid >/dev/null; then
            printf 'active\n'
            exit 0
        fi
        printf 'inactive\n'
        exit 3
        ;;
    daemon-reload)
        exit 0
        ;;
    *)
        printf 'unsupported smoke systemctl command: %s\n' "${COMMAND:-<empty>}" >&2
        exit 1
        ;;
esac
EOF

cat > "${SMOKE_ROOT}/Dockerfile" <<EOF
FROM ${NODE_BASE_IMAGE}

RUN apt-get update -qq \\
    && apt-get install -y -qq --no-install-recommends \\
        ca-certificates \\
        curl \\
        gnupg \\
        iproute2 \\
        openssh-client \\
        openssh-server \\
        sudo \\
    && rm -rf /var/lib/apt/lists/* \\
    && useradd --create-home --shell /bin/bash admin \\
    && echo 'admin ALL=(ALL) NOPASSWD:ALL' >/etc/sudoers.d/admin \\
    && chmod 440 /etc/sudoers.d/admin \\
    && mkdir -p /run/sshd /home/admin/.ssh \\
    && chmod 700 /home/admin/.ssh

COPY smoke_id.pub /home/admin/.ssh/authorized_keys
COPY systemctl /usr/local/bin/systemctl

RUN chown -R admin:admin /home/admin/.ssh \\
    && chmod 600 /home/admin/.ssh/authorized_keys \\
    && chmod 755 /usr/local/bin/systemctl \\
    && printf '%s\n' \\
        'PasswordAuthentication no' \\
        'KbdInteractiveAuthentication no' \\
        'PermitRootLogin no' \\
        'PubkeyAuthentication yes' \\
        > /etc/ssh/sshd_config.d/smoke.conf

CMD ["/usr/sbin/sshd", "-D", "-e"]
EOF

log "Building Linux binary in isolated target directory"
docker run --rm \
    -v "${REPO_ROOT}:/workspace" \
    -v "${SMOKE_ROOT}:/smoke-out" \
    -w /workspace \
    "${BUILD_IMAGE}" \
    bash -lc '
set -euo pipefail
IFS=$"\n\t"

apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl gnupg openssh-client

export PATH="/usr/local/cargo/bin:${PATH}"
export CARGO_TARGET_DIR=/tmp/cargo-target

rustup component add clippy
cargo test
cargo clippy -- -D warnings
cargo build --release

built_binary="$(find "${CARGO_TARGET_DIR}" -type f -path "*/release/nomad-bootstrapper" | head -n 1)"
if [[ -z "${built_binary}" ]]; then
    printf "could not locate built nomad-bootstrapper under %s\n" "${CARGO_TARGET_DIR}" >&2
    exit 1
fi

cp "${built_binary}" /smoke-out/nomad-bootstrapper
'

if [[ ! -f "${SMOKE_ROOT}/nomad-bootstrapper" ]]; then
    printf 'could not export the built nomad-bootstrapper binary to %s\n' "${SMOKE_ROOT}" >&2
    exit 1
fi

chmod 755 "${SMOKE_ROOT}/nomad-bootstrapper"

log "Building Debian SSH test node image: ${NODE_IMAGE_TAG}"
docker build -q -t "${NODE_IMAGE_TAG}" "${SMOKE_ROOT}" >/dev/null

log "Creating Docker network ${NETWORK_NAME}"
docker network create "${NETWORK_NAME}" >/dev/null

log "Starting smoke-test target containers"
docker run -d --name "${SERVER_ONE_CONTAINER}" \
    --privileged \
    --hostname "${SERVER_ONE_NAME}" \
    --network "${NETWORK_NAME}" \
    --network-alias "${SERVER_ONE_NAME}" \
    "${NODE_IMAGE_TAG}" >/dev/null

docker run -d --name "${SERVER_TWO_CONTAINER}" \
    --privileged \
    --hostname "${SERVER_TWO_NAME}" \
    --network "${NETWORK_NAME}" \
    --network-alias "${SERVER_TWO_NAME}" \
    "${NODE_IMAGE_TAG}" >/dev/null

wait_for_ssh() {
    local host="$1"
    log "Waiting for SSH on ${host}"
    docker run --rm \
        --network "${NETWORK_NAME}" \
        -v "${SSH_KEY_PATH}:/tmp/smoke_id:ro" \
        "${NODE_IMAGE_TAG}" \
        bash -lc "
set -euo pipefail
for attempt in \$(seq 1 30); do
    if ssh -i /tmp/smoke_id \
        -o BatchMode=yes \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        admin@${host} true >/dev/null 2>&1; then
        exit 0
    fi
    sleep 1
done
printf 'SSH did not become ready for %s\n' '${host}' >&2
exit 1
"
}

wait_for_ssh "${SERVER_ONE_NAME}"
wait_for_ssh "${SERVER_TWO_NAME}"

cat > "${INVENTORY_PATH}" <<EOF
[cluster]
datacenter = "${DATACENTER_NAME}"

[controller]
concurrency = 2

[ssh]
user = "admin"
identity_file = "/smoke/smoke_id"
options = ["StrictHostKeyChecking=no", "UserKnownHostsFile=/dev/null", "BatchMode=yes"]
privilege_escalation = ["sudo", "-n"]

[[nodes]]
name = "${SERVER_ONE_NAME}"
host = "${SERVER_ONE_NAME}"
role = "server"
bootstrap_expect = 2
server_join_address = ["${SERVER_TWO_NAME}:4648"]

[[nodes]]
name = "${SERVER_TWO_NAME}"
host = "${SERVER_TWO_NAME}"
role = "server"
bootstrap_expect = 2
server_join_address = ["${SERVER_ONE_NAME}:4648"]
EOF

log "Running multi-host preflight smoke test"
docker run --rm \
    --network "${NETWORK_NAME}" \
    -v "${SMOKE_ROOT}:/smoke:ro" \
    "${NODE_IMAGE_TAG}" \
    /smoke/nomad-bootstrapper --inventory /smoke/inventory.toml --preflight-only --log-level info

log "Running full multi-host converge smoke test"
docker run --rm \
    --network "${NETWORK_NAME}" \
    -v "${SMOKE_ROOT}:/smoke:ro" \
    "${NODE_IMAGE_TAG}" \
    /smoke/nomad-bootstrapper --inventory /smoke/inventory.toml --log-level info

verify_host_state() {
    local container="$1"
    local node_name="$2"
    log "Verifying converged phase state on ${container}"
    docker exec "${container}" dpkg -s ca-certificates curl gnupg >/dev/null
    docker exec "${container}" test -f /usr/share/keyrings/hashicorp-archive-keyring.gpg
    docker exec "${container}" grep -Fq 'https://apt.releases.hashicorp.com' /etc/apt/sources.list.d/hashicorp.list
    docker exec "${container}" bash -lc 'command -v nomad >/dev/null'
    docker exec "${container}" test -f /etc/nomad.d/nomad.hcl
    docker exec "${container}" grep -Fq "name = \"${node_name}\"" /etc/nomad.d/nomad.hcl
    docker exec "${container}" grep -Fq "datacenter = \"${DATACENTER_NAME}\"" /etc/nomad.d/nomad.hcl
    docker exec "${container}" grep -Fq 'server {' /etc/nomad.d/nomad.hcl
    docker exec "${container}" grep -Fq 'bootstrap_expect = 2' /etc/nomad.d/nomad.hcl
}

wait_for_nomad_api() {
    local container="$1"
    log "Waiting for Nomad HTTP API on ${container}"
    docker exec "${container}" bash -lc '
set -euo pipefail
for attempt in $(seq 1 30); do
    if curl -fsS http://127.0.0.1:4646/v1/agent/self >/dev/null 2>&1; then
        exit 0
    fi
    sleep 1
done
printf "Nomad HTTP API did not become ready\n" >&2
exit 1
'
}

wait_for_cluster() {
    log "Waiting for Nomad server leadership on ${SERVER_ONE_CONTAINER}"
    docker exec "${SERVER_ONE_CONTAINER}" bash -lc '
set -euo pipefail
for attempt in $(seq 1 60); do
    leader="$(curl -fsS http://127.0.0.1:4646/v1/status/leader | tr -d "\"")"
    if [[ -n "${leader}" ]]; then
        exit 0
    fi
    sleep 1
done
printf "Nomad leader was not elected on the server\n" >&2
curl -fsS http://127.0.0.1:4646/v1/status/leader >&2 || true
exit 1
'

    log "Waiting for server membership output on ${SERVER_ONE_CONTAINER}"
    docker exec "${SERVER_ONE_CONTAINER}" bash -lc '
set -euo pipefail
for attempt in $(seq 1 60); do
    membership="$(nomad server members 2>/dev/null || true)"
    if grep -Fq "'"${SERVER_ONE_NAME}"'" <<<"${membership}" && grep -Fq "'"${SERVER_TWO_NAME}"'" <<<"${membership}"; then
        exit 0
    fi
    sleep 1
done
printf "Nomad server membership did not report both '"${SERVER_ONE_NAME}"' and '"${SERVER_TWO_NAME}"'\n" >&2
nomad server members >&2 || true
exit 1
'

    log "Waiting for secondary server API leadership visibility on ${SERVER_TWO_CONTAINER}"
    docker exec "${SERVER_TWO_CONTAINER}" bash -lc '
set -euo pipefail
for attempt in $(seq 1 60); do
    leader="$(curl -fsS http://127.0.0.1:4646/v1/status/leader | tr -d "\"")"
    if [[ -n "${leader}" ]]; then
        exit 0
    fi
    sleep 1
done
printf "Nomad secondary server did not observe cluster leadership\n" >&2
curl -fsS http://127.0.0.1:4646/v1/status/leader >&2 || true
exit 1
'
}

verify_host_state "${SERVER_ONE_CONTAINER}" "${SERVER_ONE_NAME}"
verify_host_state "${SERVER_TWO_CONTAINER}" "${SERVER_TWO_NAME}"
docker exec "${SERVER_ONE_CONTAINER}" /usr/local/bin/systemctl is-active nomad >/dev/null
docker exec "${SERVER_TWO_CONTAINER}" /usr/local/bin/systemctl is-active nomad >/dev/null
wait_for_nomad_api "${SERVER_ONE_CONTAINER}"
wait_for_nomad_api "${SERVER_TWO_CONTAINER}"
wait_for_cluster

log "Debian full-cluster smoke suite passed"
