# Dependency updates and outstanding policy gates

The integrated feature adopts main's `RUSTSEC-2026-0387` ignore and the two
named BUSL-1.1 license exceptions below. This accepts an existing risk and two
specific licenses; it does not fix the crypto issue or qualify the release.
All-target/all-feature and workspace unmaintained checks remain in force.
The main-branch policy result and test counts on this page are historical, not
results for the combined feature.

The main-branch update based on `64c25cb68b03a07e120f6ad6ac5ddf5bb699753c` had two phases:

1. Compatible patches delivered in `c62edc3bb533f13dd54271ee5a264de70ce59977`
   reduced the original **20 errors to 3: 17 removed, none added**, under
   byte-identical policy and RustSec snapshot
   `ee1a49a2769a24a596afc1c20cb33748d2769586`.
2. The official engine family is now exactly **SurrealDB 4.0.1**, with resolved
   **SurrealKV 1.2.0** and independently versioned protocol **0.14.1**. This
   addresses the two remaining engine/backend advisories. The third, crypto
   advisory remained in the graph. A newly selected zstd-sys vulnerability was patched
   rather than ignored.

**Main's `just deny` passed under its changed policy. This was not an advisory-free
dependency graph.** That scan used RustSec `02e2517f457a359fc6d3440ea4560c8ea188fee8`.
The combined graph requires its own all-target/all-feature scan under the
current policy. Do not present the original two main-branch phases as a
same-policy “20 → 0 fixed.”

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
| `surrealdb-core` | 3.3.0 | 4.0.1 | RUSTSEC-2026-0454 |
| `surrealkv` | 0.21.4 | 1.2.0 | RUSTSEC-2026-0452 |
| `zstd-sys` selected by the 4.0 resolution | 2.1.1+zstd.1.5.7 | 2.1.3+zstd.1.5.7 | RUSTSEC-2026-0410; patched range starts at 2.1.2 |
| `url` (workspace and curator) | 2.5.8 | 2.5.9 with `form_urlencoded` 1.2.3 | RUSTSEC-2026-0592 |

The 17 compatible fixes remain at their delivered versions. Updates use precise
package selections, not an unconstrained workspace-wide refresh. The direct
Tokio, rmcp and Candle Core requirements start at patched releases. Candle Core
0.11.1 admits ZIP 9.x and adapts its filename API upstream; Alexandria needs no
ZIP API migration. Application feature flags and the pipeline's direct tokenizer
pin **0.22.2** are unchanged. Candle's separate 0.23.2 GGUF tokenizer helper is
not called by Alexandria's MiniLM/BERT or Granite paths.

These are not only version-line edits: the patched parents add schema-cache
hashing, tokenizer and ZIP platform helpers, including `typed-path 0.13.0`;
4.0 reorganizes the official engine family and changes the persistent format.
All resolved engine-family packages are 4.0.1. The backend is deliberately held
to the patched 1.2.0 used by the published 4.0.1 package lock, rather than taking
an unnecessary 1.4.0 refresh. No forks, Git sources or source patches are used.
The standalone curator lock retains the corresponding Tokio/TLS patches from
the compatible phase; those are not counted as additional advisory removals.

## Accepted crypto risk: RUSTSEC-2026-0387

The feature adopts exactly the advisory ignore already on main. The selected
engine has this route:

```text
SurrealDB 4.0.1 core/expr
  → jsonwebtoken 10.4.0 (WASM rust_crypto)
  → p256 0.13.2 / p384 0.13.1
  → elliptic-curve 0.13.8
  → crypto-bigint 0.5.5
```

The patched bigint range begins at **0.7.6**. A precise leaf update is outside
`elliptic-curve`'s `^0.5` requirement. Current curve 0.14.3 admits bigint
`^0.7.8`, and JWT 12 admits the newer curve parents, but the official engine's
JWT `^10.4.0` requirement excludes JWT 12. Actual precise-update probes rejected
those incompatible selections. That is the rationale for the waiver under
this dependency route, not evidence that the crypto bug was fixed or harmless.

