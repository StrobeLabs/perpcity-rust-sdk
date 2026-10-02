//! The graph-level invariants. Each is a sentence in the root node's
//! "Invariants" section and a predicate here; the sentence is the error.

use std::collections::{BTreeMap, BTreeSet};

use rustdoc_types::{Id, ItemEnum, Type};

use crate::design::Graph;
use crate::index::{Index, Kind, component_of, result_error};
use crate::nodes::Node;
use crate::summary::Summary;

/// The enforced invariants, each as the opening of its sentence in the
/// root node's Invariants section. The list and the predicates are tied:
/// a predicate without its sentence, or a sentence without its predicate,
/// is a problem.
pub const ENFORCED: &[&str] = &[
    "No read on a handle takes a block argument",
    "Every snapshot in `math` carries a `BlockContext`",
    "A validated value",
    "Nothing in `math` takes or returns a provider",
    "Every fallible public function returns one of the crate's errors",
    "Every variant of `ContractError` and `TransactionError` says",
    "Every public type is in exactly one node's table",
];

/// Run every enforced invariant, appending a sentence per violation.
pub fn enforced(index: &Index, graph: &Graph, nodes: &[Node], problems: &mut Vec<String>) {
    no_read_takes_a_block(index, problems);
    snapshots_carry_a_block(index, problems);
    private_types_come_through_results(index, problems);
    math_takes_no_provider(index, problems);
    results_are_the_crates(index, problems);
    error_variants_state_transience(index, problems);
    surface_is_in_one_table(index, graph, nodes, problems);
    listed_in_the_root(nodes, problems);
}

/// The root node's Invariants section lists exactly the predicates that
/// run: every entry of [`ENFORCED`] opens a bullet there, and every bullet
/// there opens with one.
fn listed_in_the_root(nodes: &[Node], problems: &mut Vec<String>) {
    let Some(root) = nodes.iter().find(|n| n.is_root()) else {
        return;
    };
    let section = section_of(&root.text, "## Invariants");
    let bullets: Vec<&str> = section
        .lines()
        .filter(|l| l.starts_with("- "))
        .map(|l| l.trim_start_matches("- "))
        .collect();
    for opening in ENFORCED {
        if !bullets.iter().any(|b| b.starts_with(opening)) {
            problems.push(format!(
                "invariant list: the root node's Invariants section has no bullet opening with \"{opening}\", but the predicate runs"
            ));
        }
    }
    for b in bullets {
        if !ENFORCED.iter().any(|o| b.starts_with(o)) {
            problems.push(format!(
                "invariant list: the root node lists \"{}\" as enforced, but no predicate runs for it",
                b.chars().take(60).collect::<String>()
            ));
        }
    }
}

/// The text of a node's section with the given heading, to the next
/// heading of the same level.
fn section_of(text: &str, heading: &str) -> String {
    let needle = format!("\n{heading}");
    let Some(start) = text.find(&needle) else {
        return String::new();
    };
    let rest = &text[start + 1..];
    let body = &rest[rest.find('\n').map(|i| i + 1).unwrap_or(rest.len())..];
    match body.find("\n## ") {
        Some(end) => body[..end].to_string(),
        None => body.to_string(),
    }
}

/// Where a node answers a question the report asks. There are two
/// sections, and which one a structure goes in is the answer: accepted
/// structure, when the shape is the design and the question has an
/// argument against it, and debts, when it is work owed. The ratchet takes
/// either; the report counts only the first as settled, so the debts stay
/// a work queue.
pub struct Answers {
    /// Each node's accepted-structure section, by component.
    accepted: BTreeMap<String, String>,
    /// Each node's debts, by component.
    debts: BTreeMap<String, String>,
}

impl Answers {
    pub fn of(nodes: &[Node]) -> Answers {
        Answers {
            accepted: nodes
                .iter()
                .map(|n| (n.name.clone(), accepted_section(&n.text)))
                .collect(),
            debts: nodes
                .iter()
                .map(|n| (n.name.clone(), debts_section(&n.text)))
                .collect(),
        }
    }

