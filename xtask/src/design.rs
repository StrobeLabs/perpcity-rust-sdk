//! The `design` command: build the type graph from rustdoc's JSON, read the
//! design nodes' tables as the curated layer over it, verify every claim
//! against a signature, and run what the flags ask for.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use rustdoc_types::{Id, Type};

use crate::index::{Index, Kind, Signature};
use crate::nodes::{self, Link, Node};
use crate::summary::{self, Summary};
use crate::{invariants, page, report, rustdoc};

#[derive(Debug, Default, Clone)]
pub struct Options {
    pub check: bool,
    pub fmt: bool,
    pub report: bool,
    pub open: bool,
    /// A git ref to build the graph at and diff against, `origin/main` say.
    pub diff: Option<String>,
}

/// What a link names: an item, and a member of it when the link is
/// `Type::method` or `Enum::Variant`.
#[derive(Debug, Clone)]
pub struct Ref {
    pub item: Id,
    pub member: Option<Id>,
    pub member_name: Option<String>,
}

/// One type's row in a node, resolved.
#[derive(Debug, Clone)]
pub struct TypeRow {
    pub node: String,
    pub subject: Id,
    pub companions: Vec<Id>,
    pub invariant: String,
    pub produced: String,
    pub consumed: String,
    pub public: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    /// `src` is a parameter of a function that returns `dst` (or is the
    /// function's own type when it returns nothing of the crate's).
    Flow,
    /// `dst` has a field of type `src`, or a `From`/`TryFrom` impl from it.
    Carries,
    /// `src` hands out a reference to a `dst` it holds: an accessor.
    Accessor,
    /// `dst`'s methods call the binding `src`.
    Uses,
}

/// An edge of the mechanical graph: `src` flows into `dst`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub src: Id,
    pub dst: Id,
    pub kind: EdgeKind,
    /// The functions that carry it, for `Flow` and `Accessor`.
    pub labels: BTreeSet<String>,
    pub fns: BTreeSet<Id>,
}

/// What a table row claims, verified against a signature.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Claim {
    /// `subject` is returned by function `f`.
    ProducedBy { subject: Id, f: Id },
    /// `subject` is built from type `from`.
    BuiltFrom { subject: Id, from: Id },
    /// `subject` is taken by function `g`.
    ConsumedBy { subject: Id, g: Id },
    /// `subject` is built into type `into`, or a binding `subject` is used by it.
    BuiltInto { subject: Id, into: Id },
}

pub struct Graph {
    /// Every public type, binding module and function node drawn.
    pub nodes: BTreeSet<Id>,
    pub edges: BTreeMap<(Id, Id, EdgeKind), Edge>,
    /// The rows of every node's table.
    pub rows: Vec<TypeRow>,
    /// Every verified claim; an edge a claim covers is a designed edge.
    pub claims: BTreeSet<Claim>,
}

impl Graph {
    /// The flow edges: what a function carries from one type to another.
    pub fn flows(&self) -> impl Iterator<Item = &Edge> {
        self.edges
            .values()
            .filter(|e| matches!(e.kind, EdgeKind::Flow | EdgeKind::Uses))
    }

    /// Whether some node's table names this edge.
    pub fn designed(&self, e: &Edge, index: &Index) -> bool {
        self.claims.iter().any(|c| match c {
            Claim::ProducedBy { subject, f } => *subject == e.dst && e.fns.contains(f),
            Claim::ConsumedBy { subject, g } => {
                *subject == e.src
                    && (e.fns.contains(g)
                        || (e.kind == EdgeKind::Uses && index.self_of.get(g) == Some(&e.dst)))
            }
            Claim::BuiltFrom { subject, from } => *subject == e.dst && *from == e.src,
            Claim::BuiltInto { subject, into } => *subject == e.src && *into == e.dst,
        })
    }

    pub fn designed_count(&self, index: &Index) -> usize {
        self.flows().filter(|e| self.designed(e, index)).count()
    }
}

