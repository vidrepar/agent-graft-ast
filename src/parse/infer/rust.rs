//! Rust receivers: `self` in an impl, `let x = T::new()` / `let x: T` / `let x = f()?`, `x: &T`
//! parameters, field chains (`self.db.pool`), method chains, `T::f()` and `<T as Tr>::f()`.
//! Struct field types and return types are declared for other files to use.

use super::{fld, range, shape, step, upper, usable, Ty, F};
use crate::parse::{text, Extractor};
use std::borrow::Cow;
use tree_sitter::Node;

/// Rust: the type a type node names, as a shape (`&mut a::Foo<T>` -> `Foo`, `Result<Repo, E>` ->
/// `Result<Repo>`, `dyn Tr` -> `Tr`, `Self` -> `Self`).
fn rust_type<'a>(n: Node, src: &'a [u8]) -> Option<Ty<'a>> {
    match n.kind() {
        "type_identifier" if text(n, src) == "Self" => Some(Cow::Borrowed("Self")),
        "type_identifier" => usable(text(n, src)).map(Cow::Borrowed),
        "reference_type" => rust_type(fld(n, F::Type)?, src),
        "generic_type" => {
            let base = rust_type(fld(n, F::Type)?, src)?;
            let inner = fld(n, F::TypeArguments).and_then(|a| a.named_child(0)).and_then(|t| rust_type(t, src));
            match base {
                Cow::Borrowed(b) => Some(shape(b, inner.as_deref().filter(|i| super::hint::is_shape(i) && !i.contains('<')))),
                owned => Some(owned),
            }
        }
        /* `std::sync::Arc<T>` keeps its wrapper; any other std type is not the repo's */
        "scoped_type_identifier" if std_path(fld(n, F::Path)?, src) => {
            Some(text(fld(n, F::Name)?, src)).filter(|t| super::hint::wraps(t)).map(Cow::Borrowed)
        }
        "scoped_type_identifier" => qualified(fld(n, F::Path)?, text(fld(n, F::Name)?, src), src),
        "abstract_type" | "dynamic_type" => rust_type(fld(n, F::Trait)?, src),
        /* `(SyntaxEditor, Mappings)`: element types, for `let (editor, _) = ...` */
        "tuple_type" => {
            let parts: Vec<String> = n.named_children(&mut n.walk()).take(6).map(|t| rust_type(t, src).map_or("_".into(), |c| c.into_owned())).collect();
            parts.iter().any(|p| p != "_" && super::hint::is_shape(p) && !p.contains(['(', ','])).then(|| Cow::Owned(format!("({})", parts.join(","))))
        }
        _ => None,
    }
}

/// Standard-library modules: `fmt::Formatter` is std's even when a test fixture defines a `Formatter`.
const STD_MODULES: &[&str] = &[
    "std", "core", "alloc", "fmt", "io", "sync", "collections", "ops", "iter", "cell", "rc", "borrow", "hash", "cmp",
    "mem", "ptr", "str", "string", "vec", "thread", "time", "path", "fs", "env", "process", "net", "num", "slice",
    "marker", "convert", "error", "any", "future", "task", "pin", "ffi", "os", "atomic", "mpsc", "hash_map", "btree_map",
];

/// A path rooted in the standard library (`std::fmt`, `fmt`, `io::Error`'s `io`).
fn std_path(p: Node, src: &[u8]) -> bool {
    let t = text(p, src);
    let root = t.split("::").next().unwrap_or(t);
    STD_MODULES.contains(&root.trim_start_matches("::"))
}

/// `a::b::Name` -> `b/Name`, keeping the module so same-named types can be told apart; `crate::Name`,
/// `self::Name`, `super::Name` -> `Name`.
fn qualified<'a>(path: Node, name: &str, src: &'a [u8]) -> Option<Ty<'a>> {
    usable(name)?;
    let q = text(path, src).rsplit("::").next().unwrap_or("");
    Some(match q {
        "crate" | "self" | "super" | "" => Cow::Owned(name.to_string()),
        q if usable(q).is_some() => Cow::Owned(format!("{q}/{name}")),
        _ => Cow::Owned(name.to_string()),
    })
}

