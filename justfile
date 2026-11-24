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

# Lint with clippy (warnings as errors, matches CI; --locked keeps the engine pin from drifting)
lint:
    RUSTFLAGS="-Dwarnings" cargo clippy --locked --workspace --all-targets --all-features

# Run all Rust tests (--locked: Cargo.lock is a reviewed artifact here, not a resolution to redo)
test:
    cargo test --locked --workspace --all-features

# CPU runtime/build graph: known GPU backend/native libraries must stay optional
cpu-deps:
    python3 -B -m unittest discover -s tools -p test_cpu_dependencies.py
    python3 -B tools/check_cpu_dependencies.py

# CPU-only workspace admission guards (no optional GPU backends)
test-cpu:
    cargo test --locked --workspace --no-default-features

# Standalone curator gates: no official model download/conversion or inference
artifact-fmt:
    cargo fmt --manifest-path tools/granite-artifact/Cargo.toml -- --check

artifact-lint:
    cargo clippy --locked --manifest-path tools/granite-artifact/Cargo.toml --all-targets -- -D warnings

artifact-test:
    cargo test --locked --manifest-path tools/granite-artifact/Cargo.toml

artifact-deny:
    cargo deny --manifest-path tools/granite-artifact/Cargo.toml --config deny.toml check

artifact-ci: artifact-fmt artifact-lint artifact-test artifact-deny

# Dependency-free evaluator tests; Python performs no inference
eval-test:
    python3 -m unittest discover -s tools/granite-evaluation

# Offline refinement scoring against the human-reviewed Task 1 cases.
refinement-eval-test:
    python3 -I -B -c 'import sys, unittest; sys.exit(not unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.discover("tools/refinement-evaluation")).wasSuccessful())'

refinement-eval:
    python3 -I -B tools/refinement-evaluation/score.py tools/refinement-evaluation/fixtures/cases.jsonl tools/refinement-evaluation/fixtures/deterministic-proposals.jsonl

# Scoped release admission: temp stores, deterministic providers and loopback only.
refinement-admission:
    cargo test --locked -p alexandria --test joint_admission_test --test ownership_cli_test --test refinement_cli_test
    cargo test --locked -p alexandria-mcp --test refinement_authority_test --test refinement_inference_test --test refinement_ui_test --test refinement_worker_test --test web_auth_test
    cargo test --locked -p alexandria-storage --test classification_test --test refinement_apply_test --test refinement_candidates_test --test refinement_policy_test --test refinement_proposal_test
    cargo test --locked -p alexandria-mcp --example review_fixture

# Claude hooks against disposable local fixtures, not a running Claude install.
claude-test:
    python3 -B -m unittest discover -s contrib/claude/tests -p 'test_*.py'

# Disposable loopback review UI. The example does not read Alexandria configuration.
review-fixture:
    cargo run --locked -p alexandria-mcp --example review_fixture

# Type-check and test the pi companion (node:test over tsx, hermetic)
ext-test:
    cd contrib/pi/extensions/alexandria && npm run typecheck && npm test

# Install the pi companion's dev dependencies (`node_modules/` is gitignored, so a
# fresh checkout has nothing to type-check or test against until this runs)
ext-install:
    cd contrib/pi/extensions/alexandria && npm ci --ignore-scripts

# Hermes companion (Python 3.12+, requires httpx in the selected environment)
hermes-test:
    python3 -m unittest discover -s contrib/hermes/tests -v

# All tests, Rust and client-side
test-all: test ext-test claude-test hermes-test

# Fast type-check
check:
    cargo check --locked --workspace --all-features

# Run the server
run:
    cargo run --locked --all-features

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
ci: fmt lint verify-assets cpu-deps refinement-admission test test-cpu artifact-ci eval-test refinement-eval-test refinement-eval ext-test claude-test hermes-test deny

# Install git hooks (pre-commit: fmt + clippy)
install-hooks:
    cp .githooks/pre-commit .git/hooks/pre-commit
    chmod +x .git/hooks/pre-commit
    echo "✅ Git hooks installed"
