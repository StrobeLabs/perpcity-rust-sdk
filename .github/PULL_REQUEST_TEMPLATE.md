<!-- One to three sentences: what changes and why. Prose, not a changelog. -->

## Design

<!-- Which DESIGN.md nodes this touches, and what changed in them (type
table, edges, terminology, debts). If a node got harder to write, say so.
The design job comments on the PR with what the change did to the type
graph; if it lists something under "Needs a decision", say here what you
decided and why. -->

## Evidence

<!-- For contract math: the golden vector. For a binding: the live
verification. For a behaviour change: what was measured. -->

## Gate

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets` with zero warnings
- [ ] `cargo test`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features`
- [ ] `cargo xtask design --check`, after `--fmt` if a type table changed
- [ ] `CHANGELOG.md` updated under `[Unreleased]` if a caller can observe the change
- [ ] Version bumped in `Cargo.toml` if the public API breaks (CI checks it against the last tag)
