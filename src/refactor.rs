//! Decision-native tools: the engine does the mechanical work and hands the agent a decision, not raw text.
//!
//! - `rename` and `change_signature` compute every edit and write the files (re-indexed at once), so the
//!   agent makes one call instead of a Read/Edit round trip per site.
//! - Edits are syntax-aware: string literals are never touched, `obj.name` accesses only count for
//!   methods, locals that shadow the name are left alone, and definitions that must change together
//!   (TypeScript overloads, a Rust trait method and its impls) are one target.
//! - When a name has several unrelated definitions, the reply is a choice card (A/B/C with a
//!   recommendation) instead of a guess. Every reply carries an impact verdict.

use crate::context::{is_ident, same_family};
use crate::engine::{esc, Engine};
use crate::guard::{self, Refusal};
use crate::index;
use crate::lang::Lang;
use crate::parse::{parse_tree, text, SymKind};
use crate::textrefs;
use crate::ts_exact::Fallback;
use crate::ts_refactor::{alias_note, ts_call_sites, ts_declares, Exact, FileEdits};
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

/// One-line risk of changing a symbol.
pub struct Impact {
    pub level: &'static str,
    pub reason: String,
}

/// One definition, or a group of definitions that must change together.
pub(crate) struct Target {
    pub(crate) name: String,
    pub(crate) lang: Lang,
    /// (file, symbol); the first is the primary definition
    pub(crate) defs: Vec<(u32, u32)>,
    /// a method: reached through `obj.name` / `self.name` / `Type::name`, never as a bare name
    pub(crate) method: bool,
}

/// The package a path belongs to: everything before its `src/`, else its first directory.
pub(crate) fn package_of(path: &str) -> &str {
    match (path.find("/src/"), path.rsplit_once('/')) {
        (Some(i), _) => &path[..i],
        (None, Some(_)) => path.split('/').next().unwrap_or(""),
        (None, None) => "",
    }
}

pub(crate) fn valid_ident(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && !b[0].is_ascii_digit() && b.iter().all(|&c| is_ident(c))
}

pub(crate) const IDENT_KINDS: &[&str] = &[
    "identifier", "type_identifier", "property_identifier", "field_identifier", "shorthand_property_identifier",
    "shorthand_property_identifier_pattern", "private_property_identifier",
];

const FN_SCOPES: &[&str] = &[
    "function_declaration", "generator_function_declaration", "method_definition", "arrow_function",
    "function_expression", "function", "function_item", "closure_expression", "function_definition", "lambda",
    "method_declaration", "func_literal", "constructor_declaration", "lambda_expression", "local_function_statement",
];

/// `obj.name` / `self.name` / `{ name: ... }`: the node is the member part of an access or a key.
pub(crate) fn member_access(n: Node) -> Option<Node> {
    let p = n.parent()?;
    let field = match p.kind() {
        "member_expression" => "property",
        "field_expression" => "field",
        "attribute" => "attribute",
        "pair" => "key",
        "selector_expression" | "field_access" => "field",
        "method_invocation" | "member_access_expression" => "name",
        _ => return None,
    };
    (p.child_by_field_name(field) == Some(n)).then_some(p)
}

/// The object of a member access (`x` in `x.name`), when it is a plain identifier or self/this.
fn access_object<'a>(access: Node<'a>, src: &'a [u8]) -> Option<&'a str> {
    let obj = access
        .child_by_field_name("object")
        .or_else(|| access.child_by_field_name("value"))
        .or_else(|| access.child_by_field_name("operand"))
        .or_else(|| access.child_by_field_name("expression"))?;
    matches!(obj.kind(), "identifier" | "self" | "this").then(|| text(obj, src))
}

/// A local binding of the name (variable, parameter, pattern): it shadows the target inside its function.
fn is_local_binding(n: Node) -> bool {
    let Some(p) = n.parent() else { return false };
    let named_as = |field: &str| p.child_by_field_name(field) == Some(n);
    match p.kind() {
        "variable_declarator" => named_as("name"),
        "let_declaration" => named_as("pattern"),
        "required_parameter" | "optional_parameter" | "parameter" => named_as("pattern") || named_as("name"),
        "assignment" => named_as("left"),
        "parameters" | "lambda_parameters" | "formal_parameters" | "closure_parameters" => true,
        "default_parameter" | "typed_parameter" | "typed_default_parameter" => p.named_child(0) == Some(n),
        "formal_parameter" | "parameter_declaration" | "var_spec" => named_as("name"),
        /* Go `x := ...` / `for k, v := range ...` */
        "expression_list" => p.parent().is_some_and(|d| {
            matches!(d.kind(), "short_var_declaration" | "range_clause") && d.child_by_field_name("left") == Some(p)
        }),
        _ => false,
    }
}

fn enclosing_fn(n: Node) -> Option<Node> {
    let mut cur = n.parent();
    while let Some(c) = cur {
        if FN_SCOPES.contains(&c.kind()) {
            return Some(c);
        }
        cur = c.parent();
    }
    None
}

/// Byte ranges to rename in one file: identifiers that mean the target, plus whole-word mentions in comments.
/// `defs_here` = start lines of target definitions in this file; `modules` = names imported as modules.
/// `sites` = which calls the receiver types allow editing; uses it leaves alone for want of a type are
/// returned too, as byte offsets.
fn rename_spans(t: &Target, root: Node, src: &[u8], defs_here: &[u32], modules: &HashSet<String>, sites: &Sites) -> (Vec<(usize, usize)>, Vec<usize>) {
    let name = t.name.as_str();
    let other_calls = &calls_at(root, src, name, &sites.other);
    let target_calls = sites.strict.as_ref().map(|k| calls_at(root, src, name, k));
    /* several types define the name: a use is edited only when its call is known to reach the target */
    let allowed = |callee: Node| match &target_calls {
        None => true,
        Some(ok) => called_by(callee).is_some_and(|c| ok.contains(&(c.start_byte(), c.end_byte()))),
    };
    let mut unsure: Vec<usize> = Vec::new();
    let macro_calls: Vec<(usize, (u32, usize))> =
        if t.method { macro_method_calls(root, src, name).into_iter().map(|(m, _, k)| (m.start_byte(), k)).collect() } else { Vec::new() };
    let mut idents: Vec<Node> = Vec::new();
    let mut comments: Vec<Node> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let k = n.kind();
        if k.contains("comment") {
            comments.push(n);
            continue;
        }
        if k.contains("string") && !k.contains("template") {
            continue;
        }
        if IDENT_KINDS.contains(&k) && n.child_count() == 0 && text(n, src) == name {
            idents.push(n);
        }
        let mut cur = n.walk();
        stack.extend(n.children(&mut cur));
    }
    /* 1. functions where a local binding shadows the name (unless the binding is a target definition) */
    let is_def = |n: Node| defs_here.contains(&(n.start_position().row as u32 + 1));
    let mut shadowed: Vec<(usize, usize)> = Vec::new();
    if !t.method {
        for &n in &idents {
            if is_local_binding(n) && !is_def(n) {
                if let Some(f) = enclosing_fn(n) {
                    shadowed.push((f.start_byte(), f.end_byte()));
                }
            }
        }
    }
    let in_shadow = |n: Node| shadowed.iter().any(|&(a, b)| n.start_byte() >= a && n.end_byte() <= b);
    /* 2. keep the identifiers that refer to the target */
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for n in idents {
        let def_name = is_def(n) && n.parent().map(|p| p.child_by_field_name("name") == Some(n)).unwrap_or(false);
        let in_macro = n.parent().is_some_and(|p| p.kind() == "token_tree");
        let keep = if def_name {
            true
        } else if in_macro {
            /* macro bodies are raw tokens: `.name` / `::name` is a member or path use, a bare name is not */
            let prev = n.prev_sibling().map(|p| text(p, src)).unwrap_or("");
            let member = prev == "." || prev == "::";
            if !t.method {
                prev != "." && !in_shadow(n)
            } else if !member {
                false
            } else {
                let key = macro_calls.iter().find(|m| m.0 == n.start_byte()).map(|m| m.1);
                match sites.macro_call(if prev == "." { key } else { None }) {
                    Some(edit) => edit,
                    None => {
                        unsure.push(n.start_byte());
                        false
                    }
                }
            }
        } else if let Some(access) = member_access(n).filter(|a| !(t.method && t.lang == Lang::Rust && rust_field(*a))) {
            /* a free function reached as `module.name` (Python) is still the target */
            let as_method = t.method && !on_other_type(access, other_calls);
            if as_method && !allowed(access) {
                unsure.push(n.start_byte());
            }
            (as_method && allowed(access)) || access_object(access, src).map(|o| modules.contains(o)).unwrap_or(false)
        } else if t.method {
            /* bare uses of a method name: `Type::name` paths (Rust), `Name()` calls (C#), `Type::name` refs (Java) */
            n.parent().is_some_and(|p| match p.kind() {
                "scoped_identifier" if on_other_type(p, other_calls) || module_path(p, src) => false,
                "scoped_identifier" if !allowed(p) => {
                    unsure.push(n.start_byte());
                    false
                }
                "scoped_identifier" => true,
                "method_reference" => true,
                "invocation_expression" => p.child_by_field_name("function") == Some(n),
                "generic_name" => p.parent().is_some_and(|g| member_access(p).is_some() || g.kind() == "invocation_expression"),
                _ => false,
            })
        } else {
            !in_shadow(n)
        };
        if keep {
            spans.push((n.start_byte(), n.end_byte()));
        }
    }
    /* 3. whole-word mentions in comments */
    for c in comments {
        let (a, b) = (c.start_byte(), c.end_byte());
        let body = &src[a..b];
        let mut i = 0;
        while let Some(off) = memchr::memmem::find(&body[i..], name.as_bytes()) {
            let pos = i + off;
            let end = pos + name.len();
            let left_ok = pos == 0 || !is_ident(body[pos - 1]);
            let right_ok = end >= body.len() || !is_ident(body[end]);
            if left_ok && right_ok {
                spans.push((a + pos, a + end));
            }
            i = end;
        }
    }
    spans.sort_unstable();
    spans.dedup();
    (spans, unsure)
}

