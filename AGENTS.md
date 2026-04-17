# AI Agent Guide: Clark - Nomad Bootstrap Toolkit

This repository contains tools for bootstrapping HashiCorp Nomad in Debian-based Linux systems, with a focus on high-latency network environments and idempotent, robust installations.

## Project Overview

**Current state:** Rust implementation (active development)

The toolkit ensures:
- **Idempotency**: Safe to run multiple times without side effects
- **Robustness**: Type safety and error handling via Rust
- **Modularity**: Phase-based execution (ensure-deps → setup-repo → install → configure → verify)
- **Auto-escalation**: Privilege escalation via `sudo2` crate (sudo/doas/pkexec)
- **Minimal dependencies**: Standard tools only (`apt-get`, `curl`, `gpg`, `dpkg`)

## Key Technical Conventions

### Code Quality Standards
Pre-commit hooks enforce strict standards via `.pre-commit-config.yaml`:
- **Shell scripts**: `shfmt` (4-space indentation, case-indent, binary-next-line)
- **Linting**: `shellcheck` for shell, `pymarkdown` for docs
- **Formatting**: Consistent 2-space indentation for YAML/JSON/TOML
- **Safety checks**: No trailing whitespace, LF line endings, detects merge conflicts

### Architecture Pattern
All implementations follow a **state provisioner design** with phase-based execution:

1. **Ensure Dependencies Phase**: Verify required system packages (`curl`, `gpg`, etc.)
2. **Setup Repository Phase**: Add HashiCorp GPG key and APT source
3. **Install Phase**: Download and install Nomad binary to desired version
4. **Configure Phase**: Generate and deploy Nomad HCL configuration for the node's role (server/client)
5. **Verify Phase**: Run `nomad version` and cluster connectivity checks

**Key Principle:** The tool operates as a state provisioner, not a command dispatcher. By default, all phases run in dependency order to move from current state → desired state. Individual phases can be selected via `--phase` flag for testing.

### Project Structure

```
clark/
├── nomad-bootstrapper/         # Rust implementation (active)
│   ├── src/
│   │   ├── main.rs            # CLI args, address validation, entry point
│   │   ├── models.rs          # NodeConfig, ServerConfig, ClientConfig
│   │   ├── executor.rs        # DependencyGraph, phase ordering
│   │   ├── runner.rs          # CommandRunner (real / dry-run)
│   │   ├── state.rs           # Provisioning state tracking
│   │   ├── system.rs          # System probes (dpkg, codename, config I/O)
│   │   └── modules/           # Phase implementations
│   │       ├── ensure_deps.rs
│   │       ├── setup_repo.rs
│   │       ├── install.rs
│   │       ├── configure.rs
│   │       └── verify.rs
│   ├── tests/                 # Integration tests
│   ├── scripts/               # Docker-based test runner
│   ├── Cargo.toml
│   └── README.md
├── thoughts/shared/
│   ├── designs/               # Technical specifications
│   └── plans/                 # Implementation plans
└── .pre-commit-config.yaml    # Code quality standards
```

## Build/Test Commands

### Rust Implementation
```bash
# Build release binary
cargo build --release

# Run unit tests (config generation, state probing, dependency graph)
cargo test

# Run integration tests (Docker Debian containers)
cargo test --features integration

# Default: Bootstrap to desired state (auto-escalates to root)
./target/release/nomad-bootstrapper --nomad-version 1.6.0 --role server --bootstrap-expect 3 --high-latency

# For testing: Run only a specific phase
./target/release/nomad-bootstrapper --phase ensure-deps

# For testing: Run up to a specific phase (includes dependencies)
./target/release/nomad-bootstrapper --nomad-version 1.6.0 --up-to configure

# Dry-run mode (show what would be done, skips privilege escalation)
./target/release/nomad-bootstrapper --nomad-version 1.6.0 --role server --dry-run
```

### Legacy Bash
```bash
# Direct execution (requires sudo/root)
./scripts/bootstrap_nomad.sh
```

## Documentation Links

- **Implementation Plan**: [4-phase Rust rewrite roadmap](thoughts/shared/plans/2026-04-16-nomad-rust-bootstrapper.md)
- **Design - Rust Bootstrapper**: [Technical design document](thoughts/shared/designs/2026-04-16-nomad-rust-bootstrapper-design.md)
- **Design - Extension**: [Cluster configuration capabilities](thoughts/shared/designs/2026-04-16-bootstrap-nomad-extension-design.md)

## Key Development Patterns

### Idempotency (Critical Requirement)
- **Always check system state** before making changes
- **Skip operations if already completed** (version already installed, repo already added, etc.)
- **No destructive operations** should have visible side effects on re-runs
- Test scripts using: `run once → verify → run again → verify again`

### Error Handling
- Rust: Use `anyhow` for context-rich error reporting
- Bash: Exit with non-zero codes, clear error messages
- Never silently fail; always log what operations are attempted

### Target Environment
- **OS**: Debian-based systems only (Debian, Ubuntu, etc.)
- **Privileges**: Auto-escalates via sudo, doas, or pkexec (sudo2 crate)
- **Network**: Designed for high-latency environments (>100ms tolerant)
- **Dependencies**: Minimal and standard (no heavy runtimes)

## When Working on Tasks

### Bash Scripts
- Review existing pre-commit standards before modifying
- Test idempotency: run twice with verification between runs
- Use `shellcheck` locally before committing: `shellcheck scripts/*.sh`

### Rust Implementation
- Follow the **state provisioner pattern**: Phases execute in dependency order to converge desired → actual state
- Implement the 5-phase architecture documented in the implementation plan: ensure-deps → setup-repo → install → configure → verify
- Build a `DependencyGraph` struct to enforce phase ordering and allow `--phase` and `--up-to` flags for testing
- Ensure unit tests cover: version parsing, codename detection, HCL config generation, state comparison logic
- Plan integration tests with Docker for multi-distro validation (test reconfiguration and idempotency)
- Use `log` + `env_logger` for structured logging
- Implement configuration idempotency: parse existing `/etc/nomad.d/nomad.hcl`, compare with desired config, skip service restart if unchanged

### Documentation
- Keep design docs and implementation plans up-to-date with decisions
- Link to existing docs rather than duplicating information
- Mark completed phases in the implementation plan as you finish them

## Getting Started

1. **Read the implementation plan**: [2026-04-16-nomad-rust-bootstrapper.md](thoughts/shared/plans/2026-04-16-nomad-rust-bootstrapper.md)
2. **Review design docs**: Understand the 4-phase architecture and extension capabilities
3. **Check pre-commit config**: Understand code quality standards
4. **Test in isolated environment**: Use Docker Debian containers for testing

---

*Last updated: April 17, 2026*
