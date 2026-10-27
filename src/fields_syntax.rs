//! Syntax helpers for `add_field` (see fields.rs): add a field to a struct / interface / dataclass and to every literal that builds it, in one call.
//!
//! - Rust: the struct's field list and every `Name { .. }` expression (any path, generics or `Self` inside its
//!   impl). Literals with a `..base` spread already get the field from the base and are left alone.
//! - TypeScript: the interface / object type, plus object literals that are clearly typed as it
//!   (`const x: X = {}`, `as X`, `satisfies X`, `X[]` arrays, returns of functions declared to return X).
//!   Other mentions are listed for a human look: without types, nothing else is certain.
//! - Python: a dataclass-style class (annotated fields) and keyword arguments on every `X(...)` call.

use crate::parse::text;
use crate::refactor::valid_ident;
use tree_sitter::Node;

pub(crate) type Edit = (usize, usize, String);

/// What happened to one literal / call.
pub(crate) enum Site {
    Updated(usize),
    /// `..base`, `...obj`, `**kw`: the field comes from elsewhere
    Spread(usize),
    /// already sets the field
    Already,
}

/// `pub retries: u32` / `retries?: number` / `retries: int = 3` -> ("retries", optional).
pub(crate) fn field_name(field: &str) -> Option<(String, bool)> {
    let (head, _) = field.split_once(':')?;
    let head = head.trim();
    let optional = head.ends_with('?');
    let name = head.trim_end_matches('?').split_whitespace().last()?;
    valid_ident(name).then(|| (name.to_string(), optional))
}

pub(crate) fn named_kids(n: Node) -> Vec<Node> {
    let mut cur = n.walk();
    n.named_children(&mut cur).filter(|c| !c.kind().contains("comment")).collect()
}

pub(crate) fn walk<'a>(root: Node<'a>, mut f: impl FnMut(Node<'a>)) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        f(n);
        let mut cur = n.walk();
        stack.extend(n.named_children(&mut cur));
    }
}

