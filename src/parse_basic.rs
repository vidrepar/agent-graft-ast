//! Tier-2 languages (C, C++, Ruby, PHP, Kotlin, Swift, Bash, Lua, Scala): functions, types, methods and
//! calls, from one table per language. No imports and no rename rules: these get indexing, context,
//! read_file, callers and structural rewrite.

use crate::lang::Lang;
use crate::parse::{signature_of, text, Ctx, Extractor, KindLists, SymKind};
use tree_sitter::Node;

/// What one tier-2 language declares.
struct Spec {
    /// type-like declarations: (node kind, symbol kind); members inside them are methods
    types: &'static [(&'static str, SymKind)],
    /// function / method definitions
    funcs: &'static [&'static str],
    calls: &'static [&'static str],
    opaque: &'static [&'static str],
}

const C: Spec = Spec {
    types: &[("struct_specifier", SymKind::Struct), ("union_specifier", SymKind::Struct), ("enum_specifier", SymKind::Enum)],
    funcs: &["function_definition"],
    calls: &["call_expression"],
    opaque: &["string_literal", "char_literal", "preproc_include", "parameter_list"],
};
const CPP: Spec = Spec {
    types: &[
        ("class_specifier", SymKind::Class), ("struct_specifier", SymKind::Struct), ("union_specifier", SymKind::Struct),
        ("enum_specifier", SymKind::Enum),
    ],
    funcs: &["function_definition", "field_declaration", "declaration"],
    calls: &["call_expression"],
    opaque: &["string_literal", "raw_string_literal", "char_literal", "preproc_include", "parameter_list", "template_argument_list"],
};
const RUBY: Spec = Spec {
    types: &[("class", SymKind::Class), ("module", SymKind::Module)],
    funcs: &["method", "singleton_method"],
    calls: &["call"],
    opaque: &["string", "heredoc_body", "method_parameters"],
};
const PHP: Spec = Spec {
    types: &[
        ("class_declaration", SymKind::Class), ("interface_declaration", SymKind::Interface), ("trait_declaration", SymKind::Trait),
        ("enum_declaration", SymKind::Enum),
    ],
    funcs: &["function_definition", "method_declaration"],
    calls: &["function_call_expression", "member_call_expression", "scoped_call_expression", "object_creation_expression"],
    opaque: &["string", "encapsed_string", "heredoc", "namespace_use_declaration", "formal_parameters", "text"],
};
const KOTLIN: Spec = Spec {
    types: &[("class_declaration", SymKind::Class), ("object_declaration", SymKind::Class)],
    funcs: &["function_declaration"],
    calls: &["call_expression"],
    opaque: &["string_literal", "import", "package_header", "function_value_parameters"],
};
const SWIFT: Spec = Spec {
    types: &[("class_declaration", SymKind::Class), ("protocol_declaration", SymKind::Interface)],
    funcs: &["function_declaration", "protocol_function_declaration"],
    calls: &["call_expression"],
    opaque: &["line_string_literal", "multi_line_string_literal", "import_declaration"],
};
const BASH: Spec = Spec {
    types: &[],
    funcs: &["function_definition"],
    calls: &["command"],
    opaque: &["string", "raw_string", "heredoc_body"],
};
const LUA: Spec = Spec {
    types: &[],
    funcs: &["function_declaration"],
    calls: &["function_call"],
    opaque: &["string", "parameters"],
};
const SCALA: Spec = Spec {
    types: &[("class_definition", SymKind::Class), ("object_definition", SymKind::Class), ("trait_definition", SymKind::Trait)],
    funcs: &["function_definition", "function_declaration"],
    calls: &["call_expression", "instance_expression"],
    opaque: &["string", "interpolated_string_expression", "import_declaration", "package_clause", "parameters"],
};

fn spec(l: Lang) -> &'static Spec {
    match l {
        Lang::C => &C,
        Lang::Cpp => &CPP,
        Lang::Ruby => &RUBY,
        Lang::Php => &PHP,
        Lang::Kotlin => &KOTLIN,
        Lang::Swift => &SWIFT,
        Lang::Bash => &BASH,
        Lang::Lua => &LUA,
        _ => &SCALA,
    }
}

