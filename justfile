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

# Run all Rust tests
test:
    cargo test --all-features

# Type-check and test the pi companion (node:test over tsx, hermetic)
ext-test:
    cd contrib/pi/extensions/alexandria && npm run typecheck && npm test

# Install the pi companion's dev dependencies (`node_modules/` is gitignored, so a
# fresh checkout has nothing to type-check or test against until this runs)
ext-install:
    cd contrib/pi/extensions/alexandria && npm ci

# All tests, Rust and client-side
test-all: test ext-test

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
#
# Two independent halves, and the second is the one that used to be missing. `sha256sum -c` only
# reads the files SHA256SUMS *names*, so a third .js dropped into this directory with no checksum
# line passed every gate — including the CI step — and shipped unverified bytes under a
# `max-age=31536000, immutable` header. `*.js` is the complete set of checksummed extensions that
# exist today (assets/README.md is prose, SHA256SUMS is the manifest); widen the glob if a new
# vendored kind is ever added.
verify-assets:
    cd crates/alexandria-mcp/assets && \
    sha256sum -c SHA256SUMS && \
    for f in *.js; do \
      [ -e "$f" ] || continue; \
      awk -v "f=$f" '$2 == f { hit = 1 } END { exit !hit }' SHA256SUMS || \
        { echo "verify-assets: $f has no checksum line in SHA256SUMS - add one, or delete the file" >&2; exit 1; }; \
    done

# Full CI suite locally — run before pushing
ci: fmt lint test ext-test deny verify-assets

# Install git hooks (pre-commit: fmt + clippy)
install-hooks:
    cp .githooks/pre-commit .git/hooks/pre-commit
    chmod +x .git/hooks/pre-commit
    echo "✅ Git hooks installed"
