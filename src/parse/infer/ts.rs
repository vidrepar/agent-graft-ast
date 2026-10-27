//! TypeScript / JavaScript receivers: `this` in a class, `const x = new C()` / `x: C` / `await f()`,
//! typed parameters, field and getter chains, method chains. Class fields, getters, interface
//! properties and return types are declared for other files to use.

use super::{fld, range, shape, step, usable, Ty, F};
use crate::parse::{text, Extractor};
use std::borrow::Cow;
use tree_sitter::Node;

/// TypeScript: the class or interface a type names, as a shape (`a.Foo` -> `Foo`, `Promise<Repo>`).
fn ts_type<'a>(n: Node, src: &'a [u8]) -> Option<Ty<'a>> {
    match n.kind() {
        "type_annotation" => ts_type(n.named_child(0)?, src),
        "type_identifier" => usable(text(n, src)).map(Cow::Borrowed),
        /* `ns.Foo` keeps its module, to tell same-named types apart */
        "nested_type_identifier" => {
            let name = usable(text(fld(n, F::Name)?, src))?;
            let q = fld(n, F::Module).map(|m| text(m, src)).map(|m| m.rsplit('.').next().unwrap_or(m)).and_then(usable);
            Some(match q {
                Some(q) => Cow::Owned(format!("{q}/{name}")),
                None => Cow::Borrowed(name),
            })
        }
        "generic_type" => {
            let base = ts_type(fld(n, F::Name)?, src)?;
            let inner = fld(n, F::TypeArguments).and_then(|a| a.named_child(0)).and_then(|t| ts_type(t, src));
            match base {
                Cow::Borrowed(b) => Some(shape(b, inner.as_deref().filter(|i| !i.contains('<')))),
                owned => Some(owned),
            }
        }
        _ => None,
    }
}

impl<'a> Extractor<'a> {
    pub(super) fn bind_ts(&mut self, node: Node<'a>, func: Option<u32>, parent: Option<u32>) {
        let src = self.src;
        match node.kind() {
            "function_declaration" | "generator_function_declaration" | "method_definition" | "arrow_function" | "function_expression" => {
                let scope = range(node);
                let named = matches!(node.kind(), "function_declaration" | "method_definition");
                if let (true, Some(name), Some(rt)) = (named, fld(node, F::Name), fld(node, F::ReturnType).and_then(|t| ts_type(t, src))) {
                    /* a getter reads like a field: `this.repo.get()` */
                    let mut c = node.walk();
                    let getter = node.children(&mut c).any(|k| k.kind() == "get");
                    let owner = if node.kind() == "method_definition" { self.owner(None, parent) } else { None };
                    if owner.is_some() || func.is_none() {
                        self.declare(getter, owner.as_deref(), false, text(name, src), rt);
                    }
                }
                let params = fld(node, F::Parameters);
                /* `f(.., (x) => ..)`: untyped parameters take the types f declares for its callback */
                let site = if named { None } else { self.callback_site(node) };
                if let Some(single) = fld(node, F::Parameter) {
                    let ty = site.as_ref().and_then(|s| Self::callback_param(s, 0));
                    self.env.bind(text(single, src), ty, scope.0, scope.1);
                }
                let Some(params) = params else { return };
                if let (true, Some(name)) = (named, fld(node, F::Name)) {
                    self.declare_ts_callbacks(params, text(name, src), if node.kind() == "method_definition" { self.owner(None, parent) } else { None });
                }
                let ctor = node.kind() == "method_definition" && fld(node, F::Name).is_some_and(|n| text(n, src) == "constructor");
                let kids: Vec<Node<'a>> = params.named_children(&mut params.walk()).filter(|p| matches!(p.kind(), "required_parameter" | "optional_parameter")).collect();
                for (j, p) in kids.into_iter().enumerate() {
                    let Some(pat) = fld(p, F::Pattern).filter(|n| n.kind() == "identifier") else { continue };
                    let ty = fld(p, F::Type).and_then(|t| ts_type(t, src)).or_else(|| site.as_ref().and_then(|s| Self::callback_param(s, j)));
                    /* `constructor(private repo: Repo)` declares a field */
                    let mut cc = p.walk();
                    if ctor && p.children(&mut cc).any(|m| m.kind() == "accessibility_modifier" || m.kind() == "readonly") {
                        if let (Some(t), Some(owner)) = (ty.clone(), self.owner(None, parent)) {
                            self.declare(true, Some(&owner), false, text(pat, src), t);
                        }
                    }
                    self.env.bind(text(pat, src), ty, scope.0, scope.1);
                }
            }
            "variable_declarator" => {
                let Some(name) = fld(node, F::Name).filter(|n| n.kind() == "identifier") else { return };
                let ty = match fld(node, F::Type) {
                    Some(t) => ts_type(t, src),
                    None => fld(node, F::Value).and_then(|v| self.expr_ty(v, 0)),
                };
                let n = text(name, src);
                if ty.is_some() || self.env.hides(n, node.end_byte()) {
                    let to = node.parent().and_then(|d| d.parent()).map(|b| b.end_byte()).unwrap_or(src.len());
                    self.env.bind(n, ty, node.end_byte(), to);
                }
            }
            "public_field_definition" | "property_signature" => {
                let Some(name) = fld(node, F::Name) else { return };
                let ty = match fld(node, F::Type) {
                    Some(t) => ts_type(t, src),
                    None => fld(node, F::Value).and_then(|v| self.expr_ty(v, 0)),
                };
                if let (Some(t), Some(owner)) = (ty, self.owner(None, parent)) {
                    self.declare(true, Some(&owner), false, text(name, src), t);
                }
            }
            "for_in_statement" => {
                if let Some(l) = fld(node, F::Left) {
                    self.shadow_pattern(l, node.start_byte(), || range(node));
                }
            }
            _ => {}
        }
    }

