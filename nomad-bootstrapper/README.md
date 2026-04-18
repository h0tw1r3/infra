# Nomad Bootstrapper

`nomad-bootstrapper` is a controller-led Rust CLI that bootstraps HashiCorp Nomad on **Debian** hosts over **session-managed SSH**.

Instead of installing the tool on every node, you run it once from your control machine with a declarative inventory. The controller opens retained SSH sessions to the managed hosts, requires every host to pass a strict preflight gate before provisioning begins, and then applies the provisioning phases with bounded cross-host concurrency while keeping phase order sequential within each host.

## Features

- **Remote-first**: runs from your laptop, CI runner, or admin box over SSH
- **Declarative inventory**: cluster topology, Nomad role, and SSH settings live in TOML
- **Strict fleet preflight**: connectivity, Debian compatibility, and provisioning capability are validated before any mutating phase starts
- **Configurable privilege path**: non-root SSH users can run privileged operations through a configured escalation command such as `sudo -n` or `doas`
- **Hybrid idempotency**: live remote probes are authoritative; an optional node-local state file is advisory only
- **Retained SSH sessions**: preflight-established sessions are reused for provisioning, while still honoring global and per-node SSH overrides
- **Debian-focused**: supports Debian hosts in v1, with a transport/backend split that keeps future host support straightforward
- **Phase-based converge flow**: ensure-deps -> setup-repo -> install -> configure -> verify

## Requirements

- Rust 1.70+
- `ssh` available on the control machine
- SSH access to each target host
- Debian on every managed node
- Either SSH access as `root`, or a non-root SSH user with a non-interactive privilege escalation command (for example, `sudo -n`)

## Build and Test

```bash
cargo build --release
cargo test
cargo clippy -- -D warnings
```

For the containerized smoke test, which builds the Linux binary in an isolated target directory, runs a full multi-host converge over SSH, and verifies the Nomad cluster comes up healthy:

```bash
./scripts/run_debian_integration.sh
```

## Inventory Format

The controller reads a TOML inventory file.

```toml
[cluster]
datacenter = "homelab"

[defaults]
nomad_version = "latest"
high_latency = true

[controller]
concurrency = 3

[ssh]
user = "admin"
identity_file = "~/.ssh/id_ed25519"
options = ["StrictHostKeyChecking=accept-new"]
privilege_escalation = ["sudo", "-n"]

[[nodes]]
name = "server-1"
host = "server-1.example.com"
role = "server"
bootstrap_expect = 3
server_join_address = ["10.0.1.2:4648", "10.0.1.3:4648"]
bind_addr = "10.0.1.10"
advertise = "10.0.1.20"

[[nodes]]
name = "client-1"
host = "client-1.example.com"
role = "client"
server_address = ["10.0.1.1:4647", "10.0.1.2:4647"]
bind_addr = "0.0.0.0"

[nodes.advertise]
http = "10.0.2.20"
rpc = "10.0.2.21"
serf = "10.0.2.22:4648"

[nodes.ssh]
user = "root"
privilege_escalation = []
```

### Inventory Rules

- `[[nodes]]` must contain at least one host
- `role = "server"` requires `bootstrap_expect`
- `role = "client"` requires at least one `server_address`
- `bind_addr` is optional per node and is written directly into the generated Nomad agent config as a non-empty passthrough string
- `advertise` is optional per node and supports either a scalar `advertise = "..."` value or a `[nodes.advertise]` table with `http`, `rpc`, and/or `serf`
- `bind_addr` and `advertise` values are treated uniformly as non-empty passthrough strings; they may be literal addresses, hostnames, or Nomad/go-sockaddr template expressions, and the bootstrapper does not resolve or semantically validate them
- The generated config always includes an `advertise` block; each effective advertise value resolves in order from the per-protocol override, to the shared `advertise` value, to an explicit non-`0.0.0.0` `bind_addr`, and finally to Nomad's default-interface IP template
- The scalar and table forms are mutually exclusive in TOML, but a shared scalar advertise value can still combine with per-protocol fallbacks in the resolved config
- `[controller].concurrency` is optional, defaults to `3`, must be greater than `0`, and is capped by the number of managed hosts
- SSH settings resolve as:
  1. your existing SSH agent/config when no override is provided
  2. global `[ssh]` defaults
  3. per-node `[nodes.ssh]` overrides
- `privilege_escalation` is an optional argv-style command list used only for non-root SSH users. Per-node overrides replace the global value, `[]` disables an inherited value, and remote root sessions bypass escalation.

### Network Value Notes

- Falling back from `advertise` to `bind_addr` is a convenience heuristic, not a general networking recommendation.
- Multi-homed, NAT, or split-network deployments should set explicit advertise values instead of relying on `bind_addr` fallback.

## Usage

```bash
# Full converge run
./target/release/nomad-bootstrapper --inventory ./inventory.toml

# Run only one phase for every host
./target/release/nomad-bootstrapper \
  --inventory ./inventory.toml \
  --phase configure

# Run only the fleet-wide preflight gate
./target/release/nomad-bootstrapper \
  --inventory ./inventory.toml \
  --preflight-only

# Run through verify in dry-run mode
./target/release/nomad-bootstrapper \
  --inventory ./inventory.toml \
  --dry-run

# Override inventory concurrency at runtime
./target/release/nomad-bootstrapper \
  --inventory ./inventory.toml \
  --concurrency 2
```

### CLI Options

```text
USAGE:
    nomad-bootstrapper --inventory <PATH> [OPTIONS]

OPTIONS:
    --inventory <PATH>
        Path to the inventory TOML file

    --phase <PHASE>
        Run only this phase: ensure-deps, setup-repo, install, configure, verify

    --up-to <PHASE>
        Run every phase up to and including the named phase

    --preflight-only
        Run only the fleet-wide preflight gate and skip provisioning

    --dry-run
        Show what would be executed without changing remote hosts

    --concurrency <COUNT>
        Override the inventory controller concurrency with a positive value

    --log-level <LEVEL>
        debug, info, warn, error
```

## Architecture

### Controller Flow

```text
inventory.toml
  -> resolve node + SSH settings
  -> open retained ssh sessions
  -> preflight every host (connectivity, Debian, privileges)
  -> if all pass:
       -> run ensure-deps/setup-repo/install/configure/verify
          with bounded cross-host concurrency
          and sequential per-host phase order
  -> close retained ssh sessions
```

### Failure Policy

Version 1 uses a **strict preflight gate** and **bounded host concurrency**.

- If any host fails preflight, the run aborts before provisioning starts
- If a retained session dies after preflight but before that host begins provisioning, the run aborts with a gate invalidation
- If one host fails during provisioning, no new hosts start
- Hosts already running a phase finish that current phase only; remaining phases, including `verify`, are skipped
- The final run summary is always printed, even when info logs are disabled
- Per-host outcomes include the observed state progression through preflight and provisioning

### Idempotency Model

The controller does not trust the node-local state file as the source of truth.

- **Authoritative**: live probes over SSH (`dpkg`, `/etc/os-release`, current Nomad config, etc.)
- **Advisory**: `/etc/nomad.d/.provisioned.toml` for last converge metadata

If the advisory state file is missing, stale, unreadable, unwritable, or contradictory, the controller logs and continues using live probe results.

## Debian-Only Scope

This rewrite supports **Debian** only. Ubuntu and other Debian-like systems are intentionally out of scope for v1.

The code splits SSH transport from Debian-specific host behavior so additional backends can be added later without rewriting the controller.
