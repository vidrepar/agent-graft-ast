//! Path, specifier and identifier helpers for `move_symbol` (see move_symbol.rs).

use crate::parse::text;
use tree_sitter::Node;

pub(crate) type Edit = (usize, usize, String);

pub(crate) const JS_DECLS: &[&str] = &[
    "function_declaration", "generator_function_declaration", "class_declaration", "abstract_class_declaration",
    "interface_declaration", "type_alias_declaration", "enum_declaration", "function_signature",
];
pub(crate) const JS_EXTS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum BKind {
    Named,
    Default,
    Namespace,
    /// Python `import x` / `import x as y`
    Module,
}

#[derive(Clone)]
pub(crate) struct Binding {
    pub(crate) kind: BKind,
    /// the name this file sees
    pub(crate) local: String,
    /// the name the other module exports (Named), else the module
    pub(crate) imported: String,
    /// the specifier as written (`a as b`, `type T`)
    pub(crate) text: String,
}

/// One top-level import (or `export ... from`) statement.
#[derive(Clone)]
pub(crate) struct ImportStmt {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) line: u32,
    pub(crate) spec: String,
    pub(crate) quote: char,
    pub(crate) semi: bool,
    pub(crate) type_only: bool,
    pub(crate) export: bool,
    /// `export * from` / `from m import *`
    pub(crate) star: bool,
    pub(crate) py_from: bool,
    pub(crate) bindings: Vec<Binding>,
}

pub(crate) fn unquote(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string()
}

pub(crate) fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// (path without extension, extension with dot)
pub(crate) fn split_ext(rel: &str) -> (&str, &str) {
    let file_start = rel.rfind('/').map(|i| i + 1).unwrap_or(0);
    match rel[file_start..].rfind('.') {
        Some(i) if i > 0 => (&rel[..file_start + i], &rel[file_start + i..]),
        _ => (rel, ""),
    }
}

/// The extension a JS import specifier spells out, if any.
pub(crate) fn spec_ext(spec: &str) -> Option<&str> {
    let (_, ext) = split_ext(spec);
    JS_EXTS.contains(&ext).then_some(ext)
}

/// A file path as an import specifier would spell it, in the style of an existing specifier's extension.
pub(crate) fn styled(target: &str, style: Option<&str>) -> String {
    let (stem, _) = split_ext(target);
    match style {
        Some(".ts" | ".tsx" | ".mts" | ".cts") => target.to_string(),
        Some(e) => format!("{stem}{e}"),
        None => stem.to_string(),
    }
}

/// `./x` / `../x` from a directory to a path.
pub(crate) fn rel_spec(from_dir: &str, to: &str) -> String {
    let a: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let mut s = "../".repeat(a.len() - common);
    s.push_str(&b[common..].join("/"));
    if s.starts_with("../") {
        s
    } else {
        format!("./{s}")
    }
}

/// The specifier an importer should use for the destination, matching how it wrote the old one.
pub(crate) fn new_js_spec(old: &str, importer: &str, src: &str, dest: &str) -> String {
    let style = spec_ext(old);
    let target = styled(dest, style);
    if !old.starts_with('.') {
        /* path alias (`@/lib/x`): keep the alias when the destination sits under the same base */
        if let Some((seg0, tail)) = old.split_once('/') {
            let src_styled = styled(src, style);
            if let Some(base) = src_styled.strip_suffix(tail).filter(|b| b.is_empty() || b.ends_with('/')) {
                if let Some(rest) = target.strip_prefix(base) {
                    return format!("{seg0}/{rest}");
                }
            }
        }
    }
    rel_spec(dir_of(importer), &target)
}

/// Python module path components of a file: `pkg/a/__init__.py` -> [pkg, a].
pub(crate) fn py_parts(rel: &str) -> Vec<String> {
    let p = rel.trim_end_matches(".pyi").trim_end_matches(".py");
    let p = p.strip_suffix("/__init__").unwrap_or(p);
    p.split('/').filter(|s| !s.is_empty()).map(String::from).collect()
}

/// New module path for an importer that wrote `old` for the source file; None when unsure.
pub(crate) fn py_new_module(old: &str, src: &str, dest: &str, importer: &str) -> Option<String> {
    let d = py_parts(dest);
    if old.starts_with('.') {
        let dots = old.chars().take_while(|c| *c == '.').count();
        let mut base: Vec<&str> = dir_of(importer).split('/').filter(|s| !s.is_empty()).collect();
        for _ in 1..dots {
            base.pop()?;
        }
        let ok = d.len() > base.len() && d.iter().zip(base.iter()).all(|(x, y)| x == y);
        return ok.then(|| format!("{}{}", ".".repeat(dots), d[base.len()..].join(".")));
    }
    let s = py_parts(src);
    let m: Vec<&str> = old.split('.').collect();
    if s.len() < m.len() || s[s.len() - m.len()..].iter().zip(m.iter()).any(|(x, y)| x != y) {
        return None;
    }
    let base = &s[..s.len() - m.len()];
    let ok = d.len() > base.len() && d.iter().zip(base.iter()).all(|(x, y)| x == y);
    ok.then(|| d[base.len()..].join("."))
}

/// Names a top-level statement declares.
pub(crate) fn decl_names(n: Node, src: &[u8]) -> Vec<String> {
    match n.kind() {
        "export_statement" => n.child_by_field_name("declaration").map(|d| decl_names(d, src)).unwrap_or_default(),
        "decorated_definition" => n.child_by_field_name("definition").map(|d| decl_names(d, src)).unwrap_or_default(),
        "lexical_declaration" | "variable_declaration" => {
            let mut cur = n.walk();
            n.named_children(&mut cur)
                .filter(|c| c.kind() == "variable_declarator")
                .filter_map(|c| c.child_by_field_name("name"))
                .filter(|x| x.kind() == "identifier")
                .map(|x| text(x, src).to_string())
                .collect()
        }
        "expression_statement" => n
            .named_child(0)
            .filter(|a| a.kind() == "assignment")
            .and_then(|a| a.child_by_field_name("left"))
            .filter(|l| l.kind() == "identifier")
            .map(|l| vec![text(l, src).to_string()])
            .unwrap_or_default(),
        k if JS_DECLS.contains(&k) || k == "function_definition" || k == "class_definition" => {
            n.child_by_field_name("name").map(|x| vec![text(x, src).to_string()]).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// Identifiers that read a name (not `obj.member`), with their byte offset.
pub(crate) fn idents(n: Node, src: &[u8]) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut stack = vec![n];
    while let Some(x) = stack.pop() {
        let k = x.kind();
        if k.contains("comment") {
            continue;
        }
        if matches!(k, "identifier" | "type_identifier" | "shorthand_property_identifier") && x.child_count() == 0 {
            let member = x.parent().map(|p| {
                (p.kind() == "member_expression" && p.child_by_field_name("property") == Some(x))
                    || (p.kind() == "attribute" && p.child_by_field_name("attribute") == Some(x))
            });
            if member != Some(true) {
                out.push((text(x, src).to_string(), x.start_byte()));
            }
        }
        let mut cur = x.walk();
        stack.extend(x.children(&mut cur));
    }
    out
}