    /// Parameters typed as callbacks, `cb: (x: T, n: number) => void`: the types each is called with.
    fn declare_ts_callbacks(&mut self, params: Node<'a>, name: &str, owner: Option<String>) {
        let src = self.src;
        let kids: Vec<Node<'a>> = params.named_children(&mut params.walk()).filter(|p| matches!(p.kind(), "required_parameter" | "optional_parameter")).collect();
        for (i, p) in kids.into_iter().enumerate() {
            let Some(ft) = fld(p, F::Type).and_then(|t| t.named_child(0)).filter(|t| t.kind() == "function_type") else { continue };
            let Some(fps) = fld(ft, F::Parameters) else { continue };
            let args: Vec<String> = fps
                .named_children(&mut fps.walk())
                .map(|a| fld(a, F::Type).and_then(|t| ts_type(t, src)).map_or("_".to_string(), |c| c.into_owned()))
                .collect();
            self.declare_callback(owner.as_deref(), false, name, i, args);
        }
    }

    /// TypeScript: `new C(..)` builds a C.
    pub(super) fn ts_new(&self, v: Node<'a>) -> Option<&'a str> {
        (v.kind() == "new_expression").then(|| fld(v, F::Constructor).filter(|c| c.kind() == "identifier")).flatten().and_then(|c| usable(text(c, self.src)))
    }

    /// TypeScript expressions beyond the shared ones: `new`, calls, `await`.
    pub(super) fn expr_ts(&self, e: Node<'a>, depth: u8) -> Option<Ty<'a>> {
        let src = self.src;
        match e.kind() {
            "new_expression" => self.ts_new(e).map(Cow::Borrowed),
            "await_expression" => step(self.expr_ty(e.named_child(0)?, depth + 1)?, "!"),
            "call_expression" => {
                let f = fld(e, F::Function)?;
                match f.kind() {
                    "identifier" => step(Cow::Borrowed(""), &format!("::{}()", text(f, src))),
                    "member_expression" => {
                        let base = self.expr_ty(fld(f, F::Object)?, depth + 1);
                        Self::method_ty(base, text(fld(f, F::Property)?, src))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `x.f()`: the receiver's type.
    pub(super) fn call_ts(&self, f: Node<'a>) -> Option<Ty<'a>> {
        if f.kind() != "member_expression" {
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
    fn typescript_receivers() {
        let v = typed(Lang::TypeScript, r#"
class Svc {
  private cache = new Cache();
  db: Db;
  constructor(private repo: Repo) {}
  run(r: Repo, x: string) {
    this.go();
    this.repo.get();
    this.cache.put();
    this.db.query();
    r.get();
    const a = new Cache();
    a.put();
    const b: Db = make();
    b.query();
    items.forEach((r) => r.get());
    new Repo().get();
    x.trim();
  }
}
"#);
        assert_eq!(ty(&v, "go"), [Some("Svc")]);
        assert_eq!(ty(&v, "get"), [Some("Repo"), Some("Repo"), None, Some("Repo")]);
        assert_eq!(ty(&v, "put"), [Some("Cache"), Some("Cache")]);
        assert_eq!(ty(&v, "query"), [Some("Db"), Some("Db")]);
        assert_eq!(ty(&v, "trim"), [None]);
    }

    #[test]
    fn typescript_declarations() {
        let src = r#"
interface Ctx { repo: Repo }
export async function load(): Promise<Repo> { return x; }
class Svc {
  get store(): Store { return s; }
  find(id: string): Item { return i; }
  async run(c: Ctx) {
    c.repo.get();
    (await load()).get();
    const r = await load();
    r.save();
    this.store.put();
    this.find("1").name();
  }
}
"#;
        let ex = crate::parse::extract(Lang::TypeScript, src.as_bytes());
        let f: Vec<(&str, &str, &str)> = ex.fields.iter().map(|m| (m.owner.as_str(), m.name.as_str(), m.ty.as_str())).collect();
        assert_eq!(f, [("Ctx", "repo", "Repo"), ("Svc", "store", "Store")]);
        let r: Vec<(&str, &str, &str)> = ex.rets.iter().map(|m| (m.owner.as_str(), m.name.as_str(), m.ty.as_str())).collect();
        assert_eq!(r, [("", "load", "Promise<Repo>"), ("Svc", "find", "Item")]);
        let v = typed(Lang::TypeScript, src);
        assert_eq!(ty(&v, "get"), [Some("Repo"), Some("Repo")]);
        assert_eq!(ty(&v, "save"), [Some("Repo")]);
        assert_eq!(ty(&v, "put"), [Some("Store")]);
        assert_eq!(ty(&v, "name"), [Some("Item")]);
        let v = typed(Lang::TypeScript, "class Queue { each(cb: (r: Repo, i: number) => void) {} }\nfunction f(q: Queue) { q.each((r) => r.save()); q.each(function (x) { x.load(); }); }");
        assert_eq!(ty(&v, "save"), [Some("Repo")]);
        assert_eq!(ty(&v, "load"), [Some("Repo")]);
    }
}
