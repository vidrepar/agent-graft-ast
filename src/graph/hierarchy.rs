//! The type hierarchy the code declares: `impl Trait for Type`, `trait A: B`, `class C extends B
//! implements I`, `class C(B)`. Typed method calls climb it to find inherited methods.

use crate::index::FileRecord;
use crate::lang::Lang;
use crate::parse::{SymKind, Symbol};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

/// Standard-library types and traits: a repo definition with one of these names (a test fixture's
/// `Option`, a mock `Map`) is rarely the type a receiver has, so these never steer resolution.
const STD_NAMES: &[&str] = &[
    "Option", "Result", "Vec", "VecDeque", "String", "Box", "Rc", "Arc", "Cell", "RefCell", "Mutex", "RwLock", "Cow",
    "HashMap", "HashSet", "BTreeMap", "BTreeSet", "Iterator", "IntoIterator", "IntoIter", "Iter", "Clone", "Copy",
    "Default", "Debug", "Display", "PartialEq", "Eq", "PartialOrd", "Ord", "Hash", "Drop", "Fn", "FnMut", "FnOnce",
    "From", "Into", "AsRef", "AsMut", "Deref", "DerefMut", "ToString", "ToOwned", "Send", "Sync", "Sized", "Range",
    "Map", "Set", "WeakMap", "WeakSet", "Array", "ReadonlyArray", "Promise", "Date", "Error", "RegExp", "Object",
    "Number", "Record", "Partial", "Response", "Request", "URL", "Headers", "Dict", "List", "Tuple", "Formatter",
    "DebugStruct", "DebugTuple", "DebugList", "DebugSet", "DebugMap", "Arguments", "Peekable", "Chars", "Lines",
];

/// How many supertype levels a call may climb (`C` -> base -> base of base ...).
const MAX_DEPTH: usize = 6;

/// Type name -> the names of its direct supertypes, from impl, trait and class headers; only types
/// the repo defines are keys. A type defined elsewhere (`Vec`, `Map`) has methods the index never saw.
#[derive(Default)]
pub(super) struct Supers {
    repo: HashMap<String, Vec<String>>,
    /// where each type name is defined, to tell same-named types apart
    pub(super) defs: super::which::TypeDefs,
    /// every header, also `impl Tr for Vec<T>` and std supertypes: what a receiver of any type may call
    all: HashMap<String, Vec<String>>,
}

/// `a::b::Name<T>` / `mod.Name[T]` / `!Name` -> `Name`; None for anything that is not a plain name.
pub(super) fn bare(s: &str) -> Option<&str> {
    let s = s.trim().trim_start_matches('!');
    let s = &s[..s.find(['<', '(', '[', '{', ' ']).unwrap_or(s.len())];
    let s = s.rsplit("::").next().unwrap_or(s);
    let s = s.rsplit('.').next().unwrap_or(s);
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && !s.as_bytes()[0].is_ascii_digit()).then_some(s)
}

/// Split at top-level `sep` (outside `<>`, `()`, `[]`).
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0i32, 0);
    for (i, ch) in s.char_indices() {
        match ch {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            c if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Skip a balanced `<...>` at the start of `s`.
fn skip_generics(s: &str) -> &str {
    let s = s.trim_start();
    if !s.starts_with('<') {
        return s;
    }
    let mut depth = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return &s[i + 1..];
                }
            }
            _ => {}
        }
    }
    ""
}

/// The supertypes a type-defining symbol declares: (type, supertypes). A Rust impl counts only when it
/// implements a trait; a struct, enum, trait, class or interface always counts, with or without supertypes.
fn header(s: &Symbol, lang: Lang) -> Option<(&str, Vec<&str>)> {
    let defines = matches!(s.kind, SymKind::Struct | SymKind::Enum | SymKind::Trait | SymKind::Class | SymKind::Interface);
    if !defines && s.kind != SymKind::Impl {
        return None;
    }
    let supers = declared_supers(s, lang).unwrap_or_default();
    (defines || !supers.is_empty()).then_some((bare(&s.name)?, supers))
}

/// The names after `impl .. for`, `trait X:`, `extends`/`implements` or inside `class C(...)`.
fn declared_supers(s: &Symbol, lang: Lang) -> Option<Vec<&str>> {
    let sig = s.sig.as_str();
    Some(match (lang, s.kind) {
        (Lang::Rust, SymKind::Impl) => {
            /* `impl<T> path::Trait<T> for Type<T>`: the type is the symbol's name */
            let rest = skip_generics(sig.trim_start_matches("unsafe ").strip_prefix("impl")?);
            let tr = rest.split(" for ").next().filter(|_| rest.contains(" for "))?;
            vec![bare(tr)?]
        }
        (Lang::Rust, SymKind::Trait) => {
            let after = skip_generics(&sig[sig.find(&format!("trait {}", s.name))? + 6 + s.name.len()..]);
            let bounds = after.strip_prefix(':')?;
            let bounds = bounds.split(" where ").next().unwrap_or(bounds);
            split_top(bounds, '+').into_iter().filter_map(bare).collect()
        }
        (Lang::Python, SymKind::Class) => {
            let inner = sig[sig.find('(')? + 1..].trim_end().strip_suffix(')')?;
            split_top(inner, ',').into_iter().filter(|a| !a.contains('=')).filter_map(bare).collect()
        }
        (Lang::Rust | Lang::Python, _) => return None,
        (_, SymKind::Class | SymKind::Interface) => {
            let mut out = Vec::new();
            for kw in [" extends ", " implements "] {
                if let Some(i) = sig.find(kw) {
                    let rest = &sig[i + kw.len()..];
                    let rest = &rest[..rest.find(" implements ").unwrap_or(rest.len())];
                    out.extend(split_top(rest, ',').into_iter().filter_map(bare));
                }
            }
            out
        }
        _ => return None,
    })
}