/// Build the graph, verify the nodes against it, and do what the flags ask;
/// `Ok(false)` when problems were printed.
pub fn run(opts: Options) -> Result<bool> {
    let root = repo_root()?;
    let krate = rustdoc::load(&root)?;
    let index = Index::build(krate);
    let mut nodes = nodes::discover(&root)?;
    let mut problems: Vec<String> = Vec::new();

    if opts.fmt {
        for node in &mut nodes {
            let rewritten = canonical_links(node, &index, &root);
            if rewritten != node.text {
                fs::write(&node.path, &rewritten)?;
                node.text = rewritten;
                println!(
                    "rewrote links in {}",
                    node.path
                        .strip_prefix(&root)
                        .unwrap_or(&node.path)
                        .display()
                );
            }
        }
    }

    let mut graph = mechanical(&index, &root);
    annotate(&mut graph, &nodes, &index, &root, &mut problems)?;
    invariants::enforced(&index, &graph, &nodes, &mut problems);

    if opts.report {
        report::print(&index, &graph, &nodes);
        invariants::reported(&index);
    }
    if let Some(base_ref) = &opts.diff {
        let base = at_ref(&root, base_ref)?;
        let head = Summary::of(&index, &graph);
        let mut out = summary::diff(&base, &head, base_ref);
        let mut ratchet = Vec::new();
        invariants::ratchet(&base, &head, &nodes, &mut ratchet);
        if !ratchet.is_empty() {
            out.push_str("\n**Needs a decision**: a structure the design questions is new here and no node acknowledges it.\n");
            for r in &ratchet {
                out.push_str(&format!("- {r}\n"));
            }
        }
        print!("{out}");
        problems.extend(ratchet);
    }
    if opts.open {
        let out = page::write(&root, &index, &graph, &nodes)?;
        println!("wrote {}", out.display());
        let mut opener = if cfg!(target_os = "macos") {
            Command::new("open")
        } else if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "start", ""]);
            c
        } else {
            Command::new("xdg-open")
        };
        match opener.arg(&out).status() {
            Ok(s) if s.success() => {}
            other => eprintln!(
                "could not open the page ({other:?}); open {} yourself",
                out.display()
            ),
        }
    }

    if problems.is_empty() {
        if opts.check {
            println!(
                "design: {} types, {} edges, {} of them designed; every claim matches a signature, every invariant holds",
                graph
                    .nodes
                    .iter()
                    .filter(|id| index.entries.get(id).is_some_and(|e| e.kind.is_type()))
                    .count(),
                graph.flows().count(),
                graph.designed_count(&index)
            );
        }
        return Ok(true);
    }
    for p in &problems {
        eprintln!("{p}");
    }
    eprintln!("{} problem(s)", problems.len());
    Ok(false)
}

/// The graph at another commit: `git_ref`'s tree extracted under our
/// target directory, documented with the same toolchain into a target
/// directory of its own, read by the same code. Plain files, so nothing
/// is registered with git and nothing is left to prune; the root manifest
/// excludes the path from the workspace so cargo treats the extracted
/// package as its own root. Its nodes' problems are its own and are not
/// reported.
fn at_ref(root: &Path, git_ref: &str) -> Result<Summary> {
    let dir = rustdoc::target_dir(root).join("base");
    let target = rustdoc::target_dir(root).join("base-target");
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("clearing {}", dir.display()))?;
    }
    fs::create_dir_all(&dir)?;
    let mut archive = Command::new("git")
        .args(["archive", "--format=tar", git_ref])
        .current_dir(root)
        .stdout(Stdio::piped())
        .spawn()
        .context("running git archive")?;
    let tar_status = Command::new("tar")
        .args(["-x", "-C"])
        .arg(&dir)
        .stdin(archive.stdout.take().context("git archive's output")?)
        .status()
        .context("running tar")?;
    let archive_status = archive.wait()?;
    if !archive_status.success() || !tar_status.success() {
        anyhow::bail!("git could not archive `{git_ref}`");
    }
    let result = (|| -> Result<Summary> {
        let krate = rustdoc::load_into(&dir, &target)?;
        let index = Index::build(krate);
        let mut graph = mechanical(&index, &dir);
        // A base without nodes, or with nodes the tool cannot read, still
        // has a mechanical graph to diff against.
        if let Ok(nodes) = nodes::discover(&dir) {
            let mut ignored = Vec::new();
            if let Err(e) = annotate(&mut graph, &nodes, &index, &dir, &mut ignored) {
                eprintln!("note: the base's nodes were not read: {e:#}");
            }
        }
        Ok(Summary::of(&index, &graph))
    })();
    // The extracted tree goes; the base's target directory stays as the
    // cache for the next diff.
    let _ = fs::remove_dir_all(&dir);
    result
}