/// Rust: the type a path before `::` names, when it looks like a type (`a::Foo`, `Self`, `<T as Tr>`).
fn rust_path_type<'a>(p: Node, src: &'a [u8]) -> Option<Ty<'a>> {
    match p.kind() {
        "identifier" | "type_identifier" | "self" if text(p, src) == "Self" => Some(Cow::Borrowed("Self")),
        "identifier" | "type_identifier" => Some(text(p, src)).filter(|t| upper(t)).and_then(usable).map(Cow::Borrowed),
        "scoped_identifier" if std_path(p, src) => None,
        "scoped_identifier" => {
            let name = rust_path_type(fld(p, F::Name)?, src)?;
            match fld(p, F::Path) {
                Some(q) if name != "Self" => qualified(q, &name, src).map(|t| Cow::Owned(t.into_owned())),
                _ => Some(name),
            }
        }
        "generic_type" => rust_type(fld(p, F::Type)?, src),
        "bracketed_type" => {
            let q = p.named_child(0)?;
            rust_type(if q.kind() == "qualified_type" { fld(q, F::Type)? } else { q }, src)
        }
        _ => None,
    }
}

impl<'a> Extractor<'a> {
    pub(super) fn bind_rust(&mut self, node: Node<'a>, func: Option<u32>, parent: Option<u32>) {
        let src = self.src;
        match node.kind() {
            "macro_invocation" => self.macro_calls(node, func, parent),
            "function_item" | "function_signature_item" => {
                if let (Some(name), Some(rt)) = (fld(node, F::Name), fld(node, F::ReturnType).and_then(|t| rust_type(t, src))) {
                    let owner = self.owner(None, parent);
                    let in_trait = parent.is_some_and(|p| self.out.symbols[p as usize].kind == crate::parse::SymKind::Trait);
                    let n = self.out.rets.len();
                    self.declare(false, owner.as_deref(), in_trait, text(name, src), rt);
                    if let (Some(m), Some(params)) = (self.out.rets.get_mut(n), fld(node, F::Parameters)) {
                        m.assoc = !params.named_children(&mut params.walk()).any(|p| p.kind() == "self_parameter");
                    }
                }
                self.declare_rust_callbacks(node, parent);
                let (Some(params), Some(body)) = (fld(node, F::Parameters), fld(node, F::Body)) else { return };
                let mut c = params.walk();
                for p in params.named_children(&mut c).filter(|p| p.kind() == "parameter") {
                    let (Some(pat), ty) = (fld(p, F::Pattern), fld(p, F::Type)) else { continue };
                    if pat.kind() == "identifier" {
                        self.env.bind(text(pat, src), ty.and_then(|t| rust_type(t, src)), body.start_byte(), body.end_byte());
                    }
                }
            }
            "struct_item" => {
                let (Some(name), Some(body)) = (fld(node, F::Name), fld(node, F::Body)) else { return };
                let owner = text(name, src);
                let mut c = body.walk();
                for f in body.named_children(&mut c).filter(|f| f.kind() == "field_declaration") {
                    if let (Some(n), Some(t)) = (fld(f, F::Name), fld(f, F::Type).and_then(|t| rust_type(t, src))) {
                        self.declare(true, Some(owner), false, text(n, src), t);
                    }
                }
            }
            "let_declaration" => {
                let Some(pat) = fld(node, F::Pattern) else { return };
                /* the binding lives to the end of its block */
                let block = || (0, node.parent().map(|b| b.end_byte()).unwrap_or(src.len()));
                if pat.kind() == "identifier" {
                    let ty = match fld(node, F::Type) {
                        Some(t) => rust_type(t, src),
                        None => fld(node, F::Value).and_then(|v| self.expr_ty(v, 0)),
                    };
                    let name = text(pat, src);
                    if ty.is_some() || self.env.hides(name, node.end_byte()) {
                        /* the block's end matters only when an outer `name` lives on after it (finding the
                         * block is a parent walk); otherwise the function's end is as good */
                        let to = match (self.env.lookup(name, node.end_byte()), func) {
                            (None, Some(f)) => self.sym_ends[f as usize],
                            _ => block().1,
                        };
                        self.env.bind(name, ty, node.end_byte(), to);
                    }
                } else {
                    self.shadow_pattern(pat, node.end_byte(), block);
                    self.bind_tuple(node, pat, func);
                }
            }
            "closure_expression" => {
                let Some(params) = fld(node, F::Parameters) else { return };
                /* `|t|` passed to `f(.., impl FnOnce(&mut T))`: t is a T; `|t: &mut T|` says so itself */
                let site = if params.named_children(&mut params.walk()).any(|p| p.kind() == "identifier") { self.callback_site(node) } else { None };
                let kids: Vec<Node<'a>> = params.named_children(&mut params.walk()).collect();
                for (j, p) in kids.into_iter().enumerate() {
                    let (name, ty) = match p.kind() {
                        "identifier" => (p, site.as_ref().and_then(|s| Self::callback_param(s, j))),
                        "parameter" => match (fld(p, F::Pattern), fld(p, F::Type)) {
                            (Some(n), t) if n.kind() == "identifier" => (n, t.and_then(|t| rust_type(t, src))),
                            _ => {
                                self.shadow_pattern(p, node.start_byte(), || range(node));
                                continue;
                            }
                        },
                        _ => {
                            self.shadow_pattern(p, node.start_byte(), || range(node));
                            continue;
                        }
                    };
                    self.env.bind(text(name, src), ty, node.start_byte(), node.end_byte());
                }
            }
            "for_expression" | "match_arm" => {
                if let Some(p) = fld(node, F::Pattern) {
                    self.shadow_pattern(p, node.start_byte(), || range(node));
                }
            }
            "let_condition" => {
                if let Some(p) = fld(node, F::Pattern) {
                    self.shadow_pattern(p, node.start_byte(), || node.parent().map(range).unwrap_or(range(node)));
                }
            }
            _ => {}
        }
    }

