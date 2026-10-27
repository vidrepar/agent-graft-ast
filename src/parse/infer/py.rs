//! Python receivers: `self` in a class, `x = C(..)` / `x = f()`, annotated parameters and assignments,
//! attribute and method chains. Fields (`self.x = ..`, class-level `x: C`) and `-> T` return types are
//! declared for other files to use.

use super::{fld, step, upper, usable, Ty, F};
use crate::parse::{text, Extractor};
use std::borrow::Cow;
use tree_sitter::Node;

/// `mod.sub.C` -> `sub/C`, keeping the module so same-named classes can be told apart.
fn attr_type<'a>(n: Node, src: &'a [u8]) -> Option<Ty<'a>> {
    let name = usable(text(fld(n, F::Attribute)?, src))?;
    let obj = text(fld(n, F::Object)?, src);
    Some(match usable(obj.rsplit('.').next().unwrap_or(obj)) {
        Some(q) => Cow::Owned(format!("{q}/{name}")),
        None => Cow::Borrowed(name),
    })
}

/// Python: the class an annotation names (`C`, `mod.C`, `"C"`, `Optional[C]`).
fn py_type<'a>(n: Node, src: &'a [u8]) -> Option<Ty<'a>> {
    match n.kind() {
        "type" => py_type(n.named_child(0)?, src),
        "identifier" => usable(text(n, src)).map(Cow::Borrowed),
        "attribute" => attr_type(n, src),
        "string" => {
            let t = text(n, src).trim_matches(|c| c == '"' || c == '\'');
            usable(t.rsplit('.').next().unwrap_or(t)).map(Cow::Borrowed)
        }
        "subscript" if text(fld(n, F::Value)?, src) == "Optional" => py_type(fld(n, F::Subscript)?, src),
        "generic_type" if text(n.named_child(0)?, src) == "Optional" => py_type(n.named_child(1)?.named_child(0)?, src),
        _ => None,
    }
}

