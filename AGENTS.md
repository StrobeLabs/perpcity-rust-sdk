# Working in perpcity-rust-sdk

This crate is the truth about the chain for Perp City markets, and it is
a product: outside market makers will build on it. It is also
design-anchored: the design lives in the repo, beside the code, as a
graph of nodes, and it is part of what a change must keep true.

## Before you change anything

1. Read [`DESIGN.md`](DESIGN.md), the root node: the forces, the mental
   model, the type map, and the edges between components.
2. Read the `DESIGN.md` of the component you are changing (every
   top-level module has one beside its code). Follow its edges to the
   nodes it consumes from and provides to before you touch a type.
3. Check the node's terminology section for the word, and the root's for
   the cross-cutting ones. A new concept gets a new word with one home;
   an existing concept is called what it is already called.

## Rules that are not negotiable

- **Strategy belongs above.** No policy about when or how much to trade
  lives here. A `sol!` block, a storage slot, a port of contract math or a
  send pipeline belongs here; a decision does not.
- **Bindings match deployed bytecode**, verified live and locked in
  `abi_lock`. Events keep every era decodable. See `contracts/DESIGN.md`.
- **Money is exact.** Ported math is transcribed one-to-one in integers
  with the contract's rounding and anchored to a golden vector from real
  chain state. The f64 surface is for humans and is named so.
- **A read's tense is the receiver's.** `MarketReader` reads now;
  `StateAt` reads at a block. No read takes a block argument.
- **A value with an invariant is a validated value**: one constructor,
  one check, at the boundary the value entered. Loose fields that together
  mean something are a type waiting to exist.
- **Every failure is a typed variant with a stated transience.**
- **One implementation per concept.** Before adding a helper, search for
  the existing one and export it.

## When you change a component

- Update its `DESIGN.md` in the same change: the type table, the edges,
  the terminology, the debts. If the node became harder to write, say so
  in the PR; that is the design degrading and it is worth a conversation
  before it lands.
- A new or changed type gets a row in the node's type table, and its
  *Produced by* and *Consumed by* cells link the functions that make it
  and take it, with the reason it is shaped for them.
- A behaviour that a caller can observe gets a changelog entry under
  `[Unreleased]`, breaking changes first.
- Imports at the top of the file, grouped std / external / crate, never
  an inline path. Terse docs: one fact, one home, no history.
- Commit messages are one to three sentences of prose. No trailers.

## The gate

Every commit passes all four, in this order:

```bash
cargo fmt --all --check
cargo clippy --all-targets            # zero warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
```

The doc gate includes every `DESIGN.md`, so a type renamed without its
node updated fails here.
