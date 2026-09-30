//! Repo tooling. `cargo xtask design` reads the design nodes' type tables,
//! resolves every name against rustdoc's JSON, verifies what the tables
//! claim against the real signatures, checks the graph-level invariants,
//! and draws the type graph as an interactive page. The modules are public
//! so the fixture test in `tests/` can run the same code over a planted
//! crate.

pub mod design;
pub mod index;
pub mod invariants;
pub mod nodes;
pub mod page;
pub mod report;
pub mod rustdoc;
pub mod summary;
