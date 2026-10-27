//! Import parsing and rendering, plus engine helpers for `move_symbol` (see move_symbol.rs).

use crate::engine::{esc, Engine};
use crate::graph::normalize;
use crate::guard;
use crate::index;
use crate::lang::Lang;
use crate::parse::text;
use crate::refactor::line_of;
use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::Node;
use crate::move_symbol_paths::*;

pub(crate) fn parse_imports(lang: Lang, root: Node, src: &[u8]) -> Vec<ImportStmt> {
    let mut out = Vec::new();
    let mut cur = root.walk();
    for n in root.named_children(&mut cur) {
        let base = |spec: String, quote: char| ImportStmt {
            start: n.start_byte(),
            end: n.end_byte(),
            line: n.start_position().row as u32 + 1,
            spec,
            quote,
            semi: text(n, src).trim_end().ends_with(';'),
            type_only: false,
            export: false,
            star: false,
            py_from: false,
            bindings: Vec::new(),
        };
        if lang == Lang::Python {
            match n.kind() {
                "import_from_statement" => {
                    let Some(m) = n.child_by_field_name("module_name") else { continue };
                    let mut s = base(text(m, src).to_string(), '"');
                    s.py_from = true;
                    let mut c2 = n.walk();
                    for x in n.children_by_field_name("name", &mut c2) {
                        let (imp, local) = if x.kind() == "aliased_import" {
                            let i = x.child_by_field_name("name").map(|y| text(y, src)).unwrap_or("");
                            (i, x.child_by_field_name("alias").map(|y| text(y, src)).unwrap_or(i))
                        } else {
                            (text(x, src), text(x, src))
                        };
                        s.bindings.push(Binding { kind: BKind::Named, local: local.into(), imported: imp.into(), text: text(x, src).into() });
                    }
                    let mut c3 = n.walk();
                    s.star = n.children(&mut c3).any(|c| c.kind() == "wildcard_import");
                    out.push(s);
                }
                "import_statement" => {
                    let mut c2 = n.walk();
                    for x in n.children_by_field_name("name", &mut c2) {
                        let (spec, local) = if x.kind() == "aliased_import" {
                            let i = x.child_by_field_name("name").map(|y| text(y, src)).unwrap_or("");
                            (i.to_string(), x.child_by_field_name("alias").map(|y| text(y, src)).unwrap_or(i).to_string())
                        } else {
                            let t = text(x, src);
                            (t.to_string(), t.split('.').next().unwrap_or(t).to_string())
                        };
                        let mut s = base(spec.clone(), '"');
                        s.bindings.push(Binding { kind: BKind::Module, local, imported: spec, text: text(x, src).into() });
                        out.push(s);
                    }
                }
                _ => {}
            }
            continue;
        }
        let is_import = n.kind() == "import_statement";
        if !is_import && n.kind() != "export_statement" {
            continue;
        }
        let Some(s) = n.child_by_field_name("source") else { continue };
        let raw = text(s, src);
        let mut st = base(unquote(raw), raw.chars().next().unwrap_or('\''));
        st.export = !is_import;
        let mut c2 = n.walk();
        for c in n.children(&mut c2) {
            match c.kind() {
                "type" if !c.is_named() => st.type_only = true,
                "*" | "namespace_export" => st.star = true,
                "import_clause" => {
                    let mut c3 = c.walk();
                    for k in c.named_children(&mut c3) {
                        match k.kind() {
                            "identifier" => {
                                let t = text(k, src).to_string();
                                st.bindings.push(Binding { kind: BKind::Default, local: t.clone(), imported: "default".into(), text: t });
                            }
                            "namespace_import" => {
                                let t = k.named_child(0).map(|i| text(i, src)).unwrap_or("").to_string();
                                st.bindings.push(Binding { kind: BKind::Namespace, local: t.clone(), imported: "*".into(), text: t });
                            }
                            "named_imports" => push_specifiers(&mut st, k, src),
                            _ => {}
                        }
                    }
                }
                "export_clause" => push_specifiers(&mut st, c, src),
                _ => {}
            }
        }
        out.push(st);
    }
    out
}