/// (kinds `visit` handles, kinds whose subtrees are skipped) for a tier-2 language.
pub(crate) fn kind_lists(l: Lang) -> KindLists {
    static LISTS: once_cell::sync::Lazy<Vec<(Lang, Vec<&'static str>)>> = once_cell::sync::Lazy::new(|| {
        Lang::ALL
            .iter()
            .filter(|l| l.basic())
            .map(|&l| {
                let s = spec(l);
                (l, s.types.iter().map(|t| t.0).chain(s.funcs.iter().copied()).chain(s.calls.iter().copied()).collect())
            })
            .collect()
    });
    let kinds = LISTS.iter().find(|x| x.0 == l).map(|x| x.1.as_slice()).unwrap_or(&[]);
    (kinds, spec(l).opaque)
}

/// Is `kind` a function definition in tier-2 language `l` (for read_file folding)?
pub(crate) fn is_func(l: Lang, kind: &str) -> bool {
    spec(l).funcs.contains(&kind)
}

/// The declared name of a C/C++ declarator chain (`*f(int)` -> `f`, `A::run` -> `run`), and whether it
/// declares a function. None for anything that is not a plain name.
fn c_declarator(n: Node, src: &[u8]) -> Option<(String, bool)> {
    let mut cur = n.child_by_field_name("declarator")?;
    let mut func = false;
    loop {
        match cur.kind() {
            "function_declarator" => func = true,
            "pointer_declarator" | "reference_declarator" | "parenthesized_declarator" => {}
            "identifier" | "field_identifier" | "destructor_name" | "operator_name" => return Some((text(cur, src).to_string(), func)),
            "qualified_identifier" => {
                let name = cur.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("");
                return Some((name.to_string(), func));
            }
            _ => return None,
        }
        cur = match cur.child_by_field_name("declarator") {
            Some(d) => d,
            None => cur.named_child(0)?,
        };
    }
}

/// Is a definition hidden from other files? Modifier keywords, `static` (C/C++), `local` (Lua),
/// `_`-free Ruby/C++ visibility sections.
fn is_private(lang: Lang, node: Node, src: &[u8]) -> bool {
    match lang {
        Lang::C | Lang::Cpp => {
            let mut c = node.walk();
            let is_static = node.children(&mut c).any(|ch| ch.kind() == "storage_class_specifier" && text(ch, src) == "static");
            is_static || (lang == Lang::Cpp && section_private(node, src))
        }
        Lang::Ruby => section_private(node, src),
        Lang::Lua => text(node, src).starts_with("local") || node.prev_sibling().is_some_and(|p| p.kind() == "local"),
        Lang::Bash => false,
        _ => {
            let mut c = node.walk();
            let private = |ch: Node| {
                matches!(ch.kind(), "modifiers" | "visibility_modifier")
                    && text(ch, src).split_whitespace().any(|w| matches!(w, "private" | "fileprivate") || w.starts_with("private["))
            };
            node.children(&mut c).any(private)
        }
    }
}

/// C++ `private:` / Ruby `private` sections: the nearest marker above a member decides
/// (C++ classes start private, structs public; Ruby classes start public).
fn section_private(node: Node, src: &[u8]) -> bool {
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        let marker = match p.kind() {
            "access_specifier" => Some(text(p, src)),
            "identifier" if p.parent().is_some_and(|b| b.kind() == "body_statement") => Some(text(p, src)),
            _ => None,
        };
        if let Some(m) = marker.filter(|m| matches!(m.trim_end_matches(':'), "private" | "protected" | "public")) {
            return m.trim_end_matches(':') != "public";
        }
        prev = p.prev_named_sibling();
    }
    node.parent().and_then(|b| b.parent()).is_some_and(|c| c.kind() == "class_specifier")
}

