//! Type hints: how a receiver's type follows from declarations, possibly in other files.
//!
//! A hint is an atom followed by steps, read left to right:
//! - atom: a type `Name` or `Wrapper<Name>`, or nothing (a free function call follows)
//! - `.field`: the declared type of that field
//! - `::name()`: the declared return type of that method (or free function after an empty atom)
//! - `!`: unwrapped (`?`, `.unwrap()`, `.expect()`, `await`): `Result<T>` / `Option<T>` / `Promise<T>` -> `T`
//!
//! - `?`: an atom for a receiver nothing types; only a method that one repo type alone declares can
//!   follow it (`builder.make_editor()` with `builder` untyped: `?::make_editor()`)
//!
//! `self.db.conn()?` in `impl Svc` is `Svc.db::conn()!`.

/// Wrappers that `?`, `.unwrap()` and `await` take apart.
const UNWRAP: &[&str] = &["Result", "Option", "Promise", "Awaitable", "Coroutine"];
/// Smart pointers a method call sees through.
const DEREF: &[&str] = &["Box", "Rc", "Arc", "Ref", "RefMut", "MutexGuard", "RwLockReadGuard", "RwLockWriteGuard"];
/// Methods a smart pointer answers itself instead of passing them to what it points to.
const POINTER_OWN: &[&str] = &["clone", "as_ref", "as_mut", "borrow", "borrow_mut", "deref", "downgrade", "as_ptr", "get_mut", "make_mut", "into_inner", "lock", "read", "write"];
/// Methods that hand back their receiver's type (`x.clone()`).
pub(crate) const SAME_TYPE: &[&str] = &["clone", "to_owned"];
/// Methods that unwrap their receiver.
pub(crate) const UNWRAPPING: &[&str] = &["unwrap", "expect", "unwrap_or_default", "unwrap_unchecked"];
/// Longest hint kept: deeper chains are left untyped.
pub(crate) const MAX_LEN: usize = 160;

/// `ast/Path` (written `ast::Path`) -> (Some(`ast`), `Path`); `@12/Path` names the definition in file 12.
pub(crate) fn split_qual(t: &str) -> (Option<&str>, &str) {
    match t.rsplit_once('/') {
        Some((q, n)) => (Some(q), n),
        None => (None, t),
    }
}

/// A wrapper whose inner type a shape keeps (`Result<T>`, `Arc<T>`).
pub(crate) fn wraps(base: &str) -> bool {
    UNWRAP.contains(&base) || DEREF.contains(&base)
}

/// `Wrapper<Inner>` -> (`Wrapper`, Some(`Inner`)); `Name` -> (`Name`, None).
pub(crate) fn split_shape(s: &str) -> (&str, Option<&str>) {
    match s.split_once('<') {
        Some((b, rest)) => (b, Some(rest.trim_end_matches('>')).filter(|i| !i.is_empty())),
        None => (s, None),
    }
}

/// The type a method call on a value of shape `s` looks in: through smart pointers, unless the
/// pointer has that method itself.
pub(crate) fn receiver_name<'s>(s: &'s str, method: &str) -> &'s str {
    let (b, inner) = split_shape(s);
    match inner {
        Some(i) if DEREF.contains(&b) && !POINTER_OWN.contains(&method) => i,
        _ => b,
    }
}

/// Declarations a hint is settled against.
pub(crate) trait Decls {
    /// Declared type of `owner.name`.
    fn field(&self, owner: &str, name: &str) -> Option<String>;
    /// Declared return type of `owner::name()` (`owner` "" = a free function).
    fn ret(&self, owner: &str, name: &str) -> Option<String>;
    /// `T::new()` and the like return `T` when no return type is known.
    fn ctor_fallback(&self) -> bool;
    /// Declared return type of method `name` on an untyped receiver: only when every declaration of
    /// it in the repo agrees.
    fn ret_any(&self, _name: &str) -> Option<String> {
        None
    }
}

