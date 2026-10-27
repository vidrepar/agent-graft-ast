//! Method calls with a known receiver type (`CallSite::rtype`) resolve among that type's methods first:
//! its own, then those of the traits it implements, its base classes and interfaces, nearest first.

use super::decls::DeclIndex;
use super::hierarchy::{bare, Supers};
use super::which::Which;
use crate::refactor::package_of;

/// What typed resolution reads besides the files: hierarchy, declarations, every file's imports.
pub(super) type Types<'t> = (&'t Supers, &'t DeclIndex, &'t [Vec<u32>]);
use crate::parse::infer::hint;
use super::{resolve_call, Files, Tier, WideMemo};
use crate::index::FileRecord;
use crate::parse::CallSite;
use std::collections::HashMap;

/// (owner type, member name) -> definitions, sorted: a whole-repo build looks members up here
/// instead of scanning every same-named definition for each typed call.
pub(super) struct Members<'f>(HashMap<(&'f str, &'f str), Vec<(u32, u32)>>);

impl<'f> Members<'f> {
    pub(super) fn build(files: &'f [FileRecord], by_name: &HashMap<String, Vec<(u32, u32)>>) -> Members<'f> {
        let mut m: HashMap<(&'f str, &'f str), Vec<(u32, u32)>> = HashMap::new();
        for list in by_name.values().filter(|l| l.len() > 1) {
            for &(f, si) in list {
                let syms = &files[f as usize].ex.symbols;
                let s = &syms[si as usize];
                if let Some(o) = s.parent.and_then(|p| bare(&syms[p as usize].name)) {
                    m.entry((o, s.name.as_str())).or_default().push((f, si));
                }
            }
        }
        Members(m)
    }
}

/// `resolve_call`, first among the methods of the receiver's type when the call site knows it.
/// A type without such a method (or an unknown type) resolves as before, by name.
/// `members`, when given, must describe `files` (a fresh build); otherwise `cands` is scanned.
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve<'a>(
    files: Files<'a>,
    types: Types,
    members: Option<&Members>,
    cands: &[(u32, u32)],
    imports: &[u32],
    fi: u32,
    c: &'a CallSite,
    memo: Option<&mut WideMemo<'a>>,
) -> (Tier, Vec<(u32, u32)>) {
    let typed = c.rtype.is_some() || c.rhint.is_some();
    /* settled once: both the typed lookup and the foreign-type check read it */
    let shape = if typed && cands.len() > 1 { types.1.receiver(types.0, files, types.2, fi, c) } else { None };
    if let Some(v) = shape.as_deref().and_then(|sh| by_type(files, types, members, cands, imports, fi, c, sh)) {
        return (Tier::Typed, v);
    }
    let foreign = shape.as_deref().is_some_and(|sh| foreign_receiver(files, types.0, members, cands, c, sh));
    if cands.len() > 1 && (foreign || (c.in_macro && !typed)) {
        /* a `Vec` or `Map` receiver is not calling a repo method that no repo trait gives it; a call in
         * macro tokens with nothing to type it is too loose to pick among several */
        return (Tier::Typed, Vec::new());
    }
    let (tier, chosen) = resolve_call(files, cands, imports, fi, c, memo);
    (if typed || c.in_macro { Tier::Typed } else { tier }, chosen)
}

/// The receiver's type is known and is not the repo's, and none of `cands` is reachable from it
/// through a trait the repo implements for it.
fn foreign_receiver(files: Files, supers: &Supers, members: Option<&Members>, cands: &[(u32, u32)], c: &CallSite, shape: &str) -> bool {
    let t = hint::split_qual(hint::receiver_name(shape, &c.callee)).1;
    if t == "?" || supers.is_repo_type(t) {
        return false;
    }
    let anc = supers.ancestors(t);
    if let Some(m) = members {
        return !anc.iter().any(|a| m.0.contains_key(&(*a, c.callee.as_str())));
    }
    let owner = |&(cf, cs): &(u32, u32)| {
        let syms = &files.rec(cf).ex.symbols;
        syms[cs as usize].parent.and_then(|p| bare(&syms[p as usize].name))
    };
    !cands.iter().any(|x| owner(x).is_some_and(|o| anc.contains(&o)))
}

