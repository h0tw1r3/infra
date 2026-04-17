# Contributing to Nomad Bootstrapper

Thank you for your interest in contributing! This document provides guidelines and instructions for development.

## Quick Start

### Prerequisites

See [SETUP.md](../SETUP.md) for complete installation instructions. You'll need:

- Rust 1.70+ (via rustup)
- Xcode Command Line Tools (macOS only)
- Pre-commit framework
- Docker (for integration testing, optional)

### Setup Development Environment

```bash
# Clone the repository
cd /Users/jeffrey.clark/dev/github/clark/nomad-bootstrapper

# Install pre-commit hooks
pre-commit install

# Verify setup
cargo test --all
```

## Development Workflow

### Daily Development Loop

```bash
# 1. Create a feature branch
git checkout -b feat/your-feature-name

# 2. Make changes to code

# 3. Run pre-commit checks
pre-commit run --all-files

# 4. Run tests locally
cargo test --all --verbose

# 5. Commit with conventional message
git add .
git commit -m "feat(module): description of changes"

# 6. Push and create pull request
git push origin feat/your-feature-name
```

### Running Tests

```bash
# Run all unit tests
cargo test --lib

# Run with output (helpful for debugging)
cargo test --lib -- --nocapture

# Run specific test
cargo test --lib config_generation -- --exact

# Run integration tests
cargo test --all --features integration --verbose

# Run tests with coverage
cargo tarpaulin --fail-under 70
```

### Code Quality Checks

**Before committing:**

```bash
# Format code
cargo fmt

# Check formatting
cargo fmt -- --check

# Lint code
cargo clippy --all-targets --all-features -- -D warnings

# Run all tests
cargo test --all

# Security audit
cargo audit

# Coverage report
cargo tarpaulin --fail-under 70 --timeout 300
```

**These checks run automatically via pre-commit hooks on commit.** If you commit without running them first, the commit will be blocked until issues are fixed.

### Code Standards

#### Rust Code Style

- **Formatting**: Enforced by `rustfmt` (run `cargo fmt` before committing)
- **Linting**: Enforced by `clippy` with `-D warnings` (all warnings must be fixed)
- **Max line length**: 100 characters
- **Indentation**: 4 spaces, no tabs
- **Line endings**: Unix (LF)

#### Documentation

- All public items require `///` doc comments
- Include examples where helpful
- Link to related functions/types

Example:
```rust
/// Executes a shell command and captures output.
///
/// # Arguments
///
/// * `cmd` - The command to execute (e.g., "apt-get")
/// * `args` - Command arguments
///
/// # Returns
///
/// Returns `Ok(Output)` on success, or `Err` with context if execution fails.
///
/// # Examples
///
/// ```
/// let runner = CommandRunner::new(false);
/// let output = runner.run("echo", &["hello"])?;
/// assert!(output.status.success());
/// ```
pub fn run(&self, cmd: &str, args: &[&str]) -> anyhow::Result<Output> {
    // implementation
}
```

#### Testing

- **Unit tests**: Co-located with code in `#[cfg(test)]` modules
- **Integration tests**: In `tests/` directory
- **Minimum coverage**: 70%
- **Test fixtures**: In `tests/fixtures/`
- **Mock data**: In `tests/mocks.rs` or inline in tests

Example unit test:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase_filtering() {
        let graph = DependencyGraph::new().unwrap();
        let phases = graph.filter_phases(&None, &None).unwrap();
        assert_eq!(phases.len(), 5);
    }
}
```

### Conventional Commits

All commit messages must follow the [Conventional Commits](https://www.conventionalcommits.org/) specification.

**Format:**
```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

**Types:**
- `feat`: New feature
- `fix`: Bug fix
- `refactor`: Code restructuring without feature change
- `perf`: Performance improvement
- `test`: Adding or updating tests
- `docs`: Documentation changes
- `chore`: Build, dependency, or tooling changes
- `ci`: CI/CD pipeline changes

**Examples:**
```
feat(executor): add dependency graph validation

Implements phase ordering validation to ensure phases execute in correct order.
Validates --phase and --up-to flag arguments against available phases.

Fixes #42
```

```
fix(configure): skip service restart when config unchanged

Compare desired vs. existing Nomad HCL config before restarting service.
Prevents unnecessary restarts and service disruptions.
```

**Commit message guidelines:**
- Subject line: ≤ 50 characters
- Body: Wrap at 72 characters
- Use imperative mood ("add", not "added" or "adds")
- Use lowercase in subject line (except for proper nouns)
- No period at end of subject line

## Creating a Pull Request

Before submitting a PR, ensure:

✅ All CI checks pass
✅ Code coverage ≥ 70%
✅ No security vulnerabilities (`cargo audit`)
✅ Conventional commit messages used
✅ Tests added for new functionality
✅ Documentation updated if needed

### PR Title and Description

**Title:** Use conventional commit format
```
feat(executor): add phase timeout support
```

**Description:** Include:
- What problem does this solve?
- How is it tested?
- Any breaking changes?
- Links to related issues

Example:
```markdown
## Description
Adds configurable timeout support for long-running phases to prevent hanging in high-latency environments.

## Motivation
Addresses #45 - Nomad bootstrap times out on home internet connections.

## Testing
- Unit tests for timeout configuration parsing
- Integration test with simulated 500ms+ latency
- Manual testing on Debian 12 with 1000ms RTT

## Checklist
- [x] Tests added/updated
- [x] Documentation updated
- [x] Coverage > 70%
- [x] No clippy warnings
```

## Project Structure

```
nomad-bootstrapper/
├── src/
│   ├── main.rs           # CLI entry point and orchestration
│   ├── runner.rs         # Command execution wrapper
│   ├── system.rs         # System state probing
│   ├── models.rs         # Data types (NodeConfig, etc.)
│   ├── executor.rs       # Phase execution engine
│   └── modules/
│       ├── mod.rs
│       ├── ensure_deps.rs
│       ├── setup_repo.rs
│       ├── install.rs
│       ├── configure.rs
│       └── verify.rs
├── tests/
│   ├── integration_tests.rs
│   ├── fixtures/         # Test data files
│   └── mocks.rs          # Mock implementations
├── Cargo.toml
├── rustfmt.toml
├── .clippy.toml
└── .github/workflows/ci.yml
```

## Implementation Phases

This project is implemented in phases as outlined in the [Implementation Plan](../thoughts/shared/plans/2026-04-16-nomad-rust-bootstrapper.md):

- **Phase 1**: Project Initialization ✅ (You are here)
- **Phase 2**: Core Module Implementation (in progress)
- **Phase 3**: Orchestration & CLI (planned)
- **Phase 4**: Verification & Testing (planned)

When implementing a phase, update the plan document to mark progress.

## Getting Help

- Check the [Implementation Plan](../thoughts/shared/plans/2026-04-16-nomad-rust-bootstrapper.md) for architecture details
- Review [Design Documents](../thoughts/shared/designs/) for technical decisions
- Look at existing tests for usage examples
- Open an issue for questions or blockers

## Code Review Guidelines

As a reviewer:
- ✅ All tests pass
- ✅ Code coverage maintained or improved
- ✅ No clippy warnings
- ✅ Documentation clear and complete
- ✅ Conventional commits used
- ✅ Idempotency maintained (for provisioning code)

---

**Happy coding!** 🎉
