# Releasing

1. Update `version` in `Cargo.toml`, `version` and `appVersion` in
   `charts/magnetite-sync/Chart.yaml`, and `CHANGELOG.md`; run
   `cargo test --workspace`, commit.
2. Tag `v<version>` and push the tag. The `release` environment should have
   required reviewers: images and the chart are public as soon as they are
   pushed, before the draft release is checked. The release workflow builds the
   container images for amd64 and arm64, starts each once, pushes
   `ghcr.io/magnetite-browser/magnetite-sync:<version>` and the chart
   (`oci://ghcr.io/magnetite-browser/charts/magnetite-sync`) and creates a draft
   GitHub release with the static binaries from the images
   (`magnetite-sync-<version>-linux-<arch>.tar.gz`, with their licence
   files) and `SHA256SUMS`.
3. If `proto/` changed, publish it: `cargo publish -p magnetite-sync-proto`.
   The browser depends on the published crate, so a protocol change needs a
   new version of both.
4. Check the draft and publish it. Update the public instance
   (`docs/public-instance.md`).

The protocol version is part of every path (`/v2`) and of every signature.
An incompatible change gets a new path; the server keeps answering the old
one until the browser versions that use it are gone.
