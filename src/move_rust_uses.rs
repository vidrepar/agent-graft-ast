//! The `use` side of a Rust `move_symbol` (see move_rust.rs): the per-file job and what a scan reports,
//! `use` trees pointed at the new module (grouped ones split, emptied ones deleted) and item text helpers.

use crate::move_rust_mods::{vis_text, Crate, ModPath};
use crate::move_symbol_paths::Edit;
use crate::parse::text;
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;


pub(crate) struct Job<'a> {
    pub(crate) cr: &'a Crate,
    pub(crate) name: &'a str,
    pub(crate) src_mod: &'a [String],
    pub(crate) dest_mod: &'a [String],
    /// a file of another crate: only `ident::...` paths reach the moved item
    pub(crate) foreign: bool,
    /// byte ranges that move (source file only)
    pub(crate) moved: &'a [(usize, usize)],
}

/// Where a visibility goes: `at..end` holds the current one (empty: insert at `at`).
#[derive(Clone)]
pub(crate) struct VisSite {
    pub(crate) at: usize,
    pub(crate) end: usize,
    pub(crate) vis: String,
}

/// One non-glob `use` leaf: `segs` as written (`super::x::Y`), imported as `local`.
#[derive(Clone)]
pub(crate) struct Leaf {
    pub(crate) local: String,
    pub(crate) segs: Vec<String>,
    pub(crate) alias: Option<String>,
}

pub(crate) struct Glob {
    pub(crate) target: Option<ModPath>,
    pub(crate) segs: Vec<String>,
    pub(crate) vis: String,
}

#[derive(Default)]
pub(crate) struct Scope {
    pub(crate) file: String,
    pub(crate) inline: bool,
    pub(crate) defines: HashMap<String, VisSite>,
    pub(crate) macros: HashSet<String>,
    pub(crate) imports: Vec<Leaf>,
    pub(crate) globs: Vec<Glob>,
    pub(crate) aliases: HashMap<String, ModPath>,
    /// a new `use` goes here (after the last `use`, else at the top of the body), with this indent
    pub(crate) insert_at: usize,
    pub(crate) indent: String,
    /// a new `mod x;` goes here
    pub(crate) mod_at: usize,
    /// unqualified mentions of the item's name outside the moved code
    pub(crate) bare: usize,
}

/// What the moved code reads from its old module.
#[derive(Default)]
pub(crate) struct MovedUse {
    /// unqualified names (path heads included)
    pub(crate) names: HashSet<String>,
    /// names reached as `self::x` / `super::...::x` that are items of the source module
    pub(crate) via_self: HashSet<String>,
    pub(crate) macros: HashSet<String>,
    /// names it uses as a value or a concrete type, so they cannot be traits (whose methods need no name)
    pub(crate) typed: HashSet<String>,
    /// field, method and associated names it reads (`x.f`, `x.m()`, `T::m`, `T { f }`, `x.0`)
    pub(crate) members: HashSet<String>,
}

#[derive(Default)]
pub(crate) struct FileScan {
    pub(crate) scopes: Vec<(ModPath, Scope)>,
    pub(crate) edits: Vec<Edit>,
    /// (module, line) of every reference that will name the item after the move
    pub(crate) refs: Vec<(ModPath, u32)>,
    /// visibility of each `use` re-export of the item, with its module
    pub(crate) reexports: Vec<(ModPath, String)>,
    pub(crate) moved: MovedUse,
}

/// The path segments of a simple path node, or None when it has generics or anything else.
pub(crate) fn segs<'t>(n: Node<'t>) -> Option<Vec<Node<'t>>> {
    match n.kind() {
        "identifier" | "type_identifier" | "crate" | "self" | "super" | "metavariable" => Some(vec![n]),
        "scoped_identifier" | "scoped_type_identifier" => {
            let mut v = segs(n.child_by_field_name("path")?)?;
            v.push(n.child_by_field_name("name")?);
            Some(v)
        }
        _ => None,
    }
}