/// Rust `x.name` that is not called: a field, never a method.
fn rust_field(access: Node) -> bool {
    access.kind() == "field_expression" && called_by(access).is_none()
}

/// `module::name`: a free function in a module, not `Type::name`.
fn module_path(scoped: Node, src: &[u8]) -> bool {
    scoped.child_by_field_name("path").is_some_and(|p| {
        let last = text(p, src).rsplit("::").next().unwrap_or("");
        last.starts_with(|c: char| c.is_ascii_lowercase()) && last != "self"
    })
}

/// The call whose callee is `callee` (`x.name` / `T::name`), if it is called.
fn called_by(callee: Node) -> Option<Node> {
    callee.parent().filter(|c| matches!(c.kind(), "call_expression" | "call") && c.child_by_field_name("function") == Some(callee))
}

/// `callee` is called, and that call is one of `other_calls`.
fn on_other_type(callee: Node, other_calls: &[(usize, usize)]) -> bool {
    called_by(callee).is_some_and(|c| other_calls.contains(&(c.start_byte(), c.end_byte())))
}

/// The name a call node calls (`f`, `x.f`, `T::f`, `f::<T>`), as extraction records it.
fn callee_name<'s>(call: Node, src: &'s [u8]) -> Option<&'s str> {
    let f = call.child_by_field_name("function")?;
    let f = if f.kind() == "generic_function" { f.child_by_field_name("function")? } else { f };
    let n = match f.kind() {
        "identifier" => f,
        "field_expression" => f.child_by_field_name("field")?,
        "member_expression" => f.child_by_field_name("property")?,
        "attribute" => f.child_by_field_name("attribute")?,
        "scoped_identifier" => f.child_by_field_name("name")?,
        _ => return None,
    };
    Some(text(n, src))
}

/// Byte ranges of the calls `keys` names: (start line, rank among calls of `name` starting on that
/// line, outer before inner). Extraction records calls in that order, so the ranks line up.
fn calls_at(root: Node, src: &[u8], name: &str, keys: &[(u32, usize)]) -> Vec<(usize, usize)> {
    if keys.is_empty() {
        return Vec::new();
    }
    let mut calls: Vec<(usize, usize, u32)> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), "call_expression" | "call") && callee_name(n, src) == Some(name) {
            calls.push((n.start_byte(), n.end_byte(), n.start_position().row as u32 + 1));
        }
        let mut cur = n.walk();
        stack.extend(n.named_children(&mut cur));
    }
    calls.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut out = Vec::new();
    let (mut line, mut rank) = (0, 0);
    for (a, b, l) in calls {
        rank = if l == line { rank + 1 } else { 0 };
        line = l;
        if keys.contains(&(l, rank)) {
            out.push((a, b));
        }
    }
    out
}

/// Position just after the last non-space byte before `end`.
fn trim_back(src: &[u8], mut end: usize) -> usize {
    while end > 0 && src[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    end
}

/// Argument lists of calls to the target in one file: (list start, last argument end, first keyword argument).
/// Calls inside Rust macros (`vec![f(1)]`) are found in their token trees.
/// `sites`: calls of another type are skipped; with several types defining the name, so are calls not
/// known to reach the target (their positions are pushed to `unsure`).
fn call_sites(t: &Target, root: Node, src: &[u8], modules: &HashSet<String>, sites: &Sites, unsure: &mut Vec<usize>) -> Vec<(usize, Option<usize>, Option<usize>)> {
    let name = t.name.as_str();
    let other_calls = calls_at(root, src, name, &sites.other);
    let target_calls = sites.strict.as_ref().map(|k| calls_at(root, src, name, k));
    let call_kind = if t.lang == Lang::Python { "call" } else { "call_expression" };
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == call_kind {
            if let (Some(f), Some(args)) = (n.child_by_field_name("function"), n.child_by_field_name("arguments")) {
                let f = if f.kind() == "generic_function" { f.child_by_field_name("function").unwrap_or(f) } else { f };
                let (callee, member) = match f.kind() {
                    "identifier" => (Some(f), false),
                    "scoped_identifier" => (f.child_by_field_name("name"), t.method && !module_path(f, src)),
                    "field_expression" => (f.child_by_field_name("field"), true),
                    "member_expression" => (f.child_by_field_name("property"), true),
                    "attribute" => (f.child_by_field_name("attribute"), true),
                    "selector_expression" => (f.child_by_field_name("field"), true),
                    _ => (None, false),
                };
                let module_call = matches!(f.kind(), "attribute" | "member_expression" | "selector_expression") && access_object(f, src).map(|o| modules.contains(o)).unwrap_or(false);
                let right_shape = if t.method { member } else { !member || module_call };
                let other_type = other_calls.contains(&(n.start_byte(), n.end_byte()));
                let named_so = callee.map(|c| text(c, src) == name).unwrap_or(false);
                let unknown = t.method && target_calls.as_ref().is_some_and(|ok| !ok.contains(&(n.start_byte(), n.end_byte())));
                if right_shape && !other_type && named_so && unknown {
                    unsure.push(n.start_byte());
                } else if right_shape && !other_type && named_so {
                    let mut cur = args.walk();
                    let named: Vec<Node> = args.named_children(&mut cur).filter(|c| !c.kind().contains("comment")).collect();
                    let first_kw = named.iter().find(|c| c.kind() == "keyword_argument").map(|c| c.start_byte());
                    out.push((args.start_byte(), named.last().map(|c| c.end_byte()), first_kw));
                }
            }
        }
        /* Rust macro arguments are raw tokens: `name` followed by a `( ... )` token tree is a call */
        if n.kind() == "token_tree" && !t.method {
            let mut cur = n.walk();
            let kids: Vec<Node> = n.children(&mut cur).collect();
            for w in kids.windows(2) {
                if w[0].kind() == "identifier" && text(w[0], src) == name && w[1].kind() == "token_tree" && text(w[1], src).starts_with('(') {
                    let inner = text(w[1], src);
                    let body = inner.strip_prefix('(').unwrap_or("");
                    let empty = body.strip_suffix(')').unwrap_or(body).trim().is_empty();
                    let last = (!empty).then(|| trim_back(src, w[1].end_byte() - 1));
                    out.push((w[1].start_byte(), last, None));
                }
            }
        }
        let mut cur = n.walk();
        stack.extend(n.named_children(&mut cur));
    }
    /* methods called in macro tokens: `vec![x.name(a)]` */
    if t.method {
        for (m, args, key) in macro_method_calls(root, src, name) {
            match sites.macro_call(Some(key)) {
                Some(true) => {
                    let inner = text(args, src);
                    let body = inner.strip_prefix('(').unwrap_or("");
                    let empty = body.strip_suffix(')').unwrap_or(body).trim().is_empty();
                    let last = (!empty).then(|| trim_back(src, args.end_byte() - 1));
                    out.push((args.start_byte(), last, None));
                }
                Some(false) => {}
                None => unsure.push(m.start_byte()),
            }
        }
    }
    out
}

/// Parameter lists of every target definition in this file: (list start, last param end, first star marker).
/// Python: a new positional parameter must go before `*`, `*args` or `**kwargs`.
/// Err when a definition ends in a rest/variadic parameter: no position keeps every call correct.
fn param_lists(t: &Target, root: Node, src: &[u8], def_lines: &[u32]) -> Result<Vec<(usize, Option<usize>, Option<usize>)>, u32> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let line = n.start_position().row as u32 + 1;
        let def_name = n.child_by_field_name("name").map(|x| text(x, src));
        let params = if def_lines.contains(&line) && def_name == Some(t.name.as_str()) {
            match n.kind() {
                "variable_declarator" => n.child_by_field_name("value").and_then(|v| v.child_by_field_name("parameters")),
                _ => n.child_by_field_name("parameters"),
            }
        } else {
            None
        };
        if let Some(p) = params {
            let mut cur = p.walk();
            let named: Vec<Node> = p.named_children(&mut cur).filter(|c| !c.kind().contains("comment")).collect();
            let rest = named.last().map(|c| {
                let mut cur = c.walk();
                let kids: Vec<Node> = c.named_children(&mut cur).collect();
                matches!(c.kind(), "variadic_parameter" | "variadic_parameter_declaration") || kids.iter().any(|k| k.kind() == "rest_pattern")
            });
            if rest == Some(true) {
                return Err(line);
            }
            let star = named
                .iter()
                .find(|c| matches!(c.kind(), "keyword_separator" | "list_splat_pattern" | "dictionary_splat_pattern"))
                .map(|c| c.start_byte());
            out.push((p.start_byte(), named.last().map(|c| c.end_byte()), star));
        }
        let mut cur = n.walk();
        stack.extend(n.named_children(&mut cur));
    }
    Ok(out)
}

/// Insert `item` as the last element of a parenthesized list, or before the `before` position.
fn insert_into_list(edits: &mut Vec<(usize, usize, String)>, open: usize, last: Option<usize>, before: Option<usize>, item: &str) {
    match (before, last) {
        (Some(b), _) => edits.push((b, b, format!("{item}, "))),
        (None, Some(end)) => edits.push((end, end, format!(", {item}"))),
        (None, None) => edits.push((open + 1, open + 1, item.to_string())),
    }
}

pub(crate) fn apply_edits(src: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    edits.sort_by(|a, b| b.0.cmp(&a.0));
    edits.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let mut s = src.to_string();
    for (a, b, t) in edits {
        if a <= b && b <= s.len() && s.is_char_boundary(a) && s.is_char_boundary(b) {
            s.replace_range(a..b, &t);
        }
    }
    s
}

/// First line where `word` occurs as a whole identifier, if any.
fn whole_word_line(src: &str, word: &str) -> Option<u32> {
    let b = src.as_bytes();
    let mut i = 0;
    while let Some(off) = memchr::memmem::find(&b[i..], word.as_bytes()) {
        let pos = i + off;
        let end = pos + word.len();
        if (pos == 0 || !is_ident(b[pos - 1])) && (end >= b.len() || !is_ident(b[end])) {
            return Some(line_of(src, pos));
        }
        i = end;
    }
    None
}

