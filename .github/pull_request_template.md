## Problem

<!-- What is wrong or missing, in a sentence or two. Link the issue if there is one. -->

## Solution

<!-- How this change solves it, and anything a reviewer should look at closely. -->

## Testing

<!-- What you ran or tried, e.g. cargo test --workspace, a run against Magnetite. -->

## Checklist

- [ ] The title is a conventional commit with a short subject, e.g. `fix(server): …`
- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets` and `cargo test --workspace` pass
- [ ] Protocol changes are described in `docs/design.md` and pinned by tests
- [ ] Limits, variables or stored data changed: `README.md` says so
