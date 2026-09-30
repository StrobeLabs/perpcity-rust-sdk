//! The crate's public items, indexed by name, with what the graph needs of
//! each: kind, module path, source span, fields, methods, and the
//! conversions between types.

use std::collections::{BTreeMap, HashMap, HashSet};

use rustdoc_types::{
    AssocItemConstraintKind, Crate, GenericArg, GenericArgs, Id, Item, ItemEnum, Path as RdPath,
    StructKind, Term, Type, VariantKind, Visibility,
};

/// Modules documented apart from the design node that owns them.
pub fn component_of(top: &str) -> &str {
    match top {
        "convert" | "constants" => "types",
        "storage" => "contracts",
        other => other,
    }
}

/// What an item is, for drawing and for the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Function,
    Module,
}

impl Kind {
    pub fn is_type(self) -> bool {
        matches!(
            self,
            Kind::Struct | Kind::Enum | Kind::Trait | Kind::TypeAlias
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Trait => "trait",
            Kind::TypeAlias => "type",
            Kind::Function => "fn",
            Kind::Module => "module",
        }
    }
}

/// One public item.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: Id,
    pub name: String,
    pub kind: Kind,
    /// Module path below the crate root, e.g. `["math", "pricing"]`.
    pub module: Vec<String>,
    pub file: String,
    pub line: usize,
}

impl Entry {
    /// The design node that owns the item: its top-level module, or the
    /// node a top-level module without one belongs to.
    pub fn top(&self) -> &str {
        component_of(self.module.first().map(String::as_str).unwrap_or(""))
    }

    /// The design node a top-level module link names.
    pub fn component(&self) -> &str {
        if self.kind == Kind::Module && self.module.is_empty() {
            component_of(&self.name)
        } else {
            self.top()
        }
    }

    pub fn path(&self) -> String {
        let mut p = self.module.join("::");
        if !p.is_empty() {
            p.push_str("::");
        }
        p.push_str(&self.name);
        p
    }
}

/// A struct field or variant payload: its name, the type ids it mentions,
/// its type, and whether it is public.
pub type Field = (String, Vec<Id>, Type, bool);

/// A function's signature reduced to the local type ids it mentions.
#[derive(Debug, Clone, Default)]
pub struct Signature {
    /// `(name, ids mentioned)` per parameter, `self` included.
    pub inputs: Vec<(String, Vec<Id>)>,
    /// Whether the receiver is `self` by value.
    pub consumes_self: bool,
    pub outputs: Vec<Id>,
    /// The output as rustdoc gives it, for the rules that read its shape.
    pub output: Option<Type>,
    pub input_types: Vec<Type>,
}

pub struct Index {
    pub krate: Crate,
    pub entries: HashMap<Id, Entry>,
    /// Items by bare name; a name can have several homes.
    pub by_name: HashMap<String, Vec<Id>>,
    /// Public inherent and trait methods of each public type.
    pub methods: HashMap<Id, Vec<Id>>,
    /// Every method, private ones included, of each public type.
    pub all_methods: HashMap<Id, Vec<Id>>,
    /// The self type of each method.
    pub self_of: HashMap<Id, Id>,
    /// Signatures of every function and method, private ones included.
    pub sigs: HashMap<Id, Signature>,
    /// Field types of each local struct, public or private, and the
    /// payloads of each enum's variants.
    pub fields: HashMap<Id, Vec<Field>>,
    /// What each type alias stands for.
    pub aliases: HashMap<Id, Vec<Id>>,
    /// Methods that implement a trait rather than the type's own API.
    pub trait_methods: HashSet<Id>,
    /// `From<Y> for T` and `TryFrom<Y> for T`: `T -> [Y]`.
    pub conversions: HashMap<Id, Vec<Id>>,
    /// Docs of every item with docs.
    pub docs: HashMap<Id, String>,
    /// Variants of each enum.
    pub variants: HashMap<Id, Vec<Id>>,
}

