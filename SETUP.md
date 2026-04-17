# Development Setup Guide

This guide walks you through setting up your development environment for the Clark project (Nomad Bootstrap Toolkit).

## System Requirements

- **OS**: macOS (or Linux)
- **RAM**: 4GB minimum (8GB recommended)
- **Disk Space**: 2GB for toolchain and build artifacts

## Installation Steps

### 1. Install Rust Toolchain

The Rust toolchain (compiler, cargo, rustfmt, clippy) is managed by `rustup`.

```bash
# Download and install rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Follow the on-screen prompts to complete installation
# Default option (1) is recommended

# Source the environment (add to your shell profile if not done automatically)
source "$HOME/.cargo/env"

# Verify installation
rustc --version
cargo --version
```

### 2. Install Xcode Command Line Tools

Required for C/C++ compilation and linking (needed by Rust).

```bash
# Install (will prompt for password and agreement)
xcode-select --install

# Verify installation
xcode-select --version
```

### 3. Install Pre-commit Framework

Pre-commit hooks automate code quality checks (formatting, linting, etc.) before commits.

**Option A: Homebrew (Recommended)**
```bash
# Install Homebrew (if not already installed)
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

# Install pre-commit
brew install pre-commit

# Verify
pre-commit --version
```

**Option B: pip (if Python 3 is available)**
```bash
pip3 install pre-commit
pre-commit --version
```

### 4. Install Rust Components

Additional components needed for development.

```bash
# Install clippy (linter)
rustup component add clippy

# Install rustfmt (code formatter)
rustup component add rustfmt

# Verify
cargo clippy --version
cargo fmt --version
```

### 5. Install Cargo Extensions (Optional but Recommended)

These tools enhance the development workflow.

```bash
# Code coverage analysis
cargo install cargo-tarpaulin

# Security vulnerability scanning
cargo install cargo-audit

# Dependency version tracking
cargo install cargo-outdated

# Verify installations
cargo tarpaulin --version
cargo audit --version
cargo outdated --version
```

### 6. Install Docker (Optional, for Integration Testing)

For testing the bootstrapper in isolated Debian environments.

**macOS with Homebrew:**
```bash
brew install --cask docker

# Or install Docker Desktop manually:
# https://www.docker.com/products/docker-desktop
```

**Start Docker:**
```bash
# Docker Desktop starts automatically on login (after first launch)
# Or start via Spotlight: Cmd+Space → "Docker" → Enter
```

## Post-Installation Setup

### Clone Repository and Configure Pre-commit

```bash
# Navigate to project
cd /Users/jeffrey.clark/dev/github/clark

# Install pre-commit hooks
pre-commit install

# Verify pre-commit is installed
git hooks list | grep pre-commit
```

### Verify Complete Setup

Run this script to verify all tools are installed:

```bash
#!/bin/bash
set -e

echo "=== Rust Setup ===" && \
rustc --version && \
cargo --version && \
cargo clippy --version && \
cargo fmt --version && \
echo "" && \
echo "=== Xcode Command Line Tools ===" && \
xcode-select --version && \
echo "" && \
echo "=== Pre-commit ===" && \
pre-commit --version && \
echo "" && \
echo "=== Cargo Extensions (Optional) ===" && \
cargo tarpaulin --version 2>/dev/null || echo "⚠️  cargo-tarpaulin: not installed (optional)" && \
cargo audit --version 2>/dev/null || echo "⚠️  cargo-audit: not installed (optional)" && \
cargo outdated --version 2>/dev/null || echo "⚠️  cargo-outdated: not installed (optional)" && \
echo "" && \
echo "✅ Setup complete! You're ready to develop."
```

Save as `verify-setup.sh`, make executable, and run:
```bash
chmod +x verify-setup.sh
./verify-setup.sh
```

## Rust Development Workflow

### Daily Development Loop

```bash
# 1. Make your changes
# ... edit files ...

# 2. Run pre-commit checks (automatic on commit, or manual)
pre-commit run --all-files

# 3. Run tests
cargo test --all

# 4. Commit with conventional message
git add .
git commit -m "feat(module): description of changes"
```

### Common Commands

```bash
# Build project
cargo build

# Build release binary
cargo build --release

# Run tests
cargo test --all

# Run with output
cargo test --all -- --nocapture

# Format code
cargo fmt

# Lint code
cargo clippy --all-targets --all-features -- -D warnings

# Check security vulnerabilities
cargo audit

# Generate documentation
cargo doc --no-deps --open

# Check code coverage
cargo tarpaulin --fail-under 70
```

### Troubleshooting

**Problem: `rustc` not found**
```bash
# Solution: Source cargo environment
source "$HOME/.cargo/env"

# Make permanent by adding to ~/.zshrc or ~/.bash_profile
echo 'source "$HOME/.cargo/env"' >> ~/.zshrc
```

**Problem: `xcode-select` command not found**
```bash
# Solution: Reinstall Xcode Command Line Tools
xcode-select --reset
xcode-select --install
```

**Problem: `pre-commit` hooks not running**
```bash
# Solution: Reinstall hooks
pre-commit install

# Verify installation
git hooks list
```

**Problem: Cargo build fails with linking errors**
```bash
# Solution: Ensure Xcode CLT is installed
xcode-select --install

# Or reset to default installation
xcode-select --reset
```

**Problem: Docker not starting on macOS**
```bash
# Solution: Launch Docker Desktop
open /Applications/Docker.app

# Verify Docker is running
docker ps
```

## IDE Setup (Optional)

### VS Code + Rust-analyzer

1. Install VS Code: https://code.visualstudio.com/
2. Install extension: "rust-analyzer" (ID: `rust-lang.rust-analyzer`)
3. Create `.vscode/settings.json`:

```json
{
  "rust-analyzer.checkOnSave.command": "clippy",
  "rust-analyzer.checkOnSave.extraArgs": [
    "--all-targets",
    "--all-features",
    "--",
    "-D",
    "warnings"
  ],
  "editor.formatOnSave": true,
  "[rust]": {
    "editor.defaultFormatter": "rust-lang.rust-analyzer"
  }
}
```

### JetBrains CLion / IntelliJ IDEA + Rust Plugin

1. Install IDE: https://www.jetbrains.com/
2. Install plugin: "Rust" (built-in or via plugin marketplace)
3. Configure Rust toolchain in IDE settings

## Next Steps

Once setup is complete, proceed to **Phase 1: Project Initialization** in the [Implementation Plan](thoughts/shared/plans/2026-04-16-nomad-rust-bootstrapper.md).

---

**Last Updated:** April 16, 2026

