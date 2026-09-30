//! The graph reduced to names, so two builds of it can be compared: what a
//! change to the type system adds, removes and moves.

use std::collections::{BTreeMap, BTreeSet};

use rustdoc_types::Id;

use crate::design::{EdgeKind, Graph};
use crate::index::Index;
use crate::invariants::type_name;

/// A pair of type paths, source then destination.
pub type Pair = (String, String);

/// The graph by name.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Every public type drawn, by path.
    pub types: BTreeSet<String>,
    /// The types the strategy layer builds on.
    pub surface: BTreeSet<String>,
    /// Flow edges and the functions that carry each.
    pub flows: BTreeMap<Pair, BTreeSet<String>>,
    /// The flows some node's table names.
    pub designed: BTreeSet<Pair>,
    /// Containment and accessor edges.
    pub carries: BTreeSet<Pair>,
    /// In and out flow degree per type.
    pub degree: BTreeMap<String, (usize, usize)>,
    /// Types in no signature and no field with another of the crate's.
    pub islands: BTreeSet<String>,
    /// Types produced but consumed by nothing, and not on the surface.
    pub dead_ends: BTreeSet<String>,
    /// Pairs that flow both ways.
    pub two_cycles: BTreeSet<Pair>,
    /// Flows between documented types that no node names.
    pub unnamed: BTreeSet<Pair>,
    /// Types in some node's table.
    pub documented: BTreeSet<String>,
    /// Flow edges between components, with counts.
    pub coupling: BTreeMap<Pair, usize>,
    /// Each drawn type's own shape: its public methods' signatures and its
    /// fields, rendered, so a change to the type itself is visible.
    pub shapes: BTreeMap<String, BTreeSet<String>>,
    /// Each documented type's row, as written: invariant, produced by and
    /// consumed by.
    pub rows: BTreeMap<String, String>,
}