pub(crate) fn line(src: &[u8], b: usize) -> u32 {
    src[..b].iter().filter(|c| **c == b'\n').count() as u32 + 1
}

/// The path from module `from` to `dest`, starting the way the original (`head`) did.
pub(crate) fn path_to(head: &str, from: &[String], dest: &[String], cr: &Crate, foreign: bool) -> String {
    let abs = |root: &str| std::iter::once(root.to_string()).chain(dest.iter().cloned()).collect::<Vec<_>>().join("::");
    if foreign || (!cr.ident.is_empty() && head == cr.ident) {
        return abs(&cr.ident);
    }
    if matches!(head, "crate" | "$crate") {
        return abs(head);
    }
    let common = from.iter().zip(dest).take_while(|(a, b)| a == b).count();
    let rest = dest[common..].join("::");
    if common == from.len() {
        return match (rest.is_empty(), head) {
            (true, _) => "self".into(),
            (false, "self" | "super") => format!("self::{rest}"),
            _ => rest,
        };
    }
    if from.len() - common == 1 {
        return if rest.is_empty() { "super".into() } else { format!("super::{rest}") };
    }
    abs("crate")
}

impl Job<'_> {
    pub(crate) fn in_moved(&self, b: usize) -> bool {
        self.moved.iter().any(|&(s, e)| b >= s && b < e)
    }

    /// One `use` declaration: record what it imports (module level only) and point its leaves at the new module.
    pub(crate) fn use_decl(&self, decl: Node, m: &ModPath, mut sc: Option<&mut Scope>, src: &[u8], out: &mut FileScan) {
        let Some(arg) = decl.child_by_field_name("argument") else { return };
        let vis = vis_text(decl, src);
        let moved = self.in_moved(decl.start_byte());
        let out_mod: Vec<String> = if moved { self.dest_mod.to_vec() } else { m.clone() };
        let mut removed: HashSet<usize> = HashSet::new();
        let mut adds: Vec<String> = Vec::new();
        let mut work: Vec<(Node, Vec<String>)> = vec![(arg, Vec::new())];
        while let Some((item, prefix)) = work.pop() {
            let (own, list, alias, glob) = match item.kind() {
                "use_list" => {
                    let mut c = item.walk();
                    work.extend(item.named_children(&mut c).filter(|i| !i.kind().contains("comment")).map(|i| (i, prefix.clone())));
                    continue;
                }
                "scoped_use_list" => (item.child_by_field_name("path").and_then(segs).unwrap_or_default(), item.child_by_field_name("list"), None, false),
                "use_as_clause" => (
                    item.child_by_field_name("path").and_then(segs).unwrap_or_default(),
                    None,
                    item.child_by_field_name("alias").map(|a| text(a, src).to_string()),
                    false,
                ),
                "use_wildcard" => (item.named_child(0).and_then(segs).unwrap_or_default(), None, None, true),
                _ => (segs(item).unwrap_or_default(), None, None, false),
            };
            let full: Vec<String> = prefix.iter().cloned().chain(own.iter().map(|s| text(*s, src).to_string())).collect();
            let words: Vec<&str> = full.iter().map(|s| s.as_str()).collect();
            let hit = (prefix.len().max(1)..full.len())
                .find(|&i| full[i] == self.name && self.cr.resolve(m, &words[..i], sc.as_deref().map(|s| &s.aliases), self.foreign).as_deref() == Some(self.src_mod));
            if let Some(i) = hit {
                let name_node = own[i - prefix.len()];
                let np = path_to(&full[0], &out_mod, self.dest_mod, self.cr, self.foreign);
                let plain = i + 1 == full.len() && alias.is_none() && !glob && list.is_none();
                if out_mod == self.dest_mod && plain && sc.is_some() {
                    removed.insert(item.id());
                    out.reexports.push((self.dest_mod.to_vec(), vis.clone()));
                    continue;
                }
                if item.id() == arg.id() {
                    out.edits.push((own[0].start_byte(), name_node.start_byte(), format!("{np}::")));
                } else {
                    removed.insert(item.id());
                    let pre = if vis.is_empty() { String::new() } else { format!("{vis} ") };
                    adds.push(format!("{pre}use {np}::{};", String::from_utf8_lossy(&src[name_node.start_byte()..item.end_byte()])));
                }
                out.refs.push((out_mod.clone(), line(src, decl.start_byte())));
                if !vis.is_empty() {
                    out.reexports.push((m.clone(), vis.clone()));
                }
                continue;
            }
            if let Some(l) = list {
                work.push((l, full));
                continue;
            }
            let Some(sc) = sc.as_deref_mut() else { continue };
            if glob {
                let target = self.cr.resolve(m, &words, None, self.foreign);
                sc.globs.push(Glob { target, segs: full, vis: vis.clone() });
                continue;
            }
            let (mod_words, local) = match full.last().map(|s| s.as_str()) {
                Some("self") => (&words[..words.len() - 1], full.get(full.len().wrapping_sub(2)).cloned()),
                _ => (&words[..], full.last().cloned()),
            };
            let Some(local) = alias.clone().or(local) else { continue };
            if let Some(target) = self.cr.resolve(m, mod_words, None, self.foreign) {
                sc.aliases.insert(local.clone(), target);
            }
            sc.imports.push(Leaf { local, segs: full, alias });
        }
        if removed.is_empty() && adds.is_empty() {
            return;
        }
        let indent = {
            let s = decl.start_byte();
            let ls = src[..s].iter().rposition(|c| *c == b'\n').map(|i| i + 1).unwrap_or(0);
            String::from_utf8_lossy(&src[ls..s]).into_owned()
        };
        let adds = adds.join(&format!("\n{indent}"));
        match render_use(arg, &removed, src) {
            Some(r) => {
                if r != text(arg, src) {
                    out.edits.push((arg.start_byte(), arg.end_byte(), r));
                }
                if !adds.is_empty() {
                    out.edits.push((decl.end_byte(), decl.end_byte(), format!("\n{indent}{adds}")));
                }
            }
            None if adds.is_empty() => out.edits.push(delete_item(decl, src)),
            None => {
                let s = attrs_start(decl, src);
                out.edits.push((s, decl.end_byte(), adds));
            }
        }
    }
}

