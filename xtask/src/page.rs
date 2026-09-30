//! The interactive page: the template with the graph's data inlined.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rustdoc_types::Id;
use serde::Serialize;

use crate::design::{EdgeKind, Graph};
use crate::index::Index;
use crate::nodes::{self, Node};

const TEMPLATE: &str = include_str!("../assets/design.html");

#[derive(Serialize)]
struct NodeOut {
    id: String,
    name: String,
    kind: &'static str,
    component: String,
    public: bool,
    file: String,
    line: usize,
    invariant: String,
    produced: String,
    consumed: String,
    node: String,
}

#[derive(Serialize)]
struct EdgeOut {
    source: String,
    target: String,
    label: String,
    kind: &'static str,
    designed: bool,
}

#[derive(Serialize)]
struct Data {
    nodes: Vec<NodeOut>,
    edges: Vec<EdgeOut>,
    components: Vec<String>,
    repo: String,
    root: String,
}

/// Write the page with the graph's data inlined, and return its path.
pub fn write(root: &Path, index: &Index, graph: &Graph, nodes: &[Node]) -> Result<PathBuf> {
    let repo = repository_url(root);
    let key = |id: Id| index.entries[&id].path();
    let mut out_nodes = Vec::new();
    let mut components = std::collections::BTreeSet::new();
    for &id in &graph.nodes {
        let e = &index.entries[&id];
        let row = graph
            .rows
            .iter()
            .find(|r| r.subject == id || r.companions.contains(&id));
        let node_of_row = row
            .map(|r| r.node.clone())
            .unwrap_or_else(|| e.top().to_string());
        let node = nodes.iter().find(|n| n.name == node_of_row);
        let linkify = |cell: &str| -> String {
            match node {
                Some(n) => html_cell(cell, n, index, graph),
                None => html_escape(cell),
            }
        };
        components.insert(e.top().to_string());
        out_nodes.push(NodeOut {
            id: key(id),
            name: e.name.clone(),
            kind: e.kind.label(),
            component: e.top().to_string(),
            public: row.is_some_and(|r| r.public),
            file: e.file.clone(),
            line: e.line,
            invariant: row
                .filter(|r| r.subject == id)
                .map(|r| linkify(&r.invariant))
                .unwrap_or_default(),
            produced: row
                .filter(|r| r.subject == id)
                .map(|r| linkify(&r.produced))
                .unwrap_or_default(),
            consumed: row
                .filter(|r| r.subject == id)
                .map(|r| linkify(&r.consumed))
                .unwrap_or_default(),
            node: node_of_row,
        });
    }
    let edges = graph
        .edges
        .values()
        .map(|e| EdgeOut {
            source: key(e.src),
            target: key(e.dst),
            label: e.labels.iter().cloned().collect::<Vec<_>>().join(", "),
            kind: match e.kind {
                EdgeKind::Flow => "flow",
                EdgeKind::Carries | EdgeKind::Accessor => "carries",
                EdgeKind::Uses => "uses",
            },
            designed: graph.designed(e, index),
        })
        .collect();
    let data = Data {
        nodes: out_nodes,
        edges,
        components: components.into_iter().collect(),
        repo,
        root: root.display().to_string(),
    };
    let json = serde_json::to_string(&data)?;
    let html = TEMPLATE.replace("/*DATA*/null", &json);
    let dir = root.join("target/design");
    fs::create_dir_all(&dir)?;
    let out = dir.join("index.html");
    fs::write(&out, html)?;
    Ok(out)
}

/// The manifest's repository as a `blob/main/` base, empty when it has none.
fn repository_url(root: &Path) -> String {
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    manifest
        .lines()
        .find_map(|l| {
            l.strip_prefix("repository = ")
                .map(|v| v.trim_matches('"').to_string())
        })
        .map(|r| format!("{r}/blob/main/"))
        .unwrap_or_default()
}

/// A table cell as HTML: code spans, and links that name a drawn type
/// become in-page jumps.
fn html_cell(cell: &str, node: &Node, index: &Index, graph: &Graph) -> String {
    let links = nodes::links(cell);
    let mut out = String::new();
    let mut pos = 0;
    for l in links {
        out.push_str(&code_spans(&cell[pos..l.span.0]));
        let text = html_escape(&l.text);
        match crate::design::resolve(&l, node, index) {
            Ok(r) if graph.nodes.contains(&r.item) => {
                out.push_str(&format!(
                    "<a data-node=\"{}\">{text}</a>",
                    html_escape(&index.entries[&r.item].path())
                ));
            }
            _ => out.push_str(&format!("<code>{text}</code>")),
        }
        pos = l.span.1;
    }
    out.push_str(&code_spans(&cell[pos..]));
    out
}

/// Markdown code spans as `<code>`, everything else escaped.
fn code_spans(s: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for part in s.split('`') {
        if in_code {
            out.push_str("<code>");
            out.push_str(&html_escape(part));
            out.push_str("</code>");
        } else {
            out.push_str(&html_escape(part));
        }
        in_code = !in_code;
    }
    out
}

/// Escape text for an HTML attribute or body.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
