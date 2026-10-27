//! AST folding: level 0 = full source, 1 = skeleton (bodies collapsed), 2 = public API only.

use crate::lang::Lang;
use crate::parse::{parse_tree, text, Extract, SymKind};
use crate::tokens::count;
use tree_sitter::Node;

/// Bodies with fewer hidden lines than this stay inline (folding them saves nothing).
const MIN_HIDDEN_LINES: usize = 2;
/// Data literals spanning at least this many rows are folded at level 1.
const MIN_LITERAL_ROWS: usize = 4;
/// Level-2 type lines are clipped at this many characters.
const MAX_TYPE_CHARS: usize = 400;

pub fn render(lang: Lang, src: &[u8], ex: &Extract, level: u8) -> String {
    match level {
        0 => String::from_utf8_lossy(src).into_owned(),
        1 => level1(lang, src),
        _ => level2(lang, src, ex),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Fit {
    Exact,
    /// requested level was too large; a higher (smaller) level was used
    Degraded,
    /// even level 2 overflowed; output was cut at a line boundary
    Truncated,
}

pub struct Fitted {
    pub text: String,
    pub level: u8,
    pub tokens: usize,
    pub fit: Fit,
}

/// Render at `level`, escalating to smaller levels until it fits `budget` tokens.
pub fn fit(lang: Lang, src: &[u8], ex: &Extract, level: u8, budget: usize) -> Fitted {
    let mut lvl = level.min(2);
    loop {
        let text = render(lang, src, ex, lvl);
        let tokens = count(&text);
        if tokens <= budget {
            let fit = if lvl == level { Fit::Exact } else { Fit::Degraded };
            return Fitted { text, level: lvl, tokens, fit };
        }
        if lvl == 2 {
            let text = truncate_to(&text, budget);
            let tokens = count(&text);
            return Fitted { text, level: lvl, tokens, fit: Fit::Truncated };
        }
        lvl += 1;
    }
}

fn truncate_to(text: &str, budget: usize) -> String {
    let marker_cost = 16;
    let target = budget.saturating_sub(marker_cost);
    let lines: Vec<&str> = text.lines().collect();
    /* binary search the longest line prefix under budget */
    let (mut lo, mut hi) = (0usize, lines.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if count(&lines[..mid].join("\n")) <= target {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out = lines[..lo].join("\n");
    out.push_str(&format!("\n/* ... truncated: {} more lines over budget ... */", lines.len() - lo));
    out
}

fn body_of<'a>(lang: Lang, node: Node<'a>, src: &[u8]) -> Option<Node<'a>> {
    let k = node.kind();
    let foldable = match lang {
        Lang::Rust => k == "function_item" || (k == "mod_item" && is_cfg_test(node, src)),
        Lang::Python => k == "function_definition",
        Lang::Go => matches!(k, "function_declaration" | "method_declaration" | "func_literal"),
        Lang::Java => matches!(k, "method_declaration" | "constructor_declaration"),
        Lang::CSharp => matches!(k, "method_declaration" | "constructor_declaration" | "local_function_statement"),
        _ if lang.basic() => crate::parse_basic::is_func(lang, k),
        _ => matches!(
            k,
            "function_declaration" | "generator_function_declaration" | "method_definition" | "function_expression"
                | "function" | "arrow_function" | "generator_function"
        ),
    };
    if !foldable {
        return None;
    }
    let body = match node.child_by_field_name("body") {
        Some(b) => b,
        /* Kotlin: the body is an unnamed `function_body` child */
        None if lang == Lang::Kotlin => {
            let mut c = node.walk();
            let b = node.named_children(&mut c).find(|n| n.kind() == "function_body");
            b?
        }
        None => return None,
    };
    if lang.is_js_family() && body.kind() != "statement_block" || lang == Lang::CSharp && body.kind() != "block" {
        return None;
    }
    /* Kotlin/Scala/Swift `= expr` bodies are one expression: nothing to fold */
    if matches!(lang, Lang::Kotlin | Lang::Scala | Lang::Swift) && src.get(body.start_byte()) != Some(&b'{') {
        return None;
    }
    Some(body)
}

/// Data literals (object/array/template/dict...) that are folded when they span many lines.
fn is_big_literal(lang: Lang, node: Node) -> bool {
    let k = node.kind();
    let lit = match lang {
        Lang::Rust => matches!(k, "array_expression" | "struct_expression" | "token_tree" | "raw_string_literal"),
        Lang::Python => matches!(k, "dictionary" | "list" | "set" | "tuple"),
        Lang::Go => k == "literal_value",
        Lang::Java => k == "array_initializer",
        Lang::CSharp => matches!(k, "initializer_expression" | "collection_expression"),
        _ if lang.basic() => false,
        _ => matches!(k, "object" | "array" | "template_string" | "jsx_element"),
    };
    lit && node.end_position().row - node.start_position().row >= MIN_LITERAL_ROWS
}

fn literal_marker(lang: Lang, node: Node, hidden: usize) -> String {
    let (open, close) = match node.kind() {
        "array" | "array_expression" | "list" => ("[", "]"),
        "template_string" => ("`", "`"),
        "raw_string_literal" => ("r\"", "\""),
        "tuple" | "token_tree" => ("(", ")"),
        "jsx_element" => ("<>", "</>"),
        _ => ("{", "}"),
    };
    if lang == Lang::Python {
        return format!("{open}...{close}");
    }
    format!("{open} /* {hidden} lines */ {close}")
}

fn is_cfg_test(node: Node, src: &[u8]) -> bool {
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if p.kind() != "attribute_item" {
            break;
        }
        if text(p, src).replace(' ', "").contains("cfg(test)") {
            return true;
        }
        prev = p.prev_named_sibling();
    }
    false
}

/// Doc comments survive folding; every other comment is dropped.
fn is_doc_comment(lang: Lang, t: &str) -> bool {
    match lang {
        Lang::Rust => t.starts_with("///") || t.starts_with("//!") || t.starts_with("/**") || t.starts_with("/*!"),
        Lang::Python => false,
        /* godoc is plain `//` lines; `is_go_doc` decides by position */
        Lang::Go => false,
        Lang::CSharp => t.starts_with("///") || t.starts_with("/**"),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Java => t.starts_with("/**"),
        Lang::Lua => t.starts_with("---"),
        Lang::Ruby | Lang::Bash => false,
        _ => t.starts_with("/**") || t.starts_with("///"),
    }
}

/// A Go comment directly above a top-level declaration (godoc), or a top-level comment block.
fn is_go_doc(node: Node) -> bool {
    node.parent().is_some_and(|p| p.kind() == "source_file")
}

fn python_docstring<'a>(body: Node<'a>, src: &'a [u8]) -> Option<&'a str> {
    let first = body.named_child(0)?;
    if first.kind() == "expression_statement" && first.named_child(0)?.kind() == "string" {
        return Some(text(first, src));
    }
    None
}

fn level1(lang: Lang, src: &[u8]) -> String {
    let Some(tree) = parse_tree(lang, src) else {
        return String::from_utf8_lossy(src).into_owned();
    };
    let mut repl: Vec<(usize, usize, String)> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        /* 1. drop non-doc comments */
        if node.kind().contains("comment") {
            let doc = is_doc_comment(lang, text(node, src)) || (lang == Lang::Go && is_go_doc(node));
            if !doc {
                repl.push((node.start_byte(), node.end_byte(), String::new()));
            }
            continue;
        }
        /* 2. fold function bodies, keeping Python docstrings */
        if let Some(body) = body_of(lang, node, src) {
            let rows = body.end_position().row - body.start_position().row;
            /* bodies without braces (Python, Ruby, Lua) count every row */
            let braceless = matches!(lang, Lang::Python | Lang::Ruby | Lang::Lua);
            let hidden = if braceless { rows + 1 } else { rows.saturating_sub(1) };
            if hidden >= MIN_HIDDEN_LINES {
                let r = match lang {
                    Lang::Python => {
                        let indent = " ".repeat(body.start_position().column);
                        match python_docstring(body, src) {
                            Some(doc) => format!("{doc}\n{indent}...  # {hidden} lines hidden"),
                            None => format!("...  # {hidden} lines hidden"),
                        }
                    }
                    Lang::Ruby | Lang::Bash => format!("# {hidden} lines hidden"),
                    Lang::Lua => format!("-- {hidden} lines hidden"),
                    _ => format!("{{ /* {hidden} lines hidden */ }}"),
                };
                repl.push((body.start_byte(), body.end_byte(), r));
                continue;
            }
        }
        /* 3. fold large data literals outside function bodies */
        if is_big_literal(lang, node) {
            let hidden = node.end_position().row - node.start_position().row - 1;
            repl.push((node.start_byte(), node.end_byte(), literal_marker(lang, node, hidden)));
            continue;
        }
        let mut c = node.walk();
        let kids: Vec<Node> = node.children(&mut c).filter(|n| n.is_named() || n.kind().contains("comment")).collect();
        stack.extend(kids.into_iter().rev());
    }
    repl.sort_by_key(|r| r.0);
    let mut out = String::with_capacity(src.len() / 2);
    let mut pos = 0;
    for (s, e, r) in repl {
        if s < pos {
            continue;
        }
        out.push_str(&String::from_utf8_lossy(&src[pos..s]));
        out.push_str(&r);
        pos = e;
    }
    out.push_str(&String::from_utf8_lossy(&src[pos..]));
    tidy(lang, &out)
}