/// The repository: the nearest ancestor holding the root node and a manifest.
fn repo_root() -> Result<PathBuf> {
    let dir = env::current_dir()?;
    let mut cur: &Path = &dir;
    loop {
        if cur.join("DESIGN.md").is_file() && cur.join("Cargo.toml").is_file() {
            return Ok(cur.to_path_buf());
        }
        cur = cur.parent().context("not inside the repository")?;
    }
}

/// Whether a public type is drawn: the `sol!` bindings' generated call,
/// return and event structs are the ABI's, and the binding module stands
/// for them.
pub fn drawn(e: &crate::index::Entry) -> bool {
    match e.kind {
        Kind::Module => e.top() == "contracts" && e.module.len() == 1,
        k if k.is_type() => !(e.top() == "contracts" && e.module.len() >= 2),
        _ => false,
    }
}

/// The graph as the signatures give it.
fn mechanical(index: &Index, root: &Path) -> Graph {
    let mut edges: BTreeMap<(Id, Id, EdgeKind), Edge> = BTreeMap::new();
    let is_drawn_type = |id: &Id| {
        index
            .entries
            .get(id)
            .is_some_and(|e| e.kind.is_type() && drawn(e))
    };
    // A `Result` or an error is every fallible function's second output;
    // as flow targets they would make every function an edge into `errors`.
    let transparent: BTreeSet<Id> = index
        .entries
        .values()
        .filter(|e| e.kind == Kind::TypeAlias || (e.top() == "errors" && e.kind == Kind::Enum))
        .map(|e| e.id)
        .collect();
    let mut nodes: BTreeSet<Id> = index
        .entries
        .values()
        .filter(|e| drawn(e))
        .map(|e| e.id)
        .collect();

    for (_, (fid, sig)) in index.functions() {
        let name = index.name_of(fid);
        let owner = index
            .self_of
            .get(&fid)
            .copied()
            .filter(|o| index.entries.get(o).is_some_and(drawn));
        let has_self = sig.inputs.iter().any(|(n, _)| n == "self");
        let mut sources: BTreeSet<Id> = sig
            .inputs
            .iter()
            .filter(|(n, _)| n != "self")
            .flat_map(|(_, ids)| ids.iter().copied())
            .filter(is_drawn_type)
            .collect();
        if let Some(o) = owner
            && has_self
        {
            sources.insert(o);
        }
        let mut targets: BTreeSet<Id> = sig
            .outputs
            .iter()
            .copied()
            .filter(|id| is_drawn_type(id) && !transparent.contains(id))
            .collect();
        if let Some(o) = owner
            && returns_self(sig)
        {
            targets.insert(o);
        }
        let accessor = matches!(sig.output, Some(Type::BorrowedRef { .. })) && has_self;
        if sources.is_empty() {
            // A free function that makes the crate's types from nothing of
            // the crate's is a source, drawn as its own node; a constructor
            // on a type is the type's own.
            if owner.is_none() && !targets.is_empty() && index.entries.contains_key(&fid) {
                nodes.insert(fid);
                for t in &targets {
                    push_edge(&mut edges, fid, *t, EdgeKind::Flow, None, fid);
                }
            }
            continue;
        }
        if !targets.is_empty() {
            for s in &sources {
                for t in &targets {
                    let kind = if accessor && Some(*s) == owner {
                        EdgeKind::Accessor
                    } else {
                        EdgeKind::Flow
                    };
                    push_edge(&mut edges, *s, *t, kind, Some(&name), fid);
                }
            }
        } else if let Some(o) = owner {
            for s in &sources {
                push_edge(&mut edges, *s, o, EdgeKind::Flow, Some(&name), fid);
            }
        } else if index.entries.contains_key(&fid) {
            // A free function that consumes the crate's types and returns
            // none of them is a sink, drawn as its own node.
            nodes.insert(fid);
            for s in &sources {
                push_edge(&mut edges, *s, fid, EdgeKind::Flow, None, fid);
            }
        }
    }
    for e in index.sorted() {
        if !e.kind.is_type() || !drawn(e) {
            continue;
        }
        for y in index
            .entries
            .values()
            .filter(|y| y.kind.is_type() && drawn(y) && y.id != e.id)
        {
            if index.contains(e.id, y.id) {
                push_edge(&mut edges, y.id, e.id, EdgeKind::Carries, None, e.id);
            }
        }
        for y in index
            .conversions
            .get(&e.id)
            .into_iter()
            .flatten()
            .filter(|id| is_drawn_type(id))
        {
            push_edge(&mut edges, *y, e.id, EdgeKind::Flow, Some("from"), e.id);
        }
    }
    // A binding is used by a type when the type's methods name it.
    let bindings: Vec<Id> = nodes
        .iter()
        .copied()
        .filter(|id| index.entries[id].kind == Kind::Module)
        .collect();
    let mut files: HashSet<String> = index.entries.values().map(|e| e.file.clone()).collect();
    files.extend(
        index
            .all_methods
            .values()
            .flatten()
            .filter_map(|m| index.location(*m).map(|(f, _)| f)),
    );
    let texts: BTreeMap<String, String> = files
        .into_iter()
        .filter_map(|f| fs::read_to_string(root.join(&f)).ok().map(|t| (f, t)))
        .collect();
    for e in index.sorted() {
        // The bindings' own file names every binding; a use is a call
        // from elsewhere.
        if !e.kind.is_type() || !drawn(e) || e.top() == "contracts" {
            continue;
        }
        let mut seen_files: BTreeSet<String> = BTreeSet::new();
        seen_files.insert(e.file.clone());
        for m in index.all_methods.get(&e.id).into_iter().flatten() {
            if let Some((f, _)) = index.location(*m) {
                seen_files.insert(f);
            }
        }
        for b in &bindings {
            let needle = format!("{}::", index.entries[b].name);
            if seen_files
                .iter()
                .any(|f| texts.get(f).is_some_and(|t| names(t, &needle)))
            {
                push_edge(&mut edges, *b, e.id, EdgeKind::Uses, None, e.id);
            }
        }
    }
    for e in index.sorted() {
        if e.kind == Kind::Function
            && e.top() != "contracts"
            && let Some((f, _)) = index.location(e.id)
        {
            for b in &bindings {
                let needle = format!("{}::", index.entries[b].name);
                if texts.get(&f).is_some_and(|t| names(t, &needle)) {
                    push_edge(&mut edges, *b, e.id, EdgeKind::Uses, None, e.id);
                }
            }
        }
    }
    let mut graph_nodes = nodes;
    for (s, d, _) in edges.keys() {
        graph_nodes.insert(*s);
        graph_nodes.insert(*d);
    }
    Graph {
        nodes: graph_nodes,
        edges,
        rows: Vec::new(),
        claims: BTreeSet::new(),
    }
}

