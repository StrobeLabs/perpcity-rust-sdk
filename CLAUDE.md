# perpcity-rust-sdk — assistant guidelines

## What this crate is

The single source of truth for everything that is true of the CHAIN:
contract bindings, contract math (maker equity, tick/liquidity math),
storage-slot reads, event decoding, and the nonce-managed send pipeline.
Strategy (when/how much to trade, treasury policy, guards) belongs in
Legion, never here. This crate is also a product: it may be handed to
outside market makers, so keep it ergonomic and general.

## Rules

- **Bindings match DEPLOYED bytecode, not contracts-repo HEAD.** When
  they disagree, the deployed chain wins, and the fix goes in the core
  binding — never a parallel workaround interface. Verify against a live
  market before changing a binding (a 2-arg-vs-3-arg selector probe:
  typed revert = selector exists, empty revert = falls through). Lock
  every shape in the `abi_lock` tests, with the on-chain evidence in a
  comment.
- **Events are the one era exception**: logs from a pre-upgrade era stay
  on-chain forever, so both eras' shapes must stay decodable
  (see `PerpDeployedEvents`). Calls target only what is deployed.
- Anchor new contract math to golden vectors from real on-chain data
  (see the `maker_equity` golden test), not to synthetic fixtures alone.
- One implementation per concept crate-wide (e.g. keccak mapping-slot
  math lives in `maker_equity` and is delegated to elsewhere). Before
  adding a helper, search for an existing private one and export it
  instead.
- `cargo fmt --check`, `cargo clippy --all-targets` and `cargo test`
  must be green before any commit; missing-docs warnings are errors in
  spirit — document public items.
