# Partial dependency-advisory remediation

This is a security-status snapshot for the dependency update based on commit
`64c25cb68b03a07e120f6ad6ac5ddf5bb699753c`, not a claim that Alexandria is
advisory-free. The original workspace `just deny` reported 20 advisory errors.
The after-scan reports **3 advisory errors: 17 removed, no new errors**.
This change deliberately preserves the embedded storage engine and leaves its
unresolved advisories in place. **Workspace cargo-deny remains failing.**

## Targeted updates

| Package | Before | After | Advisory IDs addressed |
| --- | --- | --- | --- |
| `ammonia` | 4.2.3 | 4.2.5 | RUSTSEC-2026-0373, RUSTSEC-2026-0533 |
| `rmcp` / `rmcp-macros` | 3.5.1 | 3.9.0 | RUSTSEC-2026-0457, RUSTSEC-2026-0461, RUSTSEC-2026-0462 (`rmcp`) |
| `rustls` | 0.23.45 | 0.23.46 | RUSTSEC-2026-0346, RUSTSEC-2026-0347, RUSTSEC-2026-0348, RUSTSEC-2026-0349 |
| `rustls-webpki` | 0.103.15 | 0.103.16 | RUSTSEC-2026-0382 |
| `webpki-roots` | 1.0.9 | 1.0.10 | RUSTSEC-2026-0381 |
| `webpki-root-certs` | 1.0.9 | 1.0.10 | RUSTSEC-2026-0383 |
| `tokio` | 1.53.2 | 1.53.3 | RUSTSEC-2026-0386 |
| `candle-core` → `zip` | 0.11.0 → 8.6.0 | 0.11.1 → 9.1.3 | RUSTSEC-2026-0367, RUSTSEC-2026-0368, RUSTSEC-2026-0369, RUSTSEC-2026-0429 (`zip`) |

Updates use precise package selections, not a workspace-wide `cargo update`.
The direct Tokio, rmcp and Candle Core requirements now start at the selected
patched releases. Candle Core 0.11.1 is a compatible parent patch that admits
ZIP 9.x; its upstream code adapts the changed ZIP filename API, so Alexandria
needs no ZIP API migration. No application feature flags are changed. The
pipeline's directly selected tokenizer remains exactly 0.22.2. Candle also
adds `tokenizers 0.23.2` for its GGUF tokenizer helper; neither Alexandria's
MiniLM/BERT path nor its Granite path calls that helper. Both continue loading
their tokenizer through the pipeline's exact-pinned 0.22.2 dependency.

The patched parents necessarily add transitive dependencies: rmcp's schema
cache uses SHA-2 0.11 and its digest family; Candle adds its tokenizer helper
and uses the already-resolved `parking_lot`; ZIP adds platform/filesystem
helpers and updates `typed-path` to 0.13. Cargo also reselects some edges among
already-locked compatible versions (`getrandom`, `hashbrown` and Windows support
crates), within the unchanged upstream requirements. This is not solely a set
of advisory-package version-line edits.

The standalone `tools/granite-artifact/Cargo.lock` receives the corresponding
Tokio, rustls, webpki and root-bundle patches. Its original `just artifact-deny`
already passed: affected-version entries existed in its lockfile without
producing errors in its selected graph. These updates are not counted as
additional advisory removals. `webpki-roots 0.26.11` and dormant `zip 0.6.6`
entries remain; they are not among the workspace's reported advisory errors.

## Known unresolved errors

| Advisory | Package retained | Patched range | Why it remains |
| --- | --- | --- | --- |
| [RUSTSEC-2026-0454](https://rustsec.org/advisories/RUSTSEC-2026-0454.html) | `surrealdb-core 3.3.0` | `>=4.0.0` | The advisory is against the engine itself, not just its `reblessive` dependency. Clearing it requires crossing the explicitly preserved engine boundary. |
| [RUSTSEC-2026-0452](https://rustsec.org/advisories/RUSTSEC-2026-0452.html) | `surrealkv 0.21.4` | `>=1.2.0` | `surrealdb-kvs-surrealkv 3.3.0` requires `^0.21.4`; the patched backend is outside that range. |
| [RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387.html) | `crypto-bigint 0.5.5` | `>=0.7.6` | The engine's `jsonwebtoken` → P-256/P-384 → `elliptic-curve 0.13.8` chain requires `crypto-bigint ^0.5`. No published stable 0.13.x elliptic-curve patch admits the fixed release. A breaking crypto-parent migration is outside this partial update. |

All 26 resolved SurrealDB engine packages stay at **3.3.0**. The independently
versioned `surrealdb-protocol` stays at **0.13.1**; `surrealkv`, `surrealmx` and
`reblessive` are also unchanged. No user or production database is opened,
upgraded or migrated by this change. Engine-major remediation and cross-version
storage-conversion validation are deferred to a separate upgrade effort; this
partial update does not change startup or migration behavior. No forks, patches,
new advisory ignores or license-policy changes are introduced. Presence in the dependency graph establishes a failing
security check, not exploit reachability in Alexandria.

## Validation

Fresh `just deny` scans of the baseline and this update used byte-identical
`deny.toml` and RustSec snapshot
`ee1a49a2769a24a596afc1c20cb33748d2769586`: **20 → 3 advisory errors,
17 removed, no new errors**. Bans, licenses and sources pass. The three remaining
IDs are enumerated above, not ignored or reclassified.

| Compatibility gate | Result |
| --- | --- |
| `just test` (all features) | Passed in the earlier normal-flags run on the same code and dependency state; that evidence is retained rather than duplicating the full build. |
| `just test-cpu` | Passed. |
| `just lint` | Passed with the recipe's warning-as-error flags. |
| `just fmt` | Passed. |
| `just cpu-deps` | Passed, including the CPU-only dependency graph guard. |
| `just artifact-ci` | Passed: standalone curator formatting, Clippy, hermetic tests and cargo-deny. |
| `just ext-test` | Passed: client typecheck and tests. |
| `just eval-test` | Passed. |
| `just verify-assets` | Passed. |

Rust checks use the isolated worktree's target directory; test runs do not
replace the host's normal Cargo flags with global warning flags. An independent
bounded read-only review found no substantive dependency, compatibility, policy
or scope defect. Its pending validation/documentation items are resolved by the
results above. **This is not a green `just ci`: workspace `just deny` still exits
1 for the three explicitly retained advisories.**