impl<'a> Extractor<'a> {
    pub(super) fn bind_py(&mut self, node: Node<'a>, func: Option<u32>, parent: Option<u32>) {
        let src = self.src;
        match node.kind() {
            "function_definition" => {
                if let (Some(name), Some(rt)) = (fld(node, F::Name), fld(node, F::ReturnType).and_then(|t| py_type(t, src))) {
                    let owner = if func.is_none() { self.owner(None, parent) } else { None };
                    if owner.is_some() || (func.is_none() && parent.is_none()) {
                        self.declare(false, owner.as_deref(), false, text(name, src), rt);
                    }
                }
                let (Some(params), Some(body)) = (fld(node, F::Parameters), fld(node, F::Body)) else { return };
                if let Some(name) = fld(node, F::Name) {
                    let owner = if func.is_none() { self.owner(None, parent) } else { None };
                    self.declare_py_callbacks(params, text(name, src), owner);
                }
                let mut c = params.walk();
                for p in params.named_children(&mut c) {
                    let (name, ty) = match p.kind() {
                        "identifier" => (Some(p), None),
                        "typed_parameter" => (p.named_child(0).filter(|n| n.kind() == "identifier"), fld(p, F::Type)),
                        "default_parameter" | "typed_default_parameter" => (fld(p, F::Name), fld(p, F::Type)),
                        _ => (None, None),
                    };
                    if let Some(n) = name {
                        self.env.bind(text(n, src), ty.and_then(|t| py_type(t, src)), body.start_byte(), node.end_byte());
                    }
                }
            }
            "assignment" => {
                let Some(left) = fld(node, F::Left) else { return };
                let ty = match fld(node, F::Type) {
                    Some(t) => py_type(t, src),
                    None => fld(node, F::Right).and_then(|v| self.expr_ty(v, 0)),
                };
                match left.kind() {
                    "identifier" if func.is_none() && parent.is_some() => {
                        /* class body: `x: C` / `x = C()` declares a field */
                        if let (Some(t), Some(owner)) = (ty, self.owner(None, parent)) {
                            self.declare(true, Some(&owner), false, text(left, src), t);
                        }
                    }
                    "identifier" => {
                        let to = func.map(|f| self.sym_ends[f as usize]).unwrap_or(src.len());
                        self.env.bind(text(left, src), ty, node.end_byte(), to);
                    }
                    "attribute" => {
                        let obj = fld(left, F::Object);
                        if let (Some(t), Some(o), Some(a)) = (ty, obj, fld(left, F::Attribute)) {
                            if text(o, src) == "self" {
                                if let Some(owner) = self.owner(func, parent) {
                                    self.declare(true, Some(&owner), false, text(a, src), t);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            "lambda" => {
                /* `f(lambda r: r.go())`: r takes the type f declares for its callback */
                let Some(params) = fld(node, F::Parameters) else { return };
                let site = self.callback_site(node);
                let kids: Vec<Node<'a>> = params.named_children(&mut params.walk()).collect();
                for (j, p) in kids.into_iter().enumerate().filter(|(_, p)| p.kind() == "identifier") {
                    let ty = site.as_ref().and_then(|s| Self::callback_param(s, j));
                    self.env.bind(text(p, src), ty, node.start_byte(), node.end_byte());
                }
            }
            "for_statement" => {
                if let Some(l) = fld(node, F::Left) {
                    let to = func.map(|f| self.sym_ends[f as usize]).unwrap_or(src.len());
                    self.shadow_pattern(l, l.end_byte(), || (0, to));
                }
            }
            _ => {}
        }
    }

    /// Parameters annotated `Callable[[A, B], R]`: the types each is called with. A method's `self` is
    /// not an argument at call sites, so it is not counted.
    fn declare_py_callbacks(&mut self, params: Node<'a>, name: &str, owner: Option<String>) {
        let src = self.src;
        let mut kids: Vec<Node<'a>> = params.named_children(&mut params.walk()).collect();
        if owner.is_some() && kids.first().is_some_and(|p| p.kind() == "identifier") {
            kids.remove(0);
        }
        for (i, p) in kids.into_iter().enumerate() {
            if !matches!(p.kind(), "typed_parameter" | "typed_default_parameter") {
                continue;
            }
            let Some(g) = fld(p, F::Type).and_then(|t| t.named_child(0)).filter(|g| g.kind() == "generic_type") else { continue };
            if g.named_child(0).is_none_or(|c| text(c, src) != "Callable") {
                continue;
            }
            let Some(list) = g.named_child(1).and_then(|tp| tp.named_child(0)).and_then(|t| t.named_child(0)).filter(|l| l.kind() == "list") else { continue };
            let args: Vec<String> = list.named_children(&mut list.walk()).map(|a| py_type(a, src).map_or("_".to_string(), |c| c.into_owned())).collect();
            self.declare_callback(owner.as_deref(), false, name, i, args);
        }
    }

    /// Python expressions beyond the shared ones: calls (`C()` builds a C, `f()` returns its
    /// declared type), `await`.
    pub(super) fn expr_py(&self, e: Node<'a>, depth: u8) -> Option<Ty<'a>> {
        let src = self.src;
        match e.kind() {
            "await" => self.expr_ty(e.named_child(0)?, depth + 1),
            "call" => {
                let f = fld(e, F::Function)?;
                match f.kind() {
                    "identifier" if upper(text(f, src)) => usable(text(f, src)).map(Cow::Borrowed),
                    "identifier" => step(Cow::Borrowed(""), &format!("::{}()", text(f, src))),
                    "attribute" => {
                        let m = text(fld(f, F::Attribute)?, src);
                        if upper(m) {
                            /* `module.Class(..)` */
                            return attr_type(f, src);
                        }
                        let base = self.expr_ty(fld(f, F::Object)?, depth + 1);
                        Self::method_ty(base, m)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `x.f()`: the receiver's type.
    pub(super) fn call_py(&self, f: Node<'a>) -> Option<Ty<'a>> {
        if f.kind() != "attribute" {
            return None;
        }
        self.expr_ty(fld(f, F::Object)?, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ty, typed};
    use crate::lang::Lang;

    #[test]
    fn python_receivers() {
        let v = typed(Lang::Python, r#"
class Svc:
    cache: Cache

    def __init__(self, repo: Repo):
        self.repo = repo
        self.db = Db()

    def run(self, r: "Repo", o: Optional[Repo], n=3):
        self.go()
        self.db.query()
        self.cache.put()
        r.get()
        o.get()
        a = Cache()
        a.put()
        for a in items:
            a.put()
        Repo().get()
"#);
        assert_eq!(ty(&v, "go"), [Some("Svc")]);
        assert_eq!(ty(&v, "query"), [Some("Db")]);
        assert_eq!(ty(&v, "put"), [Some("Cache"), Some("Cache"), None]);
        assert_eq!(ty(&v, "get"), [Some("Repo"), Some("Repo"), Some("Repo")]);
    }

    #[test]
    fn python_declarations() {
        let src = r#"
def connect() -> Db:
    pass

class Svc:
    def __init__(self, repo: Repo):
        self.repo = repo
        self.db = connect()

    def find(self, id) -> Item:
        pass

    def run(self):
        d = connect()
        d.query()
        self.find(1).name()
        self.repo.get()
        self.db.close()
"#;
        let ex = crate::parse::extract(Lang::Python, src.as_bytes());
        let f: Vec<(&str, &str, &str)> = ex.fields.iter().map(|m| (m.owner.as_str(), m.name.as_str(), m.ty.as_str())).collect();
        assert_eq!(f, [("Svc", "repo", "Repo"), ("Svc", "db", "::connect()")]);
        let r: Vec<(&str, &str, &str)> = ex.rets.iter().map(|m| (m.owner.as_str(), m.name.as_str(), m.ty.as_str())).collect();
        assert_eq!(r, [("", "connect", "Db"), ("Svc", "find", "Item")]);
        let v = typed(Lang::Python, src);
        assert_eq!(ty(&v, "query"), [Some("Db")]);
        assert_eq!(ty(&v, "name"), [Some("Item")]);
        assert_eq!(ty(&v, "get"), [Some("Repo")]);
        assert_eq!(ty(&v, "close"), [Some("Db")]);
        let v = typed(Lang::Python, "class Queue:\n    def each(self, cb: Callable[[Repo, int], None]):\n        pass\n\ndef f(q: Queue):\n    q.each(lambda r, i: r.save())\n");
        assert_eq!(ty(&v, "save"), [Some("Repo")]);
    }
}