/// Add an edge, or another function and label to one that exists.
fn push_edge(
    edges: &mut BTreeMap<(Id, Id, EdgeKind), Edge>,
    src: Id,
    dst: Id,
    kind: EdgeKind,
    label: Option<&str>,
    by: Id,
) {
    if src == dst {
        return;
    }
    let e = edges.entry((src, dst, kind)).or_insert_with(|| Edge {
        src,
        dst,
        kind,
        labels: BTreeSet::new(),
        fns: BTreeSet::new(),
    });
    if let Some(l) = label {
        e.labels.insert(l.to_string());
    }
    e.fns.insert(by);
}

/// `text` names `needle` as a path, not as the tail of a longer identifier.
fn names(text: &str, needle: &str) -> bool {
    text.match_indices(needle).any(|(i, _)| {
        i == 0 || !text.as_bytes()[i - 1].is_ascii_alphanumeric() && text.as_bytes()[i - 1] != b'_'
    })
}

/// Read every node's table over the mechanical graph: resolve each link,
/// verify each claim against a signature, and mark the designed edges.
fn annotate(
    graph: &mut Graph,
    nodes: &[Node],
    index: &Index,
    root: &Path,
    problems: &mut Vec<String>,
) -> Result<()> {
    for node in nodes {
        check_links(node, index, root, problems);
        let table = nodes::table(node)?;
        for row in table.rows {
            let rel = node
                .path
                .strip_prefix(root)
                .unwrap_or(&node.path)
                .display()
                .to_string();
            let at = |msg: String| format!("{rel}:{}: {msg}", row.line);
            let mut subjects = Vec::new();
            for l in &row.subjects {
                match resolve(l, node, index) {
                    Ok(r)
                        if r.member.is_none() && index.entries[&r.item].kind != Kind::Function =>
                    {
                        subjects.push(r.item)
                    }
                    Ok(_) => {
                        problems.push(at(format!("`{}` in the Type column is not a type", l.text)))
                    }
                    Err(e) => problems.push(at(e)),
                }
            }
            let Some(&subject) = subjects.first() else {
                continue;
            };
            for l in &row.produced_links {
                match resolve(l, node, index) {
                    Err(e) => problems.push(at(e)),
                    Ok(r) => match verify_producer(&r, subject, index) {
                        Ok(Some(c)) => {
                            graph.claims.insert(c);
                        }
                        Ok(None) => {}
                        Err(why) => problems.push(at(format!(
                            "`{}` does not produce {}: {why}",
                            l.text, index.entries[&subject].name
                        ))),
                    },
                }
            }
            for l in &row.consumed_links {
                match resolve(l, node, index) {
                    Err(e) => problems.push(at(e)),
                    Ok(r) => match verify_consumer(&r, subject, index, graph) {
                        Ok(Some(c)) => {
                            graph.claims.insert(c);
                        }
                        Ok(None) => {}
                        Err(why) => problems.push(at(format!(
                            "`{}` does not consume {}: {why}",
                            l.text, index.entries[&subject].name
                        ))),
                    },
                }
            }
            graph.rows.push(TypeRow {
                node: node.name.clone(),
                subject,
                companions: subjects[1..].to_vec(),
                invariant: row.invariant.clone(),
                produced: row.produced.clone(),
                consumed: row.consumed.clone(),
                public: row.consumed.to_lowercase().contains("strategy layer"),
            });
        }
    }
    Ok(())
}