/// Drop blank lines and trailing spaces, dedupe imports, merge single-line JS named imports per source.
fn tidy(lang: Lang, s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut js_named: Vec<(String, Vec<String>, usize)> = Vec::new();
    for raw in s.lines() {
        let l = raw.trim_end();
        if l.trim().is_empty() {
            continue;
        }
        let t = l.trim_start();
        let is_import = match lang {
            Lang::Rust => t.starts_with("use ") || t.starts_with("pub use "),
            Lang::Python => t.starts_with("import ") || t.starts_with("from "),
            Lang::CSharp => t.starts_with("using ") && t.ends_with(';'),
            _ => t.starts_with("import "),
        };
        if is_import {
            if !seen.insert(t.to_string()) {
                continue;
            }
            if lang.is_js_family() {
                if let Some((names, source)) = js_named_import(t) {
                    if let Some(e) = js_named.iter_mut().find(|e| e.0 == source) {
                        for n in names {
                            if !e.1.contains(&n) {
                                e.1.push(n);
                            }
                        }
                        continue;
                    }
                    js_named.push((source, names, lines.len()));
                }
            }
        }
        lines.push(l.to_string());
    }
    for (source, names, idx) in js_named {
        lines[idx] = format!("import {{ {} }} from \"{source}\";", names.join(", "));
    }
    lines.join("\n")
}

