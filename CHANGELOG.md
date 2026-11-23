# Changelog

## [Unreleased]

### Security

- Update ammonia, rmcp, Tokio, TLS/root bundles, Candle Core/ZIP, and zstd-sys to patched releases; upgrade the official SurrealDB family to exactly 4.0.1 and SurrealKV to 1.2.0. See [dependency advisory status](docs/dependency-advisories.md).
- Keep Pi project resolution, recall, reminder checks, and retries bound to the same configured endpoint and bearer identity.
- Adopt main's single WASM crypto advisory ignore (`RUSTSEC-2026-0387`) and two named SurrealDB BUSL-1.1 allowances. The vulnerable dependency is not patched by this policy change; all-target/all-feature and unmaintained checks remain in force.
- Require pinned, hash-checked, wheel-only Python dependencies for Hermes companion tests. Hermes setup no longer installs the Alexandria server when its health check fails; provision and start the server explicitly.

### Breaking

- The embedded storage engine is now SurrealDB 4.0.1. Do not point it at an original 3.x store. Follow the [offline copy-first upgrade procedure](docs/surrealdb-4-upgrade.md), including explicit experimental conversion on a disposable copy, mandatory HNSW rebuild, and data/identity verification. Rollback requires an untouched 3.x backup and the old binary; a 4.x-touched target is not a rollback source.
- The Pi companion reads JSON only. Convert a former `client.toml` manually. The recall-only `/alexandria` menu respects environment overrides.
- The Hermes companion requires an environment-provided agent bearer. It uses an optional issued project ID, otherwise Personal. Legacy capture/extraction/session flags no longer cause writes.

### Added

- Hermes Agent companion with authenticated scoped recall, foreground reminders, and nine explicit native memory/reminder tools. Alexandria-only curated memory is supported through Hermes' native-store configuration flags.
- Public `GET /health` with a live storage probe, a one-second deadline, and fixed HTTP 503 diagnostics. It grants no access to the bearer-authenticated MCP service or private web UI.
- `just hermes-test` and a standalone Hermes companion CI job, with hermetic host, authenticated HTTP JSON/SSE, scope, and lifecycle tests.

### Changed

- Use 4.0 expression sorting for tag count instead of the old projection workaround. Preserve tag occurrence counts despite 4.0's deduplicating `array::group`; retain the shared live-record filter, deterministic order, and cap.
- Add a frozen, separate-process 3.3 physical-store fixture covering hidden rows, identical vectors, metadata, identity, edges, sessions, heat, and reminders; test default refusal, opt-in copied-store conversion, index rebuild/readiness/plan, and unchanged source bytes.

### Fixed

- Hermes reminder scheduling, pending listing, and delivery text now use Alexandria's canonical API fields. Automatic consumption occurs only in primary foreground prefetch, separately from semantic caching, retaining notes and escalation/missed-occurrence context. Hermes skips this hook on trivial prompts; delivery then waits for a nontrivial foreground prompt or explicit check.
- A forward schema migration keeps journal writes working on SurrealDB 4.0.1 without relaxing effect or scope-kind validation or rewriting history.
- Hermes recall searches across sessions within the configured scope and applies its client similarity floor. A failed authenticated check does not replay cached semantic text.
- Hermes orders explicit tool calls and cache invalidation without closing an active client. A caller timeout can leave an accepted operation running and does not imply persistence.

### Removed

- Hermes built-in-memory write mirroring. Existing files and previously stored mirror facts are left untouched; coexistence does not synchronize the two stores.
