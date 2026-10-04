//! The design nodes: every `DESIGN.md`, its type table, and the links in it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// One `DESIGN.md`.
#[derive(Debug, Clone)]
pub struct Node {
    /// The node's name: its directory below `src`, `history` or
    /// `history/replay`; `""` for the root.
    pub name: String,
    pub path: PathBuf,
    pub text: String,
}

impl Node {
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    pub fn is_root(&self) -> bool {
        self.name.is_empty()
    }

    /// The top-level module the node sits in: `history` for both `history`
    /// and `history/replay`.
    pub fn component(&self) -> &str {
        self.name.split('/').next().unwrap_or("")
    }

    /// The module path the node documents, as segments.
    pub fn segments(&self) -> Vec<&str> {
        self.name.split('/').filter(|s| !s.is_empty()).collect()
    }

    /// Whether this node documents the module at `module` or one of its
    /// ancestors.
    pub fn covers(&self, module: &[String]) -> bool {
        let segs = self.segments();
        !segs.is_empty()
            && module.len() >= segs.len()
            && segs.iter().zip(module).all(|(a, b)| a == b)
    }
}

/// The node that owns a module: the deepest one whose path is a prefix of
/// `module`, so a type in `history::replay` belongs to the replay node when
/// one exists and to the history node otherwise.
pub fn owner<'a>(nodes: &'a [Node], module: &[String]) -> Option<&'a Node> {
    nodes
        .iter()
        .filter(|n| n.covers(module))
        .max_by_key(|n| n.segments().len())
}

/// A markdown link with code text: `[`text`](target)` or `[`text`]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub text: String,
    pub target: Option<String>,
    /// Byte range of the whole link in the file.
    pub span: (usize, usize),
    pub line: usize,
}

/// A row of a type table, as the columns the graph reads.
#[derive(Debug, Clone)]
pub struct Row {
    pub line: usize,
    pub subjects: Vec<Link>,
    pub invariant: String,
    pub produced: String,
    pub produced_links: Vec<Link>,
    pub consumed: String,
    pub consumed_links: Vec<Link>,
}

/// The rows of a node's type table.
#[derive(Debug, Clone, Default)]
pub struct Table {
    pub rows: Vec<Row>,
}

/// The root node and every `DESIGN.md` under `src`, nested modules
/// included, in path order. A nested node is named by its path below
/// `src`, so `src/history/replay/DESIGN.md` is the node `history/replay`.
pub fn discover(root: &Path) -> Result<Vec<Node>> {
    let mut nodes = vec![Node {
        name: String::new(),
        path: root.join("DESIGN.md"),
        text: fs::read_to_string(root.join("DESIGN.md")).context("reading DESIGN.md")?,
    }];
    let src = root.join("src");
    let mut dirs = Vec::new();
    collect(&src, &mut dirs)?;
    dirs.sort();
    for dir in dirs {
        let path = dir.join("DESIGN.md");
        let name = dir
            .strip_prefix(&src)
            .unwrap_or(&dir)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        nodes.push(Node {
            name,
            text: fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?,
            path,
        });
    }
    Ok(nodes)
}

/// Every directory under `dir`, itself excluded, that holds a `DESIGN.md`.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("DESIGN.md").is_file() {
            out.push(path.clone());
        }
        collect(&path, out)?;
    }
    Ok(())
}

/// Every code-text link in the file, in order.
pub fn links(text: &str) -> Vec<Link> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(start) = text[i..].find("[`") {
        let start = i + start;
        let Some(end_text) = text[start + 2..].find("`]") else {
            break;
        };
        let end_text = start + 2 + end_text;
        let inner = &text[start + 2..end_text];
        let mut end = end_text + 2;
        let mut target = None;
        if bytes.get(end) == Some(&b'(')
            && let Some(close) = text[end..].find(')')
        {
            target = Some(text[end + 1..end + close].to_string());
            end += close + 1;
        }
        let line = text[..start].matches('\n').count() + 1;
        out.push(Link {
            text: inner.to_string(),
            target,
            span: (start, end),
            line,
        });
        i = end;
    }
    out
}

/// The node's type table: the one whose header has the two flow columns.
pub fn table(node: &Node) -> Result<Table> {
    let lines: Vec<&str> = node.text.lines().collect();
    let mut offset = 0usize;
    let mut offsets = Vec::with_capacity(lines.len());
    for l in &lines {
        offsets.push(offset);
        offset += l.len() + 1;
    }
    let all = links(&node.text);
    let links_in = |lo: usize, hi: usize| -> Vec<Link> {
        all.iter()
            .filter(|l| l.span.0 >= lo && l.span.1 <= hi)
            .cloned()
            .collect()
    };
    for (i, line) in lines.iter().enumerate() {
        let header = cells(line);
        let (Some(prod), Some(cons)) = (
            header.iter().position(|c| c == "Produced by"),
            header.iter().position(|c| c == "Consumed by"),
        ) else {
            continue;
        };
        let inv = header.iter().position(|c| c == "Invariant");
        let mut rows = Vec::new();
        for (j, row) in lines.iter().enumerate().skip(i + 2) {
            if !row.trim_start().starts_with('|') {
                break;
            }
            let rc = cells(row);
            if rc.len() != header.len() {
                bail!(
                    "{}:{}: {} cells where the header has {}",
                    node.path.display(),
                    j + 1,
                    rc.len(),
                    header.len()
                );
            }
            let cell_span = |k: usize| -> (usize, usize) {
                // byte range of cell k within the line
                let mut pos = 0;
                let mut idx = 0;
                let mut start = 0;
                for (b, ch) in row.char_indices() {
                    if ch == '|' {
                        if idx == k + 1 {
                            return (offsets[j] + start, offsets[j] + b);
                        }
                        idx += 1;
                        start = b + 1;
                    }
                    pos = b;
                }
                (offsets[j] + start, offsets[j] + pos + 1)
            };
            let (s0, s1) = cell_span(0);
            let (p0, p1) = cell_span(prod);
            let (c0, c1) = cell_span(cons);
            rows.push(Row {
                line: j + 1,
                subjects: links_in(s0, s1),
                invariant: inv.map(|k| rc[k].clone()).unwrap_or_default(),
                produced: rc[prod].clone(),
                produced_links: links_in(p0, p1),
                consumed: rc[cons].clone(),
                consumed_links: links_in(c0, c1),
            });
        }
        return Ok(Table { rows });
    }
    if node.is_root() {
        return Ok(Table::default());
    }
    bail!(
        "{}: no type table with Produced by and Consumed by columns",
        node.path.display()
    )
}

/// A table row's cells, trimmed; empty for a line that is not a row.
fn cells(line: &str) -> Vec<String> {
    let t = line.trim();
    if !t.starts_with('|') {
        return vec![];
    }
    t.trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}