    /// The sections that may answer for a type one of `owners` owns: that
    /// component's, since the component owning a type is the one that
    /// accepts its shape, and two types sharing a name in two components are
    /// two questions. A type no node owns is answerable anywhere.
    fn sections(&self, owners: &[&str], accepted_only: bool) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for o in owners {
            if let Some(a) = self.accepted.get(*o) {
                out.push(a);
                if !accepted_only {
                    out.push(&self.debts[*o]);
                }
            }
        }
        if out.is_empty() {
            out.extend(self.accepted.values().map(String::as_str));
            if !accepted_only {
                out.extend(self.debts.values().map(String::as_str));
            }
        }
        out
    }

    /// The owning component names the type, in either section.
    fn names(&self, owner: &str, name: &str) -> bool {
        self.sections(&[owner], false)
            .iter()
            .any(|s| names_word(s, name))
    }

    /// Both names in one section of a node owning one of them: naming one of
    /// the types for some other reason does not answer for the pair.
    fn names_pair(&self, owners: (&str, &str), a: &str, b: &str) -> bool {
        self.sections(&[owners.0, owners.1], false)
            .iter()
            .any(|s| names_word(s, a) && names_word(s, b))
    }

    /// A section names the label anywhere: a function has no owning type.
    fn names_function(&self, label: &str) -> bool {
        self.sections(&[], false)
            .iter()
            .any(|s| names_word(s, label))
    }

    /// The owning component calls the type accepted structure.
    pub fn accepts(&self, owner: &str, name: &str) -> bool {
        self.sections(&[owner], true)
            .iter()
            .any(|s| names_word(s, name))
    }

    /// A node owning one of the pair calls the cycle accepted structure.
    pub fn accepts_pair(&self, owners: (&str, &str), a: &str, b: &str) -> bool {
        self.sections(&[owners.0, owners.1], true)
            .iter()
            .any(|s| names_word(s, a) && names_word(s, b))
    }
}

/// The component a type path belongs to, as a node names it; `""`, the
/// root node, for a type at the crate root.
fn owner_of(path: &str) -> &str {
    match path.split_once("::") {
        Some((top, _)) => component_of(top),
        None => "",
    }
}