/// Resolve a link to an item, from its target path when it has one and
/// from its text otherwise, preferring the node's own component.
pub fn resolve(link: &Link, node: &Node, index: &Index) -> Result<Ref, String> {
    let from_target = link
        .target
        .as_deref()
        .filter(|t| t.contains("::") && !t.contains('/'));
    let text = strip_generics(from_target.unwrap_or(&link.text).trim_end_matches("()"));
    let segs: Vec<&str> = text
        .split("::")
        .filter(|s| !matches!(*s, "crate" | "self" | "super"))
        .collect();
    let Some(last) = segs.last() else {
        return Err(format!("empty link `{}`", link.text));
    };
    let member = segs.len() >= 2 && starts_upper(segs[segs.len() - 2]);
    let (name, prefix): (&str, Vec<String>) = if member {
        (
            segs[segs.len() - 2],
            segs[..segs.len() - 2]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    } else {
        (
            last,
            segs[..segs.len() - 1]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    };
    let want_fn = !member && starts_lower(name);
    let mut cands: Vec<Id> = index
        .by_name
        .get(name)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|id| {
            let e = &index.entries[id];
            if want_fn {
                matches!(e.kind, Kind::Function | Kind::Module)
            } else {
                e.kind.is_type() || e.kind == Kind::Module
            }
        })
        .filter(|id| {
            prefix.is_empty()
                || index.entries[id].module.ends_with(&prefix)
                || index.entries[id].module.starts_with(&prefix)
        })
        .collect();
    // A file target says which item of that name is meant: the source file
    // it links, or the design node of the component it links.
    if cands.len() > 1
        && let Some(t) = link.target.as_deref().filter(|t| !t.contains("::"))
    {
        let file = t.split('#').next().unwrap_or(t);
        let named: Vec<Id> = if file.ends_with("DESIGN.md") {
            let comp = Path::new(file)
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned());
            cands
                .iter()
                .copied()
                .filter(|id| Some(index.entries[id].top().to_string()) == comp)
                .collect()
        } else {
            let abs = fs::canonicalize(node.dir().join(file)).unwrap_or_default();
            cands
                .iter()
                .copied()
                .filter(|id| abs.ends_with(&index.entries[id].file))
                .collect()
        };
        if !named.is_empty() {
            cands = named;
        }
    }
    if cands.len() > 1 {
        let own: Vec<Id> = cands
            .iter()
            .copied()
            .filter(|id| index.entries[id].top() == node.name)
            .collect();
        if !own.is_empty() {
            cands = own;
        } else {
            let not_contracts: Vec<Id> = cands
                .iter()
                .copied()
                .filter(|id| index.entries[id].top() != "contracts")
                .collect();
            if !not_contracts.is_empty() {
                cands = not_contracts;
            }
        }
    }
    if cands.len() > 1 {
        // A re-export module (`feeds::events`) loses to the item's home.
        let depth = cands
            .iter()
            .map(|id| index.entries[id].module.len())
            .min()
            .unwrap_or(0);
        cands.retain(|id| index.entries[id].module.len() == depth);
    }
    // A bare `quote_perp` or `BlockUnavailable` in a component node is the
    // one method or variant of that name on the node's own types.
    if !member
        && prefix.is_empty()
        && !node.is_root()
        && cands
            .iter()
            .all(|id| index.entries[id].kind != Kind::Function)
        && let Some((owner, m)) = index.member_in(&node.name, name)
    {
        return Ok(Ref {
            item: owner,
            member: Some(m),
            member_name: Some(name.to_string()),
        });
    }
    if cands.len() != 1 {
        let where_ = cands
            .iter()
            .map(|id| index.entries[id].path())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "`{}` resolves to {}",
            link.text,
            if where_.is_empty() {
                "nothing on the public surface".to_string()
            } else {
                where_
            }
        ));
    }
    let item = cands[0];
    if member {
        let m = *last;
        if let Some(mid) = index.method(item, m) {
            return Ok(Ref {
                item,
                member: Some(mid),
                member_name: Some(m.to_string()),
            });
        }
        if let Some(vid) = index.variant(item, m) {
            return Ok(Ref {
                item,
                member: Some(vid),
                member_name: Some(m.to_string()),
            });
        }
        return Err(format!(
            "`{}`: {} has no public method or variant `{}`",
            link.text,
            index.entries[&item].path(),
            m
        ));
    }
    Ok(Ref {
        item,
        member: None,
        member_name: None,
    })
}

