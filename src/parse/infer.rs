//! Receiver types of method calls, where the code states them: `self` inside an impl or class, locals
//! built by a constructor or annotated with a type, typed parameters, and fields of the caller's own type.
//! No type checker: an unknown receiver stays unknown and resolution falls back to the name alone.

use super::{text, Extract, Extractor, Lang, Member, SymKind};
use std::borrow::Cow;
use tree_sitter::Node;

pub(crate) mod hint;
mod py;
mod rust;
mod ts;

/// Node kinds that bind names, visited on top of what extraction already looks at.
pub(super) fn kinds(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => &["let_declaration", "closure_expression", "for_expression", "let_condition", "match_arm", "macro_invocation"],
        Lang::Python => &["assignment", "for_statement", "lambda"],
        l if l.is_js_family() => &["public_field_definition", "property_signature", "arrow_function", "function_expression", "for_in_statement"],
        _ => &[],
    }
}

/// Extracted kinds that also bind names (parameters, fields).
fn also_binds(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => &["function_item", "function_signature_item", "struct_item"],
        Lang::Python => &["function_definition"],
        l if l.is_js_family() => &["function_declaration", "generator_function_declaration", "method_definition", "variable_declarator"],
        _ => &[],
    }
}

/// Per language, per kind id: 0 = no binding, 1 = binds and is extracted, 2 = only binds.
static ROLE: once_cell::sync::Lazy<Vec<Vec<u8>>> = once_cell::sync::Lazy::new(|| {
    Lang::ALL
        .iter()
        .map(|&l| {
            let ts = l.ts_language();
            (0..ts.node_kind_count() as u16)
                .map(|id| match ts.node_kind_for_id(id) {
                    Some(k) if kinds(l).contains(&k) => 2,
                    Some(k) if also_binds(l).contains(&k) => 1,
                    _ => 0,
                })
                .collect()
        })
        .collect()
});

/// What a node of kind `id` means to inference (see `ROLE`).
pub(super) fn role(lang: Lang, id: u16) -> u8 {
    ROLE[lang.slot()].get(id as usize).copied().unwrap_or(0)
}

/// Field names inference reads, resolved to each grammar's field ids once (by name is a string search).
#[derive(Clone, Copy)]
pub(super) enum F {
    Attribute,
    Body,
    Bounds,
    Constructor,
    Field,
    Function,
    Left,
    Module,
    Name,
    Object,
    Parameter,
    Parameters,
    Path,
    Pattern,
    Property,
    ReturnType,
    Right,
    Subscript,
    Trait,
    Type,
    TypeArguments,
    TypeParameters,
    Value,
}

const FIELD_NAMES: &[&str] = &["attribute", "body", "bounds", "constructor", "field", "function", "left", "module", "name", "object", "parameter", "parameters", "path", "pattern", "property", "return_type", "right", "subscript", "trait", "type", "type_arguments", "type_parameters", "value"];

static FIELD_IDS: once_cell::sync::Lazy<Vec<Vec<u16>>> = once_cell::sync::Lazy::new(|| {
    Lang::ALL
        .iter()
        .map(|l| {
            let ts = l.ts_language();
            FIELD_NAMES.iter().map(|n| ts.field_id_for_name(n).map(|i| i.get()).unwrap_or(0)).collect()
        })
        .collect()
});

thread_local! {
    /// slot of the language being extracted on this thread
    static SLOT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `n.child_by_field_name(..)` for a field of the current language.
pub(super) fn fld<'t>(n: Node<'t>, f: F) -> Option<Node<'t>> {
    let id = FIELD_IDS[SLOT.with(|s| s.get())][f as usize];
    if id == 0 { None } else { n.child_by_field_id(id) }
}

/// A local name visible in `from..to` (bytes), with its type when known (a shape or a hint, see `hint`).
pub(super) struct Bind<'a> {
    pub(super) name: &'a str,
    pub(super) ty: Option<Ty<'a>>,
    pub(super) from: usize,
    pub(super) to: usize,
}

/// A type as inference carries it: `Name`, `Wrapper<Name>` or a hint, with `Self` for the enclosing type.
pub(super) type Ty<'a> = Cow<'a, str>;

#[derive(Default)]
pub(super) struct Env<'a> {
    pub(super) binds: Vec<Bind<'a>>,
    prune_at: usize,
}

