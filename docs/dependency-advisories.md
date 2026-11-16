# Dependency-advisory remediation and accepted risk

The update based on `64c25cb68b03a07e120f6ad6ac5ddf5bb699753c` has two phases:

1. Compatible patches delivered in `c62edc3bb533f13dd54271ee5a264de70ce59977`
   reduced the original **20 errors to 3: 17 removed, none added**, under
   byte-identical policy and RustSec snapshot
   `ee1a49a2769a24a596afc1c20cb33748d2769586`.
2. The official engine family is now exactly **SurrealDB 4.0.1**, with resolved
   **SurrealKV 1.2.0** and independently versioned protocol **0.14.1**. This
   addresses the two remaining engine/backend advisories. The third, crypto
   advisory is **explicitly accepted, not fixed**. A newly selected zstd-sys
   vulnerability is patched rather than ignored.

**Full `just deny` passes all four categories under the approved policy. This
is not an advisory-free dependency graph.** The current scan used RustSec
`02e2517f457a359fc6d3440ea4560c8ea188fee8`; its policy differs from the original
only by the one new crypto ignore and two package-specific BUSL exceptions
listed below. Do not present the two phases as a same-policy “20 → 0 fixed.”

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

## Accepted crypto exception: RUSTSEC-2026-0387

`deny.toml` adds exactly this one advisory ignore. The vulnerable chain remains:

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
those incompatible selections. This is why the exception is necessary **under
the selected official dependency route**, not evidence that the crypto bug is
fixed or harmless.

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

**Reconsider** the acceptance before introducing WASM support or if the
vulnerable route becomes reachable in a native graph. **Remove** the ignore
once an official engine dependency route admits patched crypto. Keep this as
an explicit risk acceptance until then, not a clean-audit claim.

## License policy

Exactly two named BUSL exceptions are added: `surrealdb-parser-next` and
`surrealdb-stored-derive`. Their BUSL text matches the already reviewed engine
license (SHA-256
`c6ced847ed26f18394b2bfdd73909f50b0a394d639305d592136014a5e6d262a`,
change date 2030-11-03). There is no blanket BUSL allowance or unrelated license
exception. Existing advisory exceptions are otherwise retained; the old
quick-xml exceptions are not encountered in the current scan. Bans, sources,
audit targets and authentication posture are unchanged.

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
- A missing-cluster fixture now obtains its quoted record literal through
  `ToSql` rather than assuming an unquoted hyphenated key is valid SQL.

| Current 4.0.1 validation | Result |
| --- | --- |
| Full `just deny` | Passed: advisories, bans, licenses and sources under the explicit policy above. |
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
