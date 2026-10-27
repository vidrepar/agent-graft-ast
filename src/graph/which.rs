//! Which definition a type name means when the repo defines several types of that name (`ast::Path`
//! and `hir_def::Path`): decided by what the code names or imports, never by guessing.

use super::{family, Files};
use crate::refactor::package_of;
use std::collections::HashMap;

/// What a type name refers to, seen from one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Which {
    /// one definition (or none): nothing to tell apart
    Only,
    /// the definition in this file
    In(u32),
    /// several, and the code does not say which
    Unknown,
}

/// Type name -> files defining a type of that name (impl blocks excluded), sorted.
#[derive(Default)]
pub(super) struct TypeDefs(pub(super) HashMap<String, Vec<u32>>);

/// A file path as module-like words: `crates/hir-def/src/path.rs` -> crates, hir_def, src, path.
fn words(path: &str) -> impl Iterator<Item = String> + '_ {
    path.split('/').map(|s| s.split('.').next().unwrap_or(s).replace('-', "_"))
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

impl TypeDefs {
    /// The definition of `name` that file `at` means; `qual` is the module it was written under
    /// (`ast` in `ast::Path`), or `@N` when an earlier step already decided on file N.
    pub(super) fn which(&self, name: &str, qual: Option<&str>, at: u32, files: Files, imports: &[Vec<u32>]) -> Which {
        if let Some(n) = qual.and_then(|q| q.strip_prefix('@')).and_then(|n| n.parse().ok()) {
            return Which::In(n);
        }
        let Some(all) = self.0.get(name) else { return Which::Only };
        let fam = family(files.rec(at).lang);
        let defs: Vec<u32> = all.iter().copied().filter(|&d| family(files.rec(d).lang) == fam).collect();
        if defs.len() <= 1 {
            return Which::Only;
        }
        if qual == Some("?") {
            /* the file that declared this type could not tell, so neither can its users */
            return Which::Unknown;
        }
        let one = |v: Vec<u32>| if v.len() == 1 { Some(v[0]) } else { None };
        /* 1. `ast::Path`: the definition under a module of that name */
        if let Some(q) = qual {
            if let Some(d) = one(defs.iter().copied().filter(|&d| words(&files.rec(d).path).any(|w| w == q)).collect()) {
                return Which::In(d);
            }
        }
        /* 2. defined right here */
        if defs.contains(&at) {
            return Which::In(at);
        }
        /* 3. a file this one imports */
        let imp = imports.get(at as usize).map(Vec::as_slice).unwrap_or(&[]);
        if let Some(d) = one(defs.iter().copied().filter(|d| imp.binary_search(d).is_ok()).collect()) {
            return Which::In(d);
        }
        /* 4. a `use a::b::Name` the resolver could not follow (another crate): the definition whose path
         * shares the most words with it */
        let rec = files.rec(at);
        for raw in rec.ex.imports.iter().map(|i| i.raw.as_str()) {
            let Some(pos) = raw.match_indices(name).map(|(i, _)| i).find(|&i| {
                let b = raw.as_bytes();
                (i == 0 || !is_word(b[i - 1])) && b.get(i + name.len()).is_none_or(|c| !is_word(*c))
            }) else {
                continue;
            };
            /* `use crate::..::Name`: a definition in this crate */
            if raw.starts_with("crate") || raw.starts_with("self") || raw.starts_with("super") {
                let own = package_of(&rec.path);
                if let Some(d) = one(defs.iter().copied().filter(|&d| package_of(&files.rec(d).path) == own).collect()) {
                    return Which::In(d);
                }
            }
            let said: Vec<&str> = raw[..pos].split(|c: char| !is_word(c as u8)).filter(|w| !w.is_empty() && !matches!(*w, "crate" | "self" | "super")).collect();
            let score = |d: u32| words(&files.rec(d).path).filter(|w| said.contains(&w.as_str())).count();
            let best = defs.iter().map(|&d| score(d)).max().unwrap_or(0);
            if best > 0 {
                if let Some(d) = one(defs.iter().copied().filter(|&d| score(d) == best).collect()) {
                    return Which::In(d);
                }
            }
        }
        Which::Unknown
    }
}
