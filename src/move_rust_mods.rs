//! The crate model behind a Rust `move_symbol` (see move_rust.rs): which crate a file is compiled into,
//! the module path of every file and inline module, `mod` visibility, and path resolution inside a module.

use crate::graph::normalize;
use crate::lang::Lang;
use crate::parse::{parse_tree, text};
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

/// `crate::a::b` is `["a", "b"]`; the crate root is `[]`.
pub(crate) type ModPath = Vec<String>;

pub(crate) struct Module {
    pub(crate) path: ModPath,
    /// the file holding the module's items (an inline module shares its parent's file)
    pub(crate) file: String,
    pub(crate) inline: bool,
    /// directory its `mod x;` children live in
    pub(crate) child_dir: String,
    /// visibility of its `mod` declaration, as written ("" = private; the root is "pub")
    pub(crate) vis: String,
    /// reached through a `#[path]` attribute somewhere on the way from the root
    pub(crate) path_attr: bool,
}

pub(crate) struct Crate {
    pub(crate) root_file: String,
    /// the name other crates use for it (`hir_def`); empty for a binary
    pub(crate) ident: String,
    pub(crate) mods: Vec<Module>,
    pub(crate) by_path: HashMap<ModPath, usize>,
    /// why the tree is incomplete (too deep or too many modules); a move refuses then
    pub(crate) limit: Option<String>,
}

/// Bounds on module discovery: nesting depth and modules per crate.
const MAX_DEPTH: usize = 64;
const MAX_MODULES: usize = 20_000;