/// `import { a, b as c } from "x";` -> (["a", "b as c"], "x")
fn js_named_import(t: &str) -> Option<(Vec<String>, String)> {
    let rest = t.strip_prefix("import {")?;
    let (names, tail) = rest.split_once('}')?;
    let source = tail.trim().strip_prefix("from")?.trim().trim_end_matches(';').trim_matches(|c| c == '"' || c == '\'');
    let names: Vec<String> = names.split(',').map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect();
    Some((names, source.to_string()))
}

/// One line for a type: comments dropped, private Rust fields dropped, whitespace collapsed.
fn type_line(lang: Lang, lines: &[&str]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.is_empty() || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') || t.starts_with('#') {
            continue;
        }
        let inner = i > 0 && i + 1 < lines.len();
        if lang == Lang::Rust && inner && lines[0].contains("struct ") && !t.starts_with("pub") {
            continue;
        }
        parts.push(t);
    }
    let mut s = parts.join(" ").replace("{ }", "{}");
    if s.len() > MAX_TYPE_CHARS {
        let cut = (0..=MAX_TYPE_CHARS).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
        s.truncate(cut);
        s.push_str(" … }");
    }
    s
}

fn level2(lang: Lang, src: &[u8], ex: &Extract) -> String {
    let owned = String::from_utf8_lossy(src);
    let lines: Vec<&str> = owned.lines().collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); ex.symbols.len()];
    let mut roots = Vec::new();
    for (i, s) in ex.symbols.iter().enumerate() {
        match s.parent {
            Some(p) => children[p as usize].push(i),
            None => roots.push(i),
        }
    }
    let term = |sig: &str| match lang {
        Lang::Python => format!("{sig}: ..."),
        _ => format!("{sig};"),
    };
    let mut out = Vec::new();
    #[allow(clippy::too_many_arguments)]
    fn emit(
        i: usize,
        depth: usize,
        lang: Lang,
        ex: &Extract,
        lines: &[&str],
        children: &[Vec<usize>],
        out: &mut Vec<String>,
        term: &dyn Fn(&str) -> String,
    ) {
        let s = &ex.symbols[i];
        let pad = "  ".repeat(depth);
        /* Java/C#/tier-2 interfaces, enums and structs hold methods: render them like classes */
        let container = !children[i].is_empty() && (matches!(lang, Lang::Java | Lang::CSharp) || lang.basic());
        match s.kind {
            SymKind::Struct | SymKind::Enum | SymKind::TypeAlias | SymKind::Interface if !container => {
                if !s.exported {
                    return;
                }
                let a = (s.start_line as usize).saturating_sub(1);
                let b = (s.end_line as usize).min(lines.len());
                if a >= b {
                    return;
                }
                out.push(format!("{pad}{}", type_line(lang, &lines[a..b])));
            }
            SymKind::Class | SymKind::Trait | SymKind::Impl | SymKind::Module | SymKind::Struct | SymKind::Enum | SymKind::Interface
                if s.kind.is_container() || container =>
            {
                let pub_kids: Vec<usize> = children[i].iter().copied().filter(|&k| ex.symbols[k].exported).collect();
                if !s.exported || (s.kind == SymKind::Impl && pub_kids.is_empty()) {
                    return;
                }
                /* trait impls carry no new API: one line is enough */
                if s.kind == SymKind::Impl && s.sig.contains(" for ") {
                    out.push(format!("{pad}{};", s.sig));
                    return;
                }
                let (open, close) = match lang {
                    Lang::Python => (":", None),
                    Lang::Ruby => ("", Some("end")),
                    _ => (" {", Some("}")),
                };
                out.push(format!("{pad}{}{open}", s.sig));
                for k in pub_kids {
                    emit(k, depth + 1, lang, ex, lines, children, out, term);
                }
                if let Some(close) = close {
                    out.push(format!("{pad}{close}"));
                }
            }
            _ => {
                if s.exported {
                    out.push(format!("{pad}{}", term(&s.sig)));
                }
            }
        }
    }
    for i in roots {
        let before = out.len();
        emit(i, 0, lang, ex, &lines, &children, &mut out, &term);
        /* JS/TS signatures come from the inner declaration; restore the `export` keyword */
        if lang.is_js_family() && out.len() > before && !out[before].starts_with("export") {
            out[before] = format!("export {}", out[before]);
        }
    }
    out.join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::extract;

    const RS: &str = r#"use std::fmt;

