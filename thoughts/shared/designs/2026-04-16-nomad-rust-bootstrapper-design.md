# Nomad Rust Bootstrapper Design

date: 2026-04-16
topic: "Nomad Rust Bootstrapper"
status: draft

## Problem Statement

The original direction assumed an on-node Rust binary that replaced a Bash bootstrap script. The project has now moved to a **controller-led remote provisioner** instead: operators should run one command from a control machine and let the tool converge Debian Nomad hosts over SSH.

## Scope

- **Supported platform:** Debian only
- **Execution model:** raw SSH orchestration from the controller
- **Inventory model:** declarative TOML file
- **Host execution policy:** serial in v1, stop on first host failure
- **Idempotency model:** live remote probes are authoritative; a node-local state file is optional and advisory

## Constraints

- Must reuse the operator's existing SSH config and agent by default
- Must allow global SSH defaults plus per-node SSH overrides
- Must not install a permanent helper agent on remote hosts
- Must keep the current five converge phases
- Must be straightforward to extend to additional host backends later

## Chosen Approach

Implement a Rust controller that:

1. Parses an inventory file
2. Resolves each host's SSH settings
3. Connects to each host over `ssh`
4. Runs the Nomad converge phases in order
5. Stops at the first host failure and surfaces the failing host + phase clearly

The tool uses `std::process::Command` to drive the local `ssh` binary rather than embedding an SSH implementation. That keeps default SSH agent/config behavior intact and minimizes auth-specific code.

## Architecture

### Top-Level Modules

- `config.rs`: CLI args, inventory parsing, SSH override resolution, node validation
- `controller.rs`: serial host orchestration and stop-on-error behavior
- `transport.rs`: raw SSH transport, remote command execution, remote file helpers
- `debian.rs`: Debian-specific probes and mutations
- `modules/`: converge phases (`ensure-deps`, `setup-repo`, `install`, `configure`, `verify`)
- `state.rs`: advisory node-local converge metadata

### Core Separation

```text
Controller
  -> Transport (ssh command execution)
  -> Debian backend (probe/mutate Debian hosts)
  -> Provisioning phases
```

This separation is the main extensibility hook:

- new SSH/auth behavior belongs in the transport layer
- new operating system support belongs in a new host backend
- phase logic stays focused on Nomad converge behavior

## Inventory Design

The inventory is TOML and contains:

- cluster-level defaults such as datacenter
- global SSH defaults
- per-node definitions
- per-node SSH overrides

Each node resolves to:

- SSH target information (`host`, optional `user`, optional `identity_file`, optional `port`, extra `-o` options)
- Nomad converge information (`role`, `nomad_version`, `bootstrap_expect`, join addresses, latency profile)

## Phase Behavior

The controller preserves the existing phase order:

1. **ensure-deps** - install required Debian packages
2. **setup-repo** - add the HashiCorp apt key and repository
3. **install** - install or update the Nomad package
4. **configure** - render and atomically replace Nomad config
5. **verify** - restart when needed and confirm Nomad version

## Remote File Semantics

Remote writes are done without staging a permanent helper artifact.

- small files are streamed over SSH stdin
- config writes use a remote temp file + atomic move
- config validation happens on the remote temp file before replacement

## Failure Policy

V1 behavior is explicit:

- process hosts one at a time
- stop immediately on the first failure
- do not roll back partial host changes
- rely on reruns for retry after the operator fixes the cause

This keeps the first release predictable and avoids inventing fragile rollback logic.

## Idempotency Model

The node-local state file is **not** the source of truth.

- live probes decide whether work is needed
- the advisory state file records last converge metadata when available
- missing, stale, unreadable, unwritable, or contradictory state is ignored with a warning

This makes reruns resilient even when the advisory file is absent or damaged.

## Testing Strategy

- **Unit tests:** inventory parsing, SSH override resolution, Debian parsing helpers, config rendering, advisory state handling, controller stop-on-error behavior
- **CLI integration tests:** help/version, inventory validation, dry-run controller execution
- **Container smoke test:** build + test + dry-run inventory execution in a Debian container
