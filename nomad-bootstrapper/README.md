# Nomad Bootstrapper

A robust, idempotent state provisioner for bootstrapping HashiCorp Nomad on Debian-based Linux systems.

## Features

- ✅ **Idempotent**: Safe to run multiple times without side effects
- ✅ **State Provisioner Model**: Converges system from current state to desired state
- ✅ **Phase-Based Architecture**: Explicit dependency ordering (ensure-deps → setup-repo → install → configure → verify)
- ✅ **Type-Safe**: Written in Rust for robustness and performance
- ✅ **High-Latency Tuning**: Optimized for home internet and other high-latency environments
- ✅ **Configuration Management**: Idempotent configuration generation and deployment
- ✅ **Comprehensive Testing**: Unit and integration tests with ≥70% code coverage

## Quick Start

### Prerequisites

- macOS, Linux, or WSL2
- Rust 1.70+ (install via [rustup](https://rustup.rs/))
- sudo/root access on target Debian system

See [SETUP.md](SETUP.md) for detailed installation instructions.

### Installation

```bash
# Clone and navigate to project
cd /Users/jeffrey.clark/dev/github/clark/nomad-bootstrapper

# Build release binary
cargo build --release

# Binary location
./target/release/nomad-bootstrapper --version
```

### Basic Usage

```bash
# Bootstrap a new server (first node in cluster)
sudo ./target/release/nomad-bootstrapper \
  --version 1.6.0 \
  --role server \
  --bootstrap-expect 1

# Bootstrap a client
sudo ./target/release/nomad-bootstrapper \
  --version 1.6.0 \
  --role client \
  --server-address "10.0.1.1:4647"

# Bootstrap server with high-latency tuning
sudo ./target/release/nomad-bootstrapper \
  --version 1.6.0 \
  --role server \
  --bootstrap-expect 3 \
  --server-join-address "10.0.1.2:4647,10.0.1.3:4647" \
  --high-latency
```

## Architecture

### State Provisioner Model

Unlike command-line tools that run a specific command once, this tool operates as a **state provisioner**:

1. You specify the **desired state**: version, role, configuration options
2. The tool checks the **current state** of the system
3. It executes only the **necessary phases** to reach the desired state
4. Running multiple times is **safe and idempotent**

### Phase Dependency Graph

```
ensure-deps
    ↓
setup-repo
    ↓
install
    ↓
configure
    ↓
verify
```

Each phase:
- Checks if its work is already done
- Skips work if unnecessary
- Provides clear logging of what changed

### Configuration Idempotency

The configure phase avoids unnecessary service restarts:

```
Current Config (on disk) = read /etc/nomad.d/nomad.hcl
Desired Config (generated) = generate from --role, --bootstrap-expect, etc.

if Current == Desired:
    log "Configuration already matches desired state"
    skip systemctl restart
else:
    apply new configuration and restart service
```

## Command-Line Options

```
USAGE:
    nomad-bootstrapper [OPTIONS]

OPTIONS:
    --version <VERSION>
        Nomad version to install (default: latest)

    --role <ROLE>
        Node role: server or client (required)

    --bootstrap-expect <N>
        For server: number of servers in initial cluster (required for --role server)

    --server-join-address <ADDRESS>
        For server: another server to join (can be specified multiple times, e.g., --server-join-address 10.0.1.2:4647)

    --server-address <ADDRESS>
        For client: a Nomad server address (can be specified multiple times, required for --role client)

    --high-latency
        Apply tuning for high-latency environments (home internet, satellite, etc.)

    --phase <PHASE>
        Run only this phase: ensure-deps, setup-repo, install, configure, or verify
        Useful for testing and debugging

    --up-to <PHASE>
        Run all phases up to and including this one
        Respects phase dependencies

    --dry-run
        Show what would be done without making changes

    --log-level <LEVEL>
        Set logging level: debug, info, warn, error (default: info)
```

## Examples

### Fresh Server Cluster (3 nodes)

**Node 1 (bootstrap node):**
```bash
sudo nomad-bootstrapper \
  --version 1.6.0 \
  --role server \
  --bootstrap-expect 3 \
  --high-latency
```

**Node 2 & 3 (join existing cluster):**
```bash
sudo nomad-bootstrapper \
  --version 1.6.0 \
  --role server \
  --bootstrap-expect 3 \
  --server-join-address "10.0.1.1:4647" \
  --high-latency
```

### Reconfigure Existing Node

Change from standalone server to cluster member:

```bash
# Original: Single server
sudo nomad-bootstrapper --version 1.6.0 --role server --bootstrap-expect 1

# Later: Join 3-node cluster
sudo nomad-bootstrapper \
  --version 1.6.0 \
  --role server \
  --bootstrap-expect 3 \
  --server-join-address "10.0.1.2:4647,10.0.1.3:4647" \
  --high-latency
```

Running twice is safe (idempotent):
```bash
# First run: applies changes
$ sudo nomad-bootstrapper --version 1.6.0 --role server --bootstrap-expect 3 --high-latency
[INFO] Running phase: ensure-deps
[INFO] Running phase: setup-repo
[INFO] Running phase: install
[INFO] Running phase: configure
[DEBUG] Configuration differs, applying changes
[INFO] Running phase: verify
[INFO] Nomad bootstrap complete

# Second run: no-op (config unchanged)
$ sudo nomad-bootstrapper --version 1.6.0 --role server --bootstrap-expect 3 --high-latency
[INFO] Running phase: ensure-deps
[INFO] Running phase: setup-repo
[INFO] Running phase: install
[INFO] Running phase: configure
[DEBUG] Configuration already matches desired state, skipping restart
[INFO] Running phase: verify
[INFO] Nomad bootstrap complete
```

### Testing with Docker

```bash
# Build image with bootstrapper
docker build -t nomad-test:debian-bookworm - <<'EOF'
FROM debian:bookworm
RUN apt-get update && apt-get install -y ca-certificates curl
COPY ./target/release/nomad-bootstrapper /usr/local/bin/
EOF

# Test bootstrap in container
docker run --rm -it --privileged nomad-test:debian-bookworm \
  nomad-bootstrapper --version 1.6.0 --role server --bootstrap-expect 1 --dry-run
```

## Supported Systems

- **Debian**: 11 (Bullseye), 12 (Bookworm)
- **Ubuntu**: 20.04 (Focal), 22.04 (Jammy), 24.04 (Noble)
- **Rocky Linux**: 8, 9

## Building from Source

```bash
# Build debug binary
cargo build

# Build release binary (optimized)
cargo build --release

# Binary location
./target/release/nomad-bootstrapper
```

## Testing

```bash
# Run all tests
cargo test --all

# Run unit tests only
cargo test --lib

# Run with output
cargo test --lib -- --nocapture

# Integration tests
cargo test --all --features integration

# Code coverage
cargo tarpaulin --fail-under 70
```

## Code Quality

This project maintains strict code quality standards enforced via pre-commit hooks:

```bash
# Format code
cargo fmt

# Lint code
cargo clippy --all-targets --all-features -- -D warnings

# Security audit
cargo audit

# Check coverage
cargo tarpaulin --fail-under 70
```

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for:
- Development setup
- Testing guidelines
- Code standards
- Pull request process
- Conventional commits

## Documentation

- [SETUP.md](SETUP.md) - Development environment setup
- [CONTRIBUTING.md](CONTRIBUTING.md) - Contribution guidelines
- [Implementation Plan](../thoughts/shared/plans/2026-04-16-nomad-rust-bootstrapper.md) - Architecture and phases
- [Design Documents](../thoughts/shared/designs/) - Technical decisions

## License

MIT License - See LICENSE file for details

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

---

**Built with ❤️ for reliable infrastructure automation**