/// A user service
pub struct UserService {
    pub id: u64,
}

impl UserService {
    /// Fetch active user profile
    pub async fn get_profile(&self, user_id: u64) -> Result<u64, ()> {
        let a = 1;
        let b = 2;
        let c = a + b;
        Ok(c + user_id)
    }
    fn hidden(&self) -> u64 {
        let x = 1;
        let y = 2;
        x + y
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn t() {}
}
"#;

    #[test]
    fn level0_is_identity() {
        let ex = extract(Lang::Rust, RS.as_bytes());
        assert_eq!(render(Lang::Rust, RS.as_bytes(), &ex, 0), RS);
    }

    #[test]
    fn level1_collapses_bodies_keeps_docs_and_headers() {
        let ex = extract(Lang::Rust, RS.as_bytes());
        let out = render(Lang::Rust, RS.as_bytes(), &ex, 1);
        assert!(out.contains("/// Fetch active user profile"));
        assert!(out.contains("pub async fn get_profile(&self, user_id: u64) -> Result<u64, ()> { /* 4 lines hidden */ }"));
        assert!(out.contains("pub struct UserService {\n    pub id: u64,\n}"));
        assert!(out.contains("mod tests { /* 2 lines hidden */ }"));
        assert!(!out.contains("let c = a + b"));
    }

    #[test]
    fn level2_emits_only_public_api() {
        let ex = extract(Lang::Rust, RS.as_bytes());
        let out = render(Lang::Rust, RS.as_bytes(), &ex, 2);
        assert!(out.contains("pub struct UserService { pub id: u64, }"), "{out}");
        assert!(out.contains("impl UserService {\n  pub async fn get_profile(&self, user_id: u64) -> Result<u64, ()>;\n}"));
        assert!(!out.contains("hidden"));
    }

