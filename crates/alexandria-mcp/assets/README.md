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