/// The ratchet: a structure the report questions may exist, but a new one
/// arrives answered. Every island, dead end and two-cycle the head has and
/// the base did not is named in some node's accepted structure or debts,
/// and every new flow between documented types is named by a table or, by
/// function, in one of those sections.
pub fn ratchet(base: &Summary, head: &Summary, nodes: &[Node], problems: &mut Vec<String>) {
    let answers = Answers::of(nodes);
    let short = |p: &String| p.rsplit("::").next().unwrap_or(p).to_string();
    for t in head.islands.difference(&base.islands) {
        if !answers.names(owner_of(t), &short(t)) {
            problems.push(format!(
                "ratchet: {t} became an island, in no signature and no field with another of the crate's types; name it in its node's accepted structure or debts, or connect it"
            ));
        }
    }
    for t in head.dead_ends.difference(&base.dead_ends) {
        if !answers.names(owner_of(t), &short(t)) {
            problems.push(format!(
                "ratchet: {t} became a dead end, produced but consumed by nothing; name it in its node's accepted structure or debts, consume it, or mark it as the strategy layer's"
            ));
        }
    }
    for (a, b) in head.two_cycles.difference(&base.two_cycles) {
        if !answers.names_pair((owner_of(a), owner_of(b)), &short(a), &short(b)) {
            problems.push(format!(
                "ratchet: {a} and {b} now flow both ways; name the pair in one of their nodes' accepted structure or debts, or move the conversion to one side"
            ));
        }
    }
    for (a, b) in head.unnamed.difference(&base.unnamed) {
        let fns = head
            .flows
            .get(&(a.clone(), b.clone()))
            .cloned()
            .unwrap_or_default();
        if !fns.iter().any(|f| answers.names_function(f)) {
            problems.push(format!(
                "ratchet: {a} now flows into {b} through {} and no node names it; add it to a type table's Produced by or Consumed by, or name the function in an accepted structure or debts section",
                fns.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    // A type whose own methods or fields changed is a type whose row must
    // change: the row is the design's claim about it, and a claim that did
    // not move when the type did is stale by construction.
    for (t, shape) in &head.shapes {
        let changed = base.shapes.get(t).is_some_and(|b| b != shape);
        let row_same = matches!((base.rows.get(t), head.rows.get(t)), (Some(b), Some(h)) if b == h);
        if changed && row_same {
            problems.push(format!(
                "ratchet: {t}'s own methods or fields changed and its row in the design node did not; say what changed in its Invariant, Produced by or Consumed by"
            ));
        }
    }
}

/// The text of a node's `## Debts` section: the work the component owes.
fn debts_section(text: &str) -> String {
    section_of(text, "## Debts")
}

/// The text of a node's `## Accepted structure` section: the shapes the
/// report questions and the design keeps, with the argument for each.
fn accepted_section(text: &str) -> String {
    section_of(text, "## Accepted structure")
}

/// `text` contains `word` as a whole identifier.
fn names_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + word.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// A type as a reader would name it, for messages, suffix checks and the
/// summary's shapes.
pub fn type_name(ty: &Type) -> String {
    match ty {
        Type::ResolvedPath(p) => p.path.rsplit("::").next().unwrap_or(&p.path).to_string(),
        Type::Primitive(p) => p.clone(),
        Type::Generic(g) => g.clone(),
        Type::Tuple(ts) => format!(
            "({})",
            ts.iter().map(type_name).collect::<Vec<_>>().join(", ")
        ),
        Type::BorrowedRef { type_, .. } => format!("&{}", type_name(type_)),
        _ => "?".into(),
    }
}

/// No read on a handle takes a block: the block is the handle's.
fn no_read_takes_a_block(index: &Index, problems: &mut Vec<String>) {
    for owner in ["MarketReader", "StateAt", "ChainReader", "PerpClient"] {
        let Some(ids) = index.by_name.get(owner) else {
            continue;
        };
        for &ty in ids {
            for &m in index.methods.get(&ty).into_iter().flatten() {
                let name = index.name_of(m);
                if name == "state_at" {
                    continue; // the one door to a named block
                }
                let Some(sig) = index.sigs.get(&m) else {
                    continue;
                };
                for ((pname, _), pty) in sig.inputs.iter().zip(&sig.input_types) {
                    let tn = type_name(pty);
                    if pname.starts_with("block") || tn == "BlockId" || tn == "BlockNumberOrTag" {
                        problems.push(format!("invariant: no read takes a block argument, but {owner}::{name} takes `{pname}: {tn}`"));
                    }
                }
            }
        }
    }
}

/// Every snapshot in `math` carries a `BlockContext`.
fn snapshots_carry_a_block(index: &Index, problems: &mut Vec<String>) {
    let Some(ctx) = index
        .by_name
        .get("BlockContext")
        .and_then(|v| v.first().copied())
    else {
        problems.push("invariant: BlockContext no longer exists".into());
        return;
    };
    for e in index.sorted() {
        if e.top() != "math" || e.kind != Kind::Struct {
            continue;
        }
        let is_snapshot = e.name.ends_with("Snapshot")
            || matches!(e.name.as_str(), "MarketCapacity" | "Mark" | "TakerQuote");
        if !is_snapshot || e.name == "AccruedMakerSnapshot" {
            continue;
        }
        let has = index
            .fields
            .get(&e.id)
            .is_some_and(|fs| fs.iter().any(|(_, ids, _, _)| ids.contains(&ctx)));
        if !has {
            problems.push(format!(
                "invariant: every snapshot carries a BlockContext, but {} has no such field",
                e.path()
            ));
        }
    }
}

/// A validated value has private fields and comes only through fallible
/// constructors: every public function returning it returns a `Result`.
fn private_types_come_through_results(index: &Index, problems: &mut Vec<String>) {
    for e in index.sorted() {
        if e.top() != "math" || e.kind != Kind::Struct {
            continue;
        }
        let Some(fs) = index.fields.get(&e.id) else {
            continue;
        };
        if fs.is_empty() || fs.iter().any(|(_, _, _, public)| *public) {
            continue;
        }
        for (label, (fid, sig)) in index.functions() {
            if index.trait_methods.contains(&fid) {
                continue; // `Default::default` and the like are the trait's shape
            }
            let returns_it = sig.outputs.contains(&e.id)
                || (index.self_of.get(&fid) == Some(&e.id)
                    && sig
                        .output
                        .as_ref()
                        .is_some_and(|o| matches!(o, Type::Generic(g) if g == "Self")));
            if !returns_it {
                continue;
            }
            if index.self_of.get(&fid) == Some(&e.id) && sig.inputs.iter().any(|(n, _)| n == "self")
            {
                continue; // a method on the value itself, e.g. a builder step
            }
            let Some(out) = &sig.output else { continue };
            if result_error(out).is_none()
                && !matches!(out, Type::ResolvedPath(p) if p.path.ends_with("Option"))
            {
                problems.push(format!("invariant: a validated value comes only through a fallible constructor, but {label} returns {} bare", e.name));
            }
        }
    }
}

/// Nothing in `math` takes or returns a provider, transport, or handle.
fn math_takes_no_provider(index: &Index, problems: &mut Vec<String>) {
    let forbidden_tops = ["client", "transport", "feeds", "history", "hft"];
    let forbidden_crates = [
        "alloy_provider",
        "alloy_transport",
        "alloy_transport_http",
        "alloy_rpc_client",
        "tokio",
        "tower",
        "reqwest",
    ];
    for (label, (id, sig)) in index.functions() {
        let owner = index.self_of.get(&id).copied().unwrap_or(id);
        let Some(e) = index.entries.get(&owner) else {
            continue;
        };
        if e.top() != "math" {
            continue;
        }
        let ids: BTreeSet<Id> = sig
            .inputs
            .iter()
            .flat_map(|(_, ids)| ids.iter().copied())
            .chain(sig.outputs.iter().copied())
            .collect();
        for t in ids {
            if let Some(te) = index.entries.get(&t) {
                if forbidden_tops.contains(&te.top()) {
                    problems.push(format!(
                        "invariant: math takes no provider or handle, but {label} mentions {}",
                        te.path()
                    ));
                }
            } else if let Some(c) = index.crate_of(t)
                && forbidden_crates.contains(&c)
            {
                problems.push(format!(
                    "invariant: math takes no provider or handle, but {label} mentions {} from {c}",
                    index.name_of(t)
                ));
            }
        }
    }
}

/// Every public function that can fail returns one of the crate's errors.
fn results_are_the_crates(index: &Index, problems: &mut Vec<String>) {
    let ours: BTreeSet<Id> = [
        "PerpCityError",
        "ValidationError",
        "ContractError",
        "TransactionError",
    ]
    .iter()
    .flat_map(|n| index.by_name.get(*n).cloned().unwrap_or_default())
    .collect();
    let alias: Option<Id> = index.by_name.get("Result").and_then(|v| {
        v.iter()
            .copied()
            .find(|id| index.entries[id].kind == Kind::TypeAlias)
    });
    for (label, (fid, sig)) in index.functions() {
        // The crate's own API, not trait impls (`fmt`, `clone`) and not the
        // bindings `sol!` generates.
        let owner = index.self_of.get(&fid).copied().unwrap_or(fid);
        if index.trait_methods.contains(&fid)
            || index
                .entries
                .get(&owner)
                .is_none_or(|e| e.top() == "contracts")
        {
            continue;
        }
        let Some(out) = &sig.output else { continue };
        let Some(err) = result_error(out) else {
            continue;
        };
        if let Type::ResolvedPath(p) = out
            && Some(p.id) == alias
        {
            continue;
        }
        match err {
            Some(eid) if ours.contains(&eid) => {}
            Some(eid) => problems.push(format!("invariant: every fallible function returns one of the crate's errors, but {label} returns Result<_, {}>", index.name_of(eid))),
            None => problems.push(format!("invariant: every fallible function returns one of the crate's errors, but {label} returns a Result whose error is not a named type")),
        }
    }
}

/// Every variant of the chain's and the send's errors says whether it is
/// transient.
fn error_variants_state_transience(index: &Index, problems: &mut Vec<String>) {
    for name in ["ContractError", "TransactionError"] {
        for &eid in index.by_name.get(name).into_iter().flatten() {
            for &v in index.variants.get(&eid).into_iter().flatten() {
                let docs = index
                    .docs
                    .get(&v)
                    .map(|s| s.to_lowercase())
                    .unwrap_or_default();
                if !docs.contains("transient") {
                    problems.push(format!("invariant: every error variant states its transience, but {name}::{} does not say", index.name_of(v)));
                }
            }
        }
    }
}

/// Every public type is in exactly one node's table.
fn surface_is_in_one_table(
    index: &Index,
    graph: &Graph,
    nodes: &[Node],
    problems: &mut Vec<String>,
) {
    let mut homes: std::collections::BTreeMap<Id, Vec<String>> = std::collections::BTreeMap::new();
    for row in &graph.rows {
        for id in std::iter::once(row.subject).chain(row.companions.iter().copied()) {
            homes.entry(id).or_default().push(row.node.clone());
        }
    }
    let node_names: BTreeSet<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    for e in index.sorted() {
        if !e.kind.is_type() || e.top().is_empty() || !node_names.contains(e.top()) {
            continue;
        }
        if e.top() == "contracts" && (e.kind == Kind::Struct || e.module.len() >= 2) {
            continue; // the bindings' generated structs and helper enums are the ABI's, not the design's
        }
        if let Some(item) = index.item(e.id)
            && let ItemEnum::Struct(_)
            | ItemEnum::Enum(_)
            | ItemEnum::Trait(_)
            | ItemEnum::TypeAlias(_) = &item.inner
            && item
                .attrs
                .iter()
                .any(|a| format!("{a:?}").contains("Hidden") || format!("{a:?}").contains("hidden"))
        {
            continue;
        }
        match homes.get(&e.id) {
            None => problems.push(format!(
                "invariant: every public type is in one node's table, but {} is in none",
                e.path()
            )),
            Some(h) if h.len() > 1 => {
                let uniq: BTreeSet<&String> = h.iter().collect();
                if uniq.len() > 1 {
                    problems.push(format!(
                        "invariant: every public type is in one node's table, but {} is in {}",
                        e.path(),
                        h.join(" and ")
                    ));
                }
            }
            _ => {}
        }
    }
}
