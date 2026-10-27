//! In-memory petgraph of files, symbols, imports and calls.

use crate::index::FileRecord;
use crate::lang::Lang;
use crate::parse::{CallSite, SymKind, Symbol};
use petgraph::stable_graph::{EdgeIndex, NodeIndex, StableDiGraph};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

mod decls;
mod hierarchy;
mod typed;
mod which;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Node {
    File(u32),
    Sym(u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Imports,
    Calls { line: u32 },
}

/// A resolved reference returned to callers of the graph API.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ref {
    pub file: u32,
    /// None = module-level code of the file
    pub sym: Option<u32>,
    pub line: u32,
}

/// The resolution-free part of a graph: nodes, containment edges and name indexes.
struct Nodes {
    g: StableDiGraph<Node, Edge>,
    file_nodes: Vec<NodeIndex>,
    sym_nodes: Vec<Vec<NodeIndex>>,
    by_name: HashMap<String, Vec<(u32, u32)>>,
    calls_by_name: HashMap<String, Vec<(u32, u32)>>,
    path_idx: HashMap<String, u32>,
}

impl Nodes {
    /// `with_calls = false` leaves the call-site index empty (see `Graph::ensure_calls_index`).
    fn new(files: &[FileRecord], with_calls: bool) -> Nodes {
        let mut g = StableDiGraph::with_capacity(files.len() * 8, files.len() * 16);
        let mut file_nodes = Vec::with_capacity(files.len());
        let mut sym_nodes = Vec::with_capacity(files.len());
        let mut by_name: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        let mut calls_by_name: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        let path_idx: HashMap<String, u32> = files.iter().enumerate().map(|(i, f)| (f.path.clone(), i as u32)).collect();

        /* 1. nodes + containment + name indexes */
        for (fi, f) in files.iter().enumerate() {
            let fnode = g.add_node(Node::File(fi as u32));
            file_nodes.push(fnode);
            let mut syms = Vec::with_capacity(f.ex.symbols.len());
            for (si, s) in f.ex.symbols.iter().enumerate() {
                let n = g.add_node(Node::Sym(fi as u32, si as u32));
                syms.push(n);
                if indexed(s) {
                    by_name.entry(s.name.clone()).or_default().push((fi as u32, si as u32));
                }
            }
            sym_nodes.push(syms);
            for (ci, c) in f.ex.calls.iter().enumerate().filter(|_| with_calls) {
                match calls_by_name.get_mut(&c.callee) {
                    Some(v) => v.push((fi as u32, ci as u32)),
                    None => {
                        calls_by_name.insert(c.callee.clone(), vec![(fi as u32, ci as u32)]);
                    }
                }
            }
        }

        Nodes { g, file_nodes, sym_nodes, by_name, calls_by_name, path_idx }
    }
}

/// Imports and call edges as resolved by `Graph::build`; persisted next to the records.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Resolved {
    pub imports: Vec<Vec<u32>>,
    /// [file, caller symbol + 1 (0 = module scope), target file, target symbol, line]
    pub calls: Vec<[u32; 5]>,
}

pub struct Graph {
    /// stable indices: patching a file never renumbers other nodes
    pub g: StableDiGraph<Node, Edge>,
    file_nodes: Vec<NodeIndex>,
    sym_nodes: Vec<Vec<NodeIndex>>,
    /// name -> definitions, sorted by (file, sym)
    pub by_name: HashMap<String, Vec<(u32, u32)>>,
    /// callee name -> call sites (file, index into `ex.calls`), sorted
    pub calls_by_name: HashMap<String, Vec<(u32, u32)>>,
    /// false after `restore` until the first edit needs `calls_by_name`
    calls_indexed: bool,
    pub path_idx: HashMap<String, u32>,
    pub imports: Vec<Vec<u32>>,
    resolver: Resolver,
    /// type -> supertypes, for calls whose receiver type is known
    supers: hierarchy::Supers,
    /// declared field and return types, for receivers settled across files
    decls: decls::DeclIndex,
}

/// Above this many same-named candidates, a call is too ambiguous to link globally.
const MAX_GLOBAL_CANDIDATES: usize = 3;
/// Names defined in more places than this are too generic to narrow by package.
const MAX_NEAREST_SCAN: usize = 32;

/// Which rule picked a call's targets; `Same`/`Imported` hits depend only on the caller's own file and imports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Same,
    Imported,
    Wide,
    /// the call knows its receiver type: depends on every file's types, never skipped as unchanged
    Typed,
}

/// Symbols that go into `by_name` (impl blocks and modules are not call targets).
fn indexed(s: &Symbol) -> bool {
    s.kind != SymKind::Impl && s.kind != SymKind::Module && !s.name.is_empty()
}

/// The file list as resolution sees it, optionally with one file swapped for its previous record.
#[derive(Clone, Copy)]
struct Files<'a> {
    files: &'a [FileRecord],
    swap: Option<(u32, &'a FileRecord)>,
}

impl<'a> Files<'a> {
    fn new(files: &'a [FileRecord]) -> Files<'a> {
        Files { files, swap: None }
    }

    fn rec(&self, i: u32) -> &'a FileRecord {
        match self.swap {
            Some((s, r)) if s == i => r,
            _ => &self.files[i as usize],
        }
    }
}

/// Resolve one call site in file `fi`: same file > imported files > qualified owner > small global set.
fn resolve_call<'a>(
    files: Files<'a>,
    cands: &[(u32, u32)],
    imports: &[u32],
    fi: u32,
    c: &'a CallSite,
    memo: Option<&mut WideMemo<'a>>,
) -> (Tier, Vec<(u32, u32)>) {
    let f = files.rec(fi);
    let ok = |&(cf, cs): &(u32, u32)| {
        let cf_lang = files.rec(cf).lang;
        if cf_lang != f.lang && !(cf_lang.is_js_family() && f.lang.is_js_family()) {
            return false;
        }
        let sym = &files.rec(cf).ex.symbols[cs as usize];
        /* another file can only reach exported symbols (methods go through their object) */
        let reachable = cf == fi || sym.exported || sym.kind == SymKind::Method;
        reachable && (sym.kind.is_callable() || sym.kind == SymKind::Struct)
    };
    /* `cands` is sorted by file, so one file's definitions are a contiguous range */
    let in_file = |t: u32| {
        let lo = cands.partition_point(|&(cf, _)| cf < t);
        let hi = lo + cands[lo..].partition_point(|&(cf, _)| cf == t);
        cands[lo..hi].iter().copied().filter(ok)
    };

    /* 1. same file */
    let chosen: Vec<(u32, u32)> = in_file(fi).collect();
    if !chosen.is_empty() {
        return (Tier::Same, pick_one(files, fi, c, chosen));
    }
    /* 2. imported files */
    let chosen: Vec<(u32, u32)> = if imports.len() < cands.len() {
        imports.iter().flat_map(|&t| in_file(t)).collect()
    } else {
        cands.iter().copied().filter(|&(cf, _)| imports.binary_search(&cf).is_ok()).filter(ok).collect()
    };
    if !chosen.is_empty() {
        return (Tier::Imported, pick_one(files, fi, c, chosen));
    }
    /* 3-4. the rest depends only on the caller's language family and path, so it can be memoized per name */
    let wide = match memo {
        Some(m) => {
            let key = (family(f.lang), c.qual.as_deref());
            m.entry(key).or_insert_with(|| resolve_wide(files, cands, f.lang, c.qual.as_deref())).finish(files, &f.path)
        }
        None => resolve_wide(files, cands, f.lang, c.qual.as_deref()).finish(files, &f.path),
    };
    (Tier::Wide, pick_one(files, fi, c, wide))
}

