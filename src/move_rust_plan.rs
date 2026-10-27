//! The plan of a Rust `move_symbol` (see move_rust.rs): locate the item and its crate, scan every file
//! that may name it, decide visibility, and build the new text of every file.

use crate::engine::Engine;
use crate::lang::Lang;
use crate::move_rust_mods::{crate_roots, package_dir, vis_scope, Crate, ModPath};
use crate::move_rust_needs::*;
use crate::move_rust_uses::{attrs_start, delete_item, site, FileScan, Job, MovedUse, Scope};
use crate::move_symbol_paths::Edit;
use crate::parse::{parse_tree, text};
use crate::refactor::line_of;
use std::collections::{BTreeSet, HashMap, HashSet};
use tree_sitter::Node;

const ITEMS: &[&str] = &["function_item", "struct_item", "enum_item", "union_item", "trait_item", "const_item", "static_item", "type_item"];

type Refuse = (&'static str, String);

pub(crate) struct Plan {
    pub(crate) writes: Vec<(String, String)>,
    pub(crate) created: Option<(String, String)>,
    pub(crate) lines: Vec<(String, Vec<u32>)>,
    pub(crate) widened: Vec<String>,
    pub(crate) from: String,
}
impl Engine {
    pub(crate) fn rust_text(&self, idx: &HashMap<String, u32>, p: &str) -> Option<String> {
        idx.get(p).and_then(|&i| self.checked_source(i).ok())
    }

    pub(crate) fn plan_rust(&mut self, fi: u32, name: &str, dest: &str, dest_exists: bool, line: u32) -> Result<Plan, Refuse> {
        let src_rel = self.files[fi as usize].path.clone();
        let rs: Vec<String> = self.files.iter().filter(|f| f.lang == Lang::Rust).map(|f| f.path.clone()).collect();
        let idx: HashMap<String, u32> = self.files.iter().enumerate().filter(|(_, f)| f.lang == Lang::Rust).map(|(i, f)| (f.path.clone(), i as u32)).collect();
        let root = self.root.clone();
        let exists = |p: &str| root.join(p).is_file();
        let pkg = package_dir(&src_rel, exists).ok_or(("no_cargo_package", format!("no Cargo.toml above {src_rel}")))?;
        if package_dir(dest, exists).as_deref() != Some(pkg.as_str()) {
            return Err(("cross_crate", "to_file is in another Cargo package; moving between crates is not supported.".into()));
        }
        let toml = std::fs::read_to_string(root.join(if pkg.is_empty() { "Cargo.toml".into() } else { format!("{pkg}/Cargo.toml") })).unwrap_or_default();
        let read = |p: &str| self.rust_text(&idx, p);
        let crates: Vec<Crate> = crate_roots(&pkg, &toml, &rs).iter().map(|(r, id)| Crate::build(r, id, &rs, &read)).collect();
        let owners: Vec<&Crate> = crates.iter().filter(|c| c.file_module(&src_rel).is_some()).collect();
        let cr = match owners.as_slice() {
            [c] => *c,
            [] => return Err(("not_in_crate", format!("{src_rel} is not reached by any `mod` declaration from a crate root."))),
            _ => return Err(("several_crates", format!("{src_rel} is compiled into several crates."))),
        };
        if let Some(why) = &cr.limit {
            return Err(("module_limit", format!("{why}; the module tree is incomplete, so the move is not safe.")));
        }
        let s_mod = cr.file_module(&src_rel).map(|m| m.path.clone()).unwrap_or_default();
        if cr.file_module(&src_rel).map(|m| m.path_attr).unwrap_or(false) {
            return Err(("path_attribute", format!("{src_rel} is loaded through a #[path] attribute.")));
        }
        /* the destination module, and the `mod` declaration a new file needs */
        let mut new_mod: Option<(ModPath, String)> = None;
        let d_mod: ModPath = if dest_exists {
            let m = cr.file_module(dest).ok_or(("cross_crate", format!("{dest} is not a module of the crate rooted at {}.", cr.root_file)))?;
            if m.path_attr {
                return Err(("path_attribute", format!("{dest} is loaded through a #[path] attribute.")));
            }
            m.path.clone()
        } else {
            let (dir, file) = dest.rsplit_once('/').unwrap_or(("", dest));
            let (dir, stem) = if file == "mod.rs" { dir.rsplit_once('/').unwrap_or(("", dir)) } else { (dir, file.trim_end_matches(".rs")) };
            if !crate::refactor::valid_ident(stem) || matches!(stem, "lib" | "main" | "mod") {
                return Err(("bad_module_name", format!("`{stem}` cannot be a module name.")));
            }
            let p = cr.dir_owner(dir).filter(|m| !m.inline).ok_or(("no_parent_module", format!("no module owns {dir}/; create its parent module first.")))?;
            let path: ModPath = [p.path.as_slice(), &[stem.to_string()]].concat();
            if cr.module(&path).is_some() {
                return Err(("module_exists", format!("module {} already exists in another file.", path.join("::"))));
            }
            new_mod = Some((p.path.clone(), p.file.clone()));
            path
        };

        /* the item and the impls that move with it */
        let src = self.rust_text(&idx, &src_rel).ok_or(("unreadable", format!("cannot read {src_rel}")))?;
        let sb = src.as_bytes();
        let tree = parse_tree(Lang::Rust, sb).ok_or(("parse_failed", "could not parse the source file.".into()))?;
        let troot = tree.root_node();
        let mut cur = troot.walk();
        let tops: Vec<Node> = troot.named_children(&mut cur).collect();
        let named = |n: &Node| n.child_by_field_name("name").map(|x| text(x, sb) == name).unwrap_or(false);
        let same: Vec<&Node> = tops.iter().filter(|n| n.kind() != "impl_item" && n.kind() != "use_declaration" && named(n)).collect();
        if same.len() > 1 {
            return Err(("several_definitions", format!("{src_rel} defines `{name}` {} times (cfg variants?); move them by hand.", same.len())));
        }
        let item = *same.first().filter(|n| n.start_position().row < line as usize && n.end_position().row + 1 >= line as usize).ok_or(("not_top_level", "only top-level items can be moved.".to_string()))?;
        if !ITEMS.contains(&item.kind()) {
            return Err(("unsupported_item", format!("a {} cannot be moved; only fn, struct, enum, union, trait, const, static and type items.", item.kind())));
        }
        let is_trait = item.kind() == "trait_item";
        let mut moved_nodes: Vec<Node> = vec![*item];
        if !is_trait {
            moved_nodes.extend(tops.iter().filter(|n| n.kind() == "impl_item" && n.child_by_field_name("type").and_then(type_base).map(|t| text(t, sb) == name).unwrap_or(false)));
        }
        moved_nodes.sort_by_key(|n| n.start_byte());
        let ranges: Vec<(usize, usize)> = moved_nodes.iter().map(|n| { let (s, e, _) = delete_item(*n, sb); (s, e) }).collect();

        /* scan the crate and the crates that may name it */
        let job = Job { cr, name, src_mod: &s_mod, dest_mod: &d_mod, foreign: false, moved: &ranges };
        let crate_files: BTreeSet<String> = cr.mods.iter().map(|m| m.file.clone()).collect();
        let mut scans: Vec<(String, FileScan, bool)> = Vec::new();
        for f in &rs {
            let ours = crate_files.contains(f);
            let globs = idx.get(f).map(|&i| self.files[i as usize].ex.imports.iter().any(|im| im.raw.contains('*'))).unwrap_or(false);
            let must = *f == src_rel || *f == dest || new_mod.as_ref().map(|m| m.1 == *f).unwrap_or(false);
            let Some(t) = self.rust_text(&idx, f) else {
                if ours {
                    return Err(("unreadable", format!("cannot read {f}")));
                }
                continue;
            };
            let wanted = if ours { must || globs || word_in(&t, name) } else { !cr.ident.is_empty() && word_in(&t, &cr.ident) && (globs || word_in(&t, name)) };
            if !wanted {
                continue;
            }
            let Some(ft) = parse_tree(Lang::Rust, t.as_bytes()) else { continue };
            let j = Job { foreign: !ours, moved: if *f == src_rel { &ranges } else { &[] }, ..job };
            scans.push((f.clone(), j.scan(f, ft.root_node(), t.as_bytes()), !ours));
        }
        /* modules the source glob-imports: the moved code may use their names */
        let glob_files: BTreeSet<String> = scans
            .iter()
            .filter(|x| x.0 == src_rel)
            .flat_map(|x| x.1.scopes.iter())
            .filter(|(m, _)| *m == s_mod)
            .flat_map(|(_, s)| s.globs.iter().filter_map(|g| cr.module(g.target.as_ref()?).map(|m| m.file.clone())))
            .filter(|f| !scans.iter().any(|x| x.0 == *f))
            .collect();
        for f in glob_files {
            let Some(t) = self.rust_text(&idx, &f) else { continue };
            let Some(ft) = parse_tree(Lang::Rust, t.as_bytes()) else { continue };
            let j = Job { moved: &[], ..job };
            scans.push((f.clone(), j.scan(&f, ft.root_node(), t.as_bytes()), false));
        }
        let mut scopes: HashMap<ModPath, Scope> = HashMap::new();
        let mut edits: HashMap<String, Vec<Edit>> = HashMap::new();
        let mut refs: Vec<ModPath> = Vec::new();
        let mut foreign_refs: Vec<String> = Vec::new();
        let mut reexports: Vec<(ModPath, String)> = Vec::new();
        let mut moved_use = MovedUse::default();
        for (f, sc, foreign) in scans {
            if foreign && !sc.refs.is_empty() {
                foreign_refs.push(format!("{f}:{}", sc.refs[0].1));
            }
            if !foreign {
                refs.extend(sc.refs.iter().map(|r| r.0.clone()));
                reexports.extend(sc.reexports);
            }
            if f == src_rel {
                moved_use = sc.moved;
            }
            edits.entry(f).or_default().extend(sc.edits);
            scopes.extend(sc.scopes);
        }
        let s_scope = scopes.get(&s_mod).ok_or(("internal", "source module not scanned".into()))?;
        if let Some(m) = moved_use.macros.iter().find(|m| s_scope.macros.contains(*m)) {
            return Err(("macro_rules", format!("the moved code uses `{m}!`, a macro_rules! macro of {src_rel}; macro scope is textual, move it by hand.")));
        }
        for sc in scopes.values() {
            let pub_glob = |t: &[String]| sc.globs.iter().any(|g| g.target.as_deref() == Some(t) && !g.vis.is_empty());
            if pub_glob(&s_mod) && !pub_glob(&d_mod) {
                return Err(("glob_reexport", format!("{} re-exports {} with `pub use ...::*`; moving `{name}` would drop it from that re-export.", sc.file, abs_path("crate", &s_mod))));
            }
        }
        if let Some(d) = scopes.get(&d_mod) {
            if d.defines.contains_key(name) {
                return Err(("clash", format!("is already defined in {dest}")));
            }
            for l in d.imports.iter().filter(|l| l.local == name) {
                let w: Vec<&str> = l.segs.iter().map(|s| s.as_str()).collect();
                if l.segs.len() < 2 || cr.resolve(&d_mod, &w[..w.len() - 1], Some(&d.aliases), false).as_deref() != Some(s_mod.as_slice()) {
                    return Err(("clash", format!("is already imported in {dest}")));
                }
            }
        }

        /* modules that named it unqualified get a `use` */
        let use_line = |root: &str| format!("use {}::{name};", abs_path(root, &d_mod));
        let s_keeps = tops.iter().any(|n| !ranges.iter().any(|r| n.start_byte() >= r.0 && n.start_byte() < r.1) && !matches!(n.kind(), "use_declaration" | "line_comment" | "block_comment" | "inner_attribute_item" | "attribute_item"));
        let mut adds: Vec<(String, usize, String)> = Vec::new();
        for (m, sc) in &scopes {
            if *m == d_mod || (sc.defines.contains_key(name) && *m != s_mod) || sc.imports.iter().any(|l| l.local == name) {
                continue;
            }
            let sees = *m == s_mod || sc.globs.iter().any(|g| g.target.as_deref() == Some(s_mod.as_slice()));
            if !sees || !(sc.bare > 0 || (is_trait && *m == s_mod && s_keeps)) {
                continue;
            }
            let foreign = m.first().map(|x| x.starts_with('\0')).unwrap_or(false);
            let l = use_line(if foreign { &cr.ident } else { "crate" });
            if foreign {
                foreign_refs.push(sc.file.clone());
            } else {
                refs.push(m.clone());
            }
            adds.push((sc.file.clone(), sc.insert_at, l));
        }
        /* insertions into the source file stay outside the moved code even where they touch it */
        let mut src_extra: Vec<Edit> = Vec::new();
        for (file, at, l) in &adds {
            let indent = scopes.values().find(|s| s.file == *file && s.insert_at == *at).map(|s| s.indent.clone()).unwrap_or_default();
            let t = if *at == 0 { format!("{l}\n\n") } else { format!("\n{indent}{l}") };
            if *file == src_rel {
                src_extra.push((*at, *at, t));
            } else {
                edits.entry(file.clone()).or_default().push((*at, *at, t));
            }
        }

        /* visibility: the item, the new module, and what the moved code reads from its old module */
        let mut req: Vec<ModPath> = refs.clone();
        let mut need_pub = !foreign_refs.is_empty();
        for (m, v) in &reexports {
            match v.as_str() {
                "" => {}
                "pub" => need_pub = true,
                v => req.push(vis_scope(v, m)),
            }
        }
        let item_vis = crate::move_rust_mods::vis_text(*item, sb);
        if need_pub && item_vis != "pub" {
            return Err(("not_public", format!("`{name}` is used from other crates ({}) but is not `pub`.", foreign_refs.join(", "))));
        }
        let mut src_edits: Vec<Edit> = edits.remove(&src_rel).unwrap_or_default();
        let mut widened: Vec<String> = Vec::new();
        let item_scope = vis_scope(&item_vis, &d_mod);
        if req.iter().any(|r| !r.starts_with(&item_scope)) {
            let s = site(*item, sb);
            if let Some(e) = widen(s.at, s.end, &item_vis) {
                src_edits.push(e);
                widened.push(name.to_string());
            }
        }
        if req.iter().any(|r| r.starts_with(&s_mod) && !r.starts_with(&d_mod)) {
            let mut at = Vec::new();
            for n in &moved_nodes {
                private_members(*n, sb, None, &mut at);
            }
            if !at.is_empty() {
                widened.push(format!("{} private fields/methods of {name}", at.len()));
            }
            src_edits.extend(at.into_iter().map(|a| (a, a, "pub(crate) ".to_string())));
        }
        if !need_pub && item_vis == "pub" && !cr.ident.is_empty() && cr.public(&s_mod) {
            need_pub = true;
        }
        match &new_mod {
            Some((p, _)) => {
                if let Some(r) = req.iter().find(|r| !cr.reachable(p, r)) {
                    return Err(("dest_unreachable", format!("{} cannot see module {}.", abs_path("crate", r), abs_path("crate", p))));
                }
                if need_pub && !cr.public(p) {
                    return Err(("dest_private", format!("`{name}` is public API but {} is not a public module.", abs_path("crate", p))));
                }
            }
            None => {
                if let Some(r) = req.iter().find(|r| !cr.reachable(&d_mod, r)) {
                    return Err(("dest_unreachable", format!("{} cannot see module {}; make it pub(crate) first.", abs_path("crate", r), abs_path("crate", &d_mod))));
                }
                if need_pub && !cr.public(&d_mod) {
                    return Err(("dest_private", format!("`{name}` is used from other crates but {} is not a public module.", abs_path("crate", &d_mod))));
                }
            }
        }

        if !d_mod.starts_with(&s_mod) {
            let mut at = Vec::new();
            for n in tops.iter().filter(|n| !moved_nodes.iter().any(|m| m.id() == n.id())) {
                private_members(*n, sb, Some(&moved_use.members), &mut at);
            }
            if !at.is_empty() {
                widened.push(format!("{} private fields/methods of {src_rel} the moved code uses", at.len()));
            }
            src_edits.extend(at.into_iter().map(|a| (a, a, "pub(crate) ".to_string())));
        }

        /* what the moved code needs in its new module */
        let (mut dest_uses, w_edits, w_names) = dest_needs(cr, name, &s_mod, &d_mod, &scopes, &moved_use);
        src_edits.extend(w_edits);
        widened.extend(w_names);
        /* the source's imports only the moved code named, as a value or a type */
        let mut rest = src.clone().into_bytes();
        let blank = tops.iter().filter(|n| n.kind() == "use_declaration").map(|n| (n.start_byte(), n.end_byte())).chain(ranges.iter().copied());
        for (a, b) in blank.collect::<Vec<_>>() {
            rest[a..b].iter_mut().for_each(|c| *c = b' ');
        }
        let rest = String::from_utf8_lossy(&rest).into_owned();
        let unused: HashSet<String> = s_scope
            .imports
            .iter()
            .map(|l| l.local.clone())
            .filter(|l| moved_use.typed.contains(l) && *l != name && !word_in(&rest, l))
            .collect();
        let drops = if unused.is_empty() { Vec::new() } else { crate::move_rust_uses::drop_leaves(troot, sb, &unused) };
        dest_uses.sort();
        dest_uses.dedup();

        /* a new file: its `mod` declaration in the parent module */
        if let Some((p, pfile)) = &new_mod {
            let stem = d_mod.last().cloned().unwrap_or_default();
            let ps = scopes.values().find(|s| s.file == *pfile && !s.inline).ok_or(("internal", "parent module not scanned".to_string()))?;
            let outside = req.iter().chain(std::iter::once(&s_mod)).any(|r| !r.starts_with(p));
            let vis = if need_pub { "pub " } else if outside { "pub(crate) " } else { "" };
            let at = ps.mod_at;
            let t = if at == 0 { format!("{vis}mod {stem};\n\n") } else { format!("\n{vis}mod {stem};") };
            if *pfile == src_rel {
                src_extra.push((at, at, t));
            } else {
                edits.entry(pfile.clone()).or_default().push((at, at, t));
            }
        }

        /* the moved text, with its own edits */
        let taken: Vec<(usize, usize)> = src_edits.iter().chain(src_extra.iter()).map(|e| (e.0, e.1)).collect();
        src_edits.extend(drops.into_iter().filter(|d| !taken.iter().any(|t| t.0 < d.1 && d.0 < t.1.max(t.0 + 1))));
        let (inner, outer): (Vec<Edit>, Vec<Edit>) = src_edits.into_iter().partition(|e| ranges.iter().any(|r| e.0 >= r.0 && e.1 <= r.1 && e.0 < r.1));
        let mut pieces: Vec<String> = Vec::new();
        for r in &ranges {
            let local: Vec<Edit> = inner.iter().filter(|e| e.0 >= r.0 && e.0 < r.1).map(|e| (e.0 - r.0, e.1 - r.0, e.2.clone())).collect();
            let mut t = apply(&src[r.0..r.1], local).ok_or(("internal", "overlapping edits in the moved code".into()))?;
            if !t.ends_with('\n') {
                t.push('\n');
            }
            pieces.push(t);
        }
        let moved_text = pieces.join("\n");
        let mut outer = outer;
        outer.extend(src_extra);
        for r in &ranges {
            let mut end = r.1;
            let blank_before = r.0 == 0 || src[..r.0].ends_with("\n\n");
            if blank_before && src[end..].starts_with('\n') {
                end += 1;
            }
            outer.push((r.0, end, String::new()));
        }
        let mut lines: Vec<(String, Vec<u32>)> = Vec::new();
        let mut writes: Vec<(String, String)> = Vec::new();
        lines.push((src_rel.clone(), outer.iter().map(|e| line_of(&src, e.0)).collect()));
        let new_src = apply(&src, outer).ok_or(("internal", format!("overlapping edits in {src_rel}")))?;
        writes.push((src_rel.clone(), new_src.trim_start_matches('\n').to_string()));

        /* the destination */
        let mut created = None;
        if new_mod.is_some() {
            let head = if dest_uses.is_empty() { String::new() } else { format!("{}\n\n", dest_uses.join("\n")) };
            created = Some((dest.to_string(), format!("{head}{moved_text}")));
            lines.push((dest.to_string(), vec![1]));
        } else {
            let dt = self.rust_text(&idx, dest).ok_or(("unreadable", format!("cannot read {dest}")))?;
            let ds = scopes.get(&d_mod).ok_or(("internal", "destination not scanned".into()))?;
            let de = edits.entry(dest.to_string()).or_default();
            if !dest_uses.is_empty() {
                let u = dest_uses.join("\n");
                de.push(if ds.insert_at == 0 { (0, 0, format!("{u}\n\n")) } else { (ds.insert_at, ds.insert_at, format!("\n{u}")) });
            }
            let dtree = parse_tree(Lang::Rust, dt.as_bytes()).ok_or(("parse_failed", "could not parse to_file.".into()))?;
            let dr = dtree.root_node();
            let mut c = dr.walk();
            let tests = dr.named_children(&mut c).filter(|n| n.kind() == "mod_item" && n.child_by_field_name("body").is_some()).last().filter(|n| {
                let s = attrs_start(*n, dt.as_bytes());
                text(*n, dt.as_bytes()).contains("mod tests") && dt[s..n.start_byte()].contains("cfg(test)") && n.end_byte() + 2 >= dt.trim_end().len()
            });
            let at = match tests {
                Some(n) => {
                    let s = attrs_start(n, dt.as_bytes());
                    dt[..s].rfind('\n').map(|i| i + 1).unwrap_or(0)
                }
                None => dt.len(),
            };
            let sep = if at == dt.len() { if dt.ends_with("\n\n") || dt.is_empty() { "" } else if dt.ends_with('\n') { "\n" } else { "\n\n" } } else { "" };
            let tail = if at == dt.len() { "" } else { "\n" };
            de.push((at, at, format!("{sep}{moved_text}{tail}")));
        }
        for (f, es) in edits {
            if es.is_empty() {
                continue;
            }
            let t = self.rust_text(&idx, &f).ok_or(("unreadable", format!("cannot read {f}")))?;
            lines.push((f.clone(), es.iter().map(|e| line_of(&t, e.0)).collect()));
            writes.push((f.clone(), apply(&t, es).ok_or(("internal", format!("overlapping edits in {f}")))?));
        }
        Ok(Plan { writes, created, lines, widened, from: src_rel })
    }
}
