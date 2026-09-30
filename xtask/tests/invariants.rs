//! Each enforced invariant can fail: rustdoc documents the fixture crate,
//! the same index and predicates run over it, and every planted violation
//! is reported, with nothing beside it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::design::{Graph, TypeRow};
use xtask::index::{Index, Kind};
use xtask::invariants;
use xtask::nodes::Node;
use xtask::rustdoc;

/// Document the fixture with rustdoc and load it.
fn fixture() -> Index {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = manifest.join("tests/fixture/lib.rs");
    let out = manifest.join("../target/design/fixture");
    fs::create_dir_all(&out).unwrap();
    let toolchain = rustdoc::toolchain();
    let status = Command::new("rustup")
        .args([
            "run",
            &toolchain,
            "rustdoc",
            "--edition",
            "2024",
            "--crate-name",
            "fixture",
            "--crate-type",
            "lib",
        ])
        .args(rustdoc::FLAGS.split(' '))
        .arg("-o")
        .arg(&out)
        .arg(&source)
        .status()
        .expect("rustup runs");
    assert!(status.success(), "rustdoc documents the fixture");
    Index::build(
        rustdoc::parse(&out.join("fixture.json"), &toolchain).expect("the fixture's JSON parses"),
    )
}

/// A graph whose tables name every drawn type once, except the two plants.
fn tables(index: &Index) -> (Graph, Vec<Node>) {
    let mut rows = Vec::new();
    for e in index.sorted() {
        if !e.kind.is_type() || e.name == "Orphan" {
            continue;
        }
        let node = e.top().to_string();
        let row = |node: String| TypeRow {
            node,
            subject: e.id,
            companions: vec![],
            invariant: String::new(),
            produced: String::new(),
            consumed: String::new(),
            public: false,
        };
        rows.push(row(node.clone()));
        if e.name == "Twice" {
            rows.push(row("client".into()));
        }
    }
    let nodes: Vec<Node> = ["math", "client", "errors"]
        .iter()
        .map(|n| Node {
            name: n.to_string(),
            path: PathBuf::from(format!("src/{n}/DESIGN.md")),
            text: String::new(),
        })
        .collect();
    let graph = Graph {
        nodes: BTreeSet::new(),
        edges: Default::default(),
        rows,
        claims: BTreeSet::new(),
    };
    (graph, nodes)
}

#[test]
fn every_invariant_fires_on_its_plant_and_nowhere_else() {
    let index = fixture();
    assert!(
        index
            .entries
            .values()
            .any(|e| e.name == "MarketReader" && e.kind == Kind::Struct),
        "the fixture indexed"
    );
    let (graph, nodes) = tables(&index);
    let mut problems = Vec::new();
    invariants::enforced(&index, &graph, &nodes, &mut problems);

    let expected = [
        (
            "no read takes a block argument",
            "MarketReader::get_capacity_at",
        ),
        ("every snapshot carries a BlockContext", "MarketCapacity"),
        (
            "a validated value comes only through a fallible constructor",
            "TickRange::raw",
        ),
        ("math takes no provider or handle", "PricePair::from_reader"),
        (
            "every fallible function returns one of the crate's errors",
            "MarketReader::dump",
        ),
        (
            "every error variant states its transience",
            "ContractError::EventNotFound",
        ),
        (
            "every public type is in one node's table, but math::Orphan is in none",
            "",
        ),
        (
            "every public type is in one node's table, but math::Twice is in",
            "",
        ),
    ];
    for (invariant, item) in expected {
        assert!(
            problems
                .iter()
                .any(|p| p.contains(invariant) && p.contains(item)),
            "expected `{invariant}` to name `{item}`; got:\n{}",
            problems.join("\n")
        );
    }
    assert_eq!(
        problems.len(),
        expected.len(),
        "only the plants fire:\n{}",
        problems.join("\n")
    );
}

/// The ratchet fires on what is new and unacknowledged, and on nothing
/// that a debts section names or that the base already had.
#[test]
fn the_ratchet_fires_on_new_unacknowledged_structures_only() {
    use xtask::summary::Summary;
    let mut base = Summary::default();
    base.islands.insert("math::OldIsland".into());
    let mut head = base.clone();
    head.islands.insert("math::NewIsland".into());
    head.islands.insert("math::NamedIsland".into());
    head.dead_ends.insert("hft::Stub".into());
    head.two_cycles
        .insert(("client::A".into(), "client::B".into()));
    head.flows.insert(
        ("math::X".into(), "math::Y".into()),
        ["convert_x".to_string()].into(),
    );
    head.unnamed.insert(("math::X".into(), "math::Y".into()));
    let node = Node {
        name: "math".into(),
        path: PathBuf::from("src/math/DESIGN.md"),
        text: "# math\n\n## Debts\n\n- **`NamedIsland` is an island** on purpose.\n- The pair `A` and `B`.\n\n## Terminology\n\nStub is not a debt here.\n".into(),
    };
    let mut problems = Vec::new();
    invariants::ratchet(&base, &head, &[node], &mut problems);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("math::NewIsland became an island")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("hft::Stub became a dead end")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("math::X now flows into math::Y through convert_x")),
        "{problems:?}"
    );
    assert!(
        !problems.iter().any(|p| p.contains("NamedIsland")),
        "acknowledged: {problems:?}"
    );
    assert!(
        !problems.iter().any(|p| p.contains("OldIsland")),
        "already in the base: {problems:?}"
    );
    assert!(
        !problems
            .iter()
            .any(|p| p.contains("client::A and client::B")),
        "the pair is named: {problems:?}"
    );
    assert_eq!(problems.len(), 3, "{problems:?}");
}