/// Leading whitespace of the line holding `byte`.
pub(crate) fn indent_at(src: &str, byte: usize) -> &str {
    let start = src[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &src[start..];
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// End of the line at `pos` when the rest of it is blank or a comment: an item goes after a trailing comment.
pub(crate) fn line_end_if_free(src: &str, pos: usize) -> usize {
    let nl = src[pos..].find('\n').map(|i| pos + i).unwrap_or(src.len());
    let rest = src[pos..nl].trim();
    if rest.is_empty() || rest.starts_with("//") || rest.starts_with('#') {
        nl
    } else {
        pos
    }
}

/// Append `item` to the `{ ... }` list spanning `open..=close`, keeping its layout and trailing-separator style.
pub(crate) fn append_to_braces(src: &str, open: usize, close: usize, elems: &[Node], item: &str, sep: Option<char>) -> Vec<Edit> {
    let multiline = src[open..close].contains('\n');
    let sep_s = sep.map(String::from).unwrap_or_default();
    let Some(last) = elems.last() else {
        return if multiline {
            let indent = format!("{}    ", indent_at(src, close));
            vec![(open + 1, open + 1, format!("\n{indent}{item}{sep_s}"))]
        } else {
            vec![(open + 1, close, format!(" {item} "))]
        };
    };
    let le = last.end_byte();
    let next = src[le..close].find(|c: char| !c.is_whitespace()).map(|i| le + i);
    let trailing = sep.is_some() && next.map(|p| src[p..].starts_with(sep_s.as_str())).unwrap_or(false);
    if multiline {
        let indent = indent_at(src, last.start_byte());
        if trailing {
            let at = line_end_if_free(src, next.unwrap_or(le) + 1);
            vec![(at, at, format!("\n{indent}{item}{sep_s}"))]
        } else {
            let at = line_end_if_free(src, le);
            let mut v = vec![(at, at, format!("\n{indent}{item}"))];
            if sep.is_some() {
                v.push((le, le, sep_s));
            }
            v
        }
    } else if trailing {
        let at = next.unwrap_or(le) + 1;
        vec![(at, at, format!(" {item}{sep_s}"))]
    } else {
        vec![(le, le, format!("{} {item}", sep.unwrap_or(';')))]
    }
}

/// Last path segment of a type or path node, ignoring generics: `a::Cfg::<u8>` -> `Cfg`.
pub(crate) fn last_segment<'a>(n: Node<'a>, src: &'a [u8]) -> &'a str {
    match n.kind() {
        "generic_type" | "generic_type_with_turbofish" => n.child_by_field_name("type").map(|t| last_segment(t, src)).unwrap_or(""),
        "scoped_type_identifier" | "scoped_identifier" | "nested_type_identifier" => {
            n.child_by_field_name("name").map(|t| text(t, src)).unwrap_or("")
        }
        _ => text(n, src),
    }
}

/// The struct an `impl` block is for (`Self` inside it means that struct).
pub(crate) fn impl_type<'a>(n: Node<'a>, src: &'a [u8]) -> Option<&'a str> {
    let mut cur = n.parent();
    while let Some(c) = cur {
        if c.kind() == "impl_item" {
            return c.child_by_field_name("type").map(|t| last_segment(t, src));
        }
        cur = c.parent();
    }
    None
}

/// TypeScript: is this type node exactly `X` (or `ns.X`, `X<T>`)?
pub(crate) fn ts_type_is(ty: Node, src: &[u8], name: &str) -> bool {
    match ty.kind() {
        "type_identifier" | "nested_type_identifier" => last_segment(ty, src) == name,
        "generic_type" => ty.child_by_field_name("name").map(|t| ts_type_is(t, src, name)).unwrap_or(false),
        _ => false,
    }
}

/// TypeScript: is this type `X[]`, `readonly X[]` or `Array<X>`?
pub(crate) fn ts_array_of(ty: Node, src: &[u8], name: &str) -> bool {
    match ty.kind() {
        "array_type" => ty.named_child(0).map(|t| ts_type_is(t, src, name)).unwrap_or(false),
        "readonly_type" => ty.named_child(0).map(|t| ts_array_of(t, src, name)).unwrap_or(false),
        "generic_type" => {
            let base = ty.child_by_field_name("name").map(|t| text(t, src));
            let args = ty.child_by_field_name("type_arguments").map(named_kids).unwrap_or_default();
            matches!(base, Some("Array" | "ReadonlyArray")) && args.len() == 1 && ts_type_is(args[0], src, name)
        }
        _ => false,
    }
}

pub(crate) fn unparen(mut n: Node) -> Node {
    while n.kind() == "parenthesized_expression" {
        match n.named_child(0) {
            Some(c) => n = c,
            None => break,
        }
    }
    n
}

/// TypeScript object literals typed as `name`, with the type node that says so.
pub(crate) fn ts_literals<'a>(root: Node<'a>, src: &'a [u8], name: &str) -> Vec<(Node<'a>, Node<'a>)> {
    let mut out = Vec::new();
    let typed = |ty: Node<'a>, val: Node<'a>, out: &mut Vec<(Node<'a>, Node<'a>)>| {
        let ty = if ty.kind() == "type_annotation" { ty.named_child(0).unwrap_or(ty) } else { ty };
        let val = unparen(val);
        if val.kind() == "object" && ts_type_is(ty, src, name) {
            out.push((val, ty));
        } else if val.kind() == "array" && ts_array_of(ty, src, name) {
            out.extend(named_kids(val).into_iter().map(unparen).filter(|e| e.kind() == "object").map(|e| (e, ty)));
        }
    };
    walk(root, |n| match n.kind() {
        "variable_declarator" => {
            if let (Some(ty), Some(v)) = (n.child_by_field_name("type"), n.child_by_field_name("value")) {
                typed(ty, v, &mut out);
            }
        }
        "as_expression" | "satisfies_expression" => {
            if let (Some(v), Some(ty)) = (n.named_child(0), n.named_child(1)) {
                typed(ty, v, &mut out);
            }
        }
        "arrow_function" => {
            if let (Some(ty), Some(b)) = (n.child_by_field_name("return_type"), n.child_by_field_name("body")) {
                if b.kind() != "statement_block" {
                    typed(ty, b, &mut out);
                }
            }
        }
        "return_statement" => {
            let mut cur = n.parent();
            while let Some(c) = cur {
                if matches!(c.kind(), "function_declaration" | "function_expression" | "arrow_function" | "method_definition" | "function") {
                    if let (Some(ty), Some(v)) = (c.child_by_field_name("return_type"), n.named_child(0)) {
                        typed(ty, v, &mut out);
                    }
                    break;
                }
                cur = c.parent();
            }
        }
        _ => {}
    });
    out
}