/// Narrow one tier's candidates to the single definition the call means; empty = unresolved.
/// A call never fans out to several same-named definitions.
fn pick_one(files: Files, fi: u32, c: &CallSite, mut v: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    if v.len() <= 1 {
        return v;
    }
    let sym = |&(cf, cs): &(u32, u32)| &files.rec(cf).ex.symbols[cs as usize];
    let parent_name = |x: &(u32, u32)| sym(x).parent.map(|p| files.rec(x.0).ex.symbols[p as usize].name.as_str());
    let narrow = |v: &mut Vec<(u32, u32)>, keep: &dyn Fn(&(u32, u32)) -> bool| -> bool {
        let n: Vec<(u32, u32)> = v.iter().copied().filter(|x| keep(x)).collect();
        if n.is_empty() {
            return false;
        }
        *v = n;
        v.len() == 1
    };
    /* 1. the owner: `self.f()` / `this.f()` -> the caller's own impl or class; `Foo::f()` -> Foo's */
    match c.qual.as_deref() {
        Some("self" | "this" | "Self") => {
            let caller_parent = c.caller.and_then(|ci| files.rec(fi).ex.symbols.get(ci as usize)).and_then(|s| s.parent);
            if let Some(cp) = caller_parent {
                if narrow(&mut v, &|x| x.0 == fi && sym(x).parent == Some(cp)) {
                    return v;
                }
                /* Rust: another `impl` block of the same type */
                let owner = files.rec(fi).ex.symbols[cp as usize].name.as_str();
                if narrow(&mut v, &|x| parent_name(x) == Some(owner)) {
                    return v;
                }
            }
        }
        Some(q) => {
            if narrow(&mut v, &|x| parent_name(x) == Some(q)) {
                return v;
            }
        }
        None => {}
    }
    /* 2. `x.f()` calls a method, `f()` a free function or constructor */
    if narrow(&mut v, &|x| (sym(x).kind == SymKind::Method) == c.recv) {
        return v;
    }
    /* 3. still ambiguous: leave it unresolved rather than guess */
    Vec::new()
}

/// Memo for the caller-independent part of resolving calls to one name.
type WideMemo<'a> = HashMap<(Option<Lang>, Option<&'a str>), Wide>;

/// Calls link across language families only inside JS/TS (None = the JS family).
fn family(l: Lang) -> Option<Lang> {
    if l.is_js_family() { None } else { Some(l) }
}

/// Outcome of the qualified-owner and global rules, before narrowing to the caller's package.
enum Wide {
    Done(Vec<(u32, u32)>),
    /// 2..=MAX_NEAREST_SCAN global candidates; keep those sharing the longest path prefix with the caller
    Nearest(Vec<(u32, u32)>),
}

impl Wide {
    fn finish(&self, files: Files, path: &str) -> Vec<(u32, u32)> {
        match self {
            Wide::Done(v) => v.clone(),
            Wide::Nearest(global) => {
                let shared = |cf: u32| common_prefix(&files.rec(cf).path, path);
                let best = global.iter().map(|&(cf, _)| shared(cf)).max().unwrap_or(0);
                let near: Vec<(u32, u32)> = global.iter().copied().filter(|&(cf, _)| shared(cf) == best).collect();
                if near.len() <= MAX_GLOBAL_CANDIDATES { near } else { Vec::new() }
            }
        }
    }
}

/// Qualified owner > small global set, for a caller with nothing callable in its own file.
fn resolve_wide(files: Files, cands: &[(u32, u32)], lang: Lang, qual: Option<&str>) -> Wide {
    /* the caller's own file has no candidate (stage 1 was empty), so only cross-file reachability matters */
    let ok = |&(cf, cs): &(u32, u32)| {
        let cf_lang = files.rec(cf).lang;
        if family(cf_lang) != family(lang) {
            return false;
        }
        let sym = &files.rec(cf).ex.symbols[cs as usize];
        /* Go/Java/C#: unexported names are still visible package-wide (the nearest-package rule narrows them) */
        let pkg_visible = matches!(cf_lang, Lang::Go | Lang::Java | Lang::CSharp);
        (sym.exported || sym.kind == SymKind::Method || pkg_visible) && (sym.kind.is_callable() || sym.kind == SymKind::Struct)
    };
    /* 3. qualified owner (`Foo::new()` -> a `new` whose parent is `Foo`) */
    if let Some(q) = qual {
        let chosen: Vec<(u32, u32)> = cands
            .iter()
            .copied()
            .filter(|&(cf, cs)| {
                let syms = &files.rec(cf).ex.symbols;
                syms[cs as usize].parent.map(|p| syms[p as usize].name == q).unwrap_or(false)
            })
            .filter(ok)
            .collect();
        if !chosen.is_empty() {
            return Wide::Done(chosen);
        }
    }
    /* 4. ambient .d.ts declarations are platform APIs, not repo code; prefer the nearest package */
    let global: Vec<(u32, u32)> =
        cands.iter().copied().filter(|&(cf, _)| !files.rec(cf).path.ends_with(".d.ts")).filter(ok).collect();
    if global.len() <= 1 || global.len() > MAX_NEAREST_SCAN {
        return Wide::Done(if global.len() == 1 { global } else { Vec::new() });
    }
    Wide::Nearest(global)
}

/// Pair a file's old symbols with its new ones (old index -> new index). A name keeps its nodes
/// only if its definitions match one for one in what resolution reads (kind, export, parent name);
/// the other names are returned as touched. Non-indexed symbols are never call targets: pair freely.
fn pair_symbols<'a>(old: &'a [Symbol], new: &'a [Symbol]) -> (Vec<Option<u32>>, HashSet<&'a str>) {
    let group = |syms: &'a [Symbol]| {
        let (mut named, mut plain): (HashMap<&'a str, Vec<u32>>, Vec<u32>) = (HashMap::new(), Vec::new());
        for (i, s) in syms.iter().enumerate() {
            match indexed(s) {
                true => named.entry(s.name.as_str()).or_default().push(i as u32),
                false => plain.push(i as u32),
            }
        }
        (named, plain)
    };
    let looks = |syms: &'a [Symbol], i: u32| {
        let s = &syms[i as usize];
        (s.kind, s.exported, s.parent.map(|p| syms[p as usize].name.as_str()))
    };
    let ((old_named, old_plain), (new_named, new_plain)) = (group(old), group(new));
    let mut pairs = vec![None; old.len()];
    let mut touched: HashSet<&str> = new_named.keys().filter(|n| !old_named.contains_key(*n)).copied().collect();
    for (name, o) in &old_named {
        match new_named.get(name) {
            Some(n) if n.len() == o.len() && o.iter().zip(n).all(|(&a, &b)| looks(old, a) == looks(new, b)) => {
                for (&a, &b) in o.iter().zip(n) {
                    pairs[a as usize] = Some(b);
                }
            }
            _ => {
                touched.insert(name);
            }
        }
    }
    for (&a, &b) in old_plain.iter().zip(&new_plain) {
        pairs[a as usize] = Some(b);
    }
    (pairs, touched)
}

/// Definitions named `name` (empty when there are none).
fn cands<'m>(by_name: &'m HashMap<String, Vec<(u32, u32)>>, name: &str) -> &'m [(u32, u32)] {
    by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
}

/// Insert `v` into a sorted list.
fn insert_sorted(list: &mut Vec<(u32, u32)>, v: (u32, u32)) {
    let at = list.partition_point(|&x| x < v);
    list.insert(at, v);
}

/// Remove every entry of file `fi` under `key`, dropping the key when it empties.
fn drop_file(map: &mut HashMap<String, Vec<(u32, u32)>>, key: &str, fi: u32) {
    let Some(list) = map.get_mut(key) else { return };
    let lo = list.partition_point(|&(f, _)| f < fi);
    let hi = lo + list[lo..].partition_point(|&(f, _)| f == fi);
    list.drain(lo..hi);
    if list.is_empty() {
        map.remove(key);
    }
}

