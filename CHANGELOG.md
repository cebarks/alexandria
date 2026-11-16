# Changelog

## [Unreleased]

### Security

- Update ammonia, rmcp, Tokio, TLS/root bundles, Candle Core/ZIP, and zstd-sys to patched releases; upgrade the official SurrealDB family to exactly 4.0.1 and SurrealKV to 1.2.0. See [dependency advisory status](docs/dependency-advisories.md).
- Explicitly accept `RUSTSEC-2026-0387` on the official engine's WASM crypto dependency route. This is a documented risk exception, **not** a crypto fix. Add only the approved package-specific BUSL exceptions for `surrealdb-parser-next` and `surrealdb-stored-derive`.

### Breaking

- The embedded storage engine is now SurrealDB 4.0.1. Do not point it at an original 3.x store. Follow the [offline copy-first upgrade procedure](docs/surrealdb-4-upgrade.md), including explicit experimental conversion on a disposable copy, mandatory HNSW rebuild, and data/identity verification. Rollback requires an untouched 3.x backup and the old binary; a 4.x-touched target is not a rollback source.

### Changed

- Use 4.0 expression sorting for tag count instead of the old projection workaround. Preserve tag occurrence counts despite 4.0's deduplicating `array::group`; retain the shared live-record filter, deterministic order, and cap.
- Add a frozen, separate-process 3.3 physical-store fixture covering hidden rows, identical vectors, metadata, identity, edges, sessions, heat, and reminders; test default refusal, opt-in copied-store conversion, index rebuild/readiness/plan, and unchanged source bytes.