/// Every type one file defines with its supertypes, as sorted (type, supertype or "") pairs:
/// equal lists mean the hierarchy did not change. Rust impls name a type without defining it.
pub(super) fn file_headers(f: &FileRecord) -> Vec<(&str, &str)> {
    let mut v: Vec<(&str, &str)> = Vec::new();
    for s in &f.ex.symbols {
        let Some((t, ss)) = header(s, f.lang) else { continue };
        if s.kind != SymKind::Impl {
            v.push((t, ""));
        }
        v.extend(ss.into_iter().map(|x| (t, x)));
    }
    v.sort_unstable();
    v.dedup();
    v
}

impl Supers {
    pub(super) fn build(files: &[FileRecord]) -> Supers {
        let mut all: HashMap<String, Vec<String>> = HashMap::new();
        /* types the repo defines; supertypes recorded only through `impl Tr for Vec<T>` are not its own */
        let mut defined: HashSet<&str> = HashSet::new();
        let mut defs: HashMap<String, Vec<u32>> = HashMap::new();
        let headers: Vec<Vec<(&str, &str)>> = files.par_iter().map(file_headers).collect();
        for (fi, (t, s)) in headers.into_iter().enumerate().flat_map(|(fi, h)| h.into_iter().map(move |x| (fi, x))) {
            if s.is_empty() {
                defined.insert(t);
                let v = defs.entry(t.to_string()).or_default();
                if v.last() != Some(&(fi as u32)) {
                    v.push(fi as u32);
                }
                continue;
            }
            let v = all.entry(t.to_string()).or_default();
            if t != s && !v.iter().any(|x| x == s) {
                v.push(s.to_string());
            }
        }
        let repo = defined
            .into_iter()
            .filter(|t| !STD_NAMES.contains(t))
            .map(|t| (t.to_string(), all.get(t).into_iter().flatten().filter(|s| !STD_NAMES.contains(&s.as_str())).cloned().collect()))
            .collect();
        Supers { repo, all, defs: super::which::TypeDefs(defs) }
    }

    /// `t` is a type the repo defines (and not a standard-library name).
    pub(super) fn is_repo_type(&self, t: &str) -> bool {
        self.repo.contains_key(t)
    }

    /// `t` and every type it reaches through declared supertypes (see `reaches`).
    pub(super) fn ancestors<'s>(&'s self, t: &'s str) -> Vec<&'s str> {
        let mut out = vec![t];
        let mut i = 0;
        while i < out.len() && out.len() < 64 {
            for s in self.all.get(out[i]).into_iter().flatten() {
                if !out.contains(&s.as_str()) {
                    out.push(s);
                }
            }
            i += 1;
        }
        out
    }

    /// A value of type `t` can have a method defined on `owner`: `t` is `owner` or reaches it through
    /// the traits, base classes and interfaces the repo declares for it.
    pub(super) fn reaches(&self, t: &str, owner: &str) -> bool {
        let (mut level, mut seen) = (vec![t], HashSet::from([t]));
        for _ in 0..MAX_DEPTH {
            if level.contains(&owner) {
                return true;
            }
            level = level.iter().flat_map(|x| self.all.get(*x).into_iter().flatten()).map(String::as_str).filter(|x| seen.insert(x)).collect();
        }
        false
    }

    /// `t`, then its direct supertypes, then theirs: one level at a time.
    pub(super) fn levels<'s>(&'s self, t: &'s str) -> impl Iterator<Item = Vec<&'s str>> + 's {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut level = vec![t];
        std::iter::from_fn(move || {
            if level.is_empty() {
                return None;
            }
            if seen.is_empty() {
                /* most calls stop at the type itself: no hashing until a supertype is needed */
                seen.insert(t);
                return Some(level.clone());
            }
            let next: Vec<&str> = level
                .iter()
                .flat_map(|x| self.repo.get(*x).into_iter().flatten())
                .map(String::as_str)
                .filter(|x| seen.insert(x))
                .collect();
            Some(std::mem::replace(&mut level, next))
        })
        .take(MAX_DEPTH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::extract;

    fn rec(path: &str, src: &str) -> FileRecord {
        let lang = Lang::from_path(std::path::Path::new(path)).unwrap();
        FileRecord { path: path.into(), lang, mtime_ns: 0, size: src.len() as u64, hash: 1, ex: extract(lang, src.as_bytes()) }
    }

    #[test]
    fn headers_in_three_languages() {
        let f = rec("a.rs", "trait Base: Clone + std::fmt::Debug {}\nstruct S;\nimpl<T: X> crate::m::Tr<T> for S {}\nimpl S {}\nunsafe impl Send for S {}\n");
        assert_eq!(file_headers(&f), [("Base", ""), ("Base", "Clone"), ("Base", "Debug"), ("S", ""), ("S", "Send"), ("S", "Tr")]);
        let f = rec("a.ts", "export class C<T> extends B<T> implements I, ns.J {}\ninterface I extends K {}\nclass D {}\n");
        assert_eq!(file_headers(&f), [("C", ""), ("C", "B"), ("C", "I"), ("C", "J"), ("D", ""), ("I", ""), ("I", "K")]);
        let f = rec("a.py", "class C(B, mod.M, metaclass=Meta):\n    pass\nclass D:\n    pass\n");
        assert_eq!(file_headers(&f), [("C", ""), ("C", "B"), ("C", "M"), ("D", "")]);
    }
}