/// Remove the `use` leaves importing `names` from the top-level `use` declarations of a file.
pub(crate) fn drop_leaves(root: Node, src: &[u8], names: &HashSet<String>) -> Vec<Edit> {
    let mut out = Vec::new();
    let mut cur = root.walk();
    for decl in root.named_children(&mut cur).filter(|d| d.kind() == "use_declaration") {
        let Some(arg) = decl.child_by_field_name("argument") else { continue };
        let mut removed = HashSet::new();
        let mut stack = vec![arg];
        while let Some(n) = stack.pop() {
            let local = match n.kind() {
                "use_list" => {
                    let mut c = n.walk();
                    stack.extend(n.named_children(&mut c));
                    None
                }
                "scoped_use_list" => {
                    stack.extend(n.child_by_field_name("list"));
                    None
                }
                "use_as_clause" => n.child_by_field_name("alias"),
                "scoped_identifier" => n.child_by_field_name("name"),
                "identifier" => Some(n),
                _ => None,
            };
            if local.map(|l| names.contains(text(l, src))).unwrap_or(false) {
                removed.insert(n.id());
            }
        }
        if removed.is_empty() {
            continue;
        }
        match render_use(arg, &removed, src) {
            Some(r) => out.push((arg.start_byte(), arg.end_byte(), r)),
            None => out.push(delete_item(decl, src)),
        }
    }
    out
}

