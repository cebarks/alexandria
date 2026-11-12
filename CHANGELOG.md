# Changelog

## [Unreleased]

### Added

- Hermes Agent companion with automatic cross-session recall, heuristic capture, session-end extraction, and nine native memory/reminder tools. Alexandria-only curated memory is supported through Hermes' native-store configuration flags.
- `GET /health` with a live storage probe and a one-second deadline; unavailable or timed-out probes return HTTP 503.
- `just hermes-test` and a standalone Hermes companion CI job, with hermetic host-stub, HTTP JSON/SSE, capture-policy, and lifecycle regression tests.

### Fixed

- Hermes reminder scheduling, pending listing, and delivery text now use Alexandria's canonical API fields. Automatic consumption occurs only in primary foreground prefetch, separately from semantic caching, retaining notes and escalation/missed-occurrence context. Hermes skips this hook on trivial prompts; delivery then waits for a nontrivial foreground prompt or explicit check.
- Hermes recall searches across sessions while writes retain session attribution, and the client enforces its configured similarity floor.
- Hermes capture opt-outs survive setup/config persistence, failed stores no longer poison deduplication, and foreign memory services cannot suppress local extraction.
- Hermes terminal error-resolution capture recognizes real command exit envelopes without treating informational nonzero exits as successful fixes; corrected Alexandria memory-tool calls can also complete tracked resolutions.
- Hermes session-end capture, rewind/compression tracking, and shutdown are ordered without closing an active client. Failed pending writes are not advertised as persisted.

### Removed

- Hermes built-in-memory write mirroring. Existing files and previously stored mirror facts are left untouched; coexistence does not synchronize the two stores.