/// `Result<T>` as `Result`.
fn strip_generics(s: &str) -> &str {
    match s.find('<') {
        Some(i) => &s[..i],
        None => s,
    }
}

/// A function or module name, by Rust's convention.
fn starts_lower(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
}

/// A type or variant name, by Rust's convention.
fn starts_upper(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Every link resolves; every target is a file that exists, never an
/// intra-doc path, since the nodes are read on GitHub and in editors.
fn check_links(node: &Node, index: &Index, root: &Path, problems: &mut Vec<String>) {
    let rel = node
        .path
        .strip_prefix(root)
        .unwrap_or(&node.path)
        .display()
        .to_string();
    for l in nodes::links(&node.text) {
        if let Err(e) = resolve(&l, node, index) {
            problems.push(format!("{rel}:{}: {e}", l.line));
        }
        match &l.target {
            None => problems.push(format!("{rel}:{}: `{}` has no link target; run `cargo xtask design --fmt`", l.line, l.text)),
            Some(t) if t.contains("::") => problems.push(format!("{rel}:{}: `{}` links to a rustdoc path, not a file; run `cargo xtask design --fmt`", l.line, l.text)),
            Some(t) => {
                let file = t.split('#').next().unwrap_or(t);
                if !file.starts_with("http") && !node.dir().join(file).exists() {
                    problems.push(format!("{rel}:{}: link target `{file}` does not exist", l.line));
                }
            }
        }
    }
}

/// A producer claim: the link's function returns the type, or the type is
/// built from the link's type.
fn verify_producer(r: &Ref, subject: Id, index: &Index) -> Result<Option<Claim>, String> {
    if let Some(m) = r.member {
        let sig = index.sigs.get(&m).ok_or("not a function")?;
        let returns_it = sig.outputs.contains(&subject) || (r.item == subject && returns_self(sig));
        if !returns_it {
            return Err(format!(
                "{}::{} returns {}",
                index.entries[&r.item].name,
                r.member_name.as_deref().unwrap_or(""),
                describe_output(sig, index)
            ));
        }
        return Ok(Some(Claim::ProducedBy { subject, f: m }));
    }
    let e = &index.entries[&r.item];
    match e.kind {
        Kind::Function => {
            let sig = index.sigs.get(&r.item).ok_or("no signature")?;
            if !sig.outputs.contains(&subject) {
                return Err(format!(
                    "{} returns {}",
                    e.name,
                    describe_output(sig, index)
                ));
            }
            Ok(Some(Claim::ProducedBy { subject, f: r.item }))
        }
        Kind::Module => Ok(None),
        _ => {
            if r.item == subject {
                return Ok(None);
            }
            if built_from(subject, r.item, index) {
                Ok(Some(Claim::BuiltFrom {
                    subject,
                    from: r.item,
                }))
            } else {
                Err(format!(
                    "{} has no field of type {}, no constructor taking one, and no From/TryFrom impl",
                    index.entries[&subject].name, e.name
                ))
            }
        }
    }
}

/// A consumer claim: the link's function takes the type, or the link's type
/// is built from it.
fn verify_consumer(
    r: &Ref,
    subject: Id,
    index: &Index,
    graph: &Graph,
) -> Result<Option<Claim>, String> {
    let subj = &index.entries[&subject];
    let uses = |consumer: Id| {
        graph
            .edges
            .contains_key(&(subject, consumer, EdgeKind::Uses))
    };
    if let Some(m) = r.member {
        let sig = index.sigs.get(&m).ok_or("not a function")?;
        if subj.kind == Kind::Module {
            return if uses(r.item) {
                Ok(Some(Claim::BuiltInto {
                    subject,
                    into: r.item,
                }))
            } else {
                Err(format!(
                    "{} never names `{}::`",
                    index.entries[&r.item].name, subj.name
                ))
            };
        }
        if r.item == subject {
            return Ok(None);
        }
        let takes = sig
            .inputs
            .iter()
            .any(|(n, ids)| n != "self" && ids.contains(&subject));
        if !takes {
            return Err(format!(
                "{}::{} takes no {}",
                index.entries[&r.item].name,
                r.member_name.as_deref().unwrap_or(""),
                subj.name
            ));
        }
        return Ok(Some(Claim::ConsumedBy { subject, g: m }));
    }
    let e = &index.entries[&r.item];
    match e.kind {
        Kind::Function => {
            if subj.kind == Kind::Module {
                return if uses(r.item) {
                    Ok(Some(Claim::BuiltInto {
                        subject,
                        into: r.item,
                    }))
                } else {
                    Err(format!("{} never names `{}::`", e.name, subj.name))
                };
            }
            let sig = index.sigs.get(&r.item).ok_or("no signature")?;
            if !sig.inputs.iter().any(|(_, ids)| ids.contains(&subject)) {
                return Err(format!("{} takes no {}", e.name, subj.name));
            }
            Ok(Some(Claim::ConsumedBy { subject, g: r.item }))
        }
        Kind::Module => Err("a binding module does not consume a type".into()),
        _ => {
            if r.item == subject {
                return Ok(None);
            }
            if subj.kind == Kind::Module {
                return if uses(r.item) {
                    Ok(Some(Claim::BuiltInto {
                        subject,
                        into: r.item,
                    }))
                } else {
                    Err(format!("no method of {} names `{}::`", e.name, subj.name))
                };
            }
            if built_from(r.item, subject, index) {
                Ok(Some(Claim::BuiltInto {
                    subject,
                    into: r.item,
                }))
            } else {
                Err(format!(
                    "{} has no field of type {}, no constructor taking one, and no From/TryFrom impl",
                    e.name, subj.name
                ))
            }
        }
    }
}

/// `t` is built from `y`: a field of type `y`, a function returning `t`
/// that takes `y`, or a `From`/`TryFrom` impl.
fn built_from(t: Id, y: Id, index: &Index) -> bool {
    if index.contains(t, y) {
        return true;
    }
    if index
        .conversions
        .get(&t)
        .is_some_and(|srcs| srcs.contains(&y))
    {
        return true;
    }
    index.sigs.iter().any(|(fid, sig)| {
        let owner = index.self_of.get(fid).copied();
        let returns_t = sig.outputs.contains(&t) || (owner == Some(t) && returns_self(sig));
        let takes_y = sig
            .inputs
            .iter()
            .any(|(n, ids)| n != "self" && ids.contains(&y))
            || (owner == Some(y) && sig.inputs.iter().any(|(n, _)| n == "self"));
        returns_t && takes_y
    })
}

/// Whether a signature returns `Self`, bare or inside a `Result`, an
/// `Option` or a tuple.
fn returns_self(sig: &Signature) -> bool {
    fn is_self(t: &Type) -> bool {
        matches!(t, Type::Generic(g) if g == "Self")
    }
    match &sig.output {
        Some(t) if is_self(t) => true,
        Some(Type::ResolvedPath(p)) => p.args.as_ref().is_some_and(|a| match a.as_ref() {
            rustdoc_types::GenericArgs::AngleBracketed { args, .. } => args
                .iter()
                .any(|x| matches!(x, rustdoc_types::GenericArg::Type(t) if is_self(t))),
            _ => false,
        }),
        Some(Type::Tuple(ts)) => ts.iter().any(is_self),
        _ => false,
    }
}

/// The crate's types a signature returns, for an error message.
fn describe_output(sig: &Signature, index: &Index) -> String {
    if sig.outputs.is_empty() {
        return "nothing of the crate's".into();
    }
    sig.outputs
        .iter()
        .map(|id| index.name_of(*id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ")
}

/// Rewrite every link's target to its canonical file: the item's source
/// for the node's own types and for the Type column, the owning design
/// node for another component's items.
fn canonical_links(node: &Node, index: &Index, root: &Path) -> String {
    let table = nodes::table(node).ok();
    let type_column: HashSet<(usize, usize)> = table
        .iter()
        .flat_map(|t| {
            t.rows
                .iter()
                .flat_map(|r| r.subjects.iter().map(|l| l.span))
        })
        .collect();
    let dir = node
        .path
        .strip_prefix(root)
        .unwrap_or(&node.path)
        .parent()
        .unwrap_or(Path::new(""))
        .to_path_buf();
    let mut edits: Vec<((usize, usize), String)> = Vec::new();
    for l in nodes::links(&node.text) {
        let Ok(r) = resolve(&l, node, index) else {
            continue;
        };
        let e = &index.entries[&r.item];
        let component = e.component();
        let is_component = e.kind == Kind::Module && e.module.is_empty();
        let target = if is_component {
            node_link(&dir, component)
        } else if type_column.contains(&l.span) || component == node.name || node.is_root() {
            let (file, line) = r
                .member
                .and_then(|m| index.location(m))
                .unwrap_or((e.file.clone(), e.line));
            source_link(&dir, &file, line)
        } else {
            node_link(&dir, component)
        };
        edits.push((l.span, format!("[`{}`]({target})", l.text)));
    }
    let mut out = String::with_capacity(node.text.len());
    let mut pos = 0;
    for ((a, b), rep) in edits {
        out.push_str(&node.text[pos..a]);
        out.push_str(&rep);
        pos = b;
    }
    out.push_str(&node.text[pos..]);
    out
}

/// A link from a node's directory to a source line.
fn source_link(dir: &Path, file: &str, line: usize) -> String {
    format!("{}#L{line}", relative(dir, Path::new(file)))
}

/// A link from a node's directory to another component's node.
fn node_link(dir: &Path, component: &str) -> String {
    relative(dir, &Path::new("src").join(component).join("DESIGN.md"))
}

/// `to` relative to the directory `from`, both repo-relative.
fn relative(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().filter(|c| c.as_os_str() != ".").collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - common];
    parts.extend(
        to[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}
