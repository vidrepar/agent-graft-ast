//! `add_field`: add a field to a struct / interface / dataclass and to every literal that builds it, in one call.
//!
//! - Rust: the struct's field list and every `Name { .. }` expression (any path, generics or `Self` inside its
//!   impl). Literals with a `..base` spread already get the field from the base and are left alone.
//! - TypeScript: the interface / object type, plus object literals that are clearly typed as it
//!   (`const x: X = {}`, `as X`, `satisfies X`, `X[]` arrays, returns of functions declared to return X).
//!   Other mentions are listed for a human look: without types, nothing else is certain.
//! - Python: a dataclass-style class (annotated fields) and keyword arguments on every `X(...)` call.

use crate::context::same_family;
use crate::engine::{esc, Engine};
use crate::lang::Lang;
use crate::parse::{parse_tree, text, SymKind};
use crate::refactor::{apply_edits, line_of, Target};
use std::collections::HashSet;
use tree_sitter::Node;
use crate::fields_syntax::*;

impl Engine {
    /// Files to edit for a type: every file naming it, unless an unrelated type shares the name; then only the
    /// definition's file, its callers' and importers' files. `impl` blocks of the type do not count as rivals.
    fn field_scope(&self, t: &Target) -> (Vec<u32>, Vec<(u32, Vec<u32>)>) {
        let sites: Vec<(u32, Vec<u32>)> =
            self.sites(&t.name, t.lang).into_iter().filter(|(f, _)| !self.files[*f as usize].path.ends_with(".d.ts")).collect();
        let rivals = self.graph.by_name.get(&t.name).into_iter().flatten().any(|d| {
            let fr = &self.files[d.0 as usize];
            !t.defs.contains(d) && same_family(fr.lang, t.lang) && fr.ex.symbols[d.1 as usize].kind != SymKind::Impl && !fr.path.ends_with(".d.ts")
        });
        let (scope, skipped) = if rivals {
            let mut linked: HashSet<u32> = HashSet::new();
            for &(f, s) in &t.defs {
                linked.insert(f);
                linked.extend(self.graph.callers(f, s).iter().map(|c| c.file));
                linked.extend(self.graph.importers(f));
            }
            sites.into_iter().partition(|(file, _)| linked.contains(file))
        } else {
            (sites, Vec::new())
        };
        let mut files: Vec<u32> = scope.into_iter().map(|s| s.0).collect();
        if !files.contains(&t.defs[0].0) {
            files.insert(0, t.defs[0].0);
        }
        (files, skipped)
    }

