//! What a moved Rust item needs in its new module (see move_rust.rs): `use` lines, widened source items,
//! and the small text helpers the move shares.

use crate::move_rust_mods::{vis_scope, Crate, ModPath};
use crate::move_rust_uses::{MovedUse, Scope};
use crate::move_symbol_paths::Edit;
use crate::parse::text;
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

pub(crate) fn word_in(hay: &str, w: &str) -> bool {
    let b = hay.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    hay.match_indices(w).any(|(i, _)| (i == 0 || !ident(b[i - 1])) && b.get(i + w.len()).map(|c| !ident(*c)).unwrap_or(true))
}

/// Apply non-overlapping edits; insertions at one spot keep their order.
pub(crate) fn apply(src: &str, mut edits: Vec<Edit>) -> Option<String> {
    edits.sort_by_key(|e| (e.0, e.1));
    edits.dedup();
    let mut out = String::with_capacity(src.len());
    let mut at = 0;
    for (s, e, t) in edits {
        if s < at || e > src.len() {
            return None;
        }
        out.push_str(&src[at..s]);
        out.push_str(&t);
        at = e;
    }
    out.push_str(&src[at..]);
    Some(out)
}

pub(crate) fn type_base<'t>(n: Node<'t>) -> Option<Node<'t>> {
    match n.kind() {
        "type_identifier" => Some(n),
        "generic_type" => n.child_by_field_name("type").and_then(type_base),
        _ => None,
    }
}

pub(crate) fn abs_path(root: &str, p: &[String]) -> String {
    std::iter::once(root).chain(p.iter().map(|s| s.as_str())).collect::<Vec<_>>().join("::")
}

/// `pub(crate)` where a visibility is missing or narrower; the edit, when one is needed.
pub(crate) fn widen(at: usize, end: usize, vis: &str) -> Option<Edit> {
    match vis {
        "pub" | "pub(crate)" => None,
        "" => Some((at, at, "pub(crate) ".into())),
        _ => Some((at, end, "pub(crate)".into())),
    }
}

/// Private fields of a struct and private methods/consts of an inherent impl, as insertion points for a
/// visibility; only those named in `only` when given (tuple fields by index).
pub(crate) fn private_members(n: Node, src: &[u8], only: Option<&HashSet<String>>, out: &mut Vec<usize>) {
    let has_vis = |x: &Node| {
        let mut c = x.walk();
        let r = x.named_children(&mut c).any(|v| v.kind() == "visibility_modifier");
        r
    };
    let keep = |nm: &str| only.map(|o| o.contains(nm)).unwrap_or(true);
    let named = |x: &Node| x.child_by_field_name("name").map(|y| keep(text(y, src))).unwrap_or(false);
    let body = match n.child_by_field_name("body") {
        Some(b) => b,
        None => return,
    };
    let mut c = body.walk();
    let kids: Vec<Node> = body.named_children(&mut c).collect();
    match (n.kind(), body.kind()) {
        ("struct_item" | "union_item", "field_declaration_list") => {
            out.extend(kids.iter().filter(|f| f.kind() == "field_declaration" && !has_vis(f) && named(f)).map(|f| f.start_byte()))
        }
        ("struct_item", "ordered_field_declaration_list") => {
            let mut idx = 0;
            for (i, k) in kids.iter().enumerate() {
                if k.kind() == "visibility_modifier" || k.kind().contains("attribute") || k.kind().contains("comment") {
                    continue;
                }
                if (i == 0 || kids[i - 1].kind() != "visibility_modifier") && keep(&idx.to_string()) {
                    out.push(k.start_byte());
                }
                idx += 1;
            }
        }
        ("impl_item", _) if n.child_by_field_name("trait").is_none() => {
            out.extend(kids.iter().filter(|f| matches!(f.kind(), "function_item" | "const_item") && !has_vis(f) && named(f)).map(|f| f.start_byte()))
        }
        _ => {}
    }
}