/// Values of `key = "..."` in one `[section]` of a Cargo.toml.
fn toml_value(toml: &str, section: &str, key: &str) -> Option<String> {
    let mut cur = String::new();
    for l in toml.lines() {
        let l = l.trim();
        if l.starts_with('[') {
            cur = l.trim_matches(|c| c == '[' || c == ']').trim().to_string();
            continue;
        }
        if cur != section {
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim();
        if k.trim() == key && v.starts_with('"') {
            return Some(v.trim_matches('"').to_string());
        }
    }
    None
}

/// The package directory of `file`: the nearest ancestor holding a Cargo.toml ("" = repo root).
pub(crate) fn package_dir(file: &str, exists: impl Fn(&str) -> bool) -> Option<String> {
    let mut dir = file.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    loop {
        let toml = if dir.is_empty() { "Cargo.toml".to_string() } else { format!("{dir}/Cargo.toml") };
        if exists(&toml) {
            return Some(dir);
        }
        if dir.is_empty() {
            return None;
        }
        dir = dir.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    }
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

/// Crate roots of a package, the library first: (root file, crate ident or "" for non-libraries).
pub(crate) fn crate_roots(pkg: &str, toml: &str, files: &[String]) -> Vec<(String, String)> {
    let name = toml_value(toml, "lib", "name").or_else(|| toml_value(toml, "package", "name")).unwrap_or_default().replace('-', "_");
    let lib = join(pkg, &toml_value(toml, "lib", "path").unwrap_or_else(|| "src/lib.rs".into()));
    let mut out = Vec::new();
    if files.contains(&lib) {
        out.push((lib, name));
    }
    let under = |d: &str| join(pkg, d) + "/";
    for f in files {
        let main = *f == join(pkg, "src/main.rs");
        let bin = f.strip_prefix(&under("src/bin")).map(|r| !r.contains('/') || r.ends_with("/main.rs")).unwrap_or(false);
        let top = ["tests", "examples", "benches"].iter().any(|d| f.strip_prefix(&under(d)).map(|r| !r.contains('/')).unwrap_or(false));
        if main || bin || top {
            out.push((f.clone(), String::new()));
        }
    }
    out
}

/// A `#[path = ...]` among the attributes right before `n`.
fn path_attr_of(n: Node, src: &[u8]) -> Option<String> {
    let mut p = n.prev_named_sibling();
    while let Some(a) = p {
        match a.kind() {
            "attribute_item" => {
                let t: String = text(a, src).chars().filter(|c| !c.is_whitespace()).collect();
                if let Some(i) = t.find("path=\"") {
                    return Some(t[i + 6..].split('"').next().unwrap_or("").to_string());
                }
            }
            k if k.contains("comment") => {}
            _ => break,
        }
        p = a.prev_named_sibling();
    }
    None
}

pub(crate) fn vis_text(n: Node, src: &[u8]) -> String {
    let mut c = n.walk();
    let v = n.named_children(&mut c).find(|x| x.kind() == "visibility_modifier");
    v.map(|v| text(v, src).chars().filter(|c| !c.is_whitespace()).collect()).unwrap_or_default()
}

impl Crate {
    /// The module tree reachable from `root_file` through `mod` declarations.
    pub(crate) fn build(root_file: &str, ident: &str, files: &[String], read: &dyn Fn(&str) -> Option<String>) -> Crate {
        let dir = root_file.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        let mut cr = Crate { root_file: root_file.into(), ident: ident.into(), mods: Vec::new(), by_path: HashMap::new(), limit: None };
        cr.push(Module { path: Vec::new(), file: root_file.into(), inline: false, child_dir: dir, vis: "pub".into(), path_attr: false });
        /* a file is loaded as a module once: a `#[path]` pointing back at a loaded file is not followed */
        let mut loaded: HashSet<String> = HashSet::from([root_file.to_string()]);
        let mut queue = vec![0usize];
        while let Some(i) = queue.pop() {
            let file = cr.mods[i].file.clone();
            let Some(src) = read(&file) else { continue };
            let Some(tree) = parse_tree(Lang::Rust, src.as_bytes()) else { continue };
            let mut found = Vec::new();
            let m = &cr.mods[i];
            let mut too_deep = false;
            collect_mods(tree.root_node(), src.as_bytes(), &file, &m.path, &m.child_dir, m.path_attr, files, &mut found, &mut too_deep);
            if too_deep {
                cr.limit = Some(format!("modules nest deeper than {MAX_DEPTH} levels in {file}"));
            }
            for (module, is_file) in found {
                if cr.mods.len() >= MAX_MODULES {
                    cr.limit = Some(format!("the crate has more than {MAX_MODULES} modules"));
                    return cr;
                }
                if is_file && !loaded.insert(module.file.clone()) {
                    continue;
                }
                let k = cr.push(module);
                if is_file {
                    queue.push(k);
                }
            }
        }
        cr
    }

    fn push(&mut self, m: Module) -> usize {
        self.by_path.insert(m.path.clone(), self.mods.len());
        self.mods.push(m);
        self.mods.len() - 1
    }

    pub(crate) fn module(&self, p: &[String]) -> Option<&Module> {
        self.by_path.get(p).map(|&i| &self.mods[i])
    }

    /// The file-backed module of `file`.
    pub(crate) fn file_module(&self, file: &str) -> Option<&Module> {
        self.mods.iter().find(|m| !m.inline && m.file == file)
    }

    /// The module owning `dir` (whose `mod x;` children live there).
    pub(crate) fn dir_owner(&self, dir: &str) -> Option<&Module> {
        self.mods.iter().find(|m| m.child_dir == dir)
    }

    /// The module a path prefix names, seen from module `m`; `aliases` are the modules `m` imports by name.
    /// `foreign`: seen from another crate, where only `ident::...` reaches this one.
    pub(crate) fn resolve(&self, m: &[String], segs: &[&str], aliases: Option<&HashMap<String, ModPath>>, foreign: bool) -> Option<ModPath> {
        let (head, rest) = segs.split_first()?;
        let mut cur: ModPath = match *head {
            h if !self.ident.is_empty() && h == self.ident && (foreign || !self.by_path.contains_key(&[m, &[h.to_string()]].concat())) => Vec::new(),
            _ if foreign => return None,
            "crate" | "$crate" => Vec::new(),
            "self" => m.to_vec(),
            "super" => m.split_last()?.1.to_vec(),
            h => {
                let child = [m, &[h.to_string()]].concat();
                if self.by_path.contains_key(&child) {
                    child
                } else {
                    aliases?.get(h)?.clone()
                }
            }
        };
        for s in rest {
            if *s == "super" {
                cur.pop()?;
            } else {
                cur.push(s.to_string());
                if !self.by_path.contains_key(&cur) {
                    return None;
                }
            }
        }
        Some(cur)
    }

    /// Can code in `from` name module `p` (every `mod` on the way is visible there)?
    pub(crate) fn reachable(&self, p: &[String], from: &[String]) -> bool {
        (1..=p.len()).all(|k| self.module(&p[..k]).map(|m| from.starts_with(&vis_scope(&m.vis, &p[..k - 1]))).unwrap_or(false))
    }

    /// Is every `mod` from the root to `p` declared `pub`?
    pub(crate) fn public(&self, p: &[String]) -> bool {
        (1..=p.len()).all(|k| self.module(&p[..k]).map(|m| m.vis == "pub").unwrap_or(false))
    }
}

/// The modules an item declared in `owner` with visibility `vis` is visible in: `owner`'s returned subtree.
pub(crate) fn vis_scope(vis: &str, owner: &[String]) -> ModPath {
    match vis {
        "" | "pub(self)" => owner.to_vec(),
        "pub(super)" => owner.split_last().map(|x| x.1.to_vec()).unwrap_or_default(),
        v if v.starts_with("pub(in") => {
            let p = v.trim_start_matches("pub(in").trim_end_matches(')');
            let mut cur = owner.to_vec();
            for (i, s) in p.split("::").enumerate() {
                match s {
                    "crate" if i == 0 => cur.clear(),
                    "self" if i == 0 => {}
                    "super" => {
                        cur.pop();
                    }
                    s if i == 0 => cur = vec![s.to_string()],
                    s => cur.push(s.to_string()),
                }
            }
            cur
        }
        _ => Vec::new(),
    }
}

/// `mod` declarations under `node` (a file root or an inline module body) of module `parent`.
#[allow(clippy::too_many_arguments)]
fn collect_mods(node: Node, src: &[u8], file: &str, parent: &[String], dir: &str, via_attr: bool, files: &[String], out: &mut Vec<(Module, bool)>, too_deep: &mut bool) {
    if parent.len() >= MAX_DEPTH {
        *too_deep = true;
        return;
    }
    let mut cur = node.walk();
    for n in node.named_children(&mut cur) {
        if n.kind() != "mod_item" {
            continue;
        }
        let Some(name) = n.child_by_field_name("name").map(|x| text(x, src).to_string()) else { continue };
        let path: ModPath = [parent, std::slice::from_ref(&name)].concat();
        let vis = vis_text(n, src);
        let attr = path_attr_of(n, src);
        let path_attr = via_attr || attr.is_some();
        if let Some(body) = n.child_by_field_name("body") {
            let child_dir = join(dir, &name);
            collect_mods(body, src, file, &path, &child_dir, path_attr, files, out, too_deep);
            out.push((Module { path, file: file.into(), inline: true, child_dir, vis, path_attr }, false));
            continue;
        }
        let candidates = match &attr {
            Some(p) => vec![normalize(&join(file.rsplit_once('/').map(|x| x.0).unwrap_or(""), p))],
            None => vec![join(dir, &format!("{name}.rs")), join(dir, &format!("{name}/mod.rs"))],
        };
        let Some(f) = candidates.into_iter().find(|c| files.contains(c)) else { continue };
        let child_dir = if f.ends_with("/mod.rs") || f == "mod.rs" { f.rsplit_once('/').map(|x| x.0.to_string()).unwrap_or_default() } else { f.trim_end_matches(".rs").to_string() };
        out.push((Module { path, file: f, inline: false, child_dir, vis, path_attr }, true));
    }
}
