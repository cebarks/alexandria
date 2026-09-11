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

# Re-download vendored debug-UI assets and verify against SHA256SUMS
vendor-assets:
    cd crates/alexandria-mcp/assets && \
    curl -fsSL -o htmx-2.0.10.min.js https://unpkg.com/htmx.org@2.0.10/dist/htmx.min.js && \
    curl -fsSL -o vis-network-10.1.2.min.js https://unpkg.com/vis-network@10.1.2/standalone/umd/vis-network.min.js && \
    sha256sum -c SHA256SUMS

# Check vendored debug-UI assets against SHA256SUMS
verify-assets:
    cd crates/alexandria-mcp/assets && sha256sum -c SHA256SUMS

# Full CI suite locally — run before pushing
ci: fmt lint test deny verify-assets

# Install git hooks (pre-commit: fmt + clippy)
install-hooks:
    cp .githooks/pre-commit .git/hooks/pre-commit
    chmod +x .git/hooks/pre-commit
    echo "✅ Git hooks installed"