/// Methods the standard library has too: a receiver of unknown type may well be calling those.
const STD_METHODS: &[&str] = &[
    "iter", "iter_mut", "into_iter", "map", "and_then", "or_else", "filter", "filter_map", "flat_map", "collect", "next",
    "get", "get_mut", "len", "is_empty", "push", "pop", "insert", "remove", "contains", "contains_key", "unwrap_or",
    "unwrap_or_else", "ok", "err", "ok_or", "ok_or_else", "as_ref", "as_mut", "as_str", "as_slice", "as_deref",
    "to_string", "to_owned", "to_vec", "into", "from", "first", "last", "find", "find_map", "any", "all", "keys",
    "values", "entry", "chars", "bytes", "split", "join", "trim", "parse", "cloned", "copied", "rev", "enumerate", "zip",
    "chain", "take", "skip", "sum", "count", "max", "min", "fold", "flatten", "sort", "sort_by", "dedup", "extend",
    "retain", "drain", "fmt", "eq", "cmp", "partial_cmp", "hash", "deref", "borrow", "lock", "read", "write", "then",
    "map_err", "is_some", "is_none", "is_ok", "is_err", "take_while", "skip_while", "position", "windows", "chunks",
    "lines", "starts_with", "ends_with", "replace", "to_lowercase", "to_uppercase", "with_capacity", "append", "clear",
    "split_off", "truncate", "resize", "swap", "get_or_insert_with", "or_insert", "or_insert_with", "or_default",
    "then_some", "filter_map", "inspect", "by_ref", "step_by", "last_mut", "first_mut", "send", "recv", "spawn",
    "forEach", "reduce", "some", "every", "findIndex", "catch", "finally", "flatMap", "includes", "indexOf", "slice",
    "splice", "concat", "entries", "has", "set", "add", "delete", "addEventListener", "removeEventListener", "on",
    "once", "emit", "toString", "valueOf", "apply", "call", "bind", "append", "items", "update", "setdefault",
];

/// `?::m()` may stand for the receiver of an untyped call of `m`: not when std has an `m` too.
pub(crate) fn untyped_method_ok(m: &str) -> bool {
    !STD_METHODS.contains(&m) && !UNWRAPPING.contains(&m) && !SAME_TYPE.contains(&m)
}

fn is_ctor(name: &str) -> bool {
    matches!(name, "new" | "default") || name.starts_with("new_") || name.starts_with("with_")
}

/// The shape a hint stands for, when the declarations settle it.
pub(crate) fn settle(hint: &str, d: &impl Decls) -> Option<String> {
    settle_at(hint, d, 0)
}

/// `settle`, `depth` hints deep: a field declared as a hint (`self.db = connect()`) is settled in turn.
fn settle_at(hint: &str, d: &impl Decls, depth: u8) -> Option<String> {
    let deeper = |t: String| if is_shape(&t) { Some(t) } else if depth < 4 { settle_at(&t, d, depth + 1) } else { None };
    let end = hint.find(['.', ':', '!']).unwrap_or(hint.len());
    let mut cur: Option<String> = (end > 0).then(|| hint[..end].to_string());
    let mut rest = &hint[end..];
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('!') {
            let c = cur?;
            let (b, inner) = split_shape(&c);
            cur = Some(match inner {
                Some(i) if UNWRAP.contains(&b) => i.to_string(),
                _ => c.clone(),
            });
            rest = r;
        } else if let Some(r) = rest.strip_prefix('.') {
            let n = r.find(['.', ':', '!']).unwrap_or(r.len());
            let c = cur.as_deref()?;
            cur = match (c.strip_prefix('(').and_then(|t| t.strip_suffix(')')), r[..n].parse::<usize>()) {
                /* an element of a tuple: `(A,B).1` */
                (Some(parts), Ok(i)) => Some(parts.split(',').nth(i).filter(|p| *p != "_")?.to_string()),
                (Some(_), Err(_)) => return None,
                (None, _) => {
                    let owner = receiver_name(c, "").to_string();
                    Some(deeper(d.field(&owner, &r[..n])?)?)
                }
            };
            rest = &r[n..];
        } else if let Some(r) = rest.strip_prefix("::") {
            let n = r.find("()")?;
            let name = &r[..n];
            cur = match cur {
                None => Some(d.ret("", name)?),
                Some(c) if c == "?" => Some(d.ret_any(name)?),
                Some(c) => {
                    let owner = receiver_name(&c, name).to_string();
                    match d.ret(&owner, name) {
                        /* a trait method's `Self` is the type it is called on */
                        Some(t) => Some(subst_self(&t, &owner)),
                        None if d.ctor_fallback() && is_ctor(name) => Some(owner),
                        None => return None,
                    }
                }
            };
            rest = &r[n + 2..];
        } else {
            return None;
        }
    }
    cur.filter(|c| c != "?")
}

