//! One file's part of a Rust `move_symbol` (see move_rust.rs): what each module in it declares and imports,
//! and the edits that point its paths at the item's new module (`use` trees: move_rust_uses.rs).

use crate::move_rust_mods::ModPath;
use crate::move_rust_uses::{path_to, segs, line, site, FileScan, Job, Scope};
use crate::parse::text;
use std::collections::HashMap;
use tree_sitter::Node;

impl Job<'_> {
    /// Modules are keyed by path; a foreign file's are private to it.
    pub(crate) fn scan(&self, file: &str, root: Node, src: &[u8]) -> FileScan {
        let mut out = FileScan::default();
        let base: ModPath = if self.foreign {
            vec![format!("\0{file}")]
        } else {
            self.cr.file_module(file).map(|m| m.path.clone()).unwrap_or_default()
        };
        self.body(file, root, &base, false, src, &mut out);
        out
    }

    fn body(&self, file: &str, body: Node, m: &ModPath, inline: bool, src: &[u8], out: &mut FileScan) {
        let mut sc = Scope { file: file.into(), inline, ..Default::default() };
        let start = if inline { body.start_byte() + 1 } else { 0 };
        sc.insert_at = start;
        sc.mod_at = start;
        let mut cur = body.walk();
        let kids: Vec<Node> = body.named_children(&mut cur).collect();
        sc.indent = kids.iter().find(|k| !k.kind().contains("comment")).map(|k| {
            let ls = src[..k.start_byte()].iter().rposition(|c| *c == b'\n').map(|i| i + 1).unwrap_or(0);
            String::from_utf8_lossy(&src[ls..k.start_byte()]).into_owned()
        }).filter(|s| s.trim().is_empty()).unwrap_or_default();
        let mut saw_use = false;
        for k in &kids {
            match k.kind() {
                "inner_attribute_item" | "extern_crate_declaration" if sc.insert_at == sc.mod_at => {
                    sc.insert_at = k.end_byte();
                    sc.mod_at = k.end_byte();
                }
                "line_comment" | "block_comment" if text(*k, src).starts_with("//!") || text(*k, src).starts_with("/*!") => {
                    if sc.insert_at == sc.mod_at {
                        sc.insert_at = k.end_byte();
                        sc.mod_at = k.end_byte();
                    }
                }
                "use_declaration" => {
                    self.use_decl(*k, m, Some(&mut sc), src, out);
                    sc.insert_at = k.end_byte();
                    saw_use = true;
                }
                "mod_item" => {
                    let name = k.child_by_field_name("name").map(|x| text(x, src).to_string()).unwrap_or_default();
                    match k.child_by_field_name("body") {
                        Some(b) => self.body(file, b, &[m.as_slice(), std::slice::from_ref(&name)].concat(), true, src, out),
                        None => sc.mod_at = k.end_byte(),
                    }
                    sc.defines.insert(name, site(*k, src));
                }
                "macro_definition" => {
                    sc.macros.extend(k.child_by_field_name("name").map(|x| text(x, src).to_string()));
                }
                kind if kind.ends_with("_item") && kind != "impl_item" => {
                    if let Some(n) = k.child_by_field_name("name") {
                        sc.defines.insert(text(n, src).to_string(), site(*k, src));
                    }
                }
                _ => {}
            }
        }
        if sc.mod_at < sc.insert_at && sc.mod_at == start {
            sc.mod_at = sc.insert_at;
        }
        if !saw_use {
            sc.insert_at = sc.mod_at;
        }
        for k in &kids {
            if !matches!(k.kind(), "use_declaration" | "mod_item") {
                self.refs(*k, m, &sc.aliases.clone(), &mut sc, src, out);
            }
        }
        out.scopes.push((m.clone(), sc));
    }

    /// Paths and unqualified names under `n` (module `m`), skipping nested modules.
    fn refs(&self, n: Node, m: &ModPath, aliases: &HashMap<String, ModPath>, sc: &mut Scope, src: &[u8], out: &mut FileScan) {
        let mut stack = vec![n];
        while let Some(x) = stack.pop() {
            let k = x.kind();
            if k.contains("comment") || k == "string_literal" || k == "mod_item" {
                continue;
            }
            if k == "use_declaration" {
                self.use_decl(x, m, None, src, out);
                continue;
            }
            let moved = self.in_moved(x.start_byte());
            let out_mod: &[String] = if moved { self.dest_mod } else { m };
            let resolve = |s: &[&str]| self.cr.resolve(m, s, Some(aliases), self.foreign);
            if moved {
                let member = match k {
                    "field_identifier" => Some(x),
                    "field_expression" => x.child_by_field_name("field").filter(|f| f.kind() == "integer_literal"),
                    "shorthand_field_initializer" => x.named_child(0),
                    "scoped_identifier" => x.child_by_field_name("name"),
                    _ => None,
                };
                out.moved.members.extend(member.map(|f| text(f, src).to_string()));
            }
            if matches!(k, "scoped_identifier" | "scoped_type_identifier") {
                if let (Some(p), Some(nm)) = (x.child_by_field_name("path"), x.child_by_field_name("name")) {
                    let ps = segs(p).unwrap_or_default();
                    let words: Vec<&str> = ps.iter().map(|s| text(*s, src)).collect();
                    if text(nm, src) == self.name && !words.is_empty() && resolve(&words).as_deref() == Some(self.src_mod) {
                        let np = path_to(words[0], out_mod, self.dest_mod, self.cr, self.foreign);
                        out.edits.push((p.start_byte(), nm.start_byte(), format!("{np}::")));
                        out.refs.push((out_mod.to_vec(), line(src, x.start_byte())));
                        continue;
                    }
                    let rel = !words.is_empty() && words.iter().all(|w| matches!(*w, "self" | "super"));
                    if moved && rel && !matches!(text(nm, src), "self" | "super") {
                        if let Some(abs) = resolve(&words) {
                            if abs == self.src_mod {
                                out.moved.via_self.insert(text(nm, src).to_string());
                            }
                            out.edits.push((p.start_byte(), p.end_byte(), path_to("crate", out_mod, &abs, self.cr, false)));
                        }
                    }
                }
            }
            if matches!(k, "identifier" | "type_identifier") && x.child_count() == 0 {
                self.ident(x, m, aliases, sc, src, out);
            }
            if k == "macro_invocation" && moved {
                if let Some(mac) = x.child_by_field_name("macro").filter(|mm| mm.kind() == "identifier") {
                    out.moved.macros.insert(text(mac, src).to_string());
                }
            }
            let mut c = x.walk();
            stack.extend(x.children(&mut c));
        }
    }

    fn ident(&self, x: Node, m: &ModPath, aliases: &HashMap<String, ModPath>, sc: &mut Scope, src: &[u8], out: &mut FileScan) {
        let Some(p) = x.parent() else { return };
        let pk = p.kind();
        let is_field = |f: &str| p.child_by_field_name(f) == Some(x);
        if (matches!(pk, "scoped_identifier" | "scoped_type_identifier") && is_field("name"))
            || ((pk.ends_with("_item") || pk == "enum_variant") && is_field("name"))
            || (pk == "macro_invocation" && is_field("macro"))
        {
            return;
        }
        let t = text(x, src);
        let moved = self.in_moved(x.start_byte());
        if pk == "token_tree" {
            let prev = x.prev_sibling();
            match prev.map(|q| q.kind()) {
                Some(".") => return,
                Some("::") if t == self.name => {
                    /* `a::b::name` inside a macro's tokens */
                    let mut words: Vec<Node> = Vec::new();
                    let mut q = prev;
                    while let Some(sep) = q.filter(|s| s.kind() == "::") {
                        let Some(w) = sep.prev_sibling().filter(|w| matches!(w.kind(), "identifier" | "crate" | "self" | "super" | "metavariable")) else { break };
                        words.insert(0, w);
                        q = w.prev_sibling();
                    }
                    let mut start = words.first().map(|w| w.start_byte()).unwrap_or(0);
                    let mut ws: Vec<&str> = words.iter().map(|w| text(*w, src)).collect();
                    if let (Some("crate"), Some(d)) = (ws.first().copied(), words[0].prev_sibling().filter(|d| d.kind() == "$")) {
                        start = d.start_byte();
                        ws[0] = "$crate";
                    }
                    if !ws.is_empty() && self.cr.resolve(m, &ws, Some(aliases), self.foreign).as_deref() == Some(self.src_mod) {
                        let out_mod: &[String] = if moved { self.dest_mod } else { m };
                        let np = path_to(ws[0], out_mod, self.dest_mod, self.cr, self.foreign);
                        out.edits.push((start, x.start_byte(), format!("{np}::")));
                        out.refs.push((out_mod.to_vec(), line(src, x.start_byte())));
                    }
                    return;
                }
                Some("::") => return,
                _ => {}
            }
        }
        if moved {
            out.moved.names.insert(t.to_string());
            if pk != "token_tree" && !not_a_trait(x, p) {
                return;
            }
            if pk != "token_tree" {
                out.moved.typed.insert(t.to_string());
            }
        } else if t == self.name {
            sc.bare += 1;
        }
    }

}

/// `x` (child of `p`) is used as a value or a concrete type: not as a path head, bound or `dyn`/`impl` type.
fn not_a_trait(x: Node, p: Node) -> bool {
    if matches!(p.kind(), "scoped_identifier" | "scoped_type_identifier") {
        return false;
    }
    if x.kind() == "identifier" {
        return true;
    }
    if p.kind() == "impl_item" && p.child_by_field_name("trait") == Some(x) {
        return false;
    }
    let mut a = Some(p);
    for _ in 0..4 {
        let Some(n) = a else { break };
        if matches!(n.kind(), "trait_bounds" | "dynamic_type" | "abstract_type" | "where_predicate" | "higher_ranked_trait_bound" | "removed_trait_bound") {
            return false;
        }
        a = n.parent();
    }
    true
}
