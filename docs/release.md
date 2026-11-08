# Releasing

A release is a pull request that raises the version; merging it publishes it.

1. In one pull request: the new `version` in `Cargo.toml`
   (`[workspace.package]`; `cargo check` updates `Cargo.lock`), the same
   `version` and `appVersion` in `charts/magnetite-sync/Chart.yaml`, and a
   section `## <version> (<date>)` in `CHANGELOG.md`.
2. Merge it. The release workflow sees a version on `main` without a tag and
   waits for approval in the `release` environment (its required reviewers).
   Then it builds the container images for amd64 and arm64, starts each once,
   pushes `ghcr.io/magnetite-browser/magnetite-sync:<version>` and the chart
   (`oci://ghcr.io/magnetite-browser/charts/magnetite-sync`), and publishes
   the GitHub release `v<version>`: the static binaries from the images
   (`magnetite-sync-<version>-linux-<arch>.tar.gz`, with their licence
   files), `SHA256SUMS` and the changelog section as notes. Publishing it
   creates the tag. If a run fails, re-run it from the Actions tab; the next
   push to `main` tries again too.
3. Update the public instance (`docs/public-instance.md`).

The browser takes `magnetite-sync-proto`, and the server for its sync test,
from this repository at a release tag. A protocol change is released here
first; the browser then moves to the new tag.

The protocol version is part of every path (`/v2`) and of every signature.
An incompatible change gets a new path; the server keeps answering the old
one until the browser versions that use it are gone.