impl Graph {
    pub fn build(files: &[FileRecord]) -> Graph {
        /* the call-site index only serves edits: built lazily by `ensure_calls_index` */
        let Nodes { mut g, file_nodes, sym_nodes, by_name, calls_by_name, path_idx } = Nodes::new(files, false);

        /* 2. resolve imports to files */
        let resolver = Resolver::new(files);
        let imports: Vec<Vec<u32>> = files.par_iter().map(|f| resolver.resolve_all(f, &path_idx)).collect();
        for (fi, targets) in imports.iter().enumerate() {
            for &t in targets {
                if t as usize != fi {
                    g.add_edge(file_nodes[fi], file_nodes[t as usize], Edge::Imports);
                }
            }
        }

        /* 3. resolve calls */
        let ((supers, decls), members) =
            rayon::join(|| rayon::join(|| hierarchy::Supers::build(files), || decls::DeclIndex::build(files)), || typed::Members::build(files, &by_name));
        let edges: Vec<(u32, Option<u32>, u32, u32, u32)> = files
            .par_iter()
            .enumerate()
            .flat_map_iter(|(fi, f)| {
                let fi = fi as u32;
                let (by_name, imports, types, members) = (&by_name, &imports, (&supers, &decls, imports.as_slice()), &members);
                f.ex.calls.iter().flat_map(move |c| {
                    let (_, chosen) = typed::resolve(Files::new(files), types, Some(members), cands(by_name, &c.callee), &imports[fi as usize], fi, c, None);
                    chosen.into_iter().map(move |(tf, ts)| (fi, c.caller, tf, ts, c.line))
                })
            })
            .collect();
        let mut graph = Graph { g, file_nodes, sym_nodes, by_name, calls_by_name, calls_indexed: false, path_idx, imports, resolver, supers, decls };
        for (fi, caller, tf, ts, line) in edges {
            graph.add_call(fi, caller, tf, ts, line);
        }
        graph
    }

    /// Build the call-site index if `restore` deferred it. `replaced` = (file, its previous record) when
    /// `files[file]` already holds an edit: the index must describe the graph as it is, i.e. the old calls.
    pub fn ensure_calls_index(&mut self, files: &[FileRecord], replaced: Option<(u32, &FileRecord)>) {
        if self.calls_indexed {
            return;
        }
        for (fi, f) in files.iter().enumerate() {
            let f = match replaced {
                Some((r, old)) if r as usize == fi => old,
                _ => f,
            };
            if fi >= self.file_nodes.len() {
                continue;
            }
            for (ci, c) in f.ex.calls.iter().enumerate() {
                self.calls_by_name.entry(c.callee.clone()).or_default().push((fi as u32, ci as u32));
            }
        }
        self.calls_indexed = true;
    }

    /// What `build` resolved (imports and call edges), so a restart can restore the graph without resolving.
    pub fn resolved(&self) -> Resolved {
        let mut calls = Vec::new();
        for e in self.g.edge_indices() {
            let Edge::Calls { line } = self.g[e] else { continue };
            let Some((a, b)) = self.g.edge_endpoints(e) else { continue };
            let (fi, caller) = match self.g[a] {
                Node::File(f) => (f, 0),
                Node::Sym(f, s) => (f, s + 1),
            };
            if let Node::Sym(tf, ts) = self.g[b] {
                calls.push([fi, caller, tf, ts, line]);
            }
        }
        Resolved { imports: self.imports.clone(), calls }
    }

    /// True when every index in `r` points inside `files` (the snapshot is read from disk: never trust it).
    pub fn fits(files: &[FileRecord], r: &Resolved) -> bool {
        let n = files.len();
        let syms = |f: u32| files[f as usize].ex.symbols.len() as u32;
        r.imports.len() == n
            && r.imports.iter().flatten().all(|&t| (t as usize) < n)
            && r.calls.iter().all(|&[fi, caller, tf, ts, _]| {
                (fi as usize) < n && (tf as usize) < n && ts < syms(tf) && caller <= syms(fi)
            })
    }

    /// Rebuild from the same `files` plus what `resolved` saved: no import or call resolution.
    /// Falls back to a full `build` when the snapshot does not fit the records.
    pub fn restore(files: &[FileRecord], r: Resolved) -> Graph {
        if !Self::fits(files, &r) {
            return Self::build(files);
        }
        /* the call-site index only serves edits: built on the first `update_file`, not at startup; the type
         * hierarchy and declarations don't depend on the nodes, so they build alongside */
        let (nodes, (supers, decls)) =
            rayon::join(|| Nodes::new(files, false), || rayon::join(|| hierarchy::Supers::build(files), || decls::DeclIndex::build(files)));
        let Nodes { mut g, file_nodes, sym_nodes, by_name, calls_by_name, path_idx } = nodes;
        for (fi, targets) in r.imports.iter().enumerate() {
            for &t in targets {
                if t as usize != fi && (t as usize) < file_nodes.len() {
                    g.add_edge(file_nodes[fi], file_nodes[t as usize], Edge::Imports);
                }
            }
        }
        let resolver = Resolver::new(files);
        let mut graph =
            Graph { g, file_nodes, sym_nodes, by_name, calls_by_name, calls_indexed: false, path_idx, imports: r.imports, resolver, supers, decls };
        for [fi, caller, tf, ts, line] in r.calls {
            graph.add_call(fi, caller.checked_sub(1), tf, ts, line);
        }
        graph
    }

    fn add_call(&mut self, fi: u32, caller: Option<u32>, tf: u32, ts: u32, line: u32) {
        let (from, to) = (self.node(fi, caller), self.sym_nodes[tf as usize][ts as usize]);
        if from != to {
            self.g.add_edge(from, to, Edge::Calls { line });
        }
    }

    /// Outgoing edges of `from` that match `keep`.
    fn out_edges(&self, from: NodeIndex, keep: impl Fn(&Edge, NodeIndex) -> bool) -> Vec<EdgeIndex> {
        self.g.edges_directed(from, Direction::Outgoing).filter(|e| keep(e.weight(), e.target())).map(|e| e.id()).collect()
    }

    fn remove_edges(&mut self, ids: Vec<EdgeIndex>) {
        for e in ids {
            self.g.remove_edge(e);
        }
    }

    /// Re-resolve every call in file `x`, reusing (and re-lining) the edges that survive.
    fn rewire_calls(&mut self, files: &[FileRecord], x: u32) {
        let key = |from: NodeIndex, to: NodeIndex| (from.index() as u64) << 32 | to.index() as u64;
        /* 1. current outgoing call edges, keyed by endpoints */
        let n_syms = files[x as usize].ex.symbols.len() as u32;
        let mut have: Vec<(u64, EdgeIndex)> = Vec::new();
        for s in std::iter::once(None).chain((0..n_syms).map(Some)) {
            let from = self.node(x, s);
            for e in self.g.edges_directed(from, Direction::Outgoing) {
                if let Edge::Calls { .. } = e.weight() {
                    have.push((key(from, e.target()), e.id()));
                }
            }
        }
        /* 2. wanted edges; within one file a call's targets depend only on (callee, qualifier, receiver type) */
        let mut memo = HashMap::new();
        let mut want: Vec<(u64, NodeIndex, NodeIndex, u32)> = Vec::new();
        for c in &files[x as usize].ex.calls {
            let chosen = memo.entry((c.callee.as_str(), c.qual.as_deref(), c.rtype.as_deref(), c.rhint.as_deref())).or_insert_with(|| {
                typed::resolve(Files::new(files), (&self.supers, &self.decls, &self.imports), None, cands(&self.by_name, &c.callee), &self.imports[x as usize], x, c, None).1
            });
            let from = self.node(x, c.caller);
            for &(tf, ts) in chosen.iter() {
                let to = self.sym_nodes[tf as usize][ts as usize];
                if from != to {
                    want.push((key(from, to), from, to, c.line));
                }
            }
        }
        /* 3. merge by endpoints: matched edges only need their line set, the rest are removed or added */
        have.par_sort_unstable_by_key(|h| h.0);
        want.par_sort_unstable_by_key(|w| w.0);
        let (mut i, mut stale) = (0, Vec::new());
        for (k, from, to, line) in want {
            while i < have.len() && have[i].0 < k {
                stale.push(have[i].1);
                i += 1;
            }
            if i < have.len() && have[i].0 == k {
                self.g[have[i].1] = Edge::Calls { line };
                i += 1;
            } else {
                self.g.add_edge(from, to, Edge::Calls { line });
            }
        }
        stale.extend(have[i..].iter().map(|h| h.1));
        self.remove_edges(stale);
    }