/// `hint` with the atom `Self` (also inside `Wrapper<Self>`) replaced by `owner`.
pub(crate) fn subst_self(hint: &str, owner: &str) -> String {
    let mut out = String::with_capacity(hint.len() + owner.len());
    let b = hint.as_bytes();
    let mut i = 0;
    while let Some(off) = hint[i..].find("Self") {
        let at = i + off;
        let before_ok = at == 0 || !(b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_');
        let after = at + 4;
        let after_ok = after >= b.len() || !(b[after].is_ascii_alphanumeric() || b[after] == b'_');
        out.push_str(&hint[i..at]);
        out.push_str(if before_ok && after_ok { owner } else { "Self" });
        i = after;
    }
    out.push_str(&hint[i..]);
    out
}

/// The hint is a plain shape: nothing left to look up.
pub(crate) fn is_shape(hint: &str) -> bool {
    !hint.contains(['.', ':', '!'])
}

#[cfg(test)]
mod tests {
    use super::*;

    struct D;
    impl Decls for D {
        fn field(&self, owner: &str, name: &str) -> Option<String> {
            match (owner, name) {
                ("Svc", "db") => Some("Arc<Db>".into()),
                ("Db", "pool") => Some("Pool".into()),
                _ => None,
            }
        }
        fn ret(&self, owner: &str, name: &str) -> Option<String> {
            match (owner, name) {
                ("Db", "conn") => Some("Result<Conn>".into()),
                ("", "open") => Some("Option<Db>".into()),
                ("Db", "fork") => Some("Self".into()),
                ("Db", "split") => Some("(Conn,Pool)".into()),
                _ => None,
            }
        }
        fn ctor_fallback(&self) -> bool {
            true
        }
    }

    #[test]
    fn steps() {
        assert_eq!(settle("Svc.db", &D).as_deref(), Some("Arc<Db>"));
        assert_eq!(settle("Svc.db.pool", &D).as_deref(), Some("Pool"));
        assert_eq!(settle("Svc.db::conn()!", &D).as_deref(), Some("Conn"));
        assert_eq!(settle("Svc.db::conn()", &D).as_deref(), Some("Result<Conn>"));
        assert_eq!(settle("::open()!", &D).as_deref(), Some("Db"));
        assert_eq!(settle("Repo::new()", &D).as_deref(), Some("Repo"));
        assert_eq!(settle("Repo::get()", &D), None);
        assert_eq!(settle("Svc.db::fork()", &D).as_deref(), Some("Db"));
        assert_eq!(settle("Db::split().1", &D).as_deref(), Some("Pool"));
        assert_eq!(receiver_name("Arc<Db>", "query"), "Db");
        assert_eq!(receiver_name("Arc<Db>", "clone"), "Arc");
        assert_eq!(subst_self("Self.db::f()", "Svc"), "Svc.db::f()");
        assert_eq!(subst_self("Arc<Self>", "Svc"), "Arc<Svc>");
        assert_eq!(subst_self("Selfish", "Svc"), "Selfish");
    }
}