/// A tier-2 type kind refined by its keyword (Kotlin `interface`, Swift `struct`/`enum`/`protocol`).
fn refine_kind(lang: Lang, node: Node, kind: SymKind) -> SymKind {
    if !matches!(lang, Lang::Kotlin | Lang::Swift) {
        return kind;
    }
    let mut c = node.walk();
    let kw = node.children(&mut c).find(|ch| !ch.is_named()).map(|ch| ch.kind()).unwrap_or("");
    match kw {
        "interface" | "protocol" => SymKind::Interface,
        "struct" => SymKind::Struct,
        "enum" => SymKind::Enum,
        "extension" => SymKind::Impl,
        _ => kind,
    }
}

impl<'a> Extractor<'a> {
    pub(crate) fn visit_basic(&mut self, node: Node<'a>, ctx: Ctx) -> Ctx {
        let src = self.src;
        let lang = self.lang;
        let s = spec(lang);
        let kind = node.kind();
        if let Some(&(_, k)) = s.types.iter().find(|t| t.0 == kind) {
            /* C/C++ `struct X x;` is a use, not a definition: only bodies define */
            if node.child_by_field_name("body").is_none() && matches!(lang, Lang::C | Lang::Cpp) {
                return ctx;
            }
            let name = node.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("");
            if name.is_empty() {
                return ctx;
            }
            let exported = ctx.func.is_none() && !is_private(lang, node, src) && (!ctx.in_class || ctx.member_public);
            let sig = signature_of(node, src);
            let i = self.push_sym(node, name.rsplit("::").next().unwrap_or(name), refine_kind(lang, node, k), exported, &ctx, sig);
            return Ctx { parent: Some(i), func: None, exported: false, member_public: exported, in_class: true };
        }
        if s.funcs.contains(&kind) {
            let (name, qualified) = match lang {
                Lang::C | Lang::Cpp => match c_declarator(node, src) {
                    /* a C++ member declaration without a body is a method only when it declares a function */
                    Some((n, true)) => {
                        let qualified = node.child_by_field_name("declarator").is_some_and(|d| text(d, src).contains("::"));
                        (n, qualified)
                    }
                    _ => return ctx,
                },
                Lang::Lua => {
                    let Some(n) = node.child_by_field_name("name") else { return ctx };
                    let last = match n.kind() {
                        "dot_index_expression" => n.child_by_field_name("field"),
                        "method_index_expression" => n.child_by_field_name("method"),
                        _ => Some(n),
                    };
                    (last.map(|x| text(x, src)).unwrap_or("").to_string(), n.kind() != "identifier")
                }
                _ => (node.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("").to_string(), false),
            };
            /* C++ `declaration` outside a class is a prototype: the definition is the symbol */
            if kind == "declaration" && !ctx.in_class || name.is_empty() {
                return ctx;
            }
            let exported = ctx.func.is_none() && !is_private(lang, node, src) && (!ctx.in_class || ctx.member_public);
            let k = if ctx.in_class || qualified || kind == "singleton_method" { SymKind::Method } else { SymKind::Function };
            let sig = signature_of(node, src).trim_end_matches(';').trim_end().to_string();
            let i = self.push_sym(node, &name, k, exported, &ctx, sig);
            return Ctx { parent: Some(i), func: Some(i), ..Ctx::default() };
        }
        if s.calls.contains(&kind) {
            self.basic_call(node, &ctx);
        }
        ctx
    }

