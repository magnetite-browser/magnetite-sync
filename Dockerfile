# The sync server as one static binary on an empty image: no shell, no libc,
# no distribution packages, only the server and its licence notices
# (README.md, "Licence").
# The build image is pinned by digest (the multi-arch index of 1.99-alpine);
# bump tag and digest together.
FROM docker.io/library/rust:1.99-alpine@sha256:44819b27be4c97b0e4d7203bd2e02c4820f97700211b6a37fb4c246a2de59383 AS build
# python3 writes the third-party notices.
RUN apk add --no-cache musl-dev python3
WORKDIR /src
COPY . .
# The image's toolchain is the pinned version; skip rustup's own switch.
RUN rm rust-toolchain.toml \
 && cargo build --release --locked -p magnetite-sync \
 && mkdir -p /out/licenses \
 && cp target/release/magnetite-sync /out/ \
 && python3 tools/third-party-notices.py >/out/licenses/THIRD_PARTY_NOTICES.md \
 && cp LICENSE licenses/musl-COPYRIGHT /out/licenses/ \
 && cp "$(rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html" /out/licenses/

FROM scratch
ARG REVISION=unknown
LABEL org.opencontainers.image.title="magnetite-sync" \
      org.opencontainers.image.description="Zero-knowledge, end-to-end encrypted sync server for the Magnetite browser" \
      org.opencontainers.image.licenses="EUPL-1.2" \
      org.opencontainers.image.source="https://github.com/magnetite-browser/magnetite-sync" \
      org.opencontainers.image.revision="${REVISION}"
COPY --from=build /out/magnetite-sync /magnetite-sync
COPY --from=build /out/licenses /licenses
USER 10001:10001
# The server writes no files; its data is in Postgres (MAGNETITE_SYNC_DATABASE_URL).
ENV MAGNETITE_SYNC_BIND=0.0.0.0:8787 RUST_LOG=info
EXPOSE 8787
ENTRYPOINT ["/magnetite-sync"]
