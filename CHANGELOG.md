# Changelog

## [Unreleased]

### Security

- Update ammonia, rmcp, Tokio, TLS/root bundles, Candle Core/ZIP, and zstd-sys to patched releases; upgrade the official SurrealDB family to exactly 4.0.1 and SurrealKV to 1.2.0. See [dependency advisory status](docs/dependency-advisories.md).
- Explicitly accept `RUSTSEC-2026-0387` on the official engine's WASM crypto dependency route. This is a documented risk exception, **not** a crypto fix. Add only the approved package-specific BUSL exceptions for `surrealdb-parser-next` and `surrealdb-stored-derive`.
- Require pinned, hash-checked, wheel-only Python dependencies for Hermes companion tests. Hermes setup no longer installs the Alexandria server when its health check fails; provision and start the server explicitly.

### Breaking

- The embedded storage engine is now SurrealDB 4.0.1. Do not point it at an original 3.x store. Follow the [offline copy-first upgrade procedure](docs/surrealdb-4-upgrade.md), including explicit experimental conversion on a disposable copy, mandatory HNSW rebuild, and data/identity verification. Rollback requires an untouched 3.x backup and the old binary; a 4.x-touched target is not a rollback source.

### Added

- Hermes Agent companion with automatic cross-session recall, heuristic capture, session-end extraction, and nine native memory/reminder tools. Alexandria-only curated memory is supported through Hermes' native-store configuration flags.
- `GET /health` with a live storage probe and a one-second deadline; unavailable or timed-out probes return HTTP 503.
- `just hermes-test` and a standalone Hermes companion CI job, with hermetic host-stub, HTTP JSON/SSE, capture-policy, and lifecycle regression tests.

### Changed

- Use 4.0 expression sorting for tag count instead of the old projection workaround. Preserve tag occurrence counts despite 4.0's deduplicating `array::group`; retain the shared live-record filter, deterministic order, and cap.
- Add a frozen, separate-process 3.3 physical-store fixture covering hidden rows, identical vectors, metadata, identity, edges, sessions, heat, and reminders; test default refusal, opt-in copied-store conversion, index rebuild/readiness/plan, and unchanged source bytes.

### Fixed

- Hermes reminder scheduling, pending listing, and delivery text now use Alexandria's canonical API fields. Automatic consumption occurs only in primary foreground prefetch, separately from semantic caching, retaining notes and escalation/missed-occurrence context. Hermes skips this hook on trivial prompts; delivery then waits for a nontrivial foreground prompt or explicit check.
- Hermes recall searches across sessions while writes retain session attribution, and the client enforces its configured similarity floor.
- Hermes capture opt-outs survive setup/config persistence, failed stores no longer poison deduplication, and foreign memory services cannot suppress local extraction.
- Hermes terminal error-resolution capture recognizes real command exit envelopes without treating informational nonzero exits as successful fixes; corrected Alexandria memory-tool calls can also complete tracked resolutions.
- Hermes session-end capture, rewind/compression tracking, and shutdown are ordered without closing an active client. Failed pending writes are not advertised as persisted.

### Removed

- Hermes built-in-memory write mirroring. Existing files and previously stored mirror facts are left untouched; coexistence does not synchronize the two stores.