    fn basic_call(&mut self, node: Node<'a>, ctx: &Ctx) {
        let src = self.src;
        let field = |n: Node<'a>, f: &str| n.child_by_field_name(f);
        /* the receiver as a qualifier: a plain name, or self/this */
        let qual_of = |o: Option<Node<'a>>| -> Option<String> {
            let o = o?;
            match o.kind() {
                "identifier" | "constant" | "simple_identifier" | "name" | "namespace_identifier" | "type_identifier" => Some(text(o, src).to_string()),
                "this" | "self" | "this_expression" | "self_expression" => Some("self".to_string()),
                "variable_name" if text(o, src) == "$this" => Some("self".to_string()),
                _ => None,
            }
        };
        match self.lang {
            Lang::C | Lang::Cpp | Lang::Scala => {
                if node.kind() == "instance_expression" {
                    let mut c = node.walk();
                    if let Some(t) = node.named_children(&mut c).find(|n| n.kind().contains("type")) {
                        let t = text(t, src);
                        self.call(node, t.split('[').next().unwrap_or(t), None, ctx);
                    }
                    return;
                }
                let Some(f) = field(node, "function") else { return };
                match f.kind() {
                    "identifier" => self.call(node, text(f, src), None, ctx),
                    "template_function" => self.call(node, field(f, "name").map(|n| text(n, src)).unwrap_or(""), None, ctx),
                    "qualified_identifier" => {
                        let name = field(f, "name").map(|n| text(n, src)).unwrap_or("");
                        let qual = field(f, "scope").map(|s| text(s, src).to_string());
                        self.call(node, name, qual, ctx)
                    }
                    "field_expression" => {
                        let m = field(f, "field");
                        let m = m.map(|m| if m.kind() == "template_method" { field(m, "name").unwrap_or(m) } else { m });
                        let recv = field(f, "argument").or_else(|| field(f, "value"));
                        self.method_call(node, m.map(|m| text(m, src)).unwrap_or(""), qual_of(recv), ctx)
                    }
                    _ => {}
                }
            }
            Lang::Ruby => {
                let name = field(node, "method").map(|m| text(m, src)).unwrap_or("");
                match field(node, "receiver") {
                    None => self.call(node, name, None, ctx),
                    r => self.method_call(node, name, qual_of(r), ctx),
                }
            }
            Lang::Php => match node.kind() {
                "function_call_expression" => {
                    if let Some(f) = field(node, "function").filter(|f| matches!(f.kind(), "name" | "qualified_name")) {
                        let t = text(f, src);
                        self.call(node, t.rsplit('\\').next().unwrap_or(t), None, ctx)
                    }
                }
                "member_call_expression" => {
                    let name = field(node, "name").map(|n| text(n, src)).unwrap_or("");
                    self.method_call(node, name, qual_of(field(node, "object")), ctx)
                }
                "scoped_call_expression" => {
                    let name = field(node, "name").map(|n| text(n, src)).unwrap_or("");
                    self.method_call(node, name, qual_of(field(node, "scope")), ctx)
                }
                _ => {
                    let mut c = node.walk();
                    if let Some(t) = node.named_children(&mut c).find(|n| matches!(n.kind(), "name" | "qualified_name")) {
                        let t = text(t, src);
                        self.call(node, t.rsplit('\\').next().unwrap_or(t), None, ctx)
                    }
                }
            },
            Lang::Kotlin | Lang::Swift => {
                let Some(f) = node.named_child(0) else { return };
                match f.kind() {
                    "identifier" | "simple_identifier" => self.call(node, text(f, src), None, ctx),
                    "navigation_expression" => {
                        /* Kotlin: (target, identifier); Swift: target + navigation_suffix.suffix */
                        let (target, name) = match field(f, "suffix") {
                            Some(sfx) => (field(f, "target"), field(sfx, "suffix")),
                            None => (f.named_child(0), f.named_child(f.named_child_count().saturating_sub(1) as u32)),
                        };
                        self.method_call(node, name.map(|n| text(n, src)).unwrap_or(""), qual_of(target), ctx)
                    }
                    _ => {}
                }
            }
            Lang::Bash => {
                if let Some(n) = field(node, "name").and_then(|n| n.named_child(0)).filter(|w| w.kind() == "word") {
                    self.call(node, text(n, src), None, ctx)
                }
            }
            Lang::Lua => {
                let Some(n) = field(node, "name") else { return };
                match n.kind() {
                    "identifier" => self.call(node, text(n, src), None, ctx),
                    "dot_index_expression" => {
                        let name = field(n, "field").map(|x| text(x, src)).unwrap_or("");
                        self.call(node, name, qual_of(field(n, "table")), ctx)
                    }
                    "method_index_expression" => {
                        let name = field(n, "method").map(|x| text(x, src)).unwrap_or("");
                        self.method_call(node, name, qual_of(field(n, "table")), ctx)
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