All-feature inverse-tree checks for these exact native targets found no path
to `crypto-bigint 0.5.5`; JWT uses its native AWS-LC route there:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

`wasm32-unknown-unknown` retains the chain above. These are dependency-graph
observations, **not** native side-channel exploitability tests, cross-platform
runtime certification, or a claim that the old native 3.3 graph was safe.
The full policy scan remains all-target/all-feature; it is not narrowed to those
native targets. Authentication is not removed or disabled.

Any future risk decision must revisit the actual all-target graph and available
official engine releases. A native-only runtime or tree does not clear the full-policy gate.

## Named license exceptions

The feature adopts the existing BUSL-1.1 allowances for
`surrealdb-parser-next` and `surrealdb-stored-derive`. Their BUSL text matches
the previously reviewed engine
license (SHA-256
`c6ced847ed26f18394b2bfdd73909f50b0a394d639305d592136014a5e6d262a`,
change date 2030-11-03). There is no blanket BUSL allowance or unrelated license
exception in either policy. Bans, sources and audit targets were not narrowed.
The old quick-xml exceptions did not appear in main's scan.

## Storage compatibility and validation

A 4.x-touched store is not rollback material for stock 3.3. Follow the
[tested offline copy-first procedure](surrealdb-4-upgrade.md), including explicit
experimental admission, mandatory HNSW rebuild, complete logical/identity and
vector/centroid verification, and rollback from an untouched backup only.
No production/user store, service configuration or deployment was changed.
There is no new migration CLI or automatic startup rebuild.

Minimal query compatibility changes preserve the existing behavior:

- 4.0 rejects approximate KNN without an index instead of 3.3's silent empty
  result; the index-selection guard and meaningful EXPLAIN assertions remain.
- 4.0 accepts expression sorting, so tag-count sorting no longer needs an extra
  alias projection. The closed literal allowlist, bound filters and unique
  secondary key remain.
- 4.0's `array::group` deduplicates. Reading only the tags column preserves the
  existing Rust histogram, including repeated tags. It remains one query and
  O(total tag occurrences), with O(number of facts) row-container overhead
  instead of one flat aggregate result; no per-fact query is introduced.
- Main quoted a missing-cluster fixture via `ToSql`; the feature passes the
  typed record ID directly instead.

| Historical main-branch 4.0.1 validation | Result |
| --- | --- |
| Full `just deny` | Passed on main under its historical exceptions and advisory snapshot; not evidence for the combined graph. |
| Complete storage suite | Passed, including fresh-store/index tests and the frozen 3.3 physical-copy regression. |
| Official 3.3/4.0.1 operators | Passed: conversion, rebuild/readiness/KnnScan, strict application JSON equality, vector/centroid preservation, source/LOCK receipts, graceful shutdown, default 4.0.1 reopen without opt-in. |
| Exact `just ci` | Passed on the 4.0.1 state, using the worktree's own target and the recipe's flags. |
| Workspace all-feature / CPU-only tests | Passed: 647 / 651 tests respectively, including storage and MCP. |
| Formatting, Clippy and CPU dependency guard | Passed; Clippy used the recipe-owned `RUSTFLAGS="-Dwarnings"`; the dependency guard's six tests passed. |
| Standalone curator | Passed formatting, Clippy, five hermetic tests and full cargo-deny. |
| Pi companion / evaluator / vendored assets | Passed typecheck and 179 tests / ten tests / both asset checksums. |
| Independent final review | Passed bounded source/test/operator review and a separate narrow check of the actual lockfile/policy delta; no concrete defects. Binary fixture provenance/hashes and runtime gate outcomes are established by the execution evidence, not the read-only review. |

Rust builds use the isolated worktree's own target and normal Cargo flags;
Clippy's recipe owns its warning flags. The procedure is fixture-scale proof,
not an assertion that every operating system, backend or production corpus has
been tested.
