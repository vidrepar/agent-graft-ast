//! `move_symbol`: move a top-level declaration to another file and fix the imports, in one call.
//!
//! - TypeScript/JavaScript and Python here; Rust items go to move_rust.rs.
//! - The moved text keeps its leading comments, decorators and `export`. Imports it needs are carried over
//!   with paths re-relativized; top-level names it uses that stay behind are exported from the source and
//!   imported by the destination.
//! - Every importer's `import { sym }` / `export { sym } from` / `from m import sym` is split to the new path.
//!   What cannot be rewritten safely (namespace access, `export *`, `import m; m.sym`) is listed in <check>.

use crate::context::same_family;
use crate::engine::{esc, Engine};
use crate::graph::normalize;
use crate::lang::Lang;
use crate::parse::{parse_tree, SymKind};
use crate::refactor::{apply_edits, line_of};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::Node;
use crate::move_symbol_paths::*;
use crate::move_symbol_imports::*;

impl Engine {
    /// `move_symbol`: move a top-level function/class/const/type to another file and fix imports.
    pub fn move_symbol(&mut self, target: &str, to_file: &str, dry_run: bool) -> String {
        let budget = 4000;
        let t = match self.resolve_target_where(target, "move_symbol", budget, |s| s.kind != SymKind::Impl) {
            Ok(t) => t,
            Err(reply) => return reply,
        };
        let (fi, si) = t.defs[0];
        if t.defs.len() > 1 {
            return self.move_err(target, to_file, "overloaded", "several definitions (overloads) share this name; move them by hand.");
        }
        let sym = self.files[fi as usize].ex.symbols[si as usize].clone();
        if sym.parent.is_some() || t.method || sym.kind == SymKind::Method {
            return self.move_err(target, to_file, "not_top_level", "only top-level functions, classes, consts and types can be moved.");
        }
        let (dest, dest_exists) = match self.dest_rel(to_file) {
            Ok(d) => d,
            Err((code, msg)) => return self.move_err(target, to_file, code, &msg),
        };
        let Some(dest_lang) = Lang::from_path(Path::new(&dest)) else {
            return self.move_err(target, to_file, "unsupported_destination", "to_file must be a .rs/.ts/.tsx/.js/.jsx/.py file.");
        };
        if t.lang == Lang::Rust && dest_lang == Lang::Rust && dest != self.files[fi as usize].path {
            return self.move_rust(fi, si, &t.name, &dest, dest_exists, dry_run, target, to_file);
        }
        if !same_family(dest_lang, t.lang) || dest_lang == Lang::Rust {
            return self.move_err(target, to_file, "language_mismatch", "to_file is a different language family than the symbol.");
        }
        let src_rel = self.files[fi as usize].path.clone();
        if dest == src_rel {
            return self.move_err(target, to_file, "same_file", "to_file is the file that already defines the symbol.");
        }
        let lang = t.lang;
        let name = t.name.clone();
        let impact = self.impact(fi, si);

        /* 1. the statement to move */
        let src = match self.checked_source(fi) {
            Ok(s) => s,
            Err(r) => return self.refusal(&src_rel, r),
        };
        let Some(tree) = parse_tree(lang, src.as_bytes()) else {
            return self.move_err(target, to_file, "parse_failed", "could not parse the source file.");
        };
        let root = tree.root_node();
        let sb = src.as_bytes();
        let Some(stmt) = top_level(root, sb, &name, sym.start_line) else {
            return self.move_err(target, to_file, "not_top_level", "only top-level declarations can be moved.");
        };
        if decl_names(stmt, sb).len() > 1 {
            return self.move_err(target, to_file, "multiple_declarators", "this statement declares several names; split it first.");
        }
        let mut c0 = stmt.walk();
        if stmt.kind() == "export_statement" && stmt.children(&mut c0).any(|c| c.kind() == "default") {
            return self.move_err(target, to_file, "default_export", "default exports are not moved; move it by hand.");
        }
        let (mstart, mend) = moved_range(stmt, &src);
        let decl_offset = stmt.start_byte() - mstart;
        let exported_in_src = lang == Lang::Python || stmt.kind() == "export_statement";

        /* 2. the destination: a name it already defines or imports from elsewhere is a decision */
        let mut dest_src = String::new();
        let mut dest_fi: Option<u32> = None;
        if dest_exists {
            match self.ensure_fresh(&dest) {
                Ok(Some(d)) => {
                    dest_fi = Some(d);
                    dest_src = match self.checked_source(d) {
                        Ok(s) => s,
                        Err(r) => return self.refusal(&dest, r),
                    };
                }
                Ok(None) => return self.move_err(target, to_file, "unsupported_destination", "to_file is not a source file."),
                Err(r) => return self.refusal(&dest, r),
            }
        }
        let dest_tree = parse_tree(dest_lang, dest_src.as_bytes());
        let mut dest_top: HashMap<String, u32> = HashMap::new();
        let mut dest_imports: Vec<ImportStmt> = Vec::new();
        if let Some(dt) = &dest_tree {
            let mut cur = dt.root_node().walk();
            for c in dt.root_node().named_children(&mut cur) {
                for n in decl_names(c, dest_src.as_bytes()) {
                    dest_top.insert(n, c.start_position().row as u32 + 1);
                }
            }
            dest_imports = parse_imports(dest_lang, dt.root_node(), dest_src.as_bytes());
        }
        let from_src = |e: &Engine, file: Option<u32>, spec: &str| file.and_then(|f| e.graph.resolve_spec(&e.files, f, spec)) == Some(fi);
        let mut clash = dest_top.get(&name).copied();
        for st in &dest_imports {
            if st.bindings.iter().any(|b| b.local == name && b.kind != BKind::Namespace) && !from_src(self, dest_fi, &st.spec) {
                clash = clash.or(Some(st.line));
            }
        }
        if let Some(line) = clash {
            let open = format!("<move_symbol target=\"{}\" to=\"{}\" applied=\"false\">", esc(&name), esc(&dest));
            let body = format!(
                "<choose>\n{}</choose>\n",
                esc(&format!("A pick another to_file (recommended): `{name}` is already defined at {dest}:{line}\nB rename one of them first -> rename(target=\"{src_rel}:{name}\", new_name), then move_symbol again\n"))
            );
            return self.finish(&open, "</move_symbol>", &body, budget, "");
        }
        let dest_has: HashSet<String> =
            dest_top.keys().cloned().chain(dest_imports.iter().flat_map(|s| s.bindings.iter().map(|b| b.local.clone()))).collect();

        /* 3. what the moved code needs: the source's imports and top-level names */
        let used: HashSet<String> = idents(stmt, sb).into_iter().map(|x| x.0).filter(|n| *n != name).collect();
        let src_imports = parse_imports(lang, root, sb);
        let js_style = src_imports.iter().filter(|s| s.spec.starts_with('.')).find_map(|s| spec_ext(&s.spec)).map(String::from);
        let quote = src_imports.iter().find(|s| !s.spec.is_empty()).map(|s| s.quote).unwrap_or('\'');
        let semi = src_imports.first().map(|s| s.semi).unwrap_or(true);
        let mut checks: Vec<String> = Vec::new();
        let mut src_edits = FileEdits::default();
        let mut dest_edits = FileEdits::default();
        let mut carried: Vec<(usize, String)> = Vec::new();
        for (k, st) in src_imports.iter().enumerate() {
            let keep: Vec<&Binding> = st.bindings.iter().filter(|b| used.contains(&b.local) && !dest_has.contains(&b.local)).collect();
            if keep.is_empty() {
                continue;
            }
            let spec = if lang == Lang::Python {
                if st.spec.starts_with('.') && dir_of(&src_rel) != dir_of(&dest) {
                    match self.graph.resolve_spec(&self.files, fi, &st.spec) {
                        Some(tf) => {
                            let tpath = self.files[tf as usize].path.clone();
                            let from: Vec<&str> = dir_of(&dest).split('/').filter(|s| !s.is_empty()).collect();
                            let to = py_parts(&tpath);
                            let common = from.iter().zip(to.iter()).take_while(|(a, b)| **a == b.as_str()).count();
                            format!("{}{}", ".".repeat(from.len() - common + 1), to[common..].join("."))
                        }
                        None => {
                            checks.push(format!("{dest}:1 add `{}` (relative import could not be re-based)", render(lang, st, &st.spec, &keep)));
                            continue;
                        }
                    }
                } else {
                    st.spec.clone()
                }
            } else if st.spec.starts_with('.') {
                rel_spec(dir_of(&dest), &normalize(&format!("{}/{}", dir_of(&src_rel), st.spec)))
            } else {
                st.spec.clone()
            };
            dest_edits.new_imports.push(render(lang, st, &spec, &keep));
            carried.extend(keep.iter().map(|b| (k, b.local.clone())));
        }
        let mut src_decls: HashMap<String, Node> = HashMap::new();
        {
            let mut cur = root.walk();
            for c in root.named_children(&mut cur) {
                if c.id() != stmt.id() {
                    for n in decl_names(c, sb) {
                        src_decls.insert(n, c);
                    }
                }
            }
        }
        let mut needs: Vec<String> = used.iter().filter(|u| src_decls.contains_key(*u) && !dest_has.contains(*u)).cloned().collect();
        needs.sort();
        let mut exported_now: Vec<String> = Vec::new();
        for n in &needs {
            let d = src_decls[n];
            if lang.is_js_family() && d.kind() != "export_statement" {
                src_edits.other.push((d.start_byte(), d.start_byte(), "export ".into()));
                exported_now.push(n.clone());
            }
        }
        if !needs.is_empty() {
            if lang == Lang::Python {
                match self.py_module_for(&dest, &src_rel) {
                    Some(m) => dest_edits.new_imports.push(format!("from {m} import {}", needs.join(", "))),
                    None => checks.push(format!("{dest}:1 import {} from {src_rel} (module path unclear)", needs.join(", "))),
                }
            } else {
                let spec = rel_spec(dir_of(&dest), &styled(&src_rel, js_style.as_deref()));
                let s = if semi { ";" } else { "" };
                dest_edits.new_imports.push(format!("import {{ {} }} from {quote}{spec}{quote}{s}", needs.join(", ")));
            }
        }

        /* 4. the source: cut the code, drop imports only it used, import the symbol back if still used */
        let mut mend2 = mend;
        let before_blank = mstart == 0 || src[..mstart].ends_with("\n\n");
        if before_blank {
            let rest = &src[mend..];
            if let Some(i) = rest.find('\n') {
                if rest[..i].trim().is_empty() {
                    mend2 = mend + i + 1;
                }
            }
        }
        src_edits.other.push((mstart, mend2, String::new()));
        let in_import = |p: usize| src_imports.iter().any(|s| p >= s.start && p < s.end);
        let remaining: HashSet<String> =
            idents(root, sb).into_iter().filter(|(_, p)| (*p < mstart || *p >= mend) && !in_import(*p)).map(|x| x.0).collect();
        for (k, st) in src_imports.iter().enumerate() {
            let drop: Vec<&String> = carried.iter().filter(|(i, l)| *i == k && !remaining.contains(l)).map(|x| &x.1).collect();
            if drop.is_empty() {
                continue;
            }
            let keep: Vec<&Binding> = st.bindings.iter().filter(|b| !drop.contains(&&b.local)).collect();
            let r = if keep.is_empty() { String::new() } else { render(lang, st, &st.spec, &keep) };
            src_edits.stmts.insert(st.start, (st.end, r));
        }
        let src_uses = remaining.contains(&name);
        if src_uses {
            if lang == Lang::Python {
                match self.py_module_for(&src_rel, &dest) {
                    Some(m) => src_edits.new_imports.push(format!("from {m} import {name}")),
                    None => checks.push(format!("{src_rel}:1 import {name} from {dest} (module path unclear)")),
                }
            } else {
                let spec = rel_spec(dir_of(&src_rel), &styled(&dest, js_style.as_deref()));
                let s = if semi { ";" } else { "" };
                src_edits.new_imports.push(format!("import {{ {name} }} from {quote}{spec}{quote}{s}"));
            }
            if !needs.is_empty() {
                checks.push(format!("{src_rel}:1 import cycle: {src_rel} imports {name} from {dest}, which imports {} from {src_rel}", needs.join(", ")));
            }
        }

        /* 5. the destination drops its own import of the symbol from the source */
        for st in &dest_imports {
            if !from_src(self, dest_fi, &st.spec) {
                continue;
            }
            if st.bindings.iter().any(|b| b.kind == BKind::Named && b.imported == name) {
                let keep: Vec<&Binding> = st.bindings.iter().filter(|b| !(b.kind == BKind::Named && b.imported == name)).collect();
                let r = if keep.is_empty() { String::new() } else { render(dest_lang, st, &st.spec, &keep) };
                dest_edits.stmts.insert(st.start, (st.end, r));
            }
        }

        /* 6. every importer: split the symbol out to the new path */
        let mut writes: Vec<(String, String)> = Vec::new();
        let mut changed: Vec<(String, Vec<u32>)> = Vec::new();
        for imp in self.graph.importers(fi) {
            if imp == fi || Some(imp) == dest_fi || !same_family(self.files[imp as usize].lang, lang) {
                continue;
            }
            let ipath = self.files[imp as usize].path.clone();
            let ilang = self.files[imp as usize].lang;
            let isrc = match self.checked_source(imp) {
                Ok(s) => s,
                Err(r) => return self.refusal(&ipath, r),
            };
            let Some(itree) = parse_tree(ilang, isrc.as_bytes()) else { continue };
            let mut fe = FileEdits::default();
            for st in parse_imports(ilang, itree.root_node(), isrc.as_bytes()) {
                if self.graph.resolve_spec(&self.files, imp, &st.spec) != Some(fi) {
                    continue;
                }
                if st.star {
                    checks.push(format!("{ipath}:{} `*` re-export no longer includes {name}; it now lives in {dest}", st.line));
                }
                for b in st.bindings.iter().filter(|b| matches!(b.kind, BKind::Namespace | BKind::Module)) {
                    let base = if b.kind == BKind::Module && !b.text.contains(" as ") { &b.text } else { &b.local };
                    let pat = format!("{base}.{name}");
                    for (i, l) in isrc.lines().enumerate() {
                        if l.contains(&pat) {
                            checks.push(format!("{ipath}:{} `{pat}` now lives in {dest}", i + 1));
                        }
                    }
                }
                let moved: Vec<&Binding> = st.bindings.iter().filter(|b| b.kind == BKind::Named && b.imported == name).collect();
                if moved.is_empty() {
                    continue;
                }
                let spec = if ilang == Lang::Python {
                    match py_new_module(&st.spec, &src_rel, &dest, &ipath) {
                        Some(m) => m,
                        None => {
                            checks.push(format!("{ipath}:{} import {name} from {dest} (module path unclear)", st.line));
                            continue;
                        }
                    }
                } else {
                    new_js_spec(&st.spec, &ipath, &src_rel, &dest)
                };
                let keep: Vec<&Binding> = st.bindings.iter().filter(|b| !(b.kind == BKind::Named && b.imported == name)).collect();
                let new_stmt = render(ilang, &st, &spec, &moved);
                let r = if keep.is_empty() { new_stmt } else { format!("{}\n{new_stmt}", render(ilang, &st, &st.spec, &keep)) };
                fe.stmts.insert(st.start, (st.end, r));
            }
            if fe.is_empty() {
                continue;
            }
            changed.push((ipath.clone(), fe.lines(&isrc)));
            let edits = fe.finalize(ilang, itree.root_node(), &isrc);
            writes.push((ipath, apply_edits(&isrc, edits)));
        }

        /* 7. assemble the source and the destination */
        changed.insert(0, (src_rel.clone(), src_edits.lines(&src)));
        let new_src = apply_edits(&src, src_edits.finalize(lang, root, &src)).trim_start_matches('\n').to_string();
        writes.insert(0, (src_rel.clone(), new_src));
        let mut moved_text = src[mstart..mend].to_string();
        if !exported_in_src && src_uses {
            moved_text.insert_str(decl_offset, "export ");
        }
        if !moved_text.ends_with('\n') {
            moved_text.push('\n');
        }
        let dest_lines = dest_edits.lines(&dest_src);
        let mut new_dest = match &dest_tree {
            Some(dt) => apply_edits(&dest_src, dest_edits.finalize(dest_lang, dt.root_node(), &dest_src)),
            None => dest_edits.new_imports.join("\n"),
        };
        if !new_dest.trim().is_empty() {
            new_dest.truncate(new_dest.trim_end().len());
            new_dest.push_str(if dest_lang == Lang::Python { "\n\n\n" } else { "\n\n" });
        } else {
            new_dest.clear();
        }
        let moved_line = line_of(&new_dest, new_dest.len());
        new_dest.push_str(&moved_text);
        let mut dl = dest_lines;
        dl.push(moved_line);
        changed.insert(1, (dest.clone(), dl));
        let n_files = writes.len() + 1;

        /* 8. write (destination first) and re-index, unless asked for a plan only */
        let applied = if dry_run {
            "false"
        } else {
            let res = if dest_exists {
                self.commit_files(vec![(dest.clone(), new_dest)])
            } else {
                let abs = self.root.join(&dest);
                let created = abs
                    .parent()
                    .map(std::fs::create_dir_all)
                    .unwrap_or(Ok(()))
                    .and_then(|_| std::fs::OpenOptions::new().write(true).create_new(true).open(&abs))
                    .and_then(|mut f| std::io::Write::write_all(&mut f, new_dest.as_bytes()));
                match created {
                    Ok(()) => {
                        self.refresh(&dest);
                        Ok(())
                    }
                    Err(e) => Err(e.to_string()),
                }
            };
            if let Err(e) = res.and_then(|_| self.commit_files(writes)) {
                return self.move_err(target, to_file, "write_failed", &e);
            }
            "true"
        };
        let mut body = Self::lines_block(&changed);
        if !exported_now.is_empty() {
            body.push_str(&format!(
                "<exported reason=\"{}\">{}</exported>\n",
                esc(&format!("the moved code uses them; now exported from {src_rel} and imported by {dest}")),
                esc(&exported_now.join(", "))
            ));
        }
        if !checks.is_empty() {
            checks.dedup();
            body.push_str(&format!("<check reason=\"not rewritten: verify by hand\">\n{}\n</check>\n", esc(&checks.join("\n"))));
        }
        body.push_str(if applied == "true" {
            "<next>Done: files are written and re-indexed. Run the type checker or tests; handle any <check> lines.</next>\n"
        } else {
            "<next>Plan only: call again without dry_run to apply.</next>\n"
        });
        let open = format!(
            "<move_symbol target=\"{}\" from=\"{}\" to=\"{}\" applied=\"{applied}\" files=\"{n_files}\" impact=\"{}\" why=\"{}\">",
            esc(&name),
            esc(&src_rel),
            esc(&dest),
            impact.level,
            esc(&impact.reason)
        );
        self.finish(&open, "</move_symbol>", &body, budget, "")
    }
}

#[cfg(test)]
#[path = "move_symbol_tests.rs"]
mod tests;