pub(crate) fn line_of(src: &str, byte: usize) -> u32 {
    src[..byte.min(src.len())].matches('\n').count() as u32 + 1
}

/// Names a file imports as whole modules (Python `import x`, `from pkg import x`, Go packages): `x.name` may mean the target.
fn module_names(src: &str, lang: Lang) -> HashSet<String> {
    if lang == Lang::Go {
        return crate::parse::extract(lang, src.as_bytes()).imports.iter().map(|i| go_import_name(src, i)).collect();
    }
    let mut out = HashSet::new();
    for l in src.lines() {
        let l = l.trim_start();
        if let Some((_, r)) = l.split_once("* as ") {
            /* TypeScript/JS namespace import: `import * as ns from "./x"` */
            let ns: String = r.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
            if !ns.is_empty() {
                out.insert(ns);
            }
            continue;
        }
        let rest = if let Some(r) = l.strip_prefix("import ") {
            r
        } else if let Some((_, r)) = l.strip_prefix("from ").and_then(|r| r.split_once(" import ")) {
            r
        } else {
            continue;
        };
        for part in rest.split(',') {
            let part = part.trim();
            let alias = part.rsplit(" as ").next().unwrap_or(part);
            let last = alias.rsplit('.').next().unwrap_or(alias).trim();
            if !last.is_empty() {
                out.insert(last.to_string());
            }
        }
    }
    out
}

/// The name a Go import binds: its alias when the line has one, else the package name of its path.
fn go_import_name(src: &str, i: &crate::parse::Import) -> String {
    let line = src.lines().nth(i.line as usize - 1).unwrap_or("").trim();
    let line = line.strip_prefix("import").unwrap_or(line).trim_start();
    match line.split_once('"') {
        Some((alias, _)) if !alias.trim().is_empty() && alias.trim() != "(" => alias.trim().to_string(),
        _ => crate::parse_go::go_package_name(&i.raw).to_string(),
    }
}

/// Text of a file, only if it hashes to `hash` (what was indexed) or passes the content guard.
pub(crate) fn checked_text(abs: &std::path::Path, hash: u64) -> Result<String, Refusal> {
    checked_bytes(index::with_bytes(abs, |b| b.to_vec()).map_err(|_| Refusal::NotFound)?, hash)
}

/// `checked_text` for bytes already read.
pub(crate) fn checked_bytes(bytes: Vec<u8>, hash: u64) -> Result<String, Refusal> {
    if xxhash_rust::xxh3::xxh3_64(&bytes) != hash {
        if let Some(r) = guard::check_content(&bytes) {
            return Err(r);
        }
    }
    String::from_utf8(bytes).map_err(|_| Refusal::Binary)
}

/// Write via a fresh, randomly named temp file in the same directory (created exclusively, so a planted
/// file or symlink cannot redirect it), then rename over the original. Symlinked targets are refused.
/// Write every resolved file in parallel; one result per file.
pub(crate) fn write_all(root: &std::path::Path, resolved: &[(String, String)]) -> Vec<Result<(), String>> {
    use rayon::prelude::*;
    resolved.par_iter().map(|(rel, c)| write_atomic(&root.join(rel), c).map_err(|e| e.to_string())).collect()
}

fn write_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::other("refusing to write through a symlink"));
    }
    let dir = path.parent().ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, content.as_bytes())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

impl Engine {
    /// Risk of changing `(fi, si)`: callers, files, packages, public or not, tests touching it.
    pub(crate) fn impact(&self, fi: u32, si: u32) -> Impact {
        let f = &self.files[fi as usize];
        let sym = &f.ex.symbols[si as usize];
        let callers = self.graph.callers(fi, si);
        let files: HashSet<u32> = callers.iter().map(|c| c.file).collect();
        let mut packages: HashSet<&str> = files.iter().map(|&c| package_of(&self.files[c as usize].path)).collect();
        packages.insert(package_of(&f.path));
        let tests = files
            .iter()
            .filter(|&&c| {
                let p = &self.files[c as usize].path;
                p.contains("test") || p.contains("spec")
            })
            .count();
        let n = callers.len();
        let level = if n > 10 || packages.len() > 2 {
            "high"
        } else if n <= 3 && packages.len() <= 1 {
            "low"
        } else {
            "medium"
        };
        let mut reason = format!("{n} callers in {} files across {} packages", files.len(), packages.len());
        if sym.exported {
            reason.push_str("; public");
        }
        if tests > 0 {
            reason.push_str(&format!("; {tests} test files call it"));
        }
        Impact { level, reason }
    }

    /// `path:name`, or `path:name@line` when that file defines the name more than once.
    fn target_ref(&self, f: u32, s: u32, name: &str, found: &[(u32, u32)]) -> String {
        let path = &self.files[f as usize].path;
        if found.iter().filter(|(of, _)| *of == f).count() > 1 {
            format!("{path}:{name}@{}", self.files[f as usize].ex.symbols[s as usize].start_line)
        } else {
            format!("{path}:{name}")
        }
    }

    /// Several definitions share a name: let the agent pick one instead of guessing.
    /// The recommendation is the most-called definition (public ones first on ties).
    pub(crate) fn choice_card(&self, name: &str, found: &[(u32, u32)], tool: &str) -> String {
        let mut ranked: Vec<(usize, bool, u32, u32)> = found
            .iter()
            .map(|&(f, s)| (self.graph.callers(f, s).len(), self.files[f as usize].ex.symbols[s as usize].exported, f, s))
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
        let mut card = format!("<choose name=\"{}\" options=\"{}\">\n", esc(name), ranked.len());
        for (k, (n, exported, f, s)) in ranked.iter().take(8).enumerate() {
            let fr = &self.files[*f as usize];
            let sym = &fr.ex.symbols[*s as usize];
            let letter = (b'A' + k as u8) as char;
            let rec = if k == 0 { " (recommended)" } else { "" };
            let public = if *exported { ", public" } else { "" };
            let owner = sym.parent.map(|p| format!(" in {}", fr.ex.symbols[p as usize].name)).unwrap_or_default();
            card.push_str(&esc(&format!(
                "{letter} {}:{} {}{owner} — {n} callers{public}{rec} -> {tool}(target=\"{}\")\n",
                fr.path,
                sym.start_line,
                sym.sig,
                self.target_ref(*f, *s, name, found)
            )));
        }
        card.push_str("</choose>\n");
        card
    }

    /// Definitions that must change together: TypeScript overloads (one file, all functions) or a Rust
    /// trait method with its `impl Trait for X` methods. None when the definitions are unrelated.
    fn as_group(&self, found: &[(u32, u32)]) -> Option<Vec<(u32, u32)>> {
        let sym = |&(f, s): &(u32, u32)| &self.files[f as usize].ex.symbols[s as usize];
        let parent_sym = |&(f, s): &(u32, u32)| {
            let fr = &self.files[f as usize];
            fr.ex.symbols[s as usize].parent.map(|p| &fr.ex.symbols[p as usize])
        };
        let f0 = found[0].0;
        if self.files[f0 as usize].lang.is_js_family() && found.iter().all(|x| x.0 == f0 && sym(x).kind == SymKind::Function) {
            return Some(found.to_vec());
        }
        /* Java/C#: overloads of one method, or a class with its constructors (one file) */
        if matches!(self.files[f0 as usize].lang, Lang::Java | Lang::CSharp) && found.iter().all(|x| x.0 == f0) {
            let owner = found.iter().find(|x| sym(x).kind != SymKind::Method);
            let parent = owner.map(|o| Some(o.1)).unwrap_or(sym(&found[0]).parent);
            let members_ok = found.iter().filter(|x| Some(**x) != owner.copied()).all(|x| sym(x).kind == SymKind::Method && sym(x).parent == parent);
            if parent.is_some() && members_ok && found.iter().filter(|x| sym(x).kind != SymKind::Method).count() <= 1 {
                let mut g: Vec<(u32, u32)> = owner.into_iter().copied().collect();
                g.extend(found.iter().filter(|x| Some(**x) != owner.copied()).copied());
                return Some(g);
            }
        }
        let traits: Vec<(u32, u32)> =
            found.iter().filter(|x| parent_sym(x).map(|p| p.kind == SymKind::Trait).unwrap_or(false)).copied().collect();
        if traits.len() == 1 && self.files[traits[0].0 as usize].lang == Lang::Rust {
            let tname = parent_sym(&traits[0]).map(|p| p.name.clone()).unwrap_or_default();
            let impls_it = |x: &(u32, u32)| {
                parent_sym(x)
                    .map(|p| p.kind == SymKind::Impl && (p.sig.contains(&format!(" {tname} for ")) || p.sig.contains(&format!("::{tname} for "))))
                    .unwrap_or(false)
            };
            if found.iter().all(|x| *x == traits[0] || impls_it(x)) {
                let mut g = vec![traits[0]];
                g.extend(found.iter().filter(|x| **x != traits[0]).copied());
                return Some(g);
            }
        }
        None
    }

    /// Name of the impl, trait or class that defines `(f, s)`, without generics or path.
    fn owner_name(&self, f: u32, s: u32) -> Option<&str> {
        let syms = &self.files[f as usize].ex.symbols;
        let p = &syms[syms[s as usize].parent? as usize].name;
        let p = p.split('<').next().unwrap_or(p);
        Some(p.rsplit("::").next().unwrap_or(p).trim())
    }

    /// `d` with the definitions that must change with it: a Rust trait method and every `impl Trait for X`
    /// method of the same name, when `d` is one of them.
    fn with_trait_family(&self, d: (u32, u32), all: &[(u32, u32)]) -> Vec<(u32, u32)> {
        let parent = |&(f, s): &(u32, u32)| {
            let syms = &self.files[f as usize].ex.symbols;
            syms[s as usize].parent.map(|p| &syms[p as usize])
        };
        let implemented = |x: &(u32, u32)| {
            parent(x).filter(|p| p.kind == SymKind::Impl).and_then(|p| {
                let head = p.sig.split(" for ").next().filter(|_| p.sig.contains(" for "))?;
                let tr = head.rsplit([' ', ':']).next()?;
                Some(tr.split('<').next().unwrap_or(tr).to_string())
            })
        };
        let tname = match parent(&d) {
            Some(p) if p.kind == SymKind::Trait => Some(p.name.clone()),
            _ => implemented(&d),
        };
        let Some(tname) = tname else { return vec![d] };
        let family: Vec<(u32, u32)> = all
            .iter()
            .copied()
            .filter(|x| parent(x).is_some_and(|p| p.kind == SymKind::Trait && p.name == tname) || implemented(x).as_deref() == Some(&tname))
            .collect();
        match self.as_group(&family) {
            Some(g) if g.contains(&d) => g,
            _ => vec![d],
        }
    }