impl<'a> Env<'a> {
    /// The binding of `name` visible at `pos`: Some(None) = a local of unknown type.
    pub(super) fn lookup(&self, name: &str, pos: usize) -> Option<Option<Ty<'a>>> {
        self.binds.iter().rev().find(|b| b.name == name && b.from <= pos && pos < b.to).map(|b| b.ty.clone())
    }

    /// A typed binding of `name` is visible at `pos`: only then does an untyped local need recording.
    pub(super) fn hides(&self, name: &str, pos: usize) -> bool {
        self.binds.iter().rev().find(|b| b.name == name && b.from <= pos && pos < b.to).is_some_and(|b| b.ty.is_some())
    }

    pub(super) fn bind(&mut self, name: &'a str, ty: Option<Ty<'a>>, from: usize, to: usize) {
        if name.is_empty() || (ty.is_none() && !self.hides(name, from)) {
            return;
        }
        if self.binds.len() >= self.prune_at.max(64) {
            self.binds.retain(|b| b.to > from);
            self.prune_at = self.binds.len() * 2;
        }
        self.binds.push(Bind { name, ty, from, to });
    }
}

/// A type name worth resolving against: one-letter names are generic parameters, not types.
pub(super) fn usable(t: &str) -> Option<&str> {
    (t.len() > 1 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).then_some(t)
}

pub(super) fn upper(t: &str) -> bool {
    t.as_bytes().first().is_some_and(|b| b.is_ascii_uppercase())
}

pub(super) fn range(n: Node) -> (usize, usize) {
    (n.start_byte(), n.end_byte())
}

/// `Wrapper<Inner>` when both are usable names and the wrapper is one hints take apart; else `base`.
pub(super) fn shape<'a>(base: &'a str, inner: Option<&str>) -> Ty<'a> {
    match inner {
        Some(i) if hint::wraps(base) => Cow::Owned(format!("{base}<{i}>")),
        _ => Cow::Borrowed(base),
    }
}

/// `base` followed by one hint step, unless the hint grows too long.
pub(super) fn step<'a>(base: Ty<'a>, s: &str) -> Option<Ty<'a>> {
    (base.len() + s.len() <= hint::MAX_LEN).then(|| Cow::Owned(format!("{base}{s}")))
}

/// Declarations of one file, for settling hints before the index sees them.
struct Local<'e>(&'e [Member], &'e [Member]);

impl hint::Decls for Local<'_> {
    /* a qualified owner (`ast/Path`) may be another file's type of the same name: the index decides */
    fn field(&self, owner: &str, name: &str) -> Option<String> {
        self.0.iter().find(|m| m.owner == owner && m.name == name).map(|m| m.ty.clone())
    }
    fn ret(&self, owner: &str, name: &str) -> Option<String> {
        self.1.iter().find(|m| m.owner == owner && m.name == name).map(|m| m.ty.clone())
    }
    fn ctor_fallback(&self) -> bool {
        /* another file may declare the constructor's real return type */
        false
    }
}

impl<'a> Extractor<'a> {
    /// The impl, trait or class whose method holds the current code (`self` / `this`), without path.
    pub(super) fn owner(&self, func: Option<u32>, parent: Option<u32>) -> Option<String> {
        let syms = &self.out.symbols;
        let at = match func {
            Some(f) => syms[f as usize].parent?,
            None => parent?,
        };
        let s = &syms[at as usize];
        let name = s.name.rsplit("::").next().unwrap_or(&s.name);
        matches!(s.kind, SymKind::Impl | SymKind::Trait | SymKind::Class | SymKind::Interface).then(|| name.to_string())
    }

    /// Record a declared field or return type of `owner` (None = a free function). A trait keeps
    /// `Self`: it stands for whichever type the method is called on.
    pub(super) fn declare(&mut self, field: bool, owner: Option<&str>, in_trait: bool, name: &str, ty: Ty) {
        let ty = match owner {
            Some(_) if in_trait => ty.into_owned(),
            Some(o) => hint::subst_self(&ty, o),
            None if ty.contains("Self") => return,
            None => ty.into_owned(),
        };
        let m = Member { owner: owner.unwrap_or("").to_string(), name: name.to_string(), ty, assoc: false };
        if field { self.out.fields.push(m) } else { self.out.rets.push(m) }
    }

    /// Declare the parameter types callback argument `i` of `name` is called with (`impl Fn(&mut T)`,
    /// `(x: T) => void`, `Callable[[T], None]`), as the "return type" of `name#i`: a tuple of them.
    pub(super) fn declare_callback(&mut self, owner: Option<&str>, in_trait: bool, name: &str, i: usize, args: Vec<String>) {
        if args.iter().all(|a| a == "_") || args.iter().any(|a| !hint::is_shape(a) || a.contains(['(', ','])) {
            return;
        }
        let tuple = Cow::Owned(format!("({})", args.join(",")));
        self.declare(false, owner, in_trait, &format!("{name}#{i}"), tuple);
    }