impl Summary {
    pub fn of(index: &Index, graph: &Graph) -> Summary {
        let path = |id: Id| {
            index
                .entries
                .get(&id)
                .map(|e| e.path())
                .unwrap_or_else(|| index.name_of(id))
        };
        let top = |id: Id| {
            index
                .entries
                .get(&id)
                .map(|e| e.top().to_string())
                .unwrap_or_default()
        };
        let mut s = Summary::default();
        let types: Vec<Id> = graph
            .nodes
            .iter()
            .copied()
            .filter(|id| index.entries.get(id).is_some_and(|e| e.kind.is_type()))
            .collect();
        s.types = types.iter().map(|id| path(*id)).collect();
        s.documented = graph
            .rows
            .iter()
            .flat_map(|r| std::iter::once(r.subject).chain(r.companions.iter().copied()))
            .map(path)
            .collect();
        s.surface = graph
            .rows
            .iter()
            .filter(|r| r.public)
            .flat_map(|r| std::iter::once(r.subject).chain(r.companions.iter().copied()))
            .map(path)
            .collect();

        let mut inn: BTreeMap<Id, usize> = BTreeMap::new();
        let mut out: BTreeMap<Id, usize> = BTreeMap::new();
        let mut held: BTreeSet<Id> = BTreeSet::new();
        let mut pairs: BTreeSet<(Id, Id)> = BTreeSet::new();
        for e in graph.edges.values() {
            match e.kind {
                EdgeKind::Flow | EdgeKind::Uses => {
                    *out.entry(e.src).or_default() += 1;
                    *inn.entry(e.dst).or_default() += 1;
                    pairs.insert((e.src, e.dst));
                    let key = (path(e.src), path(e.dst));
                    s.flows
                        .entry(key.clone())
                        .or_default()
                        .extend(e.labels.iter().cloned());
                    if graph.designed(e, index) {
                        s.designed.insert(key.clone());
                    }
                    let (a, b) = (top(e.src), top(e.dst));
                    if a != b && !a.is_empty() && !b.is_empty() {
                        *s.coupling.entry((a, b)).or_default() += 1;
                    }
                    if e.kind == EdgeKind::Flow
                        && s.documented.contains(&key.0)
                        && s.documented.contains(&key.1)
                        && !graph.designed(e, index)
                    {
                        s.unnamed.insert(key);
                    }
                }
                EdgeKind::Carries | EdgeKind::Accessor => {
                    held.insert(e.src);
                    held.insert(e.dst);
                    s.carries.insert((path(e.src), path(e.dst)));
                }
            }
        }
        for id in &types {
            let (i, o) = (
                inn.get(id).copied().unwrap_or(0),
                out.get(id).copied().unwrap_or(0),
            );
            s.degree.insert(path(*id), (i, o));
            if i == 0 && o == 0 && !held.contains(id) {
                s.islands.insert(path(*id));
            }
            let public = graph
                .rows
                .iter()
                .any(|r| (r.subject == *id || r.companions.contains(id)) && r.public);
            if i > 0 && o == 0 && !held.contains(id) && !public {
                s.dead_ends.insert(path(*id));
            }
        }
        for (a, b) in &pairs {
            if a < b && pairs.contains(&(*b, *a)) {
                s.two_cycles.insert((path(*a), path(*b)));
            }
        }
        for id in &types {
            let mut shape = BTreeSet::new();
            for m in index.methods.get(id).into_iter().flatten() {
                if let Some(sig) = index.sigs.get(m) {
                    let inputs: Vec<String> = sig.input_types.iter().map(type_name).collect();
                    let output = sig.output.as_ref().map(type_name).unwrap_or_default();
                    shape.insert(format!(
                        "{}({}) -> {output}",
                        index.name_of(*m),
                        inputs.join(", ")
                    ));
                }
            }
            for (name, _, ty, public) in index.fields.get(id).into_iter().flatten() {
                if *public {
                    shape.insert(format!("{name}: {}", type_name(ty)));
                }
            }
            s.shapes.insert(path(*id), shape);
        }
        for r in &graph.rows {
            s.rows.insert(
                path(r.subject),
                format!("{}|{}|{}", r.invariant, r.produced, r.consumed),
            );
        }
        s
    }

    /// The number of function nodes drawn is not a property of the types,
    /// so it is not here; everything a diff can move is.
    pub fn counts(&self) -> String {
        format!(
            "{} types, {} flows ({} designed), {} containment edges, {} islands, {} dead ends, {} two-cycles, {} unnamed flows, {} on the surface",
            self.types.len(),
            self.flows.len(),
            self.designed.len(),
            self.carries.len(),
            self.islands.len(),
            self.dead_ends.len(),
            self.two_cycles.len(),
            self.unnamed.len(),
            self.surface.len()
        )
    }
}

