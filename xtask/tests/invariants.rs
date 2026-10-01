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
    let mut nodes: Vec<Node> = ["math", "client", "errors"]
        .iter()
        .map(|n| Node {
            name: n.to_string(),
            path: PathBuf::from(format!("src/{n}/DESIGN.md")),
            text: String::new(),
        })
        .collect();
    // A root whose Invariants section lists exactly the predicates that
    // run, plus a plant: one sentence with no predicate behind it.
    let mut root = String::from("# root\n\n## Invariants\n\n");
    for opening in invariants::ENFORCED {
        root.push_str(&format!("- {opening}, and so on.\n"));
    }
    root.push_str("- Every type is a joy to use.\n\n## Efficiency\n");
    nodes.push(Node {
        name: String::new(),
        path: PathBuf::from("DESIGN.md"),
        text: root,
    });
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
        (
            "invariant list: the root node lists \"Every type is a joy to use.\" as enforced, but no predicate runs for it",
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

/// A predicate whose sentence is missing from the root is reported, so the
/// list cannot fall behind the code.
#[test]
fn a_predicate_without_its_sentence_is_reported() {
    let root = Node {
        name: String::new(),
        path: PathBuf::from("DESIGN.md"),
        text: "# root\n\n## Invariants\n\n- Every snapshot in `math` carries a `BlockContext`.\n\n## Efficiency\n".into(),
    };
    let index = fixture();
    let (graph, mut nodes) = tables(&index);
    nodes.retain(|n| !n.is_root());
    nodes.push(root);
    let mut problems = Vec::new();
    invariants::enforced(&index, &graph, &nodes, &mut problems);
    let missing = problems
        .iter()
        .filter(|p| p.contains("has no bullet opening with"))
        .count();
    assert_eq!(missing, invariants::ENFORCED.len() - 1, "{problems:?}");
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
    // A cycle where the debts name one of the pair for another reason.
    head.two_cycles
        .insert(("client::C".into(), "client::D".into()));
    head.flows.insert(
        ("math::X".into(), "math::Y".into()),
        ["convert_x".to_string()].into(),
    );
    head.unnamed.insert(("math::X".into(), "math::Y".into()));
    // A type whose shape changed with its row untouched, one whose row
    // moved with it, and one whose shape is the same.
    for (t, shape, row) in [
        ("math::Reshaped", "new(i32) -> Self", "the same row"),
        ("math::Rewritten", "new(i32) -> Self", "the same row"),
        ("math::Same", "as_is() -> u8", "the same row"),
    ] {
        base.shapes
            .insert(t.into(), ["old() -> u8".to_string()].into());
        base.rows.insert(t.into(), row.into());
        head.shapes.insert(t.into(), [shape.to_string()].into());
        head.rows.insert(t.into(), row.into());
    }
    base.shapes
        .insert("math::Same".into(), ["as_is() -> u8".to_string()].into());
    head.rows
        .insert("math::Rewritten".into(), "a new row".into());
    let node = Node {
        name: "math".into(),
        path: PathBuf::from("src/math/DESIGN.md"),
        text: "# math\n\n## Debts\n\n- **`NamedIsland` is an island** on purpose.\n- The pair `A` and `B`.\n- `C` is slow.\n\n## Terminology\n\nStub is not a debt here.\n".into(),
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
    assert!(
        problems
            .iter()
            .any(|p| p.contains("math::Reshaped's own methods or fields changed and its row")),
        "{problems:?}"
    );
    assert!(
        !problems.iter().any(|p| p.contains("Rewritten")),
        "the row moved with the type: {problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("client::C and client::D now flow both ways")),
        "one name is not the pair: {problems:?}"
    );
    assert!(
        !problems.iter().any(|p| p.contains("math::Same")),
        "unchanged shape: {problems:?}"
    );
    assert_eq!(problems.len(), 5, "{problems:?}");
}