/// The call's targets among its receiver type's own and inherited methods; None when the type is
/// unknown, not the repo's, or has no method of that name (resolution then goes by name).
#[allow(clippy::too_many_arguments)]
fn by_type<'a>(
    files: Files<'a>,
    (supers, _, all_imports): Types,
    members: Option<&Members>,
    cands: &[(u32, u32)],
    imports: &[u32],
    fi: u32,
    c: &'a CallSite,
    shape: &str,
) -> Option<Vec<(u32, u32)>> {
    let (q, t) = hint::split_qual(hint::receiver_name(shape, &c.callee));
    if !supers.is_repo_type(t) {
        return None;
    }
    /* several repo types named `t`: the one the file names or imports, or none at all */
    let pkg = match supers.defs.which(t, q, fi, files, all_imports) {
        Which::Only => None,
        Which::In(d) => Some(package_of(&files.rec(d).path)),
        Which::Unknown => return None,
    };
    let in_pkg = |x: &(u32, u32)| pkg.is_none_or(|p| package_of(&files.rec(x.0).path) == p);
    {
        let owner = |&(cf, cs): &(u32, u32)| {
            let syms = &files.rec(cf).ex.symbols;
            syms[cs as usize].parent.and_then(|p| bare(&syms[p as usize].name))
        };
        for level in supers.levels(t) {
            let mut typed: Vec<(u32, u32)> = match members {
                Some(m) => {
                    let mut v: Vec<(u32, u32)> =
                        level.iter().flat_map(|o| m.0.get(&(*o, c.callee.as_str())).into_iter().flatten().copied()).collect();
                    v.sort_unstable();
                    v
                }
                None => cands.iter().copied().filter(|x| owner(x).is_some_and(|o| level.contains(&o))).collect(),
            };
            if level.contains(&t) {
                typed.retain(in_pkg);
            }
            if typed.is_empty() {
                continue;
            }
            /* several types share the name: the usual tiers pick among them, or leave it unresolved */
            return Some(resolve_call(files, &typed, imports, fi, c, None).1);
        }
    }
    None
}

impl super::Graph {
    /// The receiver type `t` may have a method whose impl, trait or class is named `owner`.
    pub fn type_reaches(&self, t: &str, owner: &str) -> bool {
        self.supers.reaches(t, owner)
    }

    /// The type call `c` of file `fi` looks its method up in, when the code or the repo's declarations
    /// say (through smart pointers: `Arc<Db>` -> `Db`).
    pub fn receiver_type(&self, files: &[FileRecord], fi: u32, c: &CallSite) -> Option<String> {
        let shape = self.decls.receiver(&self.supers, Files::new(files), &self.imports, fi, c)?;
        Some(hint::split_qual(hint::receiver_name(&shape, &c.callee)).1.to_string())
    }

    /// Where call `c` of file `fi` goes when its receiver type decides it (see `by_type`).
    pub fn typed_targets(&self, files: &[FileRecord], fi: u32, c: &CallSite) -> Option<Vec<(u32, u32)>> {
        let cands = super::cands(&self.by_name, &c.callee);
        if cands.len() < 2 {
            return None;
        }
        let shape = self.decls.receiver(&self.supers, Files::new(files), &self.imports, fi, c)?;
        by_type(Files::new(files), (&self.supers, &self.decls, &self.imports), None, cands, &self.imports[fi as usize], fi, c, &shape)
    }
}

#[cfg(test)]
mod graph_tests {
    use super::super::{tests::rec, Graph};

    /// (caller name, callee name, callee owner) for every call edge.
    fn edges(files: &[crate::index::FileRecord]) -> Vec<(String, String, String)> {
        let g = Graph::build(files);
        let mut v = Vec::new();
        for (fi, f) in files.iter().enumerate() {
            for (si, s) in f.ex.symbols.iter().enumerate() {
                for r in g.callees(fi as u32, Some(si as u32)) {
                    let tf = &files[r.file as usize].ex.symbols;
                    let t = &tf[r.sym.unwrap() as usize];
                    let owner = t.parent.map(|p| tf[p as usize].name.clone()).unwrap_or_default();
                    v.push((s.name.clone(), t.name.clone(), owner));
                }
            }
        }
        v.sort();
        v
    }

    fn e(a: &str, b: &str, c: &str) -> (String, String, String) {
        (a.into(), b.into(), c.into())
    }

