# Nomad Bootstrapper

`nomad-bootstrapper` is a controller-led Rust CLI that bootstraps HashiCorp Nomad on **Debian** hosts over **raw SSH**.

Instead of installing the tool on every node, you run it once from your control machine with a declarative inventory. The controller connects to each host serially, applies the five provisioning phases, and stops on the first host failure so reruns are explicit and predictable.

## Features

- **Remote-first**: runs from your laptop, CI runner, or admin box over SSH
- **Declarative inventory**: cluster topology, Nomad role, and SSH settings live in TOML
- **Hybrid idempotency**: live remote probes are authoritative; an optional node-local state file is advisory only
- **SSH-friendly defaults**: uses your existing SSH config and agent by default, with global and per-node overrides
- **Debian-focused**: supports Debian hosts in v1, with a transport/backend split that keeps future host support straightforward
- **Phase-based converge flow**: ensure-deps -> setup-repo -> install -> configure -> verify

## Requirements

- Rust 1.70+
- `ssh` available on the control machine
- SSH access to each target host
- Debian on every managed node
- Remote privilege escalation already handled by the SSH account you use (for example, logging in as `root`)

## Build and Test

```bash
cargo build --release
cargo test
cargo clippy -- -D warnings
```

For the containerized smoke test:

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
user = "root"
identity_file = "~/.ssh/id_ed25519"
options = ["StrictHostKeyChecking=accept-new"]

[[nodes]]
name = "server-1"
host = "server-1.example.com"
role = "server"
bootstrap_expect = 3
server_join_address = ["10.0.1.2:4648", "10.0.1.3:4648"]

[[nodes]]
name = "client-1"
host = "client-1.example.com"
role = "client"
server_address = ["10.0.1.1:4647", "10.0.1.2:4647"]

[nodes.ssh]
user = "admin"
```

### Inventory Rules

- `[[nodes]]` must contain at least one host
- `role = "server"` requires `bootstrap_expect`
- `role = "client"` requires at least one `server_address`
- `[controller].concurrency` is optional, defaults to `3`, must be greater than `0`, and is capped by the number of managed hosts
- SSH settings resolve as:
  1. your existing SSH agent/config when no override is provided
  2. global `[ssh]` defaults
  3. per-node `[nodes.ssh]` overrides

## Usage

```bash
# Full converge run
./target/release/nomad-bootstrapper --inventory ./inventory.toml

# Run only one phase for every host
./target/release/nomad-bootstrapper \
  --inventory ./inventory.toml \
  --phase configure

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
  -> connect to host over ssh
  -> ensure-deps
  -> setup-repo
  -> install
  -> configure
  -> verify
  -> write advisory node-local state file
```

### Failure Policy

Version 1 runs hosts **serially** and **stops on the first host failure**.

- The failing host and phase are reported explicitly
- Partial host changes are preserved
- Retry is a normal rerun after fixing the underlying issue

### Idempotency Model

The controller does not trust the node-local state file as the source of truth.

- **Authoritative**: live probes over SSH (`dpkg`, `/etc/os-release`, current Nomad config, etc.)
- **Advisory**: `/etc/nomad.d/.provisioned.toml` for last converge metadata

If the advisory state file is missing, stale, unreadable, unwritable, or contradictory, the controller logs and continues using live probe results.

## Debian-Only Scope

This rewrite supports **Debian** only. Ubuntu and other Debian-like systems are intentionally out of scope for v1.

The code splits SSH transport from Debian-specific host behavior so additional backends can be added later without rewriting the controller.