    /// `T` in `T::f(..)`, or None for a module path (`util::f`).
    pub(super) fn rust_path_owner(&self, scoped: Node<'a>) -> Option<Ty<'a>> {
        rust_path_type(fld(scoped, F::Path)?, self.src)
    }

    /// Callback parameters of a Rust fn: for each parameter typed `impl Fn*(..)`, `Box<dyn Fn*(..)>`,
    /// `&dyn Fn*(..)` or a generic `F: Fn*(..)` (bound inline or in `where`), the types it is called with.
    fn declare_rust_callbacks(&mut self, node: Node<'a>, parent: Option<u32>) {
        let src = self.src;
        let (Some(name), Some(params)) = (fld(node, F::Name), fld(node, F::Parameters)) else { return };
        let kids: Vec<Node<'a>> = params.named_children(&mut params.walk()).filter(|p| p.kind() == "parameter").collect();
        /* no `Fn` anywhere in the header (parameters, generics, where clause): nothing to declare */
        let head_end = fld(node, F::Body).map_or(node.end_byte(), |b| b.start_byte());
        if kids.is_empty() || memchr::memmem::find(&src[node.start_byte()..head_end], b"Fn").is_none() {
            return;
        }
        /* generic parameters bound to a closure type: `F: FnOnce(&mut T)` / `where F: Fn(T)` */
        let mut bounds: Vec<(&'a str, Node<'a>)> = Vec::new();
        let mut add = |n: Option<Node<'a>>, b: Option<Node<'a>>| {
            if let (Some(n), Some(b)) = (n, b) {
                if let Some(ft) = b.named_children(&mut b.walk()).find(|t| t.kind() == "function_type") {
                    bounds.push((text(n, src), ft));
                }
            }
        };
        if let Some(tp) = fld(node, F::TypeParameters) {
            for p in tp.named_children(&mut tp.walk()) {
                add(fld(p, F::Name), fld(p, F::Bounds));
            }
        }
        if let Some(w) = node.named_children(&mut node.walk()).find(|c| c.kind() == "where_clause") {
            for p in w.named_children(&mut w.walk()) {
                add(fld(p, F::Left), fld(p, F::Bounds));
            }
        }
        let fn_type = |t: Node<'a>| -> Option<Node<'a>> {
            let mut t = t;
            for _ in 0..4 {
                t = match t.kind() {
                    "function_type" => return Some(t),
                    "abstract_type" | "dynamic_type" => fld(t, F::Trait)?,
                    "reference_type" => fld(t, F::Type)?,
                    "generic_type" if matches!(text(fld(t, F::Type)?, src), "Box" | "Arc" | "Rc") => fld(t, F::TypeArguments)?.named_child(0)?,
                    "type_identifier" => return bounds.iter().find(|b| b.0 == text(t, src)).map(|b| b.1),
                    _ => return None,
                };
            }
            None
        };
        let owner = self.owner(None, parent);
        let in_trait = parent.is_some_and(|p| self.out.symbols[p as usize].kind == crate::parse::SymKind::Trait);
        for (i, p) in kids.into_iter().enumerate() {
            let Some(ft) = fld(p, F::Type).and_then(fn_type) else { continue };
            let Some(ps) = fld(ft, F::Parameters) else { continue };
            let args: Vec<String> = ps.named_children(&mut ps.walk()).map(|a| rust_type(a, src).map_or("_".to_string(), |c| c.into_owned())).collect();
            self.declare_callback(owner.as_deref(), in_trait, text(name, src), i, args);
        }
    }

    /// `let (a, b) = f();`: each name gets its element of the tuple f returns (`f().0`, `f().1`).
    fn bind_tuple(&mut self, node: Node<'a>, pat: Node<'a>, func: Option<u32>) {
        let src = self.src;
        if pat.kind() != "tuple_pattern" || fld(node, F::Type).is_some() {
            return;
        }
        /* every element counts for the index, `_` too (an unnamed token) */
        let elems: Vec<Node<'a>> = pat.children(&mut pat.walk()).filter(|c| !matches!(c.kind(), "(" | ")" | ",")).collect();
        if !elems.iter().any(|e| e.kind() == "identifier") {
            return;
        }
        let Some(ty) = fld(node, F::Value).and_then(|v| self.expr_ty(v, 0)) else { return };
        let to = func.map(|f| self.sym_ends[f as usize]).unwrap_or(src.len());
        for (i, e) in elems.into_iter().enumerate().filter(|(_, e)| e.kind() == "identifier") {
            if let Some(t) = step(ty.clone(), &format!(".{i}")) {
                self.env.bind(text(e, src), Some(t), node.end_byte(), to);
            }
        }
    }

    /// `x.f(..)` / `self.f(..)` inside a macro's raw tokens (`vec![x.f()]`): a call extraction never
    /// sees as a node, recorded with the receiver's type when `x` is a typed local.
    fn macro_calls(&mut self, mac: Node<'a>, func: Option<u32>, parent: Option<u32>) {
        let src = self.src;
        /* no `.` in the arguments, no method call: most macros stop here */
        if memchr::memchr(b'.', &src[mac.byte_range()]).is_none() {
            return;
        }
        let Some(tt) = mac.named_children(&mut mac.walk()).find(|c| c.kind() == "token_tree") else { return };
        let mut stack = vec![tt];
        let mut found: Vec<(Node<'a>, Node<'a>)> = Vec::new();
        while let Some(t) = stack.pop() {
            /* a sliding window of four tokens: `recv . name (..)` */
            let mut c = t.walk();
            let mut w: [Option<Node<'a>>; 4] = [None; 4];
            let mut more = c.goto_first_child();
            while more {
                let k = c.node();
                w = [w[1], w[2], w[3], Some(k)];
                if k.kind() == "token_tree" {
                    stack.push(k);
                    if let [Some(r), Some(dot), Some(m), Some(args)] = w {
                        let recv_ok = matches!(r.kind(), "identifier" | "self");
                        if recv_ok && m.kind() == "identifier" && text(dot, src) == "." && src.get(args.start_byte()) == Some(&b'(') {
                            found.push((r, m));
                        }
                    }
                }
                more = c.goto_next_sibling();
            }
        }
        found.sort_by_key(|(_, m)| m.start_byte());
        for (r, m) in found {
            let ty = match (r.kind(), text(r, src)) {
                ("self", _) | (_, "self") => Some(Cow::Borrowed("Self")),
                (_, name) => self.env.lookup(name, r.start_byte()).flatten(),
            };
            self.out.calls.push(crate::parse::CallSite {
                caller: func.or(parent),
                callee: text(m, src).to_string(),
                qual: (text(r, src) == "self").then(|| "self".to_string()),
                line: r.start_position().row as u32 + 1,
                recv: true,
                rtype: None,
                rhint: None,
                in_macro: true,
            });
            if let Some(t) = ty {
                self.type_last_call(t, func, parent);
            }
        }
    }

    /// Rust expressions beyond the shared ones: calls, `?`, `.await`, `T { .. }`.
    pub(super) fn expr_rust(&self, e: Node<'a>, depth: u8) -> Option<Ty<'a>> {
        let src = self.src;
        match e.kind() {
            "try_expression" => step(self.expr_ty(e.named_child(0)?, depth + 1)?, "!"),
            "await_expression" => self.expr_ty(e.named_child(0)?, depth + 1),
            "struct_expression" => {
                let n = fld(e, F::Name)?;
                match n.kind() {
                    "scoped_type_identifier" => qualified(fld(n, F::Path)?, text(fld(n, F::Name)?, src), src),
                    _ => rust_type(n, src),
                }
            }
            "call_expression" => {
                let f = fld(e, F::Function)?;
                let f = if f.kind() == "generic_function" { fld(f, F::Function)? } else { f };
                match f.kind() {
                    "identifier" => step(Cow::Borrowed(""), &format!("::{}()", text(f, src))),
                    "scoped_identifier" => {
                        let name = text(fld(f, F::Name)?, src);
                        /* `module::f()` is a free function; `Type::f()` an associated one */
                        let owner = fld(f, F::Path).and_then(|p| rust_path_type(p, src)).unwrap_or(Cow::Borrowed(""));
                        step(owner, &format!("::{name}()"))
                    }
                    "field_expression" => {
                        let base = self.expr_ty(fld(f, F::Value)?, depth + 1);
                        Self::method_ty(base, text(fld(f, F::Field)?, src))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `x.f()` / `T::f()`: the receiver's type.
    pub(super) fn call_rust(&self, f: Node<'a>) -> Option<Ty<'a>> {
        match f.kind() {
            "field_expression" => self.expr_ty(fld(f, F::Value)?, 0),
            "scoped_identifier" => rust_path_type(fld(f, F::Path)?, self.src),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ty, typed};
    use crate::lang::Lang;

    #[test]
    fn rust_receivers() {
        let v = typed(Lang::Rust, r#"
struct Svc { repo: Repo, other: Vec<Repo> }
impl Svc {
    fn run(&self, r: &mut Repo, n: impl Named, g: T) {
        self.go();
        self.repo.get();
        r.get();
        n.name();
        g.get();
        let a = Repo::new();
        a.put();
        let b: Cache = make();
        b.put();
        let c = Self::default();
        c.go();
        <Repo as Store>::put(&a);
        if let Some(a) = opt { a.put(); }
        let a = make();
        a.put();
        util::helper();
        Repo::open()?.put();
    }
}
"#);
        /* constructors and free functions declared elsewhere stay hints for the index to settle */
        assert_eq!(ty(&v, "go"), [Some("Svc"), Some("Svc::default()")]);
        assert_eq!(ty(&v, "get"), [Some("Repo"), Some("Repo"), None]);
        assert_eq!(ty(&v, "name"), [Some("Named")]);
        assert_eq!(ty(&v, "put"), [Some("Repo::new()"), Some("Cache"), Some("Repo"), None, Some("::make()"), Some("Repo::open()!")]);
        assert_eq!(ty(&v, "new"), [Some("Repo")]);
        assert_eq!(ty(&v, "helper"), [None]);
    }

    #[test]
    fn rust_declarations_settle_in_the_file() {
        let src = r#"
struct Svc { db: Arc<Db>, cache: Option<Cache> }
fn open_db() -> Result<Db, Error> { todo!() }
impl Db { fn conn(&self) -> Conn { todo!() } }
impl Svc {
    fn new() -> Self { todo!() }
    fn run(&self) {
        let d = open_db()?;
        d.query();
        self.db.conn().exec();
        self.cache.unwrap().put();
        let s = Svc::new();
        s.run();
        other().run();
    }
}
"#;
        let ex = crate::parse::extract(Lang::Rust, src.as_bytes());
        let f: Vec<(&str, &str, &str)> = ex.fields.iter().map(|m| (m.owner.as_str(), m.name.as_str(), m.ty.as_str())).collect();
        assert_eq!(f, [("Svc", "db", "Arc<Db>"), ("Svc", "cache", "Option<Cache>")]);
        let r: Vec<(&str, &str, &str)> = ex.rets.iter().map(|m| (m.owner.as_str(), m.name.as_str(), m.ty.as_str())).collect();
        assert_eq!(r, [("", "open_db", "Result<Db>"), ("Db", "conn", "Conn"), ("Svc", "new", "Svc")]);
        let v = typed(Lang::Rust, src);
        assert_eq!(ty(&v, "query"), [Some("Db")]);
        assert_eq!(ty(&v, "exec"), [Some("Conn")]);
        assert_eq!(ty(&v, "put"), [Some("Cache")]);
        assert_eq!(ty(&v, "run"), [Some("Svc"), Some("::other()")]);
        /* a trait's `-> Self` is kept for the implementing type to fill in */
        let ex = crate::parse::extract(Lang::Rust, b"trait Node { fn cloned(&self) -> Self; }\nimpl Svc { fn me(&self) -> Self { todo!() } }");
        let r: Vec<&str> = ex.rets.iter().map(|m| m.ty.as_str()).collect();
        assert_eq!(r, ["Self", "Svc"]);
        /* a tuple's elements, through destructuring */
        let v = typed(Lang::Rust, "fn split() -> (Db, Pool) { todo!() }\nfn f() { let (a, _) = split(); a.q(); let (_, b) = split(); b.w(); }");
        assert_eq!(ty(&v, "q"), [Some("Db")]);
        assert_eq!(ty(&v, "w"), [Some("Pool")]);
        /* closure parameters, from the callee's declared closure types or their own annotation */
        let src = "struct Acc;\nimpl Acc { fn add(&mut self, id: u8, f: impl FnOnce(&mut Builder)) {} fn with<F>(&self, f: F) where F: Fn(Db, u8) {} }\nfn run(acc: &mut Acc) { acc.add(1, |b| b.edit()); acc.with(|d, _| d.q()); go(|t: &mut Table| t.r()); }";
        let ex = crate::parse::extract(Lang::Rust, src.as_bytes());
        let r: Vec<(&str, &str)> = ex.rets.iter().map(|m| (m.name.as_str(), m.ty.as_str())).collect();
        assert_eq!(r, [("add#1", "(Builder)"), ("with#0", "(Db,_)")]);
        let v = typed(Lang::Rust, src);
        assert_eq!(ty(&v, "edit"), [Some("Builder")]);
        assert_eq!(ty(&v, "q"), [Some("Db")]);
        assert_eq!(ty(&v, "r"), [Some("Table")]);
    }
}