    /// A closure passed as argument `i` of a call: (callee's receiver type or "" for a free function,
    /// callee name, i). The receiver is `?` when nothing types it.
    pub(super) fn callback_site(&self, closure: Node<'a>) -> Option<(Ty<'a>, &'a str, usize)> {
        let args = closure.parent().filter(|p| matches!(p.kind(), "arguments" | "argument_list"))?;
        let i = args.named_children(&mut args.walk()).filter(|a| !a.kind().contains("comment")).position(|a| a == closure)?;
        let call = args.parent()?;
        let f = fld(call, F::Function)?;
        let f = if f.kind() == "generic_function" { fld(f, F::Function)? } else { f };
        let src = self.src;
        let (owner, name) = match f.kind() {
            "identifier" => (Cow::Borrowed(""), text(f, src)),
            "field_expression" => (self.expr_ty(fld(f, F::Value)?, 1).unwrap_or(Cow::Borrowed("?")), text(fld(f, F::Field)?, src)),
            "member_expression" => (self.expr_ty(fld(f, F::Object)?, 1).unwrap_or(Cow::Borrowed("?")), text(fld(f, F::Property)?, src)),
            "attribute" => (self.expr_ty(fld(f, F::Object)?, 1).unwrap_or(Cow::Borrowed("?")), text(fld(f, F::Attribute)?, src)),
            "scoped_identifier" => (self.rust_path_owner(f).unwrap_or(Cow::Borrowed("")), text(fld(f, F::Name)?, src)),
            _ => return None,
        };
        /* an untyped receiver's `forEach`/`map` is the standard library's */
        if owner == "?" && !hint::untyped_method_ok(name) {
            return None;
        }
        Some((owner, name, i))
    }