    /// Patch the graph after `files[fi]` changed in place (`old` = its previous record) or was
    /// appended (`fi == files.len() - 1`, `old` = None). Ends in the same edges as `build(files)`.
    pub fn update_file(&mut self, files: &[FileRecord], fi: u32, old: Option<&FileRecord>) {
        self.ensure_calls_index(files, old.map(|o| (fi, o)));
        let f = &files[fi as usize];
        let is_new = fi as usize == self.file_nodes.len();
        debug_assert!(is_new == old.is_none(), "an existing file needs its old record");
        if is_new {
            self.file_nodes.push(self.g.add_node(Node::File(fi)));
            self.sym_nodes.push(Vec::new());
            self.imports.push(Vec::new());
            self.path_idx.insert(f.path.clone(), fi);
        }
        let old_syms: &[Symbol] = old.map(|o| o.ex.symbols.as_slice()).unwrap_or(&[]);

        /* 1. keep the nodes of symbols that look the same to resolution; drop the rest with their edges */
        let (pairs, touched) = pair_symbols(old_syms, &f.ex.symbols);
        let mut nodes = vec![NodeIndex::end(); f.ex.symbols.len()];
        for (a, n) in std::mem::take(&mut self.sym_nodes[fi as usize]).into_iter().enumerate().rev() {
            match pairs[a] {
                Some(b) => {
                    self.g[n] = Node::Sym(fi, b);
                    nodes[b as usize] = n;
                }
                None => {
                    self.g.remove_node(n);
                }
            }
        }
        for (b, n) in nodes.iter_mut().enumerate() {
            if *n == NodeIndex::end() {
                *n = self.g.add_node(Node::Sym(fi, b as u32));
            }
        }
        self.sym_nodes[fi as usize] = nodes;

        /* 2. refresh the name indexes (indices shift even for untouched names) */
        let before: HashMap<&str, Vec<(u32, u32)>> = touched.iter().map(|&n| (n, cands(&self.by_name, n).to_vec())).collect();
        let defined: HashSet<&str> = old_syms.iter().filter(|s| indexed(s)).map(|s| s.name.as_str()).collect();
        for n in defined {
            drop_file(&mut self.by_name, n, fi);
        }
        for (si, s) in f.ex.symbols.iter().enumerate().filter(|(_, s)| indexed(s)) {
            insert_sorted(self.by_name.entry(s.name.clone()).or_default(), (fi, si as u32));
        }
        if let Some(o) = old {
            let callees: HashSet<&str> = o.ex.calls.iter().map(|c| c.callee.as_str()).collect();
            for n in callees {
                drop_file(&mut self.calls_by_name, n, fi);
            }
        }
        for (ci, c) in f.ex.calls.iter().enumerate() {
            insert_sorted(self.calls_by_name.entry(c.callee.clone()).or_default(), (fi, ci as u32));
        }

        /* 3. imports: a new path can satisfy (or, for Python, make ambiguous) other files' imports */
        let mut full: Vec<u32> = vec![fi];
        let changed: Vec<(u32, Vec<u32>)> = if is_new {
            self.resolver = Resolver::new(files);
            let (resolver, path_idx, imports) = (&self.resolver, &self.path_idx, &self.imports);
            (0..files.len() as u32)
                .into_par_iter()
                .filter_map(|x| {
                    let v = resolver.resolve_all(&files[x as usize], path_idx);
                    (x == fi || v != imports[x as usize]).then_some((x, v))
                })
                .collect()
        } else {
            vec![(fi, self.resolver.resolve_all(f, &self.path_idx))]
        };
        for (x, targets) in changed {
            let from = self.file_nodes[x as usize];
            let out = self.out_edges(from, |w, _| *w == Edge::Imports);
            self.remove_edges(out);
            for &t in &targets {
                if t != x {
                    self.g.add_edge(from, self.file_nodes[t as usize], Edge::Imports);
                }
            }
            self.imports[x as usize] = targets;
            if x != fi {
                full.push(x);
            }
        }

        /* a changed type hierarchy or declared type can move any typed call: rebuild them and re-resolve
         * every file with one */
        let no_ex = crate::parse::Extract::default();
        let old_ex = old.map(|o| &o.ex).unwrap_or(&no_ex);
        let hierarchy_moved = match old {
            Some(o) => hierarchy::file_headers(o) != hierarchy::file_headers(f),
            None => !hierarchy::file_headers(f).is_empty(),
        };
        let mut moved = decls::moved_members(old_ex, &f.ex, files);
        /* this file's declarations name types through its imports: new imports can pin them differently */
        let raws = |e: &crate::parse::Extract| e.imports.iter().map(|i| i.raw.clone()).collect::<Vec<_>>();
        if raws(old_ex) != raws(&f.ex) {
            for m in f.ex.fields.iter().chain(&f.ex.rets) {
                if !moved.contains(&m.name) {
                    moved.push(m.name.clone());
                }
            }
        }
        if hierarchy_moved || !moved.is_empty() {
            self.supers = hierarchy::Supers::build(files);
            self.decls = decls::DeclIndex::build(files);
            /* a new hierarchy can move any typed call; a moved declaration only the hints that name it */
            let affected = |c: &crate::parse::CallSite| match &c.rhint {
                _ if hierarchy_moved => c.rtype.is_some() || c.rhint.is_some(),
                Some(h) => moved.iter().any(|m| decls::hint_names(h, m)),
                None => false,
            };
            full.extend((0..files.len() as u32).filter(|&x| x != fi && files[x as usize].ex.calls.iter().any(affected)));
            full.dedup();
        }

        /* 4. this file, and files whose imports moved, re-resolve every call */
        full.sort_unstable();
        full.dedup();
        for &x in &full {
            self.rewire_calls(files, x);
        }

        /* 5. other files' calls to a touched name: diff their targets before and after the edit */
        let (now, then) = (Files::new(files), Files { files, swap: old.map(|o| (fi, o)) });
        let mut stale: Vec<(NodeIndex, NodeIndex, u32)> = Vec::new();
        let mut adds: Vec<(u32, Option<u32>, u32, u32, u32)> = Vec::new();
        for name in touched {
            let Some(sites) = self.calls_by_name.get(name) else { continue };
            let (cur, prev) = (cands(&self.by_name, name), before[name].as_slice());
            let (mut memo_now, mut memo_then) = (WideMemo::new(), WideMemo::new());
            for &(x, ci) in sites {
                if full.binary_search(&x).is_ok() {
                    continue;
                }
                let c = &files[x as usize].ex.calls[ci as usize];
                let imports = &self.imports[x as usize];
                let (tier, chosen) = typed::resolve(now, (&self.supers, &self.decls, &self.imports), None, cur, imports, x, c, Some(&mut memo_now));
                /* decided by the caller's own file or an import other than this one: unchanged */
                if tier == Tier::Same || (tier == Tier::Imported && imports.binary_search(&fi).is_err()) {
                    continue;
                }
                let (_, was) = typed::resolve(then, (&self.supers, &self.decls, &self.imports), None, prev, imports, x, c, Some(&mut memo_then));
                /* old targets in this file left with their nodes; new ones there are always added */
                let from = self.node(x, c.caller);
                for &(tf, ts) in was.iter().filter(|&&t| t.0 != fi && !chosen.contains(&t)) {
                    stale.push((from, self.sym_nodes[tf as usize][ts as usize], c.line));
                }
                for &(tf, ts) in chosen.iter().filter(|&&t| t.0 == fi || !was.contains(&t)) {
                    adds.push((x, c.caller, tf, ts, c.line));
                }
            }
        }
        /* 6. drop one edge per stale target (two sites on one line can each own a copy), then add */
        for (from, to, line) in stale {
            let id = self.g.edges_connecting(from, to).find(|e| *e.weight() == Edge::Calls { line }).map(|e| e.id());
            if let Some(id) = id {
                self.g.remove_edge(id);
            }
        }
        for (x, caller, tf, ts, line) in adds {
            self.add_call(x, caller, tf, ts, line);
        }
    }

    fn to_ref(&self, n: NodeIndex, line: u32) -> Ref {
        match self.g[n] {
            Node::File(f) => Ref { file: f, sym: None, line },
            Node::Sym(f, s) => Ref { file: f, sym: Some(s), line },
        }
    }