    #[test]
    fn rust_receiver_type_picks_the_method() {
        let files = vec![
            rec("src/a.rs", "pub struct Alpha;\nimpl Alpha { pub fn syntax(&self) {} }\npub struct Beta;\nimpl Beta { pub fn syntax(&self) {} }\n"),
            rec("src/t.rs", "pub trait Named { fn name(&self) {} }\nimpl Named for crate::a::Beta {}\npub struct Gamma;\nimpl Gamma { pub fn name(&self) {} }\n"),
            rec("src/u.rs", "fn run(b: &Beta, x: Unknown) { let a = Alpha::new(); a.syntax(); b.syntax(); b.name(); x.syntax(); }\n"),
        ];
        let got = edges(&files);
        assert!(got.contains(&e("run", "syntax", "Alpha")) && got.contains(&e("run", "syntax", "Beta")), "{got:?}");
        /* Beta has no own `name`: the trait it implements does */
        assert!(got.contains(&e("run", "name", "Named")), "{got:?}");
        assert!(!got.contains(&e("run", "name", "Gamma")), "{got:?}");
        /* x's type is not in the repo: still 2 equal candidates, so no edge for it beyond the typed ones */
        assert_eq!(got.iter().filter(|x| x.1 == "syntax").count(), 2, "{got:?}");
    }

    #[test]
    fn typescript_and_python_inheritance() {
        let files = vec![
            rec("src/base.ts", "export class Base { save() {} }\nexport class Other { save() {} }\n"),
            rec("src/svc.ts", "import { Base } from './base';\nexport class Svc extends Base {\n  run() { this.save(); }\n}\nexport function f(o: Other) { o.save(); }\n"),
            rec("m.py", "class Repo:\n    def get(self): pass\nclass Cache:\n    def get(self): pass\nclass Svc(Repo):\n    def run(self):\n        self.get()\n        c = Cache()\n        c.get()\n"),
        ];
        let got = edges(&files);
        assert!(got.contains(&e("run", "save", "Base")), "{got:?}");
        assert!(got.contains(&e("f", "save", "Other")), "{got:?}");
        assert!(got.contains(&e("run", "get", "Repo")) && got.contains(&e("run", "get", "Cache")), "{got:?}");
    }

    #[test]
    fn declarations_in_other_files_type_the_receiver() {
        let files = vec![
            rec("src/a.rs", "pub struct Alpha;\nimpl Alpha { pub fn syntax(&self) {} }\npub struct Beta;\nimpl Beta { pub fn syntax(&self) {} }\npub fn open() -> Result<Beta, E> { todo!() }\n"),
            rec("src/svc.rs", "pub struct Svc { pub a: std::sync::Arc<Alpha> }\n"),
            rec("src/u.rs", "impl Svc {\n    fn run(&self) {\n        self.a.syntax();\n        let b = open()?;\n        b.syntax();\n    }\n}\n"),
            rec("src/t.ts", "export class Repo { save() {} }\nexport class Cache { save() {} }\nexport interface Ctx { cache: Cache }\nexport function make(): Promise<Repo> { return r; }\n"),
            rec("src/u.ts", "import { make, Ctx } from './t';\nasync function f(c: Ctx) { c.cache.save(); (await make()).save(); }\n"),
        ];
        let got = edges(&files);
        assert!(got.contains(&e("run", "syntax", "Alpha")) && got.contains(&e("run", "syntax", "Beta")), "{got:?}");
        assert!(got.contains(&e("f", "save", "Cache")) && got.contains(&e("f", "save", "Repo")), "{got:?}");
    }

    #[test]
    fn same_named_types_follow_what_the_file_names() {
        let files = vec![
            rec("crates/syntax/src/ast/nodes.rs", "pub struct Path;\nimpl Path { pub fn segment(&self) {} }\n"),
            rec("crates/hir-def/src/path.rs", "pub struct Path;\nimpl Path { pub fn segment(&self) {} }\npub struct Other;\nimpl Other { pub fn segment(&self) {} }\n"),
            /* qualified: ast::Path */
            rec("crates/ide/src/a.rs", "use syntax::ast;\nfn a(p: ast::Path) { p.segment(); }\n"),
            /* imported by name from another crate */
            rec("crates/ide/src/b.rs", "use hir_def::path::Path;\nfn b(p: Path) { p.segment(); }\n"),
            /* nothing says which */
            rec("crates/ide/src/c.rs", "fn c(p: Path) { p.segment(); }\n"),
        ];
        let g = super::super::Graph::build(&files);
        let target = |fi: u32| {
            let callees = g.callees(fi, Some(0));
            callees.iter().map(|r| files[r.file as usize].path.clone()).collect::<Vec<_>>()
        };
        assert_eq!(target(2), ["crates/syntax/src/ast/nodes.rs"]);
        assert_eq!(target(3), ["crates/hir-def/src/path.rs"]);
        assert_eq!(target(4), Vec::<String>::new());
    }
}
