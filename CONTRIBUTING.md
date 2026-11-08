# Contributing

Thanks for helping. Before a pull request:

- `cargo fmt --all`, `cargo clippy --workspace --all-targets` (no warnings)
  and `cargo test --workspace` pass. The tests need a Postgres they may
  create databases on, named by `DATABASE_URL`:
  `export "$(tools/test-postgres.sh)"` starts one in Docker.
- Commit subjects follow conventional commits, short: `fix(server): …`,
  `feat(proto): …`.
- Changes to the protocol or the cryptography (`proto/`) need a note in
  `docs/design.md` and tests that pin the wire format.
- A released migration (`server/migrations/`) never changes, not even a
  comment: servers refuse to start when one does. Schema changes go into a
  new file, and keep the previous release working (pods are replaced one at
  a time). Migrations run on a connection without the requests' timeouts.
- Security issues go to security@magnetitebrowser.org, not to public issues.

CI checks every pull request, builds the server for amd64 and arm64, starts
it with `compose.yml` and installs the Helm chart in kind; the binaries stay
on the run for 14 days. CodeRabbit reviews pull requests
automatically (`.coderabbit.yaml`); maintainers decide what gets merged.

Contributions come in under the EUPL-1.2 (inbound = outbound); you keep your
copyright. There is no CLA.