impl Index {
    pub fn build(krate: Crate) -> Self {
        let crate_name = krate
            .paths
            .get(&krate.root)
            .map(|p| p.path[0].clone())
            .unwrap_or_default();
        let mut entries = HashMap::new();
        let mut by_name: HashMap<String, Vec<Id>> = HashMap::new();
        let mut methods: HashMap<Id, Vec<Id>> = HashMap::new();
        let mut all_methods: HashMap<Id, Vec<Id>> = HashMap::new();
        let mut self_of = HashMap::new();
        let mut sigs = HashMap::new();
        let mut fields: HashMap<Id, Vec<Field>> = HashMap::new();
        let mut conversions: HashMap<Id, Vec<Id>> = HashMap::new();
        let mut docs = HashMap::new();
        let mut variants = HashMap::new();
        let mut aliases = HashMap::new();
        let mut trait_methods = HashSet::new();

        for (id, item) in &krate.index {
            if item.crate_id != 0 {
                continue;
            }
            if let Some(d) = &item.docs {
                docs.insert(*id, d.clone());
            }
            let kind = match &item.inner {
                ItemEnum::Struct(_) => Some(Kind::Struct),
                ItemEnum::Enum(_) => Some(Kind::Enum),
                ItemEnum::Trait(_) => Some(Kind::Trait),
                ItemEnum::TypeAlias(_) => Some(Kind::TypeAlias),
                ItemEnum::Function(_) => Some(Kind::Function),
                ItemEnum::Module(m) if !m.is_crate => Some(Kind::Module),
                _ => None,
            };
            if let Some(kind) = kind
                && matches!(item.visibility, Visibility::Public)
            {
                // Only public items with a path are the surface; methods
                // have no entry in `paths` and are indexed through impls.
                if let Some(summary) = krate.paths.get(id) {
                    let mut module: Vec<String> = summary.path.clone();
                    module.pop();
                    if module.first().map(String::as_str) == Some(crate_name.as_str()) {
                        module.remove(0);
                    }
                    let (file, line) = span_of(item);
                    let name = item.name.clone().unwrap_or_default();
                    by_name.entry(name.clone()).or_default().push(*id);
                    entries.insert(
                        *id,
                        Entry {
                            id: *id,
                            name,
                            kind,
                            module,
                            file,
                            line,
                        },
                    );
                }
            }
            match &item.inner {
                ItemEnum::Function(f) => {
                    sigs.insert(*id, signature(&f.sig));
                }
                ItemEnum::Struct(s) => {
                    let ids: Vec<Id> = match &s.kind {
                        StructKind::Plain { fields, .. } => fields.clone(),
                        StructKind::Tuple(fs) => fs.iter().flatten().copied().collect(),
                        StructKind::Unit => vec![],
                    };
                    let stripped = matches!(
                        &s.kind,
                        StructKind::Plain {
                            has_stripped_fields: true,
                            ..
                        }
                    ) || matches!(&s.kind, StructKind::Tuple(fs) if fs.iter().any(Option::is_none));
                    let mut fs = Vec::new();
                    for fid in ids {
                        if let Some(fi) = krate.index.get(&fid)
                            && let ItemEnum::StructField(ty) = &fi.inner
                        {
                            let public = matches!(fi.visibility, Visibility::Public);
                            fs.push((
                                fi.name.clone().unwrap_or_default(),
                                mentions(ty),
                                ty.clone(),
                                public,
                            ));
                        }
                    }
                    if stripped {
                        fs.push(("<private>".into(), vec![], Type::Infer, false));
                    }
                    fields.insert(*id, fs);
                }
                ItemEnum::Enum(e) => {
                    variants.insert(*id, e.variants.clone());
                    let mut fs = Vec::new();
                    for vid in &e.variants {
                        let Some(vi) = krate.index.get(vid) else {
                            continue;
                        };
                        let ItemEnum::Variant(v) = &vi.inner else {
                            continue;
                        };
                        let vname = vi.name.clone().unwrap_or_default();
                        let payload: Vec<Id> = match &v.kind {
                            VariantKind::Plain => vec![],
                            VariantKind::Tuple(ids) => ids.iter().flatten().copied().collect(),
                            VariantKind::Struct { fields, .. } => fields.clone(),
                        };
                        for fid in payload {
                            if let Some(fi) = krate.index.get(&fid)
                                && let ItemEnum::StructField(ty) = &fi.inner
                            {
                                fs.push((
                                    format!("{vname}.{}", fi.name.clone().unwrap_or_default()),
                                    mentions(ty),
                                    ty.clone(),
                                    true,
                                ));
                            }
                        }
                    }
                    fields.insert(*id, fs);
                }
                ItemEnum::TypeAlias(ta) => {
                    aliases.insert(*id, mentions(&ta.type_));
                }
                _ => {}
            }
        }

        for item in krate.index.values() {
            let ItemEnum::Impl(im) = &item.inner else {
                continue;
            };
            let Type::ResolvedPath(for_) = &im.for_ else {
                continue;
            };
            if !entries.contains_key(&for_.id) {
                continue;
            }
            if let Some(tr) = &im.trait_ {
                let name = tr.path.rsplit("::").next().unwrap_or(&tr.path).to_string();
                if name == "From" || name == "TryFrom" {
                    for src in generic_type_ids(tr) {
                        conversions.entry(for_.id).or_default().push(src);
                    }
                }
            }
            for mid in &im.items {
                if let Some(mi) = krate.index.get(mid)
                    && matches!(mi.inner, ItemEnum::Function(_))
                {
                    all_methods.entry(for_.id).or_default().push(*mid);
                    self_of.insert(*mid, for_.id);
                    // A trait method is as public as its trait's impl.
                    if im.trait_.is_some() {
                        trait_methods.insert(*mid);
                    }
                    if im.trait_.is_some() || matches!(mi.visibility, Visibility::Public) {
                        methods.entry(for_.id).or_default().push(*mid);
                    }
                }
            }
        }

        Index {
            krate,
            entries,
            by_name,
            methods,
            all_methods,
            self_of,
            sigs,
            fields,
            aliases,
            trait_methods,
            conversions,
            docs,
            variants,
        }
    }