    #[test]
    fn python_and_typescript_levels() {
        let py = "class Svc:\n    def get(self, id):\n        \"\"\"Get one.\"\"\"\n        x = 1\n        return x\n\ndef _private():\n    pass\n";
        let ex = extract(Lang::Python, py.as_bytes());
        let l1 = render(Lang::Python, py.as_bytes(), &ex, 1);
        assert!(l1.contains("def get(self, id):\n        \"\"\"Get one.\"\"\"\n        ...  # 3 lines hidden"), "{l1}");
        let l2 = render(Lang::Python, py.as_bytes(), &ex, 2);
        assert_eq!(l2, "class Svc:\n  def get(self, id): ...");

        let ts = "export class A {\n  run(x: number): number {\n    const y = x;\n    const z = y;\n    return z;\n  }\n  private p() {}\n}\nfunction internal() {}\n";
        let ex = extract(Lang::TypeScript, ts.as_bytes());
        assert!(render(Lang::TypeScript, ts.as_bytes(), &ex, 1).contains("run(x: number): number { /* 3 lines hidden */ }"));
        assert_eq!(render(Lang::TypeScript, ts.as_bytes(), &ex, 2), "export class A {\n  run(x: number): number;\n}");
    }

    #[test]
    fn level2_survives_invalid_utf8_and_unicode_indent() {
        let mut src = b"pub struct S {\n\xe2\x80\x83pub a: u8,\n}\n".to_vec();
        src.extend_from_slice(b"\xff\xfe\n");
        let ex = extract(Lang::Rust, &src);
        let out = render(Lang::Rust, &src, &ex, 2);
        assert!(out.starts_with("pub struct S {"));
    }

    #[test]
    fn level1_drops_plain_comments_folds_literals_and_dedupes_imports() {
        let ts = "import { a } from \"./x\";\nimport { b } from \"./x\";\nimport { a } from \"./x\";\n\n// plain note\n/** Doc for T */\nexport const TABLE = {\n  one: 1,\n  two: 2,\n  three: 3,\n};\n";
        let ex = extract(Lang::TypeScript, ts.as_bytes());
        let out = render(Lang::TypeScript, ts.as_bytes(), &ex, 1);
        assert_eq!(out, "import { a, b } from \"./x\";\n/** Doc for T */\nexport const TABLE = { /* 3 lines */ };");
        let rs = "// note\n/// doc\npub fn f() {}\nuse a::b;\nuse a::b;\n";
        let ex = extract(Lang::Rust, rs.as_bytes());
        assert_eq!(render(Lang::Rust, rs.as_bytes(), &ex, 1), "/// doc\npub fn f() {}\nuse a::b;");
        let py = "# note\nX = 1\n";
        let ex = extract(Lang::Python, py.as_bytes());
        assert_eq!(render(Lang::Python, py.as_bytes(), &ex, 1), "X = 1");
    }

    #[test]
    fn level2_hides_private_fields_and_trait_impl_bodies() {
        let rs = "pub struct S {\n    pub a: u8,\n    secret: u8,\n}\nimpl Clone for S {\n    fn clone(&self) -> S {\n        todo!()\n    }\n}\n";
        let ex = extract(Lang::Rust, rs.as_bytes());
        assert_eq!(render(Lang::Rust, rs.as_bytes(), &ex, 2), "pub struct S { pub a: u8, }\nimpl Clone for S;");
    }

    #[test]
    fn fit_degrades_then_truncates() {
        let ex = extract(Lang::Rust, RS.as_bytes());
        let full = fit(Lang::Rust, RS.as_bytes(), &ex, 0, 10_000);
        assert_eq!((full.level, full.fit.clone()), (0, Fit::Exact));
        let l1_tokens = count(&render(Lang::Rust, RS.as_bytes(), &ex, 1));
        let f = fit(Lang::Rust, RS.as_bytes(), &ex, 0, l1_tokens);
        assert_eq!((f.level, f.fit), (1, Fit::Degraded));
        let tiny = fit(Lang::Rust, RS.as_bytes(), &ex, 0, 20);
        assert_eq!(tiny.fit, Fit::Truncated);
        assert!(tiny.tokens <= 20, "{}", tiny.tokens);
    }
}