/// A `use` tree without the `removed` items; None when nothing is left.
fn render_use(n: Node, removed: &HashSet<usize>, src: &[u8]) -> Option<String> {
    if removed.contains(&n.id()) {
        return None;
    }
    let orig = text(n, src).to_string();
    match n.kind() {
        "use_list" => {
            let mut c = n.walk();
            let items: Vec<Node> = n.named_children(&mut c).filter(|i| !i.kind().contains("comment")).collect();
            let parts: Vec<Option<String>> = items.iter().map(|i| render_use(*i, removed, src)).collect();
            if parts.iter().all(|p| p.is_none()) {
                return None;
            }
            /* edit the list in place, so its layout survives: a removed item goes with the separator after it
            (or, at the end, the one before it) */
            let base = n.start_byte();
            let mut edits: Vec<(usize, usize, String)> = Vec::new();
            for (i, it) in items.iter().enumerate() {
                match &parts[i] {
                    Some(p) if p == text(*it, src) => {}
                    Some(p) => edits.push((it.start_byte(), it.end_byte(), p.clone())),
                    None if parts[i + 1..].iter().any(|p| p.is_some()) => edits.push((it.start_byte(), items[i + 1].start_byte(), String::new())),
                    None => edits.push((items[i - 1].end_byte(), it.end_byte(), String::new())),
                }
            }
            let mut out = orig.clone();
            for (a, b, t) in edits.into_iter().rev() {
                out.replace_range(a - base..b - base, &t);
            }
            Some(out)
        }
        "scoped_use_list" => {
            let list = n.child_by_field_name("list")?;
            let r = render_use(list, removed, src)?;
            if r == text(list, src) {
                return Some(orig);
            }
            let path = n.child_by_field_name("path").map(|p| format!("{}::", text(p, src))).unwrap_or_default();
            /* `a::{b}` -> `a::b` */
            let inner = r.trim_start_matches('{').trim_end_matches('}').trim().trim_end_matches(',');
            if !r.contains('\n') && !inner.contains([',', '{']) && inner != "self" {
                return Some(format!("{path}{inner}"));
            }
            Some(format!("{path}{r}"))
        }
        _ => Some(orig),
    }
}

/// Start of an item including the attributes and comments right above it (each on its own line).
pub(crate) fn attrs_start(n: Node, src: &[u8]) -> usize {
    let own_line = |x: Node| {
        let s = x.start_byte();
        let ls = src[..s].iter().rposition(|c| *c == b'\n').map(|i| i + 1).unwrap_or(0);
        src[ls..s].iter().all(|c| c.is_ascii_whitespace())
    };
    let mut first = n;
    while let Some(p) = first.prev_named_sibling() {
        let adjacent = p.end_position().row + 1 >= first.start_position().row;
        let doc = p.kind() == "attribute_item" || (p.kind().contains("comment") && !text(p, src).starts_with("//!") && !text(p, src).starts_with("/*!"));
        if !doc || !adjacent || !own_line(p) {
            break;
        }
        first = p;
    }
    first.start_byte()
}

/// Delete an item with its attributes, as whole lines.
pub(crate) fn delete_item(n: Node, src: &[u8]) -> Edit {
    let s = attrs_start(n, src);
    let ls = src[..s].iter().rposition(|c| *c == b'\n').map(|i| i + 1).unwrap_or(0);
    let start = if src[ls..s].iter().all(|c| c.is_ascii_whitespace()) { ls } else { s };
    let e = n.end_byte();
    let end = if src.get(e) == Some(&b'\n') { e + 1 } else { e };
    (start, end, String::new())
}

/// Where an item's visibility is (or would go).
pub(crate) fn site(n: Node, src: &[u8]) -> VisSite {
    let mut c = n.walk();
    let v = n.named_children(&mut c).find(|x| x.kind() == "visibility_modifier");
    match v {
        Some(v) => VisSite { at: v.start_byte(), end: v.end_byte(), vis: vis_text(n, src) },
        None => VisSite { at: n.start_byte(), end: n.start_byte(), vis: String::new() },
    }
}