pub(crate) fn push_specifiers(st: &mut ImportStmt, list: Node, src: &[u8]) {
    let mut cur = list.walk();
    for sp in list.named_children(&mut cur) {
        if !matches!(sp.kind(), "import_specifier" | "export_specifier") {
            continue;
        }
        let name = sp.child_by_field_name("name").map(|y| unquote(text(y, src))).unwrap_or_default();
        let local = sp.child_by_field_name("alias").map(|y| text(y, src).to_string()).unwrap_or_else(|| name.clone());
        st.bindings.push(Binding { kind: BKind::Named, local, imported: name, text: text(sp, src).to_string() });
    }
}

/// An import statement with only `keep`, pointing at `spec`.
pub(crate) fn render(lang: Lang, st: &ImportStmt, spec: &str, keep: &[&Binding]) -> String {
    if lang == Lang::Python {
        if st.py_from {
            let names: Vec<&str> = keep.iter().map(|b| b.text.as_str()).collect();
            return format!("from {spec} import {}", names.join(", "));
        }
        return keep.iter().map(|b| format!("import {}", b.text)).collect::<Vec<_>>().join("\n");
    }
    let q = st.quote;
    let semi = if st.semi { ";" } else { "" };
    let ty = if st.type_only { "type " } else { "" };
    let named: Vec<&str> = keep.iter().filter(|b| b.kind == BKind::Named).map(|b| b.text.as_str()).collect();
    if st.export {
        return format!("export {ty}{{ {} }} from {q}{spec}{q}{semi}", named.join(", "));
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = keep.iter().find(|b| b.kind == BKind::Default) {
        parts.push(d.local.clone());
    }
    if let Some(ns) = keep.iter().find(|b| b.kind == BKind::Namespace) {
        parts.push(format!("* as {}", ns.local));
    }
    if !named.is_empty() {
        parts.push(format!("{{ {} }}", named.join(", ")));
    }
    format!("import {ty}{} from {q}{spec}{q}{semi}", parts.join(", "))
}

/// Edits to one file: whole import statements replaced (empty = deleted), other edits, imports to add.
#[derive(Default)]
pub(crate) struct FileEdits {
    pub(crate) stmts: BTreeMap<usize, (usize, String)>,
    pub(crate) other: Vec<Edit>,
    pub(crate) new_imports: Vec<String>,
}

impl FileEdits {
    pub(crate) fn is_empty(&self) -> bool {
        self.stmts.is_empty() && self.other.is_empty() && self.new_imports.is_empty()
    }

    pub(crate) fn lines(&self, src: &str) -> Vec<u32> {
        let mut v: Vec<u32> = self.stmts.keys().chain(self.other.iter().map(|e| &e.0)).map(|&b| line_of(src, b)).collect();
        if !self.new_imports.is_empty() {
            v.push(1);
        }
        v
    }

    /// Byte edits; new imports go after the last top-level import (or a leading directive/docstring).
    pub(crate) fn finalize(mut self, lang: Lang, root: Node, src: &str) -> Vec<Edit> {
        let b = src.as_bytes();
        if !self.new_imports.is_empty() {
            let lines = self.new_imports.join("\n");
            let imports = parse_imports(lang, root, b);
            let anchor = imports.last().map(|s| (s.start, s.end)).or_else(|| {
                root.named_child(0)
                    .filter(|c| c.kind() == "expression_statement" && c.named_child(0).map(|s| s.kind() == "string").unwrap_or(false))
                    .map(|c| (c.start_byte(), c.end_byte()))
            });
            match anchor {
                Some((s, _)) if self.stmts.contains_key(&s) => {
                    let r = self.stmts.get_mut(&s).expect("checked");
                    r.1 = if r.1.is_empty() { lines } else { format!("{}\n{lines}", r.1) };
                }
                Some((_, e)) => self.other.push((e, e, format!("\n{lines}"))),
                None => {
                    let sep = if src.trim().is_empty() { "\n" } else if lang == Lang::Python { "\n\n\n" } else { "\n\n" };
                    self.other.push((0, 0, format!("{lines}{sep}")));
                }
            }
        }
        let mut out = Vec::new();
        for (s, (e, r)) in self.stmts {
            if r.is_empty() {
                let ls = src[..s].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let start = if src[ls..s].trim().is_empty() { ls } else { s };
                let end = if b.get(e) == Some(&b'\n') { e + 1 } else { e };
                out.push((start, end, String::new()));
            } else {
                out.push((s, e, r));
            }
        }
        out.extend(self.other);
        out
    }
}

pub(crate) fn top_level<'a>(root: Node<'a>, src: &[u8], name: &str, line: u32) -> Option<Node<'a>> {
    let row = line.saturating_sub(1) as usize;
    let mut cur = root.walk();
    root.named_children(&mut cur)
        .find(|c| c.start_position().row <= row && c.end_position().row >= row && decl_names(*c, src).iter().any(|n| n == name))
}