    fn node(&self, file: u32, sym: Option<u32>) -> NodeIndex {
        match sym {
            Some(s) => self.sym_nodes[file as usize][s as usize],
            None => self.file_nodes[file as usize],
        }
    }

    /// Direct callers of a symbol; `line` is the call-site line in the caller's file.
    pub fn callers(&self, file: u32, sym: u32) -> Vec<Ref> {
        let mut v: Vec<Ref> = self
            .g
            .edges_directed(self.node(file, Some(sym)), Direction::Incoming)
            .filter_map(|e| match e.weight() {
                Edge::Calls { line } => Some(self.to_ref(e.source(), *line)),
                _ => None,
            })
            .collect();
        v.sort_by_key(|r| (r.file, r.line, r.sym));
        v.dedup();
        v
    }

    /// Direct callees of a symbol (or of module-level code when `sym` is None).
    pub fn callees(&self, file: u32, sym: Option<u32>) -> Vec<Ref> {
        let mut v: Vec<Ref> = self
            .g
            .edges_directed(self.node(file, sym), Direction::Outgoing)
            .filter_map(|e| match e.weight() {
                Edge::Calls { line } => Some(self.to_ref(e.target(), *line)),
                _ => None,
            })
            .collect();
        v.sort_by_key(|r| (r.file, r.sym, r.line));
        v.dedup_by_key(|r| (r.file, r.sym));
        v
    }

    /// 1-hop neighbourhood of a whole file, excluding edges that stay inside it.
    /// Incoming: (caller site, target symbol in this file). Outgoing: (callee, number of call sites).
    pub fn file_neighbors(&self, file: u32, files: &[FileRecord]) -> (Vec<(Ref, u32)>, Vec<(Ref, u32)>) {
        let mut incoming = Vec::new();
        let mut outgoing: Vec<(Ref, u32)> = Vec::new();
        let mut slot: HashMap<(u32, Option<u32>), usize> = HashMap::new();
        let n_syms = files[file as usize].ex.symbols.len() as u32;
        for s in std::iter::once(None).chain((0..n_syms).map(Some)) {
            if let Some(s) = s {
                for c in self.callers(file, s) {
                    if c.file != file {
                        incoming.push((c, s));
                    }
                }
            }
            for e in self.g.edges_directed(self.node(file, s), Direction::Outgoing) {
                let Edge::Calls { line } = *e.weight() else { continue };
                let r = self.to_ref(e.target(), line);
                if r.file == file {
                    continue;
                }
                match slot.get(&(r.file, r.sym)) {
                    Some(&i) => {
                        outgoing[i].1 += 1;
                        outgoing[i].0.line = outgoing[i].0.line.min(line);
                    }
                    None => {
                        slot.insert((r.file, r.sym), outgoing.len());
                        outgoing.push((r, 1));
                    }
                }
            }
        }
        /* patched and rebuilt graphs store edges in different orders; keep the output identical */
        outgoing.sort_by_key(|o| (o.0.file, o.0.sym));
        (incoming, outgoing)
    }

    /// The file an import specifier written in `file` points at (same rules as the import edges).
    pub(crate) fn resolve_spec(&self, files: &[FileRecord], file: u32, raw: &str) -> Option<u32> {
        self.resolver.resolve(&files[file as usize], raw, &self.path_idx)
    }

    /// Files that import `file`.
    pub fn importers(&self, file: u32) -> Vec<u32> {
        let mut v: Vec<u32> = self
            .g
            .edges_directed(self.file_nodes[file as usize], Direction::Incoming)
            .filter(|e| *e.weight() == Edge::Imports)
            .filter_map(|e| match self.g[e.source()] {
                Node::File(f) => Some(f),
                _ => None,
            })
            .collect();
        v.sort_unstable();
        v
    }
}

/// Best-effort module path -> file resolution for each language.
struct Resolver {
    py_modules: HashMap<String, u32>,
}

impl Resolver {
    fn new(files: &[FileRecord]) -> Resolver {
        let mut py_modules: HashMap<String, Vec<u32>> = HashMap::new();
        for (i, f) in files.iter().enumerate() {
            if f.lang != Lang::Python {
                continue;
            }
            let p = f.path.trim_end_matches(".pyi").trim_end_matches(".py").trim_end_matches("/__init__");
            let parts: Vec<&str> = p.split('/').collect();
            for start in 0..parts.len() {
                py_modules.entry(parts[start..].join(".")).or_default().push(i as u32);
            }
        }
        let py_modules = py_modules.into_iter().filter(|(_, v)| v.len() == 1).map(|(k, v)| (k, v[0])).collect();
        Resolver { py_modules }
    }

    /// Sorted, deduplicated files that `f` imports.
    fn resolve_all(&self, f: &FileRecord, paths: &HashMap<String, u32>) -> Vec<u32> {
        let mut v: Vec<u32> = f.ex.imports.iter().filter_map(|i| self.resolve(f, &i.raw, paths)).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn resolve(&self, f: &FileRecord, raw: &str, paths: &HashMap<String, u32>) -> Option<u32> {
        let dir = f.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let try_paths = |cands: &[String]| cands.iter().find_map(|c| paths.get(&normalize(c)).copied());
        match f.lang {
            Lang::Rust => {
                if let Some(m) = raw.strip_prefix("mod ") {
                    let stem = f.path.rsplit('/').next().unwrap_or("").trim_end_matches(".rs");
                    let base = if matches!(stem, "mod" | "lib" | "main") { dir.to_string() } else { format!("{dir}/{stem}") };
                    return try_paths(&[format!("{base}/{m}.rs"), format!("{base}/{m}/mod.rs")]);
                }
                let segs: Vec<&str> = raw.split("::").map(|s| s.trim()).take_while(|s| !s.starts_with('{') && *s != "*").collect();
                let (base, rest): (String, &[&str]) = match segs.first().copied() {
                    Some("crate") => (crate_src(&f.path)?, &segs[1..]),
                    Some("super") => (dir.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default(), &segs[1..]),
                    Some("self") => (dir.to_string(), &segs[1..]),
                    _ => return None,
                };
                for n in (1..=rest.len()).rev() {
                    let p = rest[..n].join("/");
                    if let Some(x) = try_paths(&[format!("{base}/{p}.rs"), format!("{base}/{p}/mod.rs")]) {
                        return Some(x);
                    }
                }
                None
            }
            Lang::Python => {
                if raw.starts_with('.') {
                    let dots = raw.chars().take_while(|c| *c == '.').count();
                    let mut base = dir.to_string();
                    for _ in 1..dots {
                        base = base.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
                    }
                    let rest = raw[dots..].replace('.', "/");
                    if rest.is_empty() {
                        return try_paths(&[format!("{base}/__init__.py")]);
                    }
                    return try_paths(&[format!("{base}/{rest}.py"), format!("{base}/{rest}/__init__.py")]);
                }
                self.py_modules.get(raw).copied()
            }
            _ => {
                let spec = if raw.starts_with('.') {
                    format!("{dir}/{raw}")
                } else if let Some(r) = raw.strip_prefix("@/").or_else(|| raw.strip_prefix("~/")) {
                    /* `@/x` points at the importing package's own src/ (monorepos have many) */
                    format!("{}/{r}", crate_src(&f.path).unwrap_or_else(|| "src".into()))
                } else {
                    return None;
                };
                let stripped = spec.trim_end_matches(".js").trim_end_matches(".jsx").trim_end_matches(".mjs");
                let mut cands = vec![spec.clone()];
                for base in [stripped, spec.as_str()] {
                    for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "d.ts"] {
                        cands.push(format!("{base}.{ext}"));
                    }
                    for ext in ["ts", "tsx", "js", "jsx"] {
                        cands.push(format!("{base}/index.{ext}"));
                    }
                }
                try_paths(&cands)
            }
        }
    }
}

/// `crates/foo/src/a/b.rs` -> `crates/foo/src`
fn crate_src(path: &str) -> Option<String> {
    let idx = path.rfind("src/").filter(|&i| i == 0 || path.as_bytes()[i - 1] == b'/')?;
    Some(path[..idx + 3].to_string())
}