/// The `use` lines the moved code needs in its new module, and the source items it reads that must be
/// widened to `pub(crate)` (edits in the source file, with their names).
pub(crate) fn dest_needs(cr: &Crate, name: &str, s_mod: &[String], d_mod: &[String], scopes: &HashMap<ModPath, Scope>, moved_use: &MovedUse) -> (Vec<String>, Vec<Edit>, Vec<String>) {
    let s_scope = &scopes[s_mod];
    let d_scope_has = |n: &str| scopes.get(d_mod).map(|d| d.defines.contains_key(n) || d.imports.iter().any(|l| l.local == n)).unwrap_or(false);
    let mut src_edits: Vec<Edit> = Vec::new();
    let mut widened: Vec<String> = Vec::new();
    let mut dest_uses: Vec<String> = Vec::new();
    let mut from_src: Vec<String> = Vec::new();
    let mut names: Vec<&String> = moved_use.names.iter().chain(moved_use.via_self.iter()).collect();
    names.sort();
    names.dedup();
    for n in names {
        if n == name {
            continue;
        }
        if let Some(site) = s_scope.defines.get(n.as_str()) {
            if !d_mod.starts_with(&vis_scope(&site.vis, s_mod)) {
                if let Some(e) = widen(site.at, site.end, &site.vis) {
                    src_edits.push(e);
                    widened.push(n.clone());
                }
            }
            if moved_use.names.contains(n) && !d_scope_has(n) {
                from_src.push(n.clone());
            }
        }
    }
    if !from_src.is_empty() {
        let list = if from_src.len() == 1 { from_src[0].clone() } else { format!("{{{}}}", from_src.join(", ")) };
        dest_uses.push(format!("use {}::{list};", abs_path("crate", s_mod)));
    }
    let rebase = |segs: &[String]| -> Vec<String> {
        let w: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        let k = (1..=w.len()).rev().find(|&k| cr.resolve(s_mod, &w[..k], Some(&s_scope.aliases), false).is_some());
        match k {
            Some(k) if !matches!(w[0], "crate" | "$crate") => {
                let abs = cr.resolve(s_mod, &w[..k], Some(&s_scope.aliases), false).unwrap_or_default();
                std::iter::once("crate".to_string()).chain(abs).chain(segs[k..].iter().cloned()).collect()
            }
            _ => segs.to_vec(),
        }
    };
    for l in &s_scope.imports {
        if l.local == name || !moved_use.names.contains(&l.local) || s_scope.defines.contains_key(&l.local) || d_scope_has(&l.local) {
            continue;
        }
        let mut p = rebase(&l.segs);
        if p.last().map(|s| s == "self").unwrap_or(false) {
            p.pop();
        }
        if p.len() >= 2 && p[0] == "crate" && p[1..p.len() - 1] == d_mod[..] {
            continue;
        }
        let alias = l.alias.as_ref().map(|a| format!(" as {a}")).unwrap_or_default();
        dest_uses.push(format!("use {}{alias};", p.join("::")));
    }
    for g in &s_scope.globs {
        match &g.target {
            Some(t) if *t == d_mod => {}
            Some(t) => {
                let Some(ts) = scopes.get(t) else { continue };
                let mut hits: Vec<&String> = moved_use
                    .names
                    .iter()
                    .filter(|n| *n != name && !s_scope.defines.contains_key(*n) && !s_scope.imports.iter().any(|l| &l.local == *n) && !d_scope_has(n))
                    .filter(|n| ts.defines.contains_key(*n) || ts.imports.iter().any(|l| &l.local == *n))
                    .collect();
                hits.sort();
                for h in hits {
                    dest_uses.push(format!("use {}::{h};", abs_path("crate", t)));
                }
            }
            None => dest_uses.push(format!("use {}::*;", rebase(&g.segs).join("::"))),
        }
    }
    dest_uses.sort();
    dest_uses.dedup();
    (dest_uses, src_edits, widened)
}
