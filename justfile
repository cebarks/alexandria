# Alexandria — task runner
# See https://just.systems/ for just documentation

# List available recipes
default:
    @just --list

# Format check (CI-equivalent)
fmt:
    cargo fmt --all -- --check

# Auto-fix formatting
fmt-fix:
    cargo fmt --all

# Lint with clippy (warnings as errors, matches CI)
lint:
    RUSTFLAGS="-Dwarnings" cargo clippy --all-targets --all-features

# Run all tests
test:
    cargo test --all-features

# Fast type-check
check:
    cargo check --all-features

# Run the server
run:
    cargo run --all-features

# Clean build artifacts
clean:
    cargo clean

# Run cargo-deny (license/advisory check)
deny:
    cargo deny check

# Full CI suite locally — run before pushing
ci: fmt lint test deny

# Install git hooks (pre-commit: fmt + clippy)
install-hooks:
    cp .githooks/pre-commit .git/hooks/pre-commit
    chmod +x .git/hooks/pre-commit
    echo "✅ Git hooks installed"