/// What changed between two summaries, as markdown for a PR.
pub fn diff(base: &Summary, head: &Summary, base_name: &str) -> String {
    let mut out = String::new();
    let short = |p: &String| p.rsplit("::").next().unwrap_or(p).to_string();
    let pair = |(a, b): &Pair| format!("{} → {}", short(a), short(b));
    let section = |title: &str, added: Vec<String>, removed: Vec<String>| -> String {
        if added.is_empty() && removed.is_empty() {
            return String::new();
        }
        let mut s = format!("\n**{title}**: +{} −{}\n", added.len(), removed.len());
        for a in added {
            s.push_str(&format!("- added: {a}\n"));
        }
        for r in removed {
            s.push_str(&format!("- removed: {r}\n"));
        }
        s
    };
    let set_diff = |a: &BTreeSet<String>, b: &BTreeSet<String>| -> (Vec<String>, Vec<String>) {
        (
            b.difference(a).map(short).collect(),
            a.difference(b).map(short).collect(),
        )
    };
    let pair_diff = |a: &BTreeSet<Pair>, b: &BTreeSet<Pair>| -> (Vec<String>, Vec<String>) {
        (
            b.difference(a).map(pair).collect(),
            a.difference(b).map(pair).collect(),
        )
    };

    out.push_str(&format!("### Type graph against `{base_name}`\n\n"));
    if base == head {
        out.push_str("No change to the type graph.\n");
        return out;
    }
    out.push_str(&format!(
        "- base: {}\n- head: {}\n",
        base.counts(),
        head.counts()
    ));

    let (a, r) = set_diff(&base.types, &head.types);
    out.push_str(&section("Public types", a, r));
    let (a, r) = set_diff(&base.surface, &head.surface);
    out.push_str(&section(
        "Surface, types the strategy layer builds on",
        a,
        r,
    ));
    let base_flows: BTreeSet<Pair> = base.flows.keys().cloned().collect();
    let head_flows: BTreeSet<Pair> = head.flows.keys().cloned().collect();
    let added: Vec<String> = head_flows
        .difference(&base_flows)
        .map(|p| {
            format!(
                "{} ({})",
                pair(p),
                head.flows[p].iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect();
    let removed: Vec<String> = base_flows
        .difference(&head_flows)
        .map(|p| {
            format!(
                "{} ({})",
                pair(p),
                base.flows[p].iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect();
    out.push_str(&section("Flows", added, removed));
    let (a, r) = pair_diff(&base.designed, &head.designed);
    out.push_str(&section("Designed flows, named by a node", a, r));
    let (a, r) = pair_diff(&base.carries, &head.carries);
    out.push_str(&section("Containment", a, r));
    let (a, r) = set_diff(&base.islands, &head.islands);
    out.push_str(&section("Islands", a, r));
    let (a, r) = set_diff(&base.dead_ends, &head.dead_ends);
    out.push_str(&section("Dead ends", a, r));
    let (a, r) = pair_diff(&base.two_cycles, &head.two_cycles);
    out.push_str(&section("Two-cycles", a, r));
    let (a, r) = pair_diff(&base.unnamed, &head.unnamed);
    out.push_str(&section(
        "Flows between documented types that no node names",
        a,
        r,
    ));
    let reshaped: Vec<String> = head
        .shapes
        .iter()
        .filter(|(t, shape)| base.shapes.get(*t).is_some_and(|b| b != *shape))
        .map(|(t, _)| {
            let row = match (base.rows.get(t), head.rows.get(t)) {
                (Some(b), Some(h)) if b == h => "row unchanged",
                (Some(_), Some(_)) => "row updated",
                _ => "no row",
            };
            format!("{} ({row})", short(t))
        })
        .collect();
    if !reshaped.is_empty() {
        out.push_str(&format!(
            "\n**Types whose own methods or fields changed**: {}\n",
            reshaped.len()
        ));
        for t in reshaped {
            out.push_str(&format!("- {t}\n"));
        }
    }

    let mut moved: Vec<String> = Vec::new();
    for (t, (hi, ho)) in &head.degree {
        if let Some((bi, bo)) = base.degree.get(t)
            && (bi, bo) != (hi, ho)
            && (hi + ho >= 6 || bi + bo >= 6)
        {
            moved.push(format!("{}: in {bi}→{hi}, out {bo}→{ho}", short(t)));
        }
    }
    if !moved.is_empty() {
        out.push_str("\n**Hubs that moved**\n");
        for m in moved {
            out.push_str(&format!("- {m}\n"));
        }
    }
    let mut coupling: Vec<String> = Vec::new();
    for (k, hn) in &head.coupling {
        let bn = base.coupling.get(k).copied().unwrap_or(0);
        if bn != *hn {
            coupling.push(format!("{} → {}: {bn}→{hn}", k.0, k.1));
        }
    }
    for (k, bn) in &base.coupling {
        if !head.coupling.contains_key(k) {
            coupling.push(format!("{} → {}: {bn}→0", k.0, k.1));
        }
    }
    if !coupling.is_empty() {
        out.push_str("\n**Flow edges between components that changed**\n");
        for c in coupling {
            out.push_str(&format!("- {c}\n"));
        }
    }
    out
}