    /// `add_field`: add `field` to a struct / interface / dataclass and `name: value` to every literal building it.
    pub fn add_field(&mut self, target: &str, field: &str, value: &str, dry_run: bool) -> String {
        let budget = 4000;
        let field = field.trim().trim_end_matches([',', ';']).trim();
        let value = value.trim();
        let fail = |e: &mut Engine, code: &str, msg: &str| {
            let open = format!("<add_field target=\"{}\" error=\"{code}\">", esc(target));
            e.finish(&open, "</add_field>", &esc(msg), budget, "")
        };
        let Some((fname, optional)) = field_name(field).filter(|_| !field.contains('\n') && !value.contains('\n')) else {
            return fail(self, "invalid_field", "field must be one line `name: type` (e.g. `pub retries: u32`, `retries?: number`, `retries: int`)\n");
        };
        let is_type = |s: &crate::parse::Symbol| matches!(s.kind, SymKind::Struct | SymKind::Interface | SymKind::TypeAlias | SymKind::Class);
        let t = match self.resolve_target_where(target, "add_field", budget, is_type) {
            Ok(t) => t,
            Err(reply) => return reply,
        };
        if !t.lang.full_refactor() {
            return fail(self, "unsupported_language", &format!("add_field does not edit {} code yet\n", t.lang.name()));
        }
        let (fi, si) = t.defs[0];
        let sym = self.files[fi as usize].ex.symbols[si as usize].clone();
        if !is_type(&sym) {
            return fail(self, "not_a_struct", &format!("{} is a {}; add_field takes a struct, interface, object type or class\n", t.name, sym.kind.as_str()));
        }
        let ts = t.lang.is_js_family();
        let wants_literals = !(ts && optional);
        if wants_literals && value.is_empty() {
            return fail(self, "missing_value", "value is required: it is written into every literal (only optional TypeScript fields `x?: T` need none)\n");
        }
        let item = if t.lang == Lang::Python { format!("{fname}={value}") } else { format!("{fname}: {value}") };
        let impact = self.impact(fi, si);
        let (files, skipped) = self.field_scope(&t);

        let mut writes = Vec::new();
        let mut decl_at = String::new();
        let mut updated: Vec<(String, Vec<u32>)> = Vec::new();
        let mut spreads: Vec<(String, Vec<u32>)> = Vec::new();
        let mut to_check: Vec<(String, Vec<u32>)> = Vec::new();
        for file in files {
            let path = self.files[file as usize].path.clone();
            let lang = self.files[file as usize].lang;
            let src = match self.checked_source(file) {
                Ok(s) => s,
                Err(r) => return self.refusal(&path, r),
            };
            let Some(tree) = parse_tree(lang, src.as_bytes()) else { continue };
            let (root, b) = (tree.root_node(), src.as_bytes());
            let mut edits: Vec<Edit> = Vec::new();

            /* 1. the declaration */
            let mut decl: Option<Node> = None;
            if file == fi {
                walk(root, |n| {
                    let kinds: &[&str] = match t.lang {
                        Lang::Rust => &["struct_item", "union_item"],
                        Lang::Python => &["class_definition"],
                        _ => &["interface_declaration", "type_alias_declaration", "class_declaration"],
                    };
                    let line = n.start_position().row as u32 + 1;
                    if decl.is_none()
                        && kinds.contains(&n.kind())
                        && n.child_by_field_name("name").map(|x| text(x, b)) == Some(t.name.as_str())
                        && line >= sym.start_line
                        && line <= sym.end_line
                    {
                        decl = Some(n);
                    }
                });
                let Some(d) = decl else {
                    return fail(self, "definition_not_found", "");
                };
                match self.declare_field(&src, d, field, &fname, value) {
                    Ok(e) => edits.extend(e),
                    Err((code, msg)) => return fail(self, code, &msg),
                }
                decl_at = format!("{path}:{}", d.start_position().row + 1);
            }
            if !wants_literals {
                writes.push((path, apply_edits(&src, edits)));
                continue;
            }

            /* 2. literals / constructor calls */
            let mut sites: Vec<Site> = Vec::new();
            let mut used_types: HashSet<usize> = HashSet::new();
            match t.lang {
                Lang::Rust => walk(root, |n| {
                    if n.kind() != "struct_expression" {
                        return;
                    }
                    let (Some(nm), Some(body)) = (n.child_by_field_name("name"), n.child_by_field_name("body")) else { return };
                    let seg = last_segment(nm, b);
                    if seg != t.name && !(seg == "Self" && impl_type(n, b) == Some(t.name.as_str())) {
                        return;
                    }
                    let elems = named_kids(body);
                    sites.push(if elems.iter().any(|e| e.kind() == "base_field_initializer") {
                        Site::Spread(body.start_byte())
                    } else if elems.iter().any(|e| {
                        let k = e.child_by_field_name("field").or_else(|| e.named_child(0));
                        k.map(|k| text(k, b) == fname).unwrap_or(false)
                    }) {
                        Site::Already
                    } else {
                        edits.extend(append_to_braces(&src, body.start_byte(), body.end_byte() - 1, &elems, &item, Some(',')));
                        Site::Updated(body.start_byte())
                    });
                }),
                Lang::Python => walk(root, |n| {
                    if n.kind() != "call" {
                        return;
                    }
                    let (Some(f), Some(args)) = (n.child_by_field_name("function"), n.child_by_field_name("arguments")) else { return };
                    let callee = match f.kind() {
                        "identifier" => Some(f),
                        "attribute" if f.child_by_field_name("object").map(|o| o.kind() == "identifier").unwrap_or(false) => {
                            f.child_by_field_name("attribute")
                        }
                        _ => None,
                    };
                    if args.kind() != "argument_list" || callee.map(|c| text(c, b)) != Some(t.name.as_str()) {
                        return;
                    }
                    let elems = named_kids(args);
                    sites.push(if elems.iter().any(|e| e.kind() == "dictionary_splat") {
                        Site::Spread(args.start_byte())
                    } else if elems.iter().any(|e| e.kind() == "keyword_argument" && e.child_by_field_name("name").map(|k| text(k, b)) == Some(fname.as_str())) {
                        Site::Already
                    } else {
                        let at = elems.last().map(|e| e.end_byte());
                        edits.push(match at {
                            Some(e) => (e, e, format!(", {item}")),
                            None => (args.start_byte() + 1, args.start_byte() + 1, item.clone()),
                        });
                        Site::Updated(args.start_byte())
                    });
                }),
                _ => {
                    for (obj, ty) in ts_literals(root, b, &t.name) {
                        used_types.insert(ty.start_byte());
                        let elems = named_kids(obj);
                        sites.push(if elems.iter().any(|e| e.kind() == "spread_element") {
                            Site::Spread(obj.start_byte())
                        } else if elems.iter().any(|e| {
                            let k = if e.kind() == "shorthand_property_identifier" { Some(*e) } else { e.child_by_field_name("key") };
                            k.map(|k| text(k, b).trim_matches(['"', '\'']) == fname).unwrap_or(false)
                        }) {
                            Site::Already
                        } else {
                            edits.extend(append_to_braces(&src, obj.start_byte(), obj.end_byte() - 1, &elems, &item, Some(',')));
                            Site::Updated(obj.start_byte())
                        });
                    }
                    /* every other mention of the type: parameters, generics, factories */
                    let decl_range = decl.map(|d| (d.start_byte(), d.end_byte()));
                    let mut other: Vec<u32> = Vec::new();
                    walk(root, |n| {
                        if n.kind() != "type_identifier" || text(n, b) != t.name || used_types.contains(&n.start_byte()) {
                            return;
                        }
                        if decl_range.map(|(a, z)| n.start_byte() >= a && n.end_byte() <= z).unwrap_or(false) {
                            return;
                        }
                        let mut cur = n.parent();
                        while let Some(c) = cur {
                            if matches!(c.kind(), "import_statement" | "export_statement") && c.child_by_field_name("declaration").is_none() {
                                return;
                            }
                            cur = c.parent();
                        }
                        other.push(line_of(&src, n.start_byte()));
                    });
                    other.sort_unstable();
                    other.dedup();
                    if !other.is_empty() {
                        to_check.push((path.clone(), other));
                    }
                }
            }
            let lines = |want_spread: bool| -> Vec<u32> {
                sites
                    .iter()
                    .filter_map(|s| match s {
                        Site::Updated(p) if !want_spread => Some(line_of(&src, *p)),
                        Site::Spread(p) if want_spread => Some(line_of(&src, *p)),
                        _ => None,
                    })
                    .collect()
            };
            let (up, sp) = (lines(false), lines(true));
            if !up.is_empty() {
                updated.push((path.clone(), up));
            }
            if !sp.is_empty() {
                spreads.push((path.clone(), sp));
            }
            if !edits.is_empty() {
                writes.push((path, apply_edits(&src, edits)));
            }
        }
        let n_literals: usize = updated.iter().map(|u| u.1.len()).sum();
        let n_files = writes.len();

        /* 3. write and re-index, unless asked for a plan only */
        let applied = if dry_run {
            "false"
        } else if let Err(e) = self.commit_files(writes) {
            return fail(self, "write_failed", &e);
        } else {
            "true"
        };
        let mut body = format!("<declaration>{}</declaration>\n", esc(&decl_at));
        body.push_str(&format!("<literals_updated>\n{}</literals_updated>\n", Self::lines_block(&updated)));
        if !spreads.is_empty() {
            body.push_str(&format!("<unchanged reason=\"a spread (..base, ...obj, **kw) supplies the field\">\n{}</unchanged>\n", Self::lines_block(&spreads)));
        }
        if !to_check.is_empty() {
            body.push_str(&format!(
                "<check reason=\"other uses of the type: values built elsewhere (factories, untyped literals) may need the field\">\n{}</check>\n",
                Self::lines_block(&to_check)
            ));
        }
        body.push_str(&self.skipped_block(&skipped));
        let caveat = match t.lang {
            Lang::Rust => " Literals inside macros (vec![..]) are not seen: `cargo check` names any left.",
            Lang::Python => " Calls through aliases or factories are not seen.",
            _ if optional => "",
            _ => " Untyped literals are not seen: the type checker names any left.",
        };
        body.push_str(&if applied == "true" {
            format!("<next>Done: declaration and literals are written and re-indexed.{caveat}</next>\n")
        } else {
            "<next>Plan only: call again without dry_run to apply.</next>\n".to_string()
        });
        let open = format!(
            "<add_field target=\"{}\" field=\"{}\" value=\"{}\" applied=\"{applied}\" files=\"{n_files}\" literals=\"{n_literals}\" impact=\"{}\" why=\"{}\">",
            esc(&t.name),
            esc(field),
            esc(value),
            impact.level,
            esc(&impact.reason)
        );
        self.finish(&open, "</add_field>", &body, budget, "")
    }

