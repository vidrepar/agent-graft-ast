//! Declared field and return types across the repo, for receivers whose type the caller's own file
//! cannot settle (`self.db.query()` with `db` declared elsewhere, `let r = open()?; r.get()`).

use super::hierarchy::Supers;
use super::which::Which;
use super::Files;
use crate::refactor::package_of;
use crate::index::FileRecord;
use crate::parse::infer::hint::{self, Decls};
use crate::parse::{CallSite, Member};
use rayon::prelude::*;
use std::collections::HashMap;

/// (owner, member) -> every declaration of it: (file, type).
type Table = HashMap<(String, String), Vec<(u32, String)>>;
type Entry = ((String, String), (u32, String));

#[derive(Default)]
pub(super) struct DeclIndex {
    fields: Table,
    rets: Table,
    /// method name -> (file, type) of every method's declared return type
    methods: HashMap<String, Vec<(u32, String)>>,
}

fn table(files: &[FileRecord], pick: fn(&FileRecord) -> &[Member]) -> Table {
    let per_file: Vec<Vec<Entry>> = files
        .par_iter()
        .enumerate()
        .map(|(fi, f)| pick(f).iter().map(|m| ((m.owner.clone(), m.name.clone()), (fi as u32, m.ty.clone()))).collect())
        .collect();
    let mut t: Table = HashMap::new();
    for (k, v) in per_file.into_iter().flatten() {
        t.entry(k).or_default().push(v);
    }
    t
}


impl DeclIndex {
    pub(super) fn build(files: &[FileRecord]) -> DeclIndex {
        let (fields, rets) = rayon::join(|| table(files, |f| &f.ex.fields), || table(files, |f| &f.ex.rets));
        let mut methods: HashMap<String, Vec<(u32, String)>> = HashMap::new();
        for (fi, f) in files.iter().enumerate() {
            for m in f.ex.rets.iter().filter(|m| !m.owner.is_empty() && !m.assoc) {
                methods.entry(m.name.clone()).or_default().push((fi as u32, m.ty.clone()));
            }
        }
        DeclIndex { fields, rets, methods }
    }

    /// The shape of `c`'s receiver: stated in its file, or settled from the repo's declarations.
    pub(super) fn receiver(&self, supers: &Supers, files: Files, imports: &[Vec<u32>], fi: u32, c: &CallSite) -> Option<String> {
        if let Some(t) = &c.rtype {
            return Some(t.clone());
        }
        hint::settle(c.rhint.as_deref()?, &Lookup { idx: self, supers, files, imports, fi })
    }
}

/// The repo's declarations as seen from file `fi`; members are inherited through the hierarchy.
struct Lookup<'a> {
    idx: &'a DeclIndex,
    supers: &'a Supers,
    files: Files<'a>,
    imports: &'a [Vec<u32>],
    fi: u32,
}

impl Lookup<'_> {
    fn which(&self, name: &str, qual: Option<&str>, at: u32) -> Which {
        self.supers.defs.which(name, qual, at, self.files, self.imports)
    }

    /// `ty` as declared in file `at`, with each same-named type pinned to the definition `at` means
    /// (`@N/Name`), or marked undecidable (`?/Name`): the caller's own imports say nothing about it.
    fn pin(&self, ty: &str, at: u32) -> String {
        let pin1 = |t: &str| {
            let (q, n) = hint::split_qual(t);
            match self.which(n, q, at) {
                Which::Only => n.to_string(),
                Which::In(d) => format!("@{d}/{n}"),
                Which::Unknown => format!("?/{n}"),
            }
        };
        if !hint::is_shape(ty) {
            return ty.to_string();
        }
        match hint::split_shape(ty) {
            (b, Some(i)) => format!("{}<{}>", pin1(b), pin1(i)),
            (b, None) => pin1(b),
        }
    }

    /// One declaration among `decls`: when they disagree, the caller's file's own, else the one in a
    /// file it imports.
    fn choose(&self, decls: &[(u32, String)]) -> Option<String> {
        let first = decls.first()?;
        if decls.iter().all(|d| d.1 == first.1) {
            return Some(self.pin(&first.1, first.0));
        }
        let imp = self.imports.get(self.fi as usize).map(Vec::as_slice).unwrap_or(&[]);
        let d = decls.iter().find(|d| d.0 == self.fi).or_else(|| {
            let v: Vec<&(u32, String)> = decls.iter().filter(|d| imp.binary_search(&d.0).is_ok()).collect();
            (v.len() == 1).then(|| v[0])
        })?;
        Some(self.pin(&d.1, d.0))
    }

    fn find(&self, t: &Table, owner: &str, name: &str) -> Option<String> {
        if owner.is_empty() {
            return self.choose(t.get(&(String::new(), name.to_string()))?);
        }
        /* a type name several types share: only declarations in the package of the one meant */
        let (q, base) = hint::split_qual(owner);
        let pkg = match self.which(base, q, self.fi) {
            Which::Only => None,
            Which::In(d) => Some(package_of(&self.files.rec(d).path)),
            Which::Unknown => return None,
        };
        for (depth, level) in self.supers.levels(base).enumerate() {
            for o in level {
                if let Some(d) = t.get(&(o.to_string(), name.to_string())) {
                    let d: Vec<(u32, String)> = match pkg {
                        Some(p) if depth == 0 => d.iter().filter(|x| package_of(&self.files.rec(x.0).path) == p).cloned().collect(),
                        _ => d.clone(),
                    };
                    if !d.is_empty() {
                        return self.choose(&d);
                    }
                }
            }
        }
        None
    }
}

impl Decls for Lookup<'_> {
    fn field(&self, owner: &str, name: &str) -> Option<String> {
        self.find(&self.idx.fields, owner, name)
    }
    fn ret(&self, owner: &str, name: &str) -> Option<String> {
        self.find(&self.idx.rets, owner, name)
    }
    fn ctor_fallback(&self) -> bool {
        true
    }
    fn ret_any(&self, name: &str) -> Option<String> {
        let d = self.idx.methods.get(name)?;
        let first = self.pin(&d.first()?.1, d[0].0);
        d.iter().skip(1).all(|(f, t)| self.pin(t, *f) == first).then_some(first)
    }
}

/// Member names whose declarations differ between two versions of a file, plus the fields anywhere
/// whose declared type is a hint naming one of them (`self.db = connect()` moves with `connect`).
pub(super) fn moved_members(old: &crate::parse::Extract, new: &crate::parse::Extract, files: &[FileRecord]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (a, b) in [(&old.fields, &new.fields), (&old.rets, &new.rets)] {
        for m in a.iter().filter(|m| !b.contains(m)).chain(b.iter().filter(|m| !a.contains(m))) {
            if !out.contains(&m.name) {
                out.push(m.name.clone());
            }
        }
    }
    let mut seen = 0;
    while seen < out.len() {
        let fresh: Vec<String> = out[seen..].to_vec();
        seen = out.len();
        for m in files.iter().flat_map(|f| &f.ex.fields).filter(|m| !hint::is_shape(&m.ty)) {
            if !out.contains(&m.name) && fresh.iter().any(|n| hint_names(&m.ty, n)) {
                out.push(m.name.clone());
            }
        }
    }
    out
}

/// Hint `h` looks up a member named `name` (`.name` or `::name()`).
pub(super) fn hint_names(h: &str, name: &str) -> bool {
    h.match_indices(name).any(|(i, _)| {
        let before = &h[..i];
        let after = &h[i + name.len()..];
        (before.ends_with('.') || before.ends_with("::")) && (after.is_empty() || after.starts_with(['.', ':', '!', '(']))
    })
}