    /// `t` holds a `y`: a field of that type, directly or through private
    /// structs it owns (an `Arc<Inner>`, say), an enum payload, or an alias.
    pub fn contains(&self, t: Id, y: Id) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![t];
        while let Some(cur) = stack.pop() {
            if !seen.insert(cur) {
                continue;
            }
            if let Some(ids) = self.aliases.get(&cur) {
                if ids.contains(&y) {
                    return true;
                }
                stack.extend(ids.iter().copied());
            }
            for (_, ids, _, _) in self.fields.get(&cur).into_iter().flatten() {
                if ids.contains(&y) {
                    return true;
                }
                // Descend only into the crate's private types: a public
                // type's own containment is its own row's business.
                stack.extend(
                    ids.iter().copied().filter(|id| {
                        !self.entries.contains_key(id) && self.fields.contains_key(id)
                    }),
                );
            }
        }
        false
    }

    /// A method or variant named `name` on a type of component `top`, when
    /// exactly one exists; how a bare `quote_perp` in a node finds its type.
    pub fn member_in(&self, top: &str, name: &str) -> Option<(Id, Id)> {
        let mut found = Vec::new();
        for e in self.entries.values() {
            if !e.kind.is_type() || (!top.is_empty() && e.top() != top) {
                continue;
            }
            if let Some(m) = self.method(e.id, name) {
                found.push((e.id, m));
            }
            if let Some(v) = self.variant(e.id, name) {
                found.push((e.id, v));
            }
        }
        found.sort();
        found.dedup();
        (found.len() == 1).then(|| found[0])
    }

    pub fn item(&self, id: Id) -> Option<&Item> {
        self.krate.index.get(&id)
    }

    pub fn name_of(&self, id: Id) -> String {
        self.krate
            .index
            .get(&id)
            .and_then(|i| i.name.clone())
            .or_else(|| {
                self.krate
                    .paths
                    .get(&id)
                    .and_then(|p| p.path.last().cloned())
            })
            .unwrap_or_else(|| format!("#{}", id.0))
    }

    /// A public method of `ty` by name.
    pub fn method(&self, ty: Id, name: &str) -> Option<Id> {
        self.methods
            .get(&ty)?
            .iter()
            .copied()
            .find(|m| self.krate.index.get(m).and_then(|i| i.name.as_deref()) == Some(name))
    }

    /// A variant of enum `ty` by name.
    pub fn variant(&self, ty: Id, name: &str) -> Option<Id> {
        self.variants
            .get(&ty)?
            .iter()
            .copied()
            .find(|v| self.krate.index.get(v).and_then(|i| i.name.as_deref()) == Some(name))
    }

    /// Source file and line of any item, methods and variants included.
    pub fn location(&self, id: Id) -> Option<(String, usize)> {
        let item = self.krate.index.get(&id)?;
        Some(span_of(item)).filter(|(f, _)| !f.is_empty())
    }

    /// Whether a function is public, its owner's visibility included.
    pub fn is_public_fn(&self, id: Id) -> bool {
        let Some(item) = self.krate.index.get(&id) else {
            return false;
        };
        match self.self_of.get(&id) {
            Some(owner) => {
                self.entries.contains_key(owner)
                    && self.methods.get(owner).is_some_and(|ms| ms.contains(&id))
            }
            None => self.entries.contains_key(&id) && matches!(item.visibility, Visibility::Public),
        }
    }

    /// Which crate an id belongs to, by name; `None` for this crate.
    pub fn crate_of(&self, id: Id) -> Option<&str> {
        let summary = self.krate.paths.get(&id)?;
        if summary.crate_id == 0 {
            return None;
        }
        self.krate
            .external_crates
            .get(&summary.crate_id)
            .map(|c| c.name.as_str())
    }

    /// The entries sorted by path, for deterministic output.
    pub fn sorted(&self) -> Vec<&Entry> {
        let mut v: Vec<&Entry> = self.entries.values().collect();
        v.sort_by_key(|e| (e.path(), e.kind));
        v
    }

    /// Public functions and methods with their signatures, sorted.
    pub fn functions(&self) -> BTreeMap<String, (Id, &Signature)> {
        let mut out = BTreeMap::new();
        for (id, sig) in &self.sigs {
            let Some(item) = self.krate.index.get(id) else {
                continue;
            };
            if !self.is_public_fn(*id) {
                continue;
            }
            let name = item.name.clone().unwrap_or_default();
            let label = match self.self_of.get(id).and_then(|t| self.entries.get(t)) {
                Some(owner) => format!("{}::{}", owner.path(), name),
                None => match self.entries.get(id) {
                    Some(e) => e.path(),
                    None => continue,
                },
            };
            out.insert(label, (*id, sig));
        }
        out
    }
}