    /// Resolve `name`, `path:name` or `path:name@line` to a target, or the reply to send instead.
    pub(crate) fn resolve_target(&mut self, target: &str, tool: &str, budget: usize) -> Result<Target, String> {
        self.resolve_target_where(target, tool, budget, |_| true)
    }

    /// `resolve_target`, preferring definitions that pass `keep` (when any do) before deciding it is ambiguous.
    pub(crate) fn resolve_target_where(
        &mut self,
        target: &str,
        tool: &str,
        budget: usize,
        keep: impl Fn(&crate::parse::Symbol) -> bool,
    ) -> Result<Target, String> {
        /* `Type::name` / `Type.name`: the member of that type (or of the trait it implements) */
        let spec = target.replace("::", ".");
        let (target_no_line, line) = match spec.rsplit_once('@') {
            Some((t, l)) if !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()) => (t, l.parse::<u32>().ok()),
            _ => (spec.as_str(), None),
        };
        let (path, name) = match target_no_line.rsplit_once(':') {
            Some((p, n)) if p.contains('.') || p.contains('/') => (Some(p), n),
            _ => (None, target_no_line),
        };
        if let Some(p) = path {
            match self.resolve_rel(p) {
                Ok(rel) => {
                    if let Err(r) = self.ensure_fresh(&rel) {
                        return Err(self.refusal(&rel, r));
                    }
                }
                Err(r) => return Err(self.refusal(p, r)),
            }
        }
        let (owner, name) = match name.rsplit_once('.') {
            Some((o, n)) => (Some(o.rsplit('.').next().unwrap_or(o)), n),
            None => (None, name),
        };
        let mut found = self.find_symbols(name, path);
        if let Some(o) = owner {
            let all = found.clone();
            found.retain(|&(f, s)| self.owner_name(f, s) == Some(o));
            if let [d] = found[..] {
                found = self.with_trait_family(d, &all);
            }
        }
        if let Some(l) = line {
            found.retain(|(f, s)| self.files[*f as usize].ex.symbols[*s as usize].start_line == l);
        }
        if found.iter().any(|(f, s)| keep(&self.files[*f as usize].ex.symbols[*s as usize])) {
            found.retain(|(f, s)| keep(&self.files[*f as usize].ex.symbols[*s as usize]));
        }
        /* ambient `.d.ts` declarations never compete with a real definition */
        if found.iter().any(|(f, _)| !self.files[*f as usize].path.ends_with(".d.ts")) {
            found.retain(|(f, _)| !self.files[*f as usize].path.ends_with(".d.ts"));
        }
        if found.is_empty() {
            let open = format!("<{tool} target=\"{}\" found=\"false\">", esc(target));
            let hint = self.near_names(name);
            return Err(self.finish(&open, &format!("</{tool}>"), &hint, budget, ""));
        }
        let defs = if found.len() == 1 {
            found
        } else if let Some(g) = self.as_group(&found) {
            g
        } else {
            let open = format!("<{tool} target=\"{}\" ambiguous=\"true\">", esc(target));
            let card = self.choice_card(name, &found, tool);
            return Err(self.finish(&open, &format!("</{tool}>"), &card, budget, ""));
        };
        let (f0, s0) = defs[0];
        let sym = &self.files[f0 as usize].ex.symbols[s0 as usize];
        Ok(Target { name: sym.name.clone(), lang: self.files[f0 as usize].lang, method: sym.kind == SymKind::Method, defs })
    }

    /// Files to edit: every file naming the target when no unrelated definition shares the name; otherwise
    /// only the definitions' files, their callers' files and their importers. Returns (to edit, skipped).
    fn edit_scope(&self, t: &Target) -> (Vec<(u32, Vec<u32>)>, Vec<(u32, Vec<u32>)>) {
        /* every call is judged by its receiver type: look at every file, edit only what is known */
        let strict = self.strict_target(t);
        /* ambient `.d.ts` declarations describe platform APIs, not this code */
        let def_in_dts = t.defs.iter().any(|(f, _)| self.files[*f as usize].path.ends_with(".d.ts"));
        let sites: Vec<(u32, Vec<u32>)> = self
            .sites(&t.name, t.lang)
            .into_iter()
            .filter(|(file, _)| def_in_dts || !self.files[*file as usize].path.ends_with(".d.ts"))
            .collect();
        let others = self
            .graph
            .by_name
            .get(&t.name)
            .map(|v| {
                v.iter()
                    .filter(|d| !t.defs.contains(d) && same_family(self.files[d.0 as usize].lang, t.lang))
                    .filter(|d| !self.files[d.0 as usize].path.ends_with(".d.ts"))
                    .count()
            })
            .unwrap_or(0);
        if others == 0 || strict {
            return (sites, Vec::new());
        }
        let mut linked: HashSet<u32> = HashSet::new();
        for &(f, s) in &t.defs {
            linked.insert(f);
            linked.extend(self.graph.callers(f, s).iter().map(|c| c.file));
            linked.extend(self.graph.importers(f));
        }
        sites.into_iter().partition(|(file, _)| linked.contains(file))
    }

    /// Every file to look at: the scope plus the definitions' own files.
    fn scope_files(t: &Target, scope: &[(u32, Vec<u32>)]) -> Vec<u32> {
        let mut files: Vec<u32> = scope.iter().map(|s| s.0).collect();
        for d in &t.defs {
            if !files.contains(&d.0) {
                files.push(d.0);
            }
        }
        files
    }

    fn def_lines(&self, t: &Target, file: u32) -> Vec<u32> {
        t.defs.iter().filter(|(f, _)| *f == file).map(|(f, s)| self.files[*f as usize].ex.symbols[*s as usize].start_line).collect()
    }

    /// Text of an indexed file, only if it is exactly what was parsed or passes the content guard.
    pub(crate) fn checked_source(&self, file: u32) -> Result<String, Refusal> {
        let f = &self.files[file as usize];
        checked_text(&self.root.join(&f.path), f.hash)
    }

    /// Write the new contents and re-index them.
    pub(crate) fn commit_files(&mut self, writes: Vec<(String, String)>) -> Result<(), String> {
        let resolved = self.resolve_writes(writes)?;
        let wrote = write_all(&self.root, &resolved);
        self.after_write(resolved, wrote, Vec::new())
    }

    /// Every path of a planned write, checked against the read rules before anything is written.
    pub(crate) fn resolve_writes(&self, writes: Vec<(String, String)>) -> Result<Vec<(String, String)>, String> {
        writes.into_iter().map(|(rel, c)| Ok((self.resolve_rel(&rel).map_err(|r| r.code().to_string())?, c))).collect()
    }

    /// Re-index the files that were written (now, or after the reply when `defer_reindex`); the first write error.
    /// `trees[i]`, when present, is the tree of `resolved[i]`'s new text, so it need not be parsed again.
    pub(crate) fn after_write(
        &mut self,
        resolved: Vec<(String, String)>,
        wrote: Vec<Result<(), String>>,
        mut trees: Vec<Option<(Lang, tree_sitter::Tree)>>,
    ) -> Result<(), String> {
        trees.resize_with(resolved.len(), || None);
        /* tsserver gets the written text before its next answer */
        for (rel, _) in resolved.iter().filter(|(rel, _)| Lang::from_path(std::path::Path::new(rel)).is_some_and(|l| l.is_js_family())) {
            self.ts.touch(self.root.join(rel));
        }
        let mut first_err = Ok(());
        let mut done = Vec::new();
        for (((rel, text), tree), w) in resolved.into_iter().zip(trees).zip(wrote) {
            match w {
                Ok(()) => done.push((rel, text, tree)),
                Err(e) if first_err.is_ok() => first_err = Err(e),
                Err(_) => {}
            }
        }
        if self.defer_reindex {
            self.deferred.extend(done);
        } else {
            self.reindex_written(done);
        }
        first_err
    }

    pub(crate) fn lines_block(files: &[(String, Vec<u32>)]) -> String {
        let mut s = String::new();
        for (p, lines) in files {
            let mut lines = lines.clone();
            lines.sort_unstable();
            lines.dedup();
            let ls: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            s.push_str(&esc(&format!("{p} lines {}\n", ls.join(","))));
        }
        s
    }

    pub(crate) fn skipped_block(&self, skipped: &[(u32, Vec<u32>)]) -> String {
        if skipped.is_empty() {
            return String::new();
        }
        let named: Vec<(String, Vec<u32>)> = skipped.iter().map(|(f, l)| (self.files[*f as usize].path.clone(), l.clone())).collect();
        format!(
            "<skipped reason=\"other definitions share this name; these files do not reference this one\">\n{}</skipped>\n",
            Self::lines_block(&named)
        )
    }

    /// `rename`: rename a symbol everywhere it is referenced and write the files; text mentions are reported.
    pub fn rename(&mut self, target: &str, new_name: &str, dry_run: bool, force: bool) -> String {
        self.rename_opts(target, new_name, dry_run, force, "report", false)
    }

    /// `rename` with `text` = "report" | "replace" | "skip": what to do with whole-word mentions of the
    /// old name in docs and config (Markdown, YAML, JSON...) that the code rename does not touch.
    pub fn rename_with_text(&mut self, target: &str, new_name: &str, dry_run: bool, force: bool, text: &str) -> String {
        self.rename_opts(target, new_name, dry_run, force, text, false)
    }

    /// `rename`, plus (`check`) the compile result of the written files in the same reply.
    pub fn rename_and_check(&mut self, target: &str, new_name: &str, dry_run: bool, force: bool, check: bool) -> String {
        self.rename_opts(target, new_name, dry_run, force, "report", check)
    }

    /// `rename` with every option: doc mentions (`text`) and a compile check of the written files (`check`).
    pub fn rename_opts(&mut self, target: &str, new_name: &str, dry_run: bool, force: bool, text: &str, check: bool) -> String {
        let budget = 4000;
        if !matches!(text, "report" | "replace" | "skip") {
            let open = format!("<rename target=\"{}\" error=\"invalid_text\">", esc(target));
            return self.finish(&open, "</rename>", "text must be report, replace or skip\n", budget, "");
        }
        if !valid_ident(new_name) {
            let open = format!("<rename target=\"{}\" error=\"invalid_name\">", esc(target));
            return self.finish(&open, "</rename>", "new_name must be an identifier\n", budget, "");
        }
        /* the follow-up to a report: code already renamed, so only the text mentions are left */
        let old = target.rsplit_once('@').map(|(t, _)| t).unwrap_or(target);
        let old = old.rsplit_once(':').map(|(_, n)| n).unwrap_or(old);
        if text == "replace" && !self.graph.by_name.contains_key(old) && self.graph.by_name.contains_key(new_name) {
            let mentions = textrefs::scan(&self.root, old);
            let applied = !dry_run;
            if applied {
                if let Err(e) = self.write_text_mentions(&mentions, new_name, text) {
                    let open = format!("<rename from=\"{}\" to=\"{}\" error=\"write_failed\">", esc(old), esc(new_name));
                    return self.finish(&open, "</rename>", &esc(&e), budget, "");
                }
            }
            let open = format!("<rename from=\"{}\" to=\"{}\" applied=\"{applied}\" files=\"0\" sites=\"0\" text_only=\"true\">", esc(old), esc(new_name));
            let body = Self::text_block(&mentions, text, applied);
            return self.finish(&open, "</rename>", &body, budget, "");
        }
        let t = match self.resolve_target(target, "rename", budget) {
            Ok(t) => t,
            Err(reply) => return reply,
        };
        if t.lang.basic() {
            let open = format!("<rename target=\"{}\" error=\"unsupported_language\">", esc(target));
            let msg = format!("rename has no {} rules yet; rewrite(pattern, replacement, lang=\"{}\") edits matching code.\n", t.lang.name(), t.lang.name());
            return self.finish(&open, "</rename>", &esc(&msg), budget, "");
        }
        let (fi, si) = t.defs[0];
        let impact = self.impact(fi, si);
        let (scope, mut skipped) = self.edit_scope(&t);
        let mut files = Self::scope_files(&t, &scope);
        /* TS/JS: tsserver's rename locations, when it can answer */
        let mut exact = self.ts_mode(t.lang);
        let mut plan = None;
        if let Exact::Yes = exact {
            match self.ts_rename_plan(&t, new_name, text == "replace") {
                Ok(Ok(p)) => {
                    files = p.iter().filter_map(|f: &FileEdits| self.graph.path_idx.get(&f.0).copied()).collect();
                    skipped.clear();
                    plan = Some(p);
                }
                Ok(Err(why)) => {
                    let open = format!("<rename target=\"{}\" error=\"tsserver_refused\" exact=\"true\">", esc(target));
                    return self.finish(&open, "</rename>", &esc(&format!("{why}\n")), budget, "");
                }
                Err(Fallback(why)) => exact = Exact::No(why),
            }
        }

        /* 1. a new name that already exists where we would write it is a decision, not a default */
        let clash: Vec<String> = self
            .graph
            .by_name
            .get(new_name)
            .into_iter()
            .flatten()
            .filter(|(df, _)| files.contains(df))
            .map(|(df, ds)| format!("{}:{}", self.files[*df as usize].path, self.files[*df as usize].ex.symbols[*ds as usize].start_line))
            .collect();
        if !clash.is_empty() && !force {
            let open = format!("<rename from=\"{}\" to=\"{}\" applied=\"false\">", esc(&t.name), esc(new_name));
            let body = format!(
                "<choose>\n{}</choose>\n",
                esc(&format!("A pick another name (recommended): `{new_name}` is already defined at {}\nB rename anyway -> rename(target, new_name, force=true)\n",
                clash.join(", ")))
            );
            return self.finish(&open, "</rename>", &body, budget, "");
        }

        /* 2. syntax-aware spans per file; a file that already uses the new name (a local, a parameter,
         * a prop) could change meaning, so that is a decision too */
        let mut local_clash: Vec<String> = Vec::new();
        let mut writes = Vec::new();
        let mut changed: Vec<(String, Vec<u32>)> = Vec::new();
        let mut unsure: Vec<(String, Vec<u32>)> = Vec::new();
        let strict = self.strict_target(&t);
        let mut total = 0;
        let planned: Vec<FileEdits> = match plan {
            Some(p) => p,
            None => {
                let mut v = Vec::new();
                for file in files {
                    let path = self.files[file as usize].path.clone();
                    let lang = self.files[file as usize].lang;
                    let src = match self.checked_source(file) {
                        Ok(s) => s,
                        Err(r) => return self.refusal(&path, r),
                    };
                    let Some(tree) = parse_tree(lang, src.as_bytes()) else { continue };
                    let sites = self.method_sites(&t, file);
                    let (mut spans, left) = rename_spans(&t, tree.root_node(), src.as_bytes(), &self.def_lines(&t, file), &module_names(&src, lang), &sites);
                    /* a file where no code changes keeps its comments too: the word may mean another type's method */
                    let in_comment = |a: usize| tree.root_node().descendant_for_byte_range(a, a).is_some_and(|n| n.kind().contains("comment"));
                    if strict && spans.iter().all(|s| in_comment(s.0)) {
                        spans.clear();
                    }
                    if !left.is_empty() {
                        let mut lines: Vec<u32> = left.iter().map(|&b| line_of(&src, b)).collect();
                        lines.dedup();
                        unsure.push((path.clone(), lines));
                    }
                    let edits: Vec<(usize, usize, String)> = spans.into_iter().map(|(a, b)| (a, b, new_name.to_string())).collect();
                    v.push((path, src, edits));
                }
                v
            }
        };
        let kept = alias_note(&planned, &t.name);
        for (path, src, edits) in planned {
            if edits.is_empty() {
                continue;
            }
            if let Some(l) = whole_word_line(&src, new_name) {
                local_clash.push(format!("{path}:{l}"));
            }
            total += edits.len();
            changed.push((path.clone(), edits.iter().map(|s| line_of(&src, s.0)).collect()));
            writes.push((path, apply_edits(&src, edits)));
        }
        if !local_clash.is_empty() && !force {
            let open = format!("<rename from=\"{}\" to=\"{}\" applied=\"false\">", esc(&t.name), esc(new_name));
            local_clash.truncate(8);
            let body = format!(
                "<choose>\n{}</choose>\n",
                esc(&format!("A pick another name (recommended): `{new_name}` is already used in files this rename edits: {}\nB rename anyway -> rename(target, new_name, force=true)\n",
                local_clash.join(", ")))
            );
            return self.finish(&open, "</rename>", &body, budget, "");
        }
        let n_files = writes.len();
        let mentions = if text == "skip" { Vec::new() } else { textrefs::scan(&self.root, &t.name) };

        /* 3. write and re-index, unless asked for a plan only; tsserver's errors before and after */
        let written: Vec<String> = writes.iter().map(|w| w.0.clone()).collect();
        let errors_before = match exact {
            Exact::Yes if !dry_run => self.ts_errors(&written),
            _ => None,
        };
        let applied = if dry_run {
            "false"
        } else if let Err(e) = self.commit_files(writes).and_then(|_| self.write_text_mentions(&mentions, new_name, text)) {
            let open = format!("<rename from=\"{}\" to=\"{}\" error=\"write_failed\">", esc(&t.name), esc(new_name));
            return self.finish(&open, "</rename>", &esc(&e), budget, "");
        } else {
            "true"
        };
        let mut body = exact.note();
        body.push_str(&Self::lines_block(&changed));
        body.push_str(&kept);
        body.push_str(&self.skipped_block(&skipped));
        body.push_str(&self.unsure_block(&t, &unsure));
        if applied == "true" {
            body.push_str(&self.verify_gone(&t, &skipped));
            if let Some((before, after)) = errors_before.zip(self.ts_errors(&written)) {
                body.push_str(&self.diagnostics_block(before, after, Some((&t.name, new_name))));
            }
        }
        body.push_str(if applied == "true" {
            "<next>Done: files are written and re-indexed. No Read, Edit or Grep is needed for these sites.</next>\n"
        } else {
            "<next>Plan only: call again without dry_run to apply.</next>\n"
        });
        body.push_str(&Self::text_block(&mentions, text, applied == "true"));
        if check && applied == "true" {
            body.push_str(&self.check_written(&written));
        }
        let group = if t.defs.len() > 1 { format!(" definitions=\"{}\"", t.defs.len()) } else { String::new() };
        let open = format!(
            "<rename from=\"{}\" to=\"{}\" applied=\"{applied}\" files=\"{n_files}\" sites=\"{total}\"{group} impact=\"{}\" why=\"{}\"{}>",
            esc(&t.name),
            esc(new_name),
            impact.level,
            esc(&impact.reason),
            exact.attr()
        );
        self.finish(&open, "</rename>", &body, budget, "")
    }

    /// After a rename: re-scan the index for code lines still naming the old symbol, outside files that
    /// belong to other same-named definitions. Proof in the reply saves the agent a Grep.
    fn verify_gone(&self, t: &Target, skipped: &[(u32, Vec<u32>)]) -> String {
        let skip: HashSet<u32> = skipped.iter().map(|s| s.0).collect();
        /* only identifiers count: the old name inside strings or comments is left alone by design */
        let mut left: Vec<(String, Vec<u32>)> = Vec::new();
        for (f, _) in self.sites(&t.name, t.lang) {
            let path = self.files[f as usize].path.clone();
            if skip.contains(&f) || path.ends_with(".d.ts") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(self.root.join(&path)) else { continue };
            let Some(tree) = parse_tree(self.files[f as usize].lang, src.as_bytes()) else { continue };
            let mut lines = Vec::new();
            let mut stack = vec![tree.root_node()];
            while let Some(n) = stack.pop() {
                let k = n.kind();
                if k.contains("comment") || (k.contains("string") && !k.contains("template")) {
                    continue;
                }
                if IDENT_KINDS.contains(&k) && n.child_count() == 0 && text(n, src.as_bytes()) == t.name {
                    lines.push(n.start_position().row as u32 + 1);
                }
                let mut cur = n.walk();
                stack.extend(n.children(&mut cur));
            }
            if !lines.is_empty() {
                lines.sort_unstable();
                left.push((path, lines));
            }
        }
        if left.is_empty() {
            /* comments were renamed with the code; whatever is left can only be inside string literals */
            let in_strings: usize = self.sites(&t.name, t.lang).iter().filter(|(f, _)| !skip.contains(f)).map(|(_, l)| l.len()).sum();
            let strings = if in_strings == 0 {
                "No code file mentions it at all (comments included).".to_string()
            } else {
                format!("{in_strings} line(s) still mention it inside string literals, left as is on purpose.")
            };
            return format!(
                "<verified>Re-scanned every indexed file: no identifier or comment names `{}` any more. {strings} No Grep needed.</verified>\n",
                esc(&t.name)
            );
        }
        format!(
            "<still_named reason=\"identifiers with the old name left in code: unrelated locals or members of other types\">\n{}</still_named>\n",
            Self::lines_block(&left)
        )
    }

    /// After change_signature: re-parse the written files and confirm every call now passes the new argument.
    fn verify_calls(&self, t: &Target, written: &[String], arg: &str) -> String {
        let (mut total, mut missing) = (0usize, Vec::new());
        for path in written {
            let Ok(src) = std::fs::read_to_string(self.root.join(path)) else { continue };
            let lang = Lang::from_path(std::path::Path::new(path)).unwrap_or(t.lang);
            let Some(tree) = parse_tree(lang, src.as_bytes()) else { continue };
            /* adding an argument moves no line, so the indexed call lines still match */
            let sites = self.graph.path_idx.get(path).map(|&f| self.method_sites(t, f)).unwrap_or_default();
            for (open, last, _) in call_sites(t, tree.root_node(), src.as_bytes(), &module_names(&src, lang), &sites, &mut Vec::new()) {
                total += 1;
                /* the argument list runs from `(` to the end of its last argument */
                let end = last.unwrap_or(open).min(src.len());
                if !src.get(open..end).is_some_and(|args| args.contains(arg)) {
                    missing.push(format!("{path}:{}", line_of(&src, open)));
                }
            }
        }
        if missing.is_empty() {
            return format!("<verified>Re-parsed the written files: all {total} calls of `{}` now pass the new argument. No Grep needed.</verified>\n", esc(&t.name));
        }
        format!("<unverified reason=\"calls whose argument list does not show the new argument\">\n{}\n</unverified>\n", esc(&missing.join("\n")))
    }

    /// After change_signature: the primary definition as it is now on disk, with line numbers, so the
    /// agent can Edit the body without another lookup. Capped at 60 lines.
    fn definition_block(&self, t: &Target) -> String {
        let path = self.files[t.defs[0].0 as usize].path.clone();
        let Some(&(f, s)) = self.find_symbols(&t.name, Some(&path)).first() else { return String::new() };
        let sym = &self.files[f as usize].ex.symbols[s as usize];
        let Ok(src) = std::fs::read_to_string(self.root.join(&path)) else { return String::new() };
        let (a, b) = (sym.start_line, sym.end_line.min(sym.start_line + 59));
        let code: String = src
            .lines()
            .enumerate()
            .skip(a as usize - 1)
            .take((b - a + 1) as usize)
            .map(|(i, l)| format!("{}\t{l}\n", i + 1))
            .collect();
        let cut = if sym.end_line > b { format!(" shown=\"{a}-{b}\"") } else { String::new() };
        format!("<definition path=\"{}\" lines=\"{a}-{}\"{cut}>\n{}</definition>\n", esc(&path), sym.end_line, esc(&code))
    }

    /// Rewrite text mentions (text="replace" only). These files are not indexed: nothing to refresh.
    fn write_text_mentions(&self, mentions: &[textrefs::Mention], new_name: &str, text: &str) -> Result<(), String> {
        if text != "replace" {
            return Ok(());
        }
        for m in mentions {
            let rel = self.resolve_rel(&m.path).map_err(|r| r.code().to_string())?;
            write_atomic(&self.root.join(&rel), &m.replaced(new_name)).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// `<text_mentions>`: docs and config naming the old name, at most 30 files listed.
    fn text_block(mentions: &[textrefs::Mention], text: &str, applied: bool) -> String {
        if mentions.is_empty() {
            return String::new();
        }
        let lines: usize = mentions.iter().map(|m| m.lines().len()).sum();
        let listed: Vec<(String, Vec<u32>)> = mentions.iter().take(30).map(|m| (m.path.clone(), m.lines())).collect();
        let more = if mentions.len() > 30 { format!("... {} more files\n", mentions.len() - 30) } else { String::new() };
        let replaced = if text == "replace" && applied { " replaced=\"true\"" } else { "" };
        let mut s = format!("<text_mentions files=\"{}\" lines=\"{lines}\"{replaced}>\n{}{more}</text_mentions>\n", mentions.len(), Self::lines_block(&listed));
        if text == "report" {
            s.push_str("<next>Docs/config above still name the old symbol: call rename again with text=\"replace\" to update them (same target and new_name; the code is already renamed).</next>\n");
        }
        s
    }

    /// `change_signature`: append a parameter to a function's definition(s) and the matching argument to every call.
    /// `move_symbol`, refused up front for a destination language it does not know (it moves TS/JS/Python).
    pub fn move_symbol_checked(&mut self, target: &str, to_file: &str, dry_run: bool) -> String {
        match Lang::from_path(std::path::Path::new(to_file)) {
            Some(l) if !l.full_refactor() => {
                let msg = format!("move_symbol does not move {} code yet; move it by hand, then `rename` fixes references.", l.name());
                self.move_err(target, to_file, "unsupported_language", &msg)
            }
            _ => self.move_symbol(target, to_file, dry_run),
        }
    }

    pub fn change_signature(&mut self, target: &str, add_param: &str, call_arg: &str, dry_run: bool) -> String {
        self.change_signature_and_check(target, add_param, call_arg, dry_run, false)
    }

    /// `change_signature`, plus (`check`) the compile result of the written files in the same reply.
    pub fn change_signature_and_check(&mut self, target: &str, add_param: &str, call_arg: &str, dry_run: bool, check: bool) -> String {
        let budget = 4000;
        if add_param.trim().is_empty() || call_arg.trim().is_empty() || add_param.contains('\n') || call_arg.contains('\n') {
            let open = format!("<change_signature target=\"{}\" error=\"invalid_input\">", esc(target));
            return self.finish(&open, "</change_signature>", "add_param and call_arg must be one-line, non-empty\n", budget, "");
        }
        let t = match self.resolve_target(target, "change_signature", budget) {
            Ok(t) => t,
            Err(reply) => return reply,
        };
        let (fi, si) = t.defs[0];
        if !t.lang.full_refactor() && t.lang != Lang::Go {
            let open = format!("<change_signature target=\"{}\" error=\"unsupported_language\">", esc(target));
            let msg = format!("change_signature does not edit {} code yet; rename, rewrite and context do.\n", t.lang.name());
            return self.finish(&open, "</change_signature>", &esc(&msg), budget, "");
        }
        if !matches!(self.files[fi as usize].ex.symbols[si as usize].kind, SymKind::Function | SymKind::Method) {
            let open = format!("<change_signature target=\"{}\" error=\"not_a_function\">", esc(target));
            return self.finish(&open, "</change_signature>", "", budget, "");
        }
        let impact = self.impact(fi, si);
        let (scope, mut skipped) = self.edit_scope(&t);
        let mut files = Self::scope_files(&t, &scope);
        /* TS/JS: tsserver's references decide which calls and declarations are the target's */
        let mut exact = self.ts_mode(t.lang);
        let mut refs: HashMap<u32, (HashSet<usize>, Vec<u32>)> = HashMap::new();
        if let Exact::Yes = exact {
            match self.ts_refs(&t) {
                Ok(r) => {
                    skipped.clear();
                    refs = r.into_iter().map(|(f, starts, defs)| (f, (starts.into_iter().collect(), defs))).collect();
                    files = refs.keys().copied().collect();
                    for d in &t.defs {
                        if !files.contains(&d.0) {
                            files.push(d.0);
                        }
                    }
                    files.sort_by(|a, b| self.files[*a as usize].path.cmp(&self.files[*b as usize].path));
                }
                Err(Fallback(why)) => exact = Exact::No(why),
            }
        }

        /* 1. per file: every definition's parameter list and every call's argument list */
        let mut writes = Vec::new();
        let mut calls: Vec<(String, Vec<u32>)> = Vec::new();
        let mut to_check: Vec<(String, Vec<u32>)> = Vec::new();
        let mut unsure: Vec<(String, Vec<u32>)> = Vec::new();
        let mut defs_updated = 0;
        for file in files {
            let path = self.files[file as usize].path.clone();
            let lang = self.files[file as usize].lang;
            let src = match self.checked_source(file) {
                Ok(s) => s,
                Err(r) => return self.refusal(&path, r),
            };
            let Some(tree) = parse_tree(lang, src.as_bytes()) else { continue };
            let file_refs = refs.get(&file);
            let mut def_lines = self.def_lines(&t, file);
            if let Some((starts, d)) = file_refs {
                /* declarations among the references: the target's own, and implementations of an interface method */
                let decls = starts.iter().filter(|&&a| ts_declares(tree.root_node(), a)).map(|&a| line_of(&src, a));
                let more: Vec<u32> = d.iter().copied().chain(decls).collect();
                for l in more {
                    if !def_lines.contains(&l) {
                        def_lines.push(l);
                    }
                }
            }
            let mut edits = Vec::new();
            let lists = match param_lists(&t, tree.root_node(), src.as_bytes(), &def_lines) {
                Ok(l) => l,
                Err(line) => {
                    let open = format!("<change_signature target=\"{}\" error=\"rest_parameter\">", esc(target));
                    let body = esc(&format!("{path}:{line} ends in a rest parameter; calls cannot get a positional argument after it. Edit by hand.\n"));
                    return self.finish(&open, "</change_signature>", &body, budget, "");
                }
            };
            for (open, last, star) in lists {
                insert_into_list(&mut edits, open, last, star, add_param.trim());
                defs_updated += 1;
            }
            let modules = module_names(&src, lang);
            let sites = self.method_sites(&t, file);
            let mut call_lines = Vec::new();
            let mut left = Vec::new();
            let found = match (&exact, file_refs) {
                (Exact::Yes, Some((starts, _))) => ts_call_sites(tree.root_node(), starts),
                (Exact::Yes, None) => Vec::new(),
                _ => call_sites(&t, tree.root_node(), src.as_bytes(), &modules, &sites, &mut left),
            };
            for (open, last, first_kw) in found {
                insert_into_list(&mut edits, open, last, first_kw, call_arg.trim());
                call_lines.push(line_of(&src, open));
            }
            /* references that are not calls (callbacks, re-exports) need a human look */
            let src_lines: Vec<&str> = src.lines().collect();
            let skip_line = |l: u32| {
                let tl = src_lines.get(l as usize - 1).map(|x| x.trim_start()).unwrap_or("");
                ["import ", "use ", "pub use ", "from ", "export {", "export *", "//", "#", "*", "/*"].iter().any(|p| tl.starts_with(p))
            };
            let (named, left_refs): (Vec<usize>, Vec<usize>) = match (&exact, file_refs) {
                (Exact::Yes, Some((starts, _))) => (starts.iter().copied().collect(), Vec::new()),
                (Exact::Yes, None) => (Vec::new(), Vec::new()),
                _ => {
                    let (spans, l) = rename_spans(&t, tree.root_node(), src.as_bytes(), &def_lines, &modules, &sites);
                    (spans.iter().map(|s| s.0).collect(), l)
                }
            };
            let mut left: Vec<u32> = left.iter().chain(&left_refs).map(|&b| line_of(&src, b)).collect();
            left.sort_unstable();
            left.dedup();
            if !left.is_empty() {
                unsure.push((path.clone(), left.clone()));
            }
            let mut other: Vec<u32> = named
                .iter()
                .map(|&a| line_of(&src, a))
                .filter(|l| !call_lines.contains(l) && !def_lines.contains(l) && !skip_line(*l) && !left.contains(l))
                .collect();
            other.sort_unstable();
            other.dedup();
            if !other.is_empty() {
                to_check.push((path.clone(), other));
            }
            if !edits.is_empty() {
                if !call_lines.is_empty() {
                    calls.push((path.clone(), call_lines));
                }
                writes.push((path, apply_edits(&src, edits)));
            }
        }
        if defs_updated == 0 {
            let open = format!("<change_signature target=\"{}\" error=\"definition_not_found\">", esc(target));
            return self.finish(&open, "</change_signature>", "", budget, "");
        }
        let n_calls: usize = calls.iter().map(|c| c.1.len()).sum();
        let n_files = writes.len();

        /* 2. write and re-index; tsserver's errors before and after */
        let written: Vec<String> = writes.iter().map(|w| w.0.clone()).collect();
        let errors_before = match exact {
            Exact::Yes if !dry_run => self.ts_errors(&written),
            _ => None,
        };
        let applied = if dry_run {
            "false"
        } else if let Err(e) = self.commit_files(writes) {
            let open = format!("<change_signature target=\"{}\" error=\"write_failed\">", esc(target));
            return self.finish(&open, "</change_signature>", &esc(&e), budget, "");
        } else {
            "true"
        };
        let mut body = exact.note();
        body.push_str(&format!("<calls_updated>\n{}</calls_updated>\n", Self::lines_block(&calls)));
        body.push_str(&self.unsure_block(&t, &unsure));
        if !to_check.is_empty() {
            body.push_str(&format!("<check reason=\"named here but not called: callbacks, re-exports\">\n{}</check>\n", Self::lines_block(&to_check)));
        }
        body.push_str(&self.skipped_block(&skipped));
        if applied == "true" {
            match errors_before.zip(self.ts_errors(&written)) {
                /* the compiler's verdict replaces the syntax re-check: a missed call is an arity error */
                Some((before, after)) => body.push_str(&self.diagnostics_block(before, after, None)),
                None => body.push_str(&self.verify_calls(&t, &written, call_arg.trim())),
            }
            body.push_str(&self.definition_block(&t));
        }
        body.push_str(if applied == "true" {
            "<next>Done: definitions and every call found are written and re-indexed; no Grep needed. If the body must use the new parameter, Edit it using the definition above (exact lines, no context call needed).</next>\n"
        } else {
            "<next>Plan only: call again without dry_run to apply.</next>\n"
        });
        if check && applied == "true" {
            body.push_str(&self.check_written(&written));
        }
        let open = format!(
            "<change_signature target=\"{}\" param=\"{}\" arg=\"{}\" applied=\"{applied}\" files=\"{n_files}\" definitions=\"{defs_updated}\" calls=\"{n_calls}\" impact=\"{}\" why=\"{}\"{}>",
            esc(&t.name),
            esc(add_param.trim()),
            esc(call_arg.trim()),
            impact.level,
            esc(&impact.reason),
            exact.attr()
        );
        self.finish(&open, "</change_signature>", &body, budget, "")
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::Engine;
    use std::fs;

    fn engine(files: &[(&str, &str)]) -> (tempfile::TempDir, Engine) {
        let d = tempfile::tempdir().unwrap();
        for (p, s) in files {
            let path = d.path().join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, s).unwrap();
        }
        let (e, _, _) = Engine::open(d.path(), Some(&d.path().join("i.redb"))).unwrap();
        (d, e)
    }

    fn read(d: &tempfile::TempDir, p: &str) -> String {
        fs::read_to_string(d.path().join(p)).unwrap()
    }

    #[test]
    fn rename_writes_every_reference_and_reindexes() {
        let (d, mut e) = engine(&[
            ("src/hours.ts", "// durationHours: hours in a package\nexport function durationHours(b: B): number {\n  return 1;\n}\n"),
            ("src/a.ts", "import { durationHours } from './hours';\nconst x = durationHours(b) + durationHoursX;\nconst s = 'durationHours';\n"),
        ]);
        let out = e.rename("durationHours", "packageDurationHours", false, false);
        assert!(out.contains("applied=\"true\" files=\"2\" sites=\"4\""), "{out}");
        assert!(out.contains("impact=\"low\""), "{out}");
        assert!(out.contains("<verified>Re-scanned every indexed file: no identifier or comment names `durationHours` any more. 1 line(s) still mention it inside string literals"), "{out}");
        /* string literal untouched, comment renamed */
        assert_eq!(read(&d, "src/a.ts"), "import { packageDurationHours } from './hours';\nconst x = packageDurationHours(b) + durationHoursX;\nconst s = 'durationHours';\n");
        assert!(read(&d, "src/hours.ts").starts_with("// packageDurationHours: hours in a package\nexport function packageDurationHours(b: B)"));
        assert!(e.graph.by_name.contains_key("packageDurationHours"));
        assert!(!e.graph.by_name.contains_key("durationHours"));
    }

    #[test]
    fn rename_leaves_shadowed_locals_and_unrelated_members() {
        let (d, mut e) = engine(&[
            ("a.ts", "export function find(id: string) { return id; }\n"),
            ("b.ts", "import { find } from './a';\nfind('x');\n[1, 2].find(x => x > 1);\nconst o = { find: 1 };\nfunction g() { const find = 1; return find; }\n"),
        ]);
        e.rename("find", "lookup", false, false);
        assert_eq!(
            read(&d, "b.ts"),
            "import { lookup } from './a';\nlookup('x');\n[1, 2].find(x => x > 1);\nconst o = { find: 1 };\nfunction g() { const find = 1; return find; }\n"
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (d, mut e) = engine(&[("a.py", "def go():\n    pass\n\ngo()\n")]);
        let out = e.rename("go", "run", true, false);
        assert!(out.contains("applied=\"false\" files=\"1\" sites=\"2\""), "{out}");
        assert_eq!(read(&d, "a.py"), "def go():\n    pass\n\ngo()\n");
    }

    #[test]
    fn ambiguous_names_get_a_choice_card() {
        let (d, mut e) = engine(&[
            ("db.ts", "export function find(id: string) {}\n"),
            ("cache.ts", "export function find(k: string) {}\n"),
            ("use.ts", "import { find } from './db';\nfind('1'); find('2');\n"),
        ]);
        let out = e.rename("find", "lookup", false, false);
        assert!(out.contains("ambiguous=\"true\""), "{out}");
        assert!(out.contains("A db.ts:1 function find(id: string) — 1 callers, public (recommended) -&gt; rename(target=&quot;db.ts:find&quot;)"), "{out}");
        assert!(out.contains("B cache.ts:1"), "{out}");
        assert_eq!(read(&d, "use.ts"), "import { find } from './db';\nfind('1'); find('2');\n");
        let out = e.rename("db.ts:find", "lookup", false, false);
        assert!(out.contains("applied=\"true\""), "{out}");
        assert!(out.contains("<skipped"), "{out}");
        assert_eq!(read(&d, "use.ts"), "import { lookup } from './db';\nlookup('1'); lookup('2');\n");
        assert_eq!(read(&d, "cache.ts"), "export function find(k: string) {}\n");
    }

    #[test]
    fn same_named_methods_in_one_file_are_addressed_by_line() {
        let (d, mut e) = engine(&[("m.py", "class A:\n    def go(self):\n        self.go()\n\nclass B:\n    def go(self):\n        pass\n")]);
        let out = e.rename("m.py:go", "run", false, false);
        assert!(out.contains("rename(target=&quot;m.py:go@2&quot;)") && out.contains(" in A "), "{out}");
        let out = e.rename("m.py:go@2", "run", false, false);
        assert!(out.contains("applied=\"true\""), "{out}");
        /* a method rename follows `obj.name`, so B.go's definition stays but any `x.go()` would follow */
        assert_eq!(read(&d, "m.py"), "class A:\n    def run(self):\n        self.run()\n\nclass B:\n    def go(self):\n        pass\n");
    }

    #[test]
    fn method_calls_inside_macros_are_renamed() {
        let (d, mut e) = engine(&[(
            "lib.rs",
            "pub struct B;\nimpl B { pub fn add_edits(&mut self) {} }\nfn f(b: &mut B) { let _ = vec![{ b.add_edits(); 1 }]; other.add_edits; }\n",
        )]);
        let out = e.rename("add_edits", "push_edits", false, false);
        /* `other.add_edits` is not called: in Rust that is a field, not this method */
        assert!(out.contains("<still_named"), "{out}");
        assert_eq!(
            read(&d, "lib.rs"),
            "pub struct B;\nimpl B { pub fn push_edits(&mut self) {} }\nfn f(b: &mut B) { let _ = vec![{ b.push_edits(); 1 }]; other.add_edits; }\n"
        );
    }

    #[test]
    fn rust_trait_method_renames_as_one_group() {
        let (d, mut e) = engine(&[("lib.rs", "trait Go { fn go(&self); }\nstruct A;\nimpl Go for A { fn go(&self) {} }\nfn main() { A.go(); }\n")]);
        let out = e.rename("go", "run", false, false);
        assert!(out.contains("applied=\"true\"") && out.contains("definitions=\"2\""), "{out}");
        assert_eq!(read(&d, "lib.rs"), "trait Go { fn run(&self); }\nstruct A;\nimpl Go for A { fn run(&self) {} }\nfn main() { A.run(); }\n");
    }

    #[test]
    fn rename_onto_an_existing_name_asks_first() {
        let (d, mut e) = engine(&[("a.rs", "fn old() {}\nfn new_one() {}\nfn main() { old(); }\n")]);
        let out = e.rename("old", "new_one", false, false);
        assert!(out.contains("applied=\"false\"") && out.contains("A pick another name (recommended)"), "{out}");
        assert!(out.contains("\n<choose>\n"), "card must be markup, not escaped text: {out}");
        assert_eq!(read(&d, "a.rs"), "fn old() {}\nfn new_one() {}\nfn main() { old(); }\n");
        assert!(e.rename("old", "new_one", false, true).contains("applied=\"true\""));
    }

    #[cfg(unix)]
    #[test]
    fn never_writes_through_a_symlink() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim.ts");
        fs::write(&target, "keep me\n").unwrap();
        let d = tempfile::tempdir().unwrap();
        let link = d.path().join("link.ts");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(super::write_atomic(&link, "pwned").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me\n");
    }

    #[test]
    fn change_signature_updates_definition_and_calls_in_three_languages() {
        let (d, mut e) = engine(&[
            ("text.ts", "export function truncate(s: string, max: number): string {\n  return s;\n}\n"),
            ("use.ts", "import { truncate } from './text';\ntruncate(a, 3);\nconst f = truncate;\n[1].truncate(b, 4);\n"),
            ("lib.rs", "pub fn invert(e: Expr) -> Expr { e }\nfn main() { invert(x); let v = vec![invert(y)]; }\n"),
            ("m.py", "def price(a, b, *, k=1):\n    return a\n\nprice(1, 2)\nprice(1, b=2)\n"),
            ("env.d.ts", "interface S { truncate(n: number): void }\n"),
        ]);
        let out = e.change_signature("truncate", "suffix: string", "\"…\"", false);
        assert!(out.contains("applied=\"true\" files=\"2\" definitions=\"1\" calls=\"1\""), "{out}");
        assert!(out.contains("<check") && out.contains("use.ts lines 3"), "{out}");
        assert!(!out.contains("env.d.ts"), "{out}");
        assert!(out.contains("<verified>Re-parsed the written files: all 1 calls of `truncate` now pass the new argument."), "{out}");
        assert!(out.contains("<definition path=\"text.ts\" lines=\"1-3\">\n1\texport function truncate(s: string, max: number, suffix: string): string {"), "{out}");
        assert!(read(&d, "text.ts").starts_with("export function truncate(s: string, max: number, suffix: string): string {"));
        /* a member call on an unrelated object is not this function */
        assert_eq!(read(&d, "use.ts"), "import { truncate } from './text';\ntruncate(a, 3, \"…\");\nconst f = truncate;\n[1].truncate(b, 4);\n");

        e.change_signature("invert", "flip: bool", "true", false);
        assert_eq!(read(&d, "lib.rs"), "pub fn invert(e: Expr, flip: bool) -> Expr { e }\nfn main() { invert(x, true); let v = vec![invert(y, true)]; }\n");

        e.change_signature("price", "discount=0", "0", false);
        assert_eq!(read(&d, "m.py"), "def price(a, b, discount=0, *, k=1):\n    return a\n\nprice(1, 2, 0)\nprice(1, 0, b=2)\n");
    }

    #[test]
    fn new_name_used_locally_asks_first() {
        let (d, mut e) = engine(&[
            ("u.ts", "export function cn(a: string) { return a; }\n"),
            ("c.tsx", "import { cn } from './u';\nexport function C({ classNames }: P) { return cn(classNames.x); }\n"),
        ]);
        let out = e.rename("cn", "classNames", false, false);
        assert!(out.contains("applied=\"false\"") && out.contains("c.tsx:2"), "{out}");
        assert_eq!(read(&d, "c.tsx"), "import { cn } from './u';\nexport function C({ classNames }: P) { return cn(classNames.x); }\n");
    }

    #[test]
    fn namespace_imports_are_followed() {
        let (d, mut e) = engine(&[
            ("m.ts", "export function get(id: string) { return id; }\n"),
            ("t.ts", "import * as mail from './m';\nmail.get('1');\nother.get('2');\n"),
        ]);
        e.rename("get", "fetchOne", false, false);
        assert_eq!(read(&d, "t.ts"), "import * as mail from './m';\nmail.fetchOne('1');\nother.get('2');\n");
        e.change_signature("fetchOne", "x: number", "0", false);
        assert_eq!(read(&d, "t.ts"), "import * as mail from './m';\nmail.fetchOne('1', 0);\nother.get('2');\n");
    }

    #[test]
    fn rest_parameters_are_refused() {
        let (d, mut e) = engine(&[("a.ts", "export function sum(...n: number[]) { return 0; }\nsum(...xs);\n")]);
        let out = e.change_signature("sum", "k: number", "1", false);
        assert!(out.contains("error=\"rest_parameter\""), "{out}");
        assert_eq!(read(&d, "a.ts"), "export function sum(...n: number[]) { return 0; }\nsum(...xs);\n");
    }

    #[test]
    fn rename_reports_then_replaces_text_mentions() {
        let docs = "Use `fetchUser` to load.\nfetchUsers is different.\n";
        let (d, mut e) = engine(&[
            ("api.ts", "export function fetchUser() {}\nfetchUser();\n"),
            ("README.md", docs),
            ("conf/app.yaml", "handler: fetchUser\n"),
            (".env", "H=fetchUser\n"),
        ]);
        /* dry run: plan lists mentions, writes nothing */
        let out = e.rename_with_text("fetchUser", "loadUser", true, false, "replace");
        assert!(out.contains("applied=\"false\"") && out.contains("<text_mentions files=\"2\" lines=\"2\">"), "{out}");
        assert_eq!(read(&d, "README.md"), docs);
        assert_eq!(read(&d, "api.ts"), "export function fetchUser() {}\nfetchUser();\n");
        /* default report: code renamed, docs listed with a hint, untouched */
        let out = e.rename("fetchUser", "loadUser", false, false);
        assert!(out.contains("README.md lines 1") && out.contains("conf/app.yaml lines 1") && out.contains("text=\"replace\""), "{out}");
        assert!(!out.contains(".env"), "{out}");
        assert_eq!(read(&d, "README.md"), docs);
        assert_eq!(read(&d, "api.ts"), "export function loadUser() {}\nloadUser();\n");
        /* follow-up replace with the same arguments: text only */
        let out = e.rename_with_text("fetchUser", "loadUser", false, false, "replace");
        assert!(out.contains("text_only=\"true\"") && out.contains("replaced=\"true\""), "{out}");
        assert_eq!(read(&d, "README.md"), "Use `loadUser` to load.\nfetchUsers is different.\n");
        assert_eq!(read(&d, "conf/app.yaml"), "handler: loadUser\n");
        assert_eq!(read(&d, ".env"), "H=fetchUser\n");
    }

    #[test]
    fn rename_replace_and_skip_in_one_call() {
        let (d, mut e) = engine(&[("a.py", "def go():\n    pass\n"), ("notes.txt", "go go\n"), ("b.py", "def stay():\n    pass\n"), ("n.md", "stay\n")]);
        let out = e.rename_with_text("go", "run", false, false, "replace");
        assert!(out.contains("applied=\"true\"") && out.contains("notes.txt lines 1"), "{out}");
        assert_eq!(read(&d, "notes.txt"), "run run\n");
        let out = e.rename_with_text("stay", "keep", false, false, "skip");
        assert!(!out.contains("text_mentions"), "{out}");
        assert_eq!(read(&d, "n.md"), "stay\n");
        assert!(e.rename_with_text("keep", "x", false, false, "bogus").contains("error=\"invalid_text\""));
    }

    #[test]
    fn typescript_overloads_change_together() {
        let (d, mut e) = engine(&[(
            "a.ts",
            "export function f(a: string): string;\nexport function f(a: number): number;\nexport function f(a: any) { return a; }\nf(1);\n",
        )]);
        let out = e.change_signature("f", "b?: boolean", "true", false);
        assert!(out.contains("definitions=\"3\" calls=\"1\""), "{out}");
        assert_eq!(
            read(&d, "a.ts"),
            "export function f(a: string, b?: boolean): string;\nexport function f(a: number, b?: boolean): number;\nexport function f(a: any, b?: boolean) { return a; }\nf(1, true);\n"
        );
    }
}

#[path = "refactor_sites.rs"]
mod sites;
use sites::{macro_method_calls, Sites};

#[cfg(test)]
#[path = "refactor_typed_tests.rs"]
mod typed_tests;