/// Number of leading path segments two paths share.
fn common_prefix(a: &str, b: &str) -> usize {
    a.split('/').zip(b.split('/')).take_while(|(x, y)| x == y).count()
}

/// Collapse `.`/`..` segments in a relative path.
pub fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::extract;
    use petgraph::visit::IntoEdgeReferences;

    pub(super) fn rec(path: &str, src: &str) -> FileRecord {
        let lang = Lang::from_path(std::path::Path::new(path)).unwrap();
        FileRecord { path: path.into(), lang, mtime_ns: 0, size: 0, hash: 0, ex: extract(lang, src.as_bytes()) }
    }

    fn sym(g: &Graph, name: &str) -> (u32, u32) {
        g.by_name[name][0]
    }

    #[test]
    fn rust_cross_file_calls_and_imports() {
        let files = vec![
            rec("src/db/repo.rs", "pub struct Repo;\nimpl Repo { pub fn get_by_id(&self, id: u64) -> u64 { id } pub fn new() -> Repo { Repo } }"),
            rec("src/services/user.rs", "use crate::db::repo::Repo;\npub fn get_profile(r: &Repo) { r.get_by_id(1); Repo::new(); }"),
            rec("src/controllers/auth.rs", "use crate::services::user::get_profile;\npub fn login_handler() { get_profile(todo()); }"),
            rec("src/lib.rs", "mod db;\nmod services;"),
        ];
        let g = Graph::build(&files);
        assert_eq!(g.imports[1], vec![0]);
        assert_eq!(g.imports[2], vec![1]);
        let (gf, gs) = sym(&g, "get_profile");
        let callers = g.callers(gf, gs);
        assert_eq!(callers.len(), 1);
        assert_eq!(files[callers[0].file as usize].path, "src/controllers/auth.rs");
        let callees: Vec<_> = g.callees(gf, Some(gs)).iter().map(|r| files[r.file as usize].ex.symbols[r.sym.unwrap() as usize].name.clone()).collect();
        assert_eq!(callees, vec!["get_by_id", "new"]);
        let (inc, out) = g.file_neighbors(1, &files);
        assert_eq!(inc.len(), 1);
        assert_eq!(out.len(), 2);
        assert_eq!(g.importers(0), vec![1]);
    }

    #[test]
    fn ts_and_python_import_resolution() {
        let files = vec![
            rec("src/db.ts", "export function find(id: string) {}"),
            rec("src/api/user.ts", "import { find } from '../db';\nexport function get() { find('1'); }"),
            rec("pkg/models.py", "def load():\n    pass\n"),
            rec("pkg/svc.py", "from .models import load\nimport pkg.models\ndef run():\n    load()\n"),
        ];
        let g = Graph::build(&files);
        assert_eq!(g.imports[1], vec![0]);
        assert_eq!(g.imports[3], vec![2]);
        let (f, s) = g.by_name["load"][0];
        assert_eq!(g.callers(f, s).len(), 1);
    }

    #[test]
    fn ambiguous_global_names_are_not_linked() {
        let mut files: Vec<FileRecord> = (0..5).map(|i| rec(&format!("m{i}.py"), "def get():\n    pass\n")).collect();
        files.push(rec("main.py", "def run():\n    get()\n"));
        let g = Graph::build(&files);
        assert!(g.callees(5, Some(0)).is_empty());
    }

    #[test]
    fn calls_do_not_cross_language_families() {
        let files = vec![rec("a.rs", "fn main() { lines(); }"), rec("b.js", "export function lines() {}"), rec("c.ts", "lines();")];
        let g = Graph::build(&files);
        assert!(g.callees(0, Some(0)).is_empty());
        assert_eq!(g.callees(2, None).len(), 1);
    }

    #[test]
    fn monorepo_alias_and_nearest_package_win() {
        let files = vec![
            rec("apps/web/src/lib/nav.ts", "export function norm() {}"),
            rec("apps/admin/src/lib/nav.ts", "export function norm() {}"),
            rec("apps/web/src/c/a.tsx", "import { norm } from '@/lib/nav';\nexport function A() { norm(); }"),
            rec("apps/web/src/c/b.ts", "export function B() { norm(); toString(); }"),
            rec("worker.d.ts", "declare function toString(): string;"),
        ];
        let g = Graph::build(&files);
        assert_eq!(g.imports[2], vec![0]);
        let callee_files = |fi: u32| -> Vec<u32> { g.callees(fi, Some(0)).iter().map(|r| r.file).collect() };
        assert_eq!(callee_files(2), vec![0]);
        assert_eq!(callee_files(3), vec![0]);
    }

    #[test]
    fn private_helpers_in_other_files_are_not_call_targets() {
        let files = vec![
            rec("result.ts", "export function ok() {}"),
            rec("storage.ts", "function ok() {}\nexport function get() { ok(); }"),
            rec("svc.ts", "import { ok } from './result';\nimport { get } from './storage';\nexport function run() { ok(); get(); }"),
        ];
        let g = Graph::build(&files);
        let callee_files: Vec<u32> = g.callees(2, Some(0)).iter().map(|r| r.file).collect();
        assert_eq!(callee_files, vec![0, 1]);
        assert_eq!(g.callees(2, Some(0))[0].sym, Some(0));
        assert_eq!(g.callers(1, 0).len(), 1);
    }

    /// Every edge as (source, target, kind) with nodes named by path / (path, symbol, start line).
    fn edge_set(g: &Graph, files: &[FileRecord]) -> Vec<(String, String, (u8, u32))> {
        let id = |n: NodeIndex| match g.g[n] {
            Node::File(f) => files[f as usize].path.clone(),
            Node::Sym(f, s) => {
                let sym = &files[f as usize].ex.symbols[s as usize];
                format!("{}#{}@{}", files[f as usize].path, sym.name, sym.start_line)
            }
        };
        let mut v: Vec<_> = g
            .g
            .edge_references()
            .map(|e| {
                let w = match *e.weight() {
                    Edge::Imports => (1, 0),
                    Edge::Calls { line } => (2, line),
                };
                (id(e.source()), id(e.target()), w)
            })
            .collect();
        v.sort();
        v
    }

    /// Apply each step (path, new source) in turn by patching, and check it against a fresh build.
    fn check_patches(base: &[(&str, &str)], steps: &[(&str, &str)]) {
        let mut files: Vec<FileRecord> = base.iter().map(|(p, s)| rec(p, s)).collect();
        let mut g = Graph::build(&files);
        for (path, src) in steps {
            let r = rec(path, src);
            let (fi, old) = match files.iter().position(|f| f.path == *path) {
                Some(i) => (i, Some(std::mem::replace(&mut files[i], r))),
                None => {
                    files.push(r);
                    (files.len() - 1, None)
                }
            };
            g.update_file(&files, fi as u32, old.as_ref());
            let mut fresh = Graph::build(&files);
            fresh.ensure_calls_index(&files, None);
            assert_eq!(edge_set(&g, &files), edge_set(&fresh, &files), "edges after patching {path}");
            assert_eq!(g.g.node_count(), fresh.g.node_count());
            assert_eq!(g.by_name, fresh.by_name, "by_name after {path}");
            assert_eq!(g.calls_by_name, fresh.calls_by_name, "calls_by_name after {path}");
            assert_eq!(g.imports, fresh.imports, "imports after {path}");
            assert_eq!(g.path_idx, fresh.path_idx);
            for f in 0..files.len() as u32 {
                assert_eq!(g.file_neighbors(f, &files), fresh.file_neighbors(f, &files));
                assert_eq!(g.importers(f), fresh.importers(f));
            }
        }
    }

    #[test]
    fn patch_matches_rebuild_typed_calls() {
        let base = [
            ("src/a.rs", "pub struct Alpha;\nimpl Alpha { pub fn syntax(&self) {} }\npub struct Beta;\nimpl Beta { pub fn syntax(&self) {} }\n"),
            ("src/t.rs", "pub trait Named { fn name(&self) {} }\npub struct Gamma;\nimpl Gamma { pub fn name(&self) {} }\n"),
            ("src/u.rs", "fn run(b: &Beta) { let a = Alpha::new(); a.syntax(); b.syntax(); b.name(); }\n"),
        ];
        check_patches(
            &base,
            &[
                /* the hierarchy changes in another file: b.name() now reaches Named */
                ("src/t.rs", "pub trait Named { fn name(&self) {} }\nimpl Named for Beta {}\npub struct Gamma;\nimpl Gamma { pub fn name(&self) {} }\n"),
                /* Beta gains its own name() */
                ("src/a.rs", "pub struct Alpha;\nimpl Alpha { pub fn syntax(&self) {} }\npub struct Beta;\nimpl Beta { pub fn syntax(&self) {} pub fn name(&self) {} }\n"),
                /* and loses it again */
                ("src/a.rs", "pub struct Alpha;\nimpl Alpha { pub fn syntax(&self) {} }\npub struct Beta;\nimpl Beta { pub fn syntax(&self) {} }\n"),
                ("src/t.rs", "pub trait Named { fn name(&self) {} }\npub struct Gamma;\nimpl Gamma { pub fn name(&self) {} }\n"),
                ("src/v.rs", "pub struct Delta;\nimpl Named for Delta {}\nfn go(d: Delta) { d.name(); }\n"),
            ],
        );
    }

    #[test]
    fn patch_matches_rebuild_declared_types() {
        let base = [
            ("src/a.rs", "pub struct Alpha;\nimpl Alpha { pub fn syntax(&self) {} }\npub struct Beta;\nimpl Beta { pub fn syntax(&self) {} }\n"),
            ("src/svc.rs", "pub struct Svc { pub x: Alpha }\npub fn open() -> Alpha { Alpha }\n"),
            ("src/u.rs", "impl Svc {\n    fn run(&self) {\n        self.x.syntax();\n        let o = open();\n        o.syntax();\n    }\n}\n"),
        ];
        check_patches(
            &base,
            &[
                /* the field and the return type change in another file */
                ("src/svc.rs", "pub struct Svc { pub x: Beta }\npub fn open() -> Beta { Beta }\n"),
                ("src/svc.rs", "pub struct Svc { pub y: Beta }\n"),
                ("src/w.rs", "pub fn open() -> Alpha { Alpha }\n"),
                ("src/svc.rs", "pub struct Svc { pub x: Alpha }\n"),
            ],
        );
    }

    #[test]
    fn patch_matches_rebuild_rust() {
        let base = [
            ("src/db/repo.rs", "pub struct Repo;\nimpl Repo { pub fn get_by_id(&self, id: u64) -> u64 { id } pub fn new() -> Repo { Repo } }"),
            ("src/services/user.rs", "use crate::db::repo::Repo;\npub fn get_profile(r: &Repo) { r.get_by_id(1); Repo::new(); }"),
            ("src/controllers/auth.rs", "use crate::services::user::get_profile;\npub fn login_handler() { get_profile(todo()); helper(); }"),
            ("src/lib.rs", "mod db;\nmod services;\nmod util;\nfn boot() { util_init(); }"),
        ];
        check_patches(
            &base,
            &[
                /* rename a method its caller uses */
                ("src/db/repo.rs", "pub struct Repo;\nimpl Repo { pub fn fetch_by_id(&self, id: u64) -> u64 { id } pub fn new() -> Repo { Repo } }"),
                /* add a function another file already calls */
                ("src/services/user.rs", "use crate::db::repo::Repo;\npub fn helper() {}\npub fn get_profile(r: &Repo) { r.get_by_id(1); Repo::new(); }"),
                /* change a call */
                ("src/services/user.rs", "use crate::db::repo::Repo;\npub fn helper() {}\npub fn get_profile(r: &Repo) { r.fetch_by_id(1); helper(); }"),
                /* remove a function */
                ("src/services/user.rs", "use crate::db::repo::Repo;\npub fn get_profile(r: &Repo) { r.fetch_by_id(1); }"),
                /* new file that satisfies `mod util;` and defines a called name */
                ("src/util.rs", "pub fn util_init() { helper(); }\npub fn helper() {}"),
                /* edit with no name changes, only line shifts */
                ("src/db/repo.rs", "\n\npub struct Repo;\nimpl Repo { pub fn fetch_by_id(&self, id: u64) -> u64 { id } pub fn new() -> Repo { Repo } }"),
            ],
        );
    }

    #[test]
    fn patch_matches_rebuild_ts() {
        let base = [
            ("apps/web/src/lib/nav.ts", "export function norm() {}"),
            ("apps/admin/src/lib/nav.ts", "export function norm() {}"),
            ("apps/web/src/c/a.tsx", "import { norm } from '@/lib/nav';\nimport { x } from './extra';\nexport function A() { norm(); x(); }"),
            ("apps/web/src/c/b.ts", "export function B() { norm(); toString(); A(); }"),
            ("worker.d.ts", "declare function toString(): string;"),
        ];
        check_patches(
            &base,
            &[
                ("apps/web/src/lib/nav.ts", "export function normalize() {}"),
                ("apps/admin/src/lib/nav.ts", "function norm() {}\nexport function A() { norm(); }"),
                ("apps/web/src/c/extra.ts", "export function x() { norm(); }\nexport function norm() {}"),
                ("apps/web/src/c/b.ts", "import { x } from './extra';\nexport function B() { x(); norm(); }"),
                ("apps/web/src/lib/nav.ts", "export function norm() {}\nexport function x() {}"),
            ],
        );
    }

    #[test]
    fn patch_matches_rebuild_python() {
        let base = [
            ("pkg/models.py", "def load():\n    pass\n"),
            ("pkg/svc.py", "from .models import load\nimport models\ndef run():\n    load()\n    save()\n"),
            ("pkg/m1.py", "def get():\n    pass\n"),
            ("pkg/m2.py", "def get():\n    pass\n"),
            ("main.py", "def go():\n    get()\n    load()\n"),
        ];
        check_patches(
            &base,
            &[
                ("pkg/models.py", "def load():\n    pass\n\ndef save():\n    load()\n"),
                ("pkg/m2.py", "def got():\n    pass\n"),
                /* a second `models` module makes `import models` ambiguous */
                ("lib/models.py", "def save():\n    pass\n"),
                ("pkg/svc.py", "from .models import save\ndef run():\n    save()\n"),
                ("pkg/m3.py", "def get():\n    pass\n"),
            ],
        );
    }

    /// Synthetic repo shaped like rust-analyzer: 1,500 files, ~35k symbols, ~130k calls.
    fn synthetic(seed_edit: u32) -> Vec<FileRecord> {
        use crate::parse::{Extract, Import};
        let mut rng = 0x9e3779b97f4a7c15u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let common = ["new", "from", "default", "fmt", "clone", "eq", "hash", "len", "get", "map"];
        (0..1500u32)
            .map(|fi| {
                let mut ex = Extract::default();
                for si in 0..23u32 {
                    let name = if si < 3 { common[(fi as usize + si as usize) % common.len()].to_string() } else { format!("f{}_{}", fi % 700, si) };
                    let kind = if si < 3 { SymKind::Method } else { SymKind::Function };
                    ex.symbols.push(Symbol { name, kind, start_line: si * 10 + seed_edit, end_line: si * 10 + 9, sig: String::new(), exported: true, parent: None });
                }
                for ci in 0..87u32 {
                    let r = next();
                    let callee = if r % 4 == 0 { common[(r >> 8) as usize % common.len()].to_string() } else { format!("f{}_{}", (r >> 16) % 700, 3 + (r >> 32) % 20) };
                    ex.calls.push(CallSite { caller: Some(ci % 23), callee, qual: None, line: ci + 1, recv: false, rtype: None, rhint: None, in_macro: false });
                }
                for k in 1..4u32 {
                    ex.imports.push(Import { raw: format!("crate::m{}", (fi + k * 37) % 1500), line: k });
                }
                FileRecord { path: format!("crates/c{}/src/m{fi}.rs", fi % 40), lang: Lang::Rust, mtime_ns: 0, size: 0, hash: 0, ex }
            })
            .collect()
    }

    #[test]
    #[ignore = "benchmark: cargo test --release --lib patch_bench -- --ignored --nocapture"]
    fn patch_bench() {
        let mut files = synthetic(0);
        let t = std::time::Instant::now();
        let mut g = Graph::build(&files);
        let build = t.elapsed();
        let calls: usize = files.iter().map(|f| f.ex.calls.len()).sum();
        let mut worst = std::time::Duration::ZERO;
        for fi in [0u32, 1, 777, 1499] {
            /* edit: shift every line and change one call */
            let mut r = files[fi as usize].clone();
            r.ex.symbols.iter_mut().for_each(|s| s.start_line += 1);
            r.ex.calls[0].callee = "f1_5".into();
            let old = std::mem::replace(&mut files[fi as usize], r);
            let t = std::time::Instant::now();
            g.update_file(&files, fi, Some(&old));
            worst = worst.max(t.elapsed());
        }
        eprintln!(
            "synthetic: {} files, {} symbols, {calls} calls, {} edges; build {build:?}, worst one-file patch {worst:?}",
            files.len(),
            g.by_name.values().map(|v| v.len()).sum::<usize>(),
            g.g.edge_count()
        );
        assert_eq!(edge_set(&g, &files), edge_set(&Graph::build(&files), &files));
    }

    #[test]
    #[ignore = "benchmark on a real repo: GRAFT_BENCH_REPO=/path cargo test --release --lib patch_bench_repo -- --ignored --nocapture"]
    fn patch_bench_repo() {
        let Ok(root) = std::env::var("GRAFT_BENCH_REPO") else { return };
        let mut stats = crate::index::IndexStats::default();
        let mut files = crate::index::build(std::path::Path::new(&root), HashMap::new(), &mut stats);
        let t = std::time::Instant::now();
        let mut g = Graph::build(&files);
        let build = t.elapsed();
        /* patch the files that define the most-called names: the worst case for re-resolution */
        let mut order: Vec<(usize, u32)> = (0..files.len() as u32)
            .map(|fi| {
                let n: usize = files[fi as usize].ex.symbols.iter().filter_map(|s| g.calls_by_name.get(&s.name)).map(|v| v.len()).sum();
                (n, fi)
            })
            .collect();
        order.sort_unstable_by(|a, b| b.cmp(a));
        let mut times = Vec::new();
        for &(_, fi) in order.iter().take(5).chain([(0, 0u32), (0, files.len() as u32 / 2)].iter()) {
            let mut r = files[fi as usize].clone();
            r.ex.symbols.iter_mut().for_each(|s| s.start_line += 1);
            let old = std::mem::replace(&mut files[fi as usize], r);
            let t = std::time::Instant::now();
            g.update_file(&files, fi, Some(&old));
            times.push((t.elapsed(), format!("shift lines  {}", files[fi as usize].path)));
            /* rename the most-called symbol: every call site of the old and new name is re-resolved */
            let mut r = files[fi as usize].clone();
            let Some(s) = r.ex.symbols.iter_mut().filter(|s| indexed(s)).max_by_key(|s| g.calls_by_name.get(&s.name).map_or(0, |v| v.len()))
            else {
                continue;
            };
            s.name.push_str("_renamed");
            let old = std::mem::replace(&mut files[fi as usize], r);
            let t = std::time::Instant::now();
            g.update_file(&files, fi, Some(&old));
            times.push((t.elapsed(), format!("rename hub   {}", files[fi as usize].path)));
        }
        eprintln!("{} files, {} edges; build {build:?}", files.len(), g.g.edge_count());
        for (d, p) in &times {
            eprintln!("  patch {d:?}  {p}");
        }
        assert_eq!(edge_set(&g, &files), edge_set(&Graph::build(&files), &files));
    }

    #[test]
    fn same_file_calls_pick_one_target() {
        /* the rust-analyzer nodes.rs pattern: many impls with the same method, each calling self.syntax() */
        let mut src = String::new();
        for t in ["A", "B", "C", "D"] {
            src.push_str(&format!(
                "pub struct {t};\nimpl {t} {{\n    pub fn syntax(&self) -> u8 {{ 0 }}\n    pub fn kind(&self) -> u8 {{ self.syntax() }}\n}}\n"
            ));
        }
        src.push_str("pub fn free() {}\npub fn run(a: &A) { a.syntax(); free(); A::kind(a); }\n");
        let files = vec![rec("nodes.rs", &src)];
        let g = Graph::build(&files);
        let calls = files[0].ex.calls.len();
        let call_edges = g.g.edge_weights().filter(|w| matches!(w, Edge::Calls { .. })).count();
        assert!(call_edges <= calls, "{call_edges} edges from {calls} calls");
        /* each kind() links to its own impl's syntax() */
        let f = &files[0].ex;
        for (i, s) in f.symbols.iter().enumerate().filter(|(_, s)| s.name == "kind") {
            let callees = g.callees(0, Some(i as u32));
            assert_eq!(callees.len(), 1);
            let target = &f.symbols[callees[0].sym.unwrap() as usize];
            assert_eq!(target.parent, s.parent);
        }
        /* `a.syntax()` has 4 equal candidates: unresolved, not 4 edges; free() and A::kind resolve */
        let run = f.symbols.iter().position(|s| s.name == "run").unwrap() as u32;
        let mut got: Vec<(String, Option<String>)> = g
            .callees(0, Some(run))
            .iter()
            .map(|r| {
                let t = &f.symbols[r.sym.unwrap() as usize];
                (t.name.clone(), t.parent.map(|p| f.symbols[p as usize].name.clone()))
            })
            .collect();
        got.sort();
        assert_eq!(got, vec![("free".into(), None), ("kind".into(), Some("A".into()))]);
    }

    #[test]
    fn restore_from_resolved_matches_build() {
        let files = vec![
            rec("src/db.ts", "export function find(id: string) {}\nexport class R { get() { return find('1'); } }"),
            rec("src/api.ts", "import { find } from './db';\nexport function h() { find('x'); new R().get(); }\nh();"),
            rec("pkg/m.py", "def load():\n    pass\n"),
            rec("pkg/s.py", "from .m import load\ndef run():\n    load()\n"),
        ];
        let mut built = Graph::build(&files);
        built.ensure_calls_index(&files, None);
        let mut restored = Graph::restore(&files, built.resolved());
        restored.ensure_calls_index(&files, None);
        let edges = |g: &Graph| {
            let mut v: Vec<String> = g
                .g
                .edge_indices()
                .map(|e| {
                    let (a, b) = g.g.edge_endpoints(e).unwrap();
                    format!("{:?}->{:?}:{:?}", g.g[a], g.g[b], g.g[e])
                })
                .collect();
            v.sort();
            v
        };
        assert_eq!(edges(&built), edges(&restored));
        assert_eq!(built.imports, restored.imports);
        assert_eq!(built.by_name, restored.by_name);
        assert_eq!(built.calls_by_name, restored.calls_by_name);

        /* a snapshot pointing outside the records is ignored, not trusted */
        let mut bad = built.resolved();
        bad.calls.push([0, 0, 99, 0, 1]);
        assert!(!Graph::fits(&files, &bad));
        assert_eq!(edges(&Graph::restore(&files, bad)), edges(&built));

        /* an edit right after a restore (index still deferred) must land exactly like on a built graph */
        let mut edited = files.clone();
        let old = std::mem::replace(&mut edited[0], rec("src/db.ts", "export function find2(id: string) {}\nexport class R { get() { return find2('1'); } }"));
        let mut a = Graph::build(&files);
        a.update_file(&edited, 0, Some(&old));
        let mut b = Graph::restore(&files, Graph::build(&files).resolved());
        b.update_file(&edited, 0, Some(&old));
        assert_eq!(edges(&a), edges(&b));
        assert_eq!(a.calls_by_name, b.calls_by_name);
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize("src/a/../b/./c.ts"), "src/b/c.ts");
        assert_eq!(crate_src("crates/x/src/a/b.rs").as_deref(), Some("crates/x/src"));
    }
}
