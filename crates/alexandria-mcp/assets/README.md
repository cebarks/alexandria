# Vendored debug-UI assets

Third-party browser scripts used by the `/debug` web UI, committed to the repository rather than
fetched from a CDN at runtime.

## Why these are vendored

The debug UI must work on an air-gapped or egress-restricted host — a memory server holding data
that cannot leave the machine is a first-class deployment, and a UI that silently renders as a blank
page without internet access is worse than no UI. Loading `<script src="https://unpkg.com/...">`
also means executing unsigned third-party code on every page view with no `integrity` attribute and
no pinning beyond a version string, which is an unnecessary remote-trust dependency for a local
admin surface.

Vendoring removes both problems: the bytes are pinned in-tree, their identity is fixed by
`SHA256SUMS`, and the debug UI can therefore serve them itself instead of trusting a CDN.

## Inventory

| File | Version | Upstream URL | License |
| --- | --- | --- | --- |
| `htmx-2.0.10.min.js` | 2.0.10 | <https://unpkg.com/htmx.org@2.0.10/dist/htmx.min.js> | MIT |
| `vis-network-10.1.2.min.js` | 10.1.2 | <https://unpkg.com/vis-network@10.1.2/standalone/umd/vis-network.min.js> | MIT OR Apache-2.0 |

Retrieved: **2026-09-11**.

Both licenses are permissive and compatible with this project's AGPL-3.0-or-later as inbound
contributions. `vis-network` is dual-licensed and may be used under either term; the `standalone/umd`
build is used because the v10 ESM/CJS packaging change does not apply to it.

### Bundled transitive dependencies

The `vis-network` `standalone/umd` build inlines its dependencies, so their licenses travel with it.
Recorded here because a license audit of this repository will not find them in `Cargo.lock` or via
`cargo deny` — these bytes are not a crate dependency.

| Bundled in | Component | Version | License |
| --- | --- | --- | --- |
| `vis-network-10.1.2.min.js` | Hammer.JS | 2.0.17-rc (2019-12-16) | MIT |

`htmx-2.0.10.min.js` has no runtime dependencies.

## Byte-for-byte fidelity

These files are committed **exactly as upstream publishes them** and are never edited, minified
further, or stripped. Both end with a `//# sourceMappingURL=` trailer pointing at a `.map` file that
is deliberately *not* vendored, so browser devtools will 404 on the source map. That is accepted:
modifying the bytes to remove the trailer would break the guarantee that `SHA256SUMS` can be checked
against the upstream release, which is worth more than a silent devtools 404 on an internal admin
surface. Vendoring the maps instead would roughly triple the committed size for no runtime benefit.

## Verifying

```sh
cd crates/alexandria-mcp/assets && sha256sum -c SHA256SUMS
```

Or from the repository root:

```sh
just verify-assets
```

`SHA256SUMS` is in `sha256sum -c` format (hash, two spaces, filename). Any mismatch means the bytes
on disk are not the bytes that were reviewed — stop and investigate rather than rewriting the sums.

## Refreshing

```sh
just vendor-assets
```

This re-downloads the exact pinned versions above and re-runs `sha256sum -c SHA256SUMS`, so a
refresh that changes the bytes fails instead of silently updating the record. To move to a new
version, change the URLs and filenames in the `justfile` recipes, delete the stale file, and commit
the updated `SHA256SUMS` alongside it — the sums are the review artifact, so a hash change belongs
in a commit that says why.