    /// Edits that add the field to the declaration node, or (error code, message).
    fn declare_field(&self, src: &str, d: Node, field: &str, fname: &str, value: &str) -> Result<Vec<Edit>, (&'static str, String)> {
        let b = src.as_bytes();
        let exists = |names: Vec<&str>| names.contains(&fname);
        match d.kind() {
            "struct_item" => {
                let body = d.child_by_field_name("body").filter(|x| x.kind() == "field_declaration_list");
                let Some(body) = body else {
                    return Err(("not_a_named_struct", "tuple and unit structs have no named fields; edit by hand\n".into()));
                };
                let elems: Vec<Node> = named_kids(body).into_iter().filter(|e| e.kind() == "field_declaration").collect();
                if exists(elems.iter().filter_map(|e| e.child_by_field_name("name").map(|n| text(n, b))).collect()) {
                    return Err(("field_exists", format!("{fname} is already a field\n")));
                }
                Ok(append_to_braces(src, body.start_byte(), body.end_byte() - 1, &elems, field, Some(',')))
            }
            "union_item" => Err(("not_a_struct", "unions are not supported; edit by hand\n".into())),
            "class_definition" => {
                let body = d.child_by_field_name("body").ok_or(("definition_not_found", String::new()))?;
                let fields: Vec<Node> = named_kids(body)
                    .into_iter()
                    .filter(|s| s.kind() == "expression_statement")
                    .filter_map(|s| s.named_child(0))
                    .filter(|a| a.kind() == "assignment" && a.child_by_field_name("type").is_some())
                    .collect();
                if exists(fields.iter().filter_map(|a| a.child_by_field_name("left").map(|n| text(n, b))).collect()) {
                    return Err(("field_exists", format!("{fname} is already a field\n")));
                }
                let Some(last) = fields.last() else {
                    return Err(("no_annotated_fields", "the class declares no annotated fields (`x: int`); edit by hand\n".into()));
                };
                let decl = if field.contains('=') { field.to_string() } else { format!("{field} = {value}") };
                let stmt = last.parent().unwrap_or(*last);
                Ok(vec![(stmt.end_byte(), stmt.end_byte(), format!("\n{}{decl}", indent_at(src, stmt.start_byte())))])
            }
            _ => {
                let body = match d.kind() {
                    "interface_declaration" => d.child_by_field_name("body"),
                    "type_alias_declaration" => d.child_by_field_name("value").filter(|v| v.kind() == "object_type"),
                    _ => None,
                };
                let Some(body) = body else {
                    return Err(("not_an_object_type", "only interfaces and `type X = { ... }` object types are supported\n".into()));
                };
                let elems = named_kids(body);
                if exists(elems.iter().filter_map(|e| e.child_by_field_name("name").map(|n| text(n, b))).collect()) {
                    return Err(("field_exists", format!("{fname} is already a field\n")));
                }
                /* separator style: what follows the first member (`;`, `,`, or a bare line break) */
                let sep = elems.first().and_then(|e| src[e.end_byte()..].chars().find(|c| *c != ' ' && *c != '\t')).and_then(|c| match c {
                    ';' | ',' => Some(c),
                    '\n' | '\r' => None,
                    _ => Some(';'),
                });
                let sep = if elems.is_empty() { Some(';') } else { sep };
                Ok(append_to_braces(src, body.start_byte(), body.end_byte() - 1, &elems, field, sep))
            }
        }
    }
}

#[cfg(test)]
#[path = "fields_tests.rs"]
mod tests;