/// An item's source file and first line, empty when rustdoc has none.
fn span_of(item: &Item) -> (String, usize) {
    match &item.span {
        Some(s) => (s.filename.to_string_lossy().into_owned(), s.begin.0),
        None => (String::new(), 0),
    }
}

/// Reduce rustdoc's signature to the type ids each parameter and the
/// return mention.
fn signature(sig: &rustdoc_types::FunctionSignature) -> Signature {
    let mut out = Signature::default();
    for (name, ty) in &sig.inputs {
        if name == "self" {
            out.consumes_self = matches!(ty, Type::Generic(g) if g == "Self");
        }
        out.inputs.push((name.clone(), mentions(ty)));
        out.input_types.push(ty.clone());
    }
    if let Some(o) = &sig.output {
        out.outputs = mentions(o);
        out.output = Some(o.clone());
    }
    out
}

/// Every resolved type id a type mentions, generics included.
pub fn mentions(ty: &Type) -> Vec<Id> {
    let mut out = Vec::new();
    walk(ty, &mut out);
    out
}

/// Collect the ids a type mentions, through references, tuples, arrays,
/// generic arguments, trait objects and function pointers.
fn walk(ty: &Type, out: &mut Vec<Id>) {
    match ty {
        Type::ResolvedPath(p) => {
            out.push(p.id);
            walk_args(p, out);
        }
        Type::Tuple(ts) => ts.iter().for_each(|t| walk(t, out)),
        Type::Slice(t) | Type::RawPointer { type_: t, .. } | Type::BorrowedRef { type_: t, .. } => {
            walk(t, out)
        }
        Type::Array { type_, .. } => walk(type_, out),
        Type::QualifiedPath {
            self_type,
            trait_,
            args,
            ..
        } => {
            walk(self_type, out);
            if let Some(t) = trait_ {
                out.push(t.id);
            }
            if let Some(a) = args {
                walk_generic_args(a, out);
            }
        }
        Type::ImplTrait(bounds) => {
            for b in bounds {
                if let rustdoc_types::GenericBound::TraitBound { trait_, .. } = b {
                    out.push(trait_.id);
                    walk_args(trait_, out);
                }
            }
        }
        Type::DynTrait(d) => {
            for t in &d.traits {
                out.push(t.trait_.id);
                walk_args(&t.trait_, out);
            }
        }
        Type::FunctionPointer(f) => {
            f.sig.inputs.iter().for_each(|(_, t)| walk(t, out));
            if let Some(o) = &f.sig.output {
                walk(o, out);
            }
        }
        Type::Generic(_) | Type::Primitive(_) | Type::Infer | Type::Pat { .. } => {}
    }
}