    /// The type of closure parameter `j` passed as argument `i` of `name` on `owner`.
    pub(super) fn callback_param(site: &(Ty<'a>, &'a str, usize), j: usize) -> Option<Ty<'a>> {
        let (owner, name, i) = site;
        step(owner.clone(), &format!("::{name}#{i}().{j}"))
    }

    /// The identifiers a pattern binds (`(a, Some(b))` -> a, b) become untyped locals over `scope`,
    /// computed only when one of them hides a typed binding.
    pub(super) fn shadow_pattern(&mut self, pat: Node<'a>, from: usize, scope: impl FnOnce() -> (usize, usize)) {
        /* walk the pattern only if its text mentions a typed local that is visible here */
        let pat_text = &self.src[pat.byte_range()];
        let typed_named = |b: &Bind| b.ty.is_some() && b.from <= from && from < b.to && memchr::memmem::find(pat_text, b.name.as_bytes()).is_some();
        if !self.env.binds.iter().any(typed_named) {
            return;
        }
        let mut hit: Vec<&'a str> = Vec::new();
        if pat.kind() == "identifier" {
            hit.push(text(pat, self.src));
        }
        let mut c = pat.walk();
        let mut more = c.goto_first_child();
        while more {
            let n = c.node();
            /* `Some` in `Some(x)` is the pattern's type, not a binding */
            if n.kind() == "identifier" && c.field_name() != Some("type") {
                hit.push(text(n, self.src));
            }
            more = if n.kind() != "identifier" && c.goto_first_child() {
                true
            } else {
                loop {
                    if c.goto_next_sibling() {
                        break true;
                    }
                    if !c.goto_parent() || c.node() == pat {
                        break false;
                    }
                }
            };
        }
        hit.retain(|n| self.env.hides(n, from));
        if !hit.is_empty() {
            let (_, to) = scope();
            for n in hit {
                self.env.bind(n, None, from, to);
            }
        }
    }

    /// Record what a binding node says about local names, fields, parameter and return types.
    pub(super) fn infer_bind(&mut self, node: Node<'a>, func: Option<u32>, parent: Option<u32>) {
        SLOT.with(|s| s.set(self.lang.slot()));
        match self.lang {
            Lang::Rust => self.bind_rust(node, func, parent),
            Lang::Python => self.bind_py(node, func, parent),
            l if l.is_js_family() => self.bind_ts(node, func, parent),
            _ => {}
        }
    }

    /// The receiver type of the call just pushed for `call`, when the code states it.
    pub(super) fn infer_call(&mut self, call: Node<'a>, func: Option<u32>, parent: Option<u32>) {
        SLOT.with(|s| s.set(self.lang.slot()));
        let Some(f) = fld(call, F::Function) else { return };
        let f = if f.kind() == "generic_function" { fld(f, F::Function).unwrap_or(f) } else { f };
        let ty = match self.lang {
            Lang::Rust => self.call_rust(f),
            Lang::Python => self.call_py(f),
            l if l.is_js_family() => self.call_ts(f),
            _ => None,
        };
        if let Some(t) = ty {
            self.set_type(t, func, parent);
        }
    }

    /// The type of an expression: a shape when the code states it, a hint when declarations elsewhere
    /// decide it, None when nothing does. `self`/`this` are `Self`.
    pub(super) fn expr_ty(&self, e: Node<'a>, depth: u8) -> Option<Ty<'a>> {
        if depth > 8 {
            return None;
        }
        let src = self.src;
        let self_word = if self.lang == Lang::Python { "self" } else { "this" };
        match e.kind() {
            "self" | "this" => Some(Cow::Borrowed("Self")),
            "identifier" if text(e, src) == self_word => Some(Cow::Borrowed("Self")),
            "identifier" => self.env.lookup(text(e, src), e.start_byte()).flatten(),
            "field_expression" | "attribute" | "member_expression" => {
                let (obj, field) = match e.kind() {
                    "field_expression" => (fld(e, F::Value), fld(e, F::Field)),
                    "attribute" => (fld(e, F::Object), fld(e, F::Attribute)),
                    _ => (fld(e, F::Object), fld(e, F::Property)),
                };
                let field = usable(text(field?, src)).or_else(|| Some(text(field?, src)).filter(|f| f.len() == 1 && !f.as_bytes()[0].is_ascii_digit()))?;
                step(self.expr_ty(obj?, depth + 1)?, &format!(".{field}"))
            }
            "parenthesized_expression" | "reference_expression" | "non_null_expression" => self.expr_ty(e.named_child(0)?, depth + 1),
            _ => match self.lang {
                Lang::Rust => self.expr_rust(e, depth),
                Lang::Python => self.expr_py(e, depth),
                l if l.is_js_family() => self.expr_ts(e, depth),
                _ => None,
            },
        }
    }

    /// `x.m(..)` called on a receiver of type `base`: `!` for unwrapping methods, the same type for
    /// `clone`, else the method's return type.
    pub(super) fn method_ty(base: Option<Ty<'a>>, m: &str) -> Option<Ty<'a>> {
        /* an untyped receiver: a method only one repo type declares still says what comes back */
        let Some(base) = base else {
            return hint::untyped_method_ok(m).then(|| Cow::Owned(format!("?::{m}()")));
        };
        if hint::UNWRAPPING.contains(&m) {
            step(base, "!")
        } else if hint::SAME_TYPE.contains(&m) {
            Some(base)
        } else {
            step(base, &format!("::{m}()"))
        }
    }

    pub(super) fn type_last_call(&mut self, ty: Ty<'a>, func: Option<u32>, parent: Option<u32>) {
        self.set_type(ty, func, parent)
    }

    fn set_type(&mut self, ty: Ty<'a>, func: Option<u32>, parent: Option<u32>) {
        let ty = if ty.contains("Self") {
            match self.owner(func, parent) {
                Some(o) => hint::subst_self(&ty, &o),
                None => return,
            }
        } else {
            ty.into_owned()
        };
        if let Some(c) = self.out.calls.last_mut() {
            if hint::is_shape(&ty) { c.rtype = Some(ty) } else { c.rhint = Some(ty) }
        }
    }

    /// Settle what this file's own declarations can: the rest waits for the index.
    pub(super) fn infer_finish(&mut self) {
        let Extract { calls, fields, rets, .. } = &mut self.out;
        let local = Local(fields, rets);
        for c in calls.iter_mut().filter(|c| c.rhint.is_some()) {
            if let Some(t) = c.rhint.as_deref().and_then(|h| hint::settle(h, &local)) {
                c.rtype = Some(t);
                c.rhint = None;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::lang::Lang;
    use crate::parse::extract;

    /// (callee, receiver type or hint) of every call, in source order.
    pub(crate) fn typed(lang: Lang, src: &str) -> Vec<(String, Option<String>)> {
        extract(lang, src.as_bytes()).calls.into_iter().map(|c| (c.callee, c.rtype.or(c.rhint))).collect()
    }

    pub(crate) fn ty<'a>(v: &'a [(String, Option<String>)], callee: &str) -> Vec<Option<&'a str>> {
        v.iter().filter(|c| c.0 == callee).map(|c| c.1.as_deref()).collect()
    }
}