/// Byte range of a top-level statement with its leading comments, as whole lines.
pub(crate) fn moved_range(stmt: Node, src: &str) -> (usize, usize) {
    let mut first = stmt;
    while let Some(p) = first.prev_named_sibling() {
        let adjacent = p.end_position().row + 1 >= first.start_position().row;
        let trailing = p.prev_named_sibling().map(|q| q.end_position().row == p.start_position().row).unwrap_or(false);
        if !p.kind().contains("comment") || !adjacent || trailing {
            break;
        }
        first = p;
    }
    let s = first.start_byte();
    let ls = src[..s].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let start = if src[ls..s].trim().is_empty() { ls } else { s };
    let e = stmt.end_byte();
    let le = src[e..].find('\n').map(|i| e + i + 1).unwrap_or(src.len());
    let end = if src[e..le].trim().is_empty() { le } else { e };
    (start, end)
}

impl Engine {
    pub(crate) fn move_err(&mut self, target: &str, to: &str, code: &str, msg: &str) -> String {
        let open = format!("<move_symbol target=\"{}\" to=\"{}\" error=\"{code}\">", esc(target), esc(to));
        self.finish(&open, "</move_symbol>", &esc(&format!("{msg}\n")), 4000, "")
    }

    /// The destination as a repo-relative path, and whether it exists. Never outside root, never through a
    /// symlink, never hidden or skipped: the same visibility rules as reads.
    pub(crate) fn dest_rel(&self, to: &str) -> Result<(String, bool), (&'static str, String)> {
        let outside = || ("REFUSED_OUTSIDE_ROOT", "to_file must be inside the repository root".to_string());
        let p = Path::new(to);
        let raw = if p.is_absolute() {
            let abs = p.parent().and_then(|d| d.canonicalize().ok()).map(|d| d.join(p.file_name().unwrap_or_default())).unwrap_or(p.to_path_buf());
            abs.strip_prefix(&self.root).map_err(|_| outside())?.to_string_lossy().replace('\\', "/")
        } else {
            to.replace('\\', "/")
        };
        if raw.split('/').any(|c| c == "..") {
            return Err(outside());
        }
        let rel = normalize(&raw);
        if rel.is_empty() {
            return Err(("NOT_FOUND", "to_file is empty".into()));
        }
        let parts: Vec<&str> = rel.split('/').collect();
        if parts.iter().any(|c| c.starts_with('.') || index::SKIP_DIRS.contains(c)) {
            return Err(("REFUSED_HIDDEN", "to_file is in a hidden or skipped directory".into()));
        }
        if let Some(r) = guard::check_path(&rel) {
            return Err((r.code(), r.reason()));
        }
        let mut cur = self.root.clone();
        let mut exists = true;
        for (i, c) in parts.iter().enumerate() {
            cur.push(c);
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if m.file_type().is_symlink() => return Err(("REFUSED_SYMLINK", format!("{} is a symlink", parts[..=i].join("/")))),
                Ok(m) if i + 1 < parts.len() && !m.is_dir() => return Err(("NOT_FOUND", format!("{} is not a directory", parts[..=i].join("/")))),
                Ok(_) => {}
                Err(_) => {
                    exists = false;
                    break;
                }
            }
        }
        if exists {
            match self.resolve_rel(&rel) {
                Ok(r) if r == rel => {}
                Ok(_) => return Err(outside()),
                Err(r) => return Err((r.code(), r.reason())),
            }
        }
        Ok((rel, exists))
    }

    /// Module path for a fresh Python import of `target` written in `from`; None when unsure.
    pub(crate) fn py_module_for(&self, from: &str, target: &str) -> Option<String> {
        let has_init = |d: &str| self.root.join(d).join("__init__.py").exists();
        let parts = py_parts(target);
        let tdir = dir_of(target);
        if !tdir.is_empty() && has_init(tdir) {
            let dirs: Vec<&str> = tdir.split('/').collect();
            let mut top = dirs.len() - 1;
            while top > 0 && has_init(&dirs[..top].join("/")) {
                top -= 1;
            }
            return Some(parts[top..].join("."));
        }
        (tdir == dir_of(from)).then(|| parts.last().cloned()).flatten()
    }

}