/// The ids in a path's generic arguments.
fn walk_args(p: &RdPath, out: &mut Vec<Id>) {
    if let Some(a) = &p.args {
        walk_generic_args(a, out);
    }
}

/// The ids in generic arguments, associated-type constraints included.
fn walk_generic_args(a: &GenericArgs, out: &mut Vec<Id>) {
    match a {
        GenericArgs::AngleBracketed { args, constraints } => {
            for arg in args {
                if let GenericArg::Type(t) = arg {
                    walk(t, out);
                }
            }
            // `impl IntoIterator<Item = &TapeEvent>` mentions TapeEvent.
            for c in constraints {
                if let AssocItemConstraintKind::Equality(Term::Type(t)) = &c.binding {
                    walk(t, out);
                }
            }
        }
        GenericArgs::Parenthesized { inputs, output } => {
            inputs.iter().for_each(|t| walk(t, out));
            if let Some(o) = output {
                walk(o, out);
            }
        }
        GenericArgs::ReturnTypeNotation => {}
    }
}

/// The type ids in a trait path's generic arguments, e.g. `Y` in `From<Y>`.
fn generic_type_ids(p: &RdPath) -> Vec<Id> {
    let mut out = Vec::new();
    walk_args(p, &mut out);
    out
}

/// Whether a type is a `Result`, and its error type's id if it is a path.
pub fn result_error(ty: &Type) -> Option<Option<Id>> {
    let Type::ResolvedPath(p) = ty else {
        return None;
    };
    if p.path.rsplit("::").next() != Some("Result") {
        return None;
    }
    let Some(args) = &p.args else {
        return Some(None);
    };
    let GenericArgs::AngleBracketed { args, .. } = args.as_ref() else {
        return Some(None);
    };
    match args.get(1) {
        Some(GenericArg::Type(Type::ResolvedPath(e))) => Some(Some(e.id)),
        Some(_) => Some(None),
        None => Some(None),
    }
}

/// Whether a type mentions a primitive by name, e.g. `f64`, anywhere.
pub fn mentions_primitive(ty: &Type, prim: &str) -> bool {
    match ty {
        Type::Primitive(p) => p == prim,
        Type::ResolvedPath(p) => p
            .args
            .as_ref()
            .is_some_and(|a| args_mention_primitive(a, prim)),
        Type::Tuple(ts) => ts.iter().any(|t| mentions_primitive(t, prim)),
        Type::Slice(t) | Type::RawPointer { type_: t, .. } | Type::BorrowedRef { type_: t, .. } => {
            mentions_primitive(t, prim)
        }
        Type::Array { type_, .. } => mentions_primitive(type_, prim),
        _ => false,
    }
}

/// Whether generic arguments mention a primitive by name.
fn args_mention_primitive(a: &GenericArgs, prim: &str) -> bool {
    match a {
        GenericArgs::AngleBracketed { args, .. } => args
            .iter()
            .any(|arg| matches!(arg, GenericArg::Type(t) if mentions_primitive(t, prim))),
        GenericArgs::Parenthesized { inputs, output } => {
            inputs.iter().any(|t| mentions_primitive(t, prim))
                || output.as_ref().is_some_and(|o| mentions_primitive(o, prim))
        }
        GenericArgs::ReturnTypeNotation => false,
    }
}
