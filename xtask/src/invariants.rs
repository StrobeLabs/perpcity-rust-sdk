//! The graph-level invariants. Each enforced one is a sentence in the root
//! node's "Invariants" section and a predicate here; the sentence is the
//! error. Reported ones print under `--report` until the type system is
//! ready for them.

use std::collections::BTreeSet;

use rustdoc_types::{Id, ItemEnum, Type};

use crate::design::Graph;
use crate::index::{Index, Kind, mentions_primitive, result_error};
use crate::nodes::Node;

const WIRE_SUFFIXES: [&str; 5] = ["_x96", "_x128", "_atoms", "_e6", "_wad"];

/// Run every enforced invariant, appending a sentence per violation.
pub fn enforced(index: &Index, graph: &Graph, nodes: &[Node], problems: &mut Vec<String>) {
    no_read_takes_a_block(index, problems);
    snapshots_carry_a_block(index, problems);
    private_types_come_through_results(index, problems);
    math_takes_no_provider(index, problems);
    results_are_the_crates(index, problems);
    error_variants_state_transience(index, problems);
    surface_is_in_one_table(index, graph, nodes, problems);
}

/// Print the findings of the invariants that are measured but not yet
/// enforced.
pub fn reported(index: &Index) {
    println!("\nReported invariants (not enforced until #124's conversions land):");
    let mut n = 0;
    for (label, (id, sig)) in index.functions() {
        let e = match index
            .self_of
            .get(&id)
            .or(Some(&id))
            .and_then(|t| index.entries.get(t))
        {
            Some(e) => e,
            None => continue,
        };
        if e.top() != "math" {
            continue;
        }
        let name = index.name_of(id);
        if WIRE_SUFFIXES.iter().any(|s| name.ends_with(s)) {
            let leaks = sig
                .input_types
                .iter()
                .chain(sig.output.iter())
                .any(|t| mentions_primitive(t, "f64") || mentions_primitive(t, "f32"));
            if leaks {
                println!("  float in an exact path: {label} carries an f64");
                n += 1;
            }
        }
    }
    for e in index.sorted() {
        if !e.kind.is_type() || e.top() != "math" {
            continue;
        }
        for (fname, _, ty, _) in index.fields.get(&e.id).into_iter().flatten() {
            let expected: Option<&[&str]> = if fname.ends_with("_x96") || fname.ends_with("_x128") {
                Some(&["U256", "I256", "u128", "i128", "u256"])
            } else if fname.ends_with("_atoms") {
                Some(&["u128", "i128", "U256", "I256"])
            } else if fname.ends_with("_e6") {
                Some(&["u32", "u64"])
            } else if fname.ends_with("_wad") {
                Some(&["U256", "I256", "u64", "i64"])
            } else {
                None
            };
            if let Some(exp) = expected {
                let shown = type_name(ty);
                if !exp.iter().any(|x| shown.ends_with(x)) {
                    println!(
                        "  suffix does not match the primitive: {}.{fname} is {shown}",
                        e.path()
                    );
                    n += 1;
                }
            }
            if mentions_primitive(ty, "f64") && WIRE_SUFFIXES.iter().any(|s| fname.ends_with(s)) {
                println!("  float in an exact field: {}.{fname}", e.path());
                n += 1;
            }
        }
    }
    println!("  {n} finding(s)");
}

/// A type as a reader would name it, for messages and suffix checks.
fn type_name(ty: &Type) -> String {
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
