//! The graph's numbers: what a change to the type system moves.

use std::collections::{BTreeMap, BTreeSet};

use rustdoc_types::Id;

use crate::design::{EdgeKind, Graph};
use crate::index::{Index, Kind};
use crate::invariants;
use crate::nodes::Node;

/// One of the report's questions: what it found, and what some node has
/// already answered by calling it accepted structure.
fn question(title: &str, questioned: &[String], accepted: &[String]) {
    println!(
        "\n{title}: {} questioned, {} accepted",
        questioned.len(),
        accepted.len()
    );
    if !questioned.is_empty() {
        println!("  {}", questioned.join(", "));
    }
    if !accepted.is_empty() {
        println!("  accepted: {}", accepted.join(", "));
    }
}

/// Print the graph's numbers and the structures worth a second look.
pub fn print(index: &Index, graph: &Graph, nodes: &[Node]) {
    let name = |id: Id| {
        index
            .entries
            .get(&id)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| index.name_of(id))
    };
    let top = |id: Id| {
        index
            .entries
            .get(&id)
            .map(|e| e.top().to_string())
            .unwrap_or_default()
    };
    let flows: Vec<_> = graph.flows().collect();
    let mut inn: BTreeMap<Id, usize> = BTreeMap::new();
    let mut out: BTreeMap<Id, usize> = BTreeMap::new();
    for e in &flows {
        *out.entry(e.src).or_default() += 1;
        *inn.entry(e.dst).or_default() += 1;
    }
    // Containment and accessors count as being consumed, for the questions
    // below, but not as flow.
    let mut held: BTreeSet<Id> = BTreeSet::new();
    for e in graph.edges.values() {
        if matches!(e.kind, EdgeKind::Carries | EdgeKind::Accessor) {
            held.insert(e.src);
            held.insert(e.dst);
        }
    }
    let types: Vec<Id> = graph
        .nodes
        .iter()
        .copied()
        .filter(|id| index.entries.get(id).is_some_and(|e| e.kind.is_type()))
        .collect();
    let in_tables: BTreeSet<Id> = graph
        .rows
        .iter()
        .flat_map(|r| std::iter::once(r.subject).chain(r.companions.iter().copied()))
        .collect();
    let public = graph.rows.iter().filter(|r| r.public).count();
    println!(
        "Type graph from the signatures: {} public types, {} flow edges ({} designed, named by a node), {} containment edges, {} function nodes, {} components; {} types in a table, {} on the strategy layer's surface",
        types.len(),
        flows.len(),
        graph.designed_count(index),
        graph
            .edges
            .values()
            .filter(|e| matches!(e.kind, EdgeKind::Carries | EdgeKind::Accessor))
            .count(),
        graph
            .nodes
            .iter()
            .filter(|id| index
                .entries
                .get(id)
                .is_some_and(|e| e.kind == Kind::Function))
            .count(),
        nodes.len() - 1,
        in_tables.len(),
        public
    );

    let mut hubs: Vec<Id> = types.clone();
    hubs.sort_by_key(|id| {
        (
            std::cmp::Reverse(
                inn.get(id).copied().unwrap_or(0) + out.get(id).copied().unwrap_or(0),
            ),
            name(*id),
        )
    });
    println!("\nHubs (in/out):");
    for id in hubs.iter().take(12) {
        println!(
            "  {:26} {:10} {:>3} {:>3}",
            name(*id),
            top(*id),
            inn.get(id).copied().unwrap_or(0),
            out.get(id).copied().unwrap_or(0)
        );
    }

    let answers = invariants::Answers::of(nodes);
    let sort = |ids: Vec<Id>| -> (Vec<String>, Vec<String>) {
        let mut questioned = Vec::new();
        let mut accepted = Vec::new();
        for id in ids {
            if answers.accepts(&top(id), &name(id)) {
                accepted.push(name(id));
            } else {
                questioned.push(format!("{} [{}]", name(id), top(id)));
            }
        }
        (questioned, accepted)
    };

    let (q, a) = sort(
        types
            .iter()
            .copied()
            .filter(|id| !inn.contains_key(id) && !out.contains_key(id) && !held.contains(id))
            .collect(),
    );
    question(
        "Islands, in no signature and no field with another of the crate's types",
        &q,
        &a,
    );

    let (q, a) = sort(
        types
            .iter()
            .copied()
            .filter(|id| {
                inn.contains_key(id)
                    && !out.contains_key(id)
                    && !held.contains(id)
                    && !graph
                        .rows
                        .iter()
                        .any(|r| (r.subject == *id || r.companions.contains(id)) && r.public)
            })
            .collect(),
    );
    question(
        "Dead ends, produced but consumed by nothing and not on the surface",
        &q,
        &a,
    );

    let set: BTreeSet<(Id, Id)> = flows.iter().map(|e| (e.src, e.dst)).collect();
    let (mut q, mut a) = (Vec::new(), Vec::new());
    for (x, y) in &set {
        if x < y && set.contains(&(*y, *x)) {
            let pair = format!("{} <-> {}", name(*x), name(*y));
            if answers.accepts_pair((&top(*x), &top(*y)), &name(*x), &name(*y)) {
                a.push(pair);
            } else {
                q.push(pair);
            }
        }
    }
    question("Two-cycles, a conversion that may be mis-homed", &q, &a);

    let undocumented: Vec<String> = flows
        .iter()
        .filter(|e| {
            in_tables.contains(&e.src)
                && in_tables.contains(&e.dst)
                && !graph.designed(e, index)
                && e.kind == EdgeKind::Flow
        })
        .map(|e| {
            format!(
                "{} -> {} ({})",
                name(e.src),
                name(e.dst),
                e.labels.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect();
    println!(
        "\nFlows between documented types that no node names: {}",
        undocumented.len()
    );
    for u in undocumented.iter().take(40) {
        println!("  {u}");
    }
    if undocumented.len() > 40 {
        println!("  ... and {} more", undocumented.len() - 40);
    }

    let mut coupling: BTreeMap<(String, String), usize> = BTreeMap::new();
    for e in &flows {
        let (a, b) = (top(e.src), top(e.dst));
        if a != b && !a.is_empty() && !b.is_empty() {
            *coupling.entry((a, b)).or_default() += 1;
        }
    }
    let mut coupling: Vec<_> = coupling.into_iter().collect();
    coupling.sort_by_key(|((a, b), n)| (std::cmp::Reverse(*n), a.clone(), b.clone()));
    println!("\nFlow edges between components:");
    for ((a, b), n) in coupling.iter().take(14) {
        println!("  {a:10} -> {b:10} {n}");
    }
}
