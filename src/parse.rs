use crate::lang::Lang;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use tree_sitter::{Node, Parser, Tree};

pub(crate) mod infer;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    TypeAlias,
    Impl,
    Module,
    Const,
    Macro,
}

impl SymKind {
    pub fn is_callable(self) -> bool {
        matches!(self, SymKind::Function | SymKind::Method | SymKind::Class | SymKind::Macro)
    }
    /// Kinds whose members are listed under them in a public-API skeleton.
    pub fn is_container(self) -> bool {
        matches!(self, SymKind::Class | SymKind::Trait | SymKind::Impl | SymKind::Module)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            SymKind::Function => "function",
            SymKind::Method => "method",
            SymKind::Class => "class",
            SymKind::Struct => "struct",
            SymKind::Enum => "enum",
            SymKind::Trait => "trait",
            SymKind::Interface => "interface",
            SymKind::TypeAlias => "type",
            SymKind::Impl => "impl",
            SymKind::Module => "module",
            SymKind::Const => "const",
            SymKind::Macro => "macro",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymKind,
    pub start_line: u32,
    pub end_line: u32,
    pub sig: String,
    pub exported: bool,
    pub parent: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallSite {
    /// enclosing symbol index; None = module-level code
    pub caller: Option<u32>,
    pub callee: String,
    /// `Foo` in `Foo::new()`; helps resolve associated functions
    pub qual: Option<String>,
    pub line: u32,
    /// called on a receiver (`x.f()`, `self.f()`): a method, not a free function
    #[serde(default)]
    pub recv: bool,
    /// the receiver's type when the code states it (`self` in `impl Foo`, `let x = Foo::new()`, `x: &Foo`)
    #[serde(default)]
    pub rtype: Option<String>,
    /// how the receiver's type follows from other files when it is not stated here: `Svc.db` (a field),
    /// `::open()` / `Repo::get()` (a return type), `!` (unwrapped by `?`, `.unwrap()`, `await`)
    #[serde(default)]
    pub rhint: Option<String>,
    /// `x.f()` written inside a macro's arguments (raw tokens, no call node)
    #[serde(default)]
    pub in_macro: bool,
}

/// A type member's declared type: a field (`db: Database`) or a function's return type (`-> Repo`,
/// owner "" for a free function). Types are `Name` or `Wrapper<Name>` (`Result<Repo>`, `Arc<Db>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub owner: String,
    pub name: String,
    pub ty: String,
    /// an associated function with no receiver (`fn new() -> Self`): never `x.name()`
    #[serde(default)]
    pub assoc: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Import {
    pub raw: String,
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectiveKind {
    /// explicit `@agent-rule:` comment
    Rule,
    /// sensitive syntactic pattern (unsafe, eval, ...)
    Pattern,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Directive {
    pub kind: DirectiveKind,
    pub line: u32,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Extract {
    pub symbols: Vec<Symbol>,
    pub calls: Vec<CallSite>,
    pub imports: Vec<Import>,
    pub directives: Vec<Directive>,
    pub lines: u32,
    /// declared field types, for receivers like `self.db.query()` typed in another file
    #[serde(default)]
    pub fields: Vec<Member>,
    /// declared return types, for `let x = f()` / `f().m()`
    #[serde(default)]
    pub rets: Vec<Member>,
}

const RUST_KINDS: &[&str] = &[
    "function_item", "function_signature_item", "struct_item", "enum_item", "union_item", "type_item", "const_item",
    "static_item", "trait_item", "impl_item", "mod_item", "macro_definition", "use_declaration", "unsafe_block",
    "call_expression",
];
const JS_KINDS: &[&str] = &[
    "export_statement", "import_statement", "function_declaration", "generator_function_declaration",
    "function_signature", "class_declaration", "abstract_class_declaration", "class", "interface_declaration",
    "type_alias_declaration", "enum_declaration", "method_definition", "method_signature", "abstract_method_signature",
    "variable_declarator", "call_expression", "new_expression", "assignment_expression", "jsx_attribute",
];
const PY_KINDS: &[&str] = &["function_definition", "class_definition", "import_statement", "import_from_statement", "call"];

/// Subtrees that can hold no definition, call, import or directive: the walk does not descend into them.
/// (Rust macro arguments are raw `token_tree`s, so calls inside macros never were call nodes.)
const RUST_OPAQUE: &[&str] = &[
    "token_tree", "string_literal", "raw_string_literal", "char_literal", "attribute_item", "inner_attribute_item",
    "type_parameters", "scoped_type_identifier", "where_clause",
    "parameters", "visibility_modifier", "use_declaration", "lifetime", "primitive_type",
];
const JS_OPAQUE: &[&str] = &[
    "string", "regex", "jsx_text", "import_statement",
    "literal_type", "predefined_type",
];
const PY_OPAQUE: &[&str] = &["string", "import_statement", "import_from_statement"];

/// The kinds `visit` handles, and the kinds whose subtrees it skips.
pub(crate) type KindLists = (&'static [&'static str], &'static [&'static str]);

/// Per language: the kinds `visit` handles and the kinds whose subtrees it skips.
fn kind_lists(l: Lang) -> KindLists {
    match l {
        Lang::Rust => (RUST_KINDS, RUST_OPAQUE),
        Lang::Python => (PY_KINDS, PY_OPAQUE),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => (JS_KINDS, JS_OPAQUE),
        Lang::Go => crate::parse_go::kind_lists(),
        Lang::Java | Lang::CSharp => crate::parse_jvm::kind_lists(l),
        _ => crate::parse_basic::kind_lists(l),
    }
}

/// Per language: kind id -> true for every kind named in `pick(lists)` (and every comment kind).
fn kind_table(pick: fn(KindLists) -> &'static [&'static str]) -> Vec<Vec<bool>> {
    Lang::ALL
        .iter()
        .map(|&l| {
            let ts = l.ts_language();
            let kinds = pick(kind_lists(l));
            (0..ts.node_kind_count() as u16)
                .map(|id| ts.node_kind_for_id(id).map(|k| k.contains("comment") || kinds.contains(&k)).unwrap_or(false))
                .collect()
        })
        .collect()
}

/// Subtrees `walk` does not descend into.
static OPAQUE: once_cell::sync::Lazy<Vec<Vec<bool>>> = once_cell::sync::Lazy::new(|| kind_table(|k| k.1));

/// Per language: kind id -> does `visit` handle it (definitions, calls, imports, patterns, comments).
static INTERESTING: once_cell::sync::Lazy<Vec<Vec<bool>>> = once_cell::sync::Lazy::new(|| {
    /* plus the kinds receiver-type inference reads (bindings) */
    let mut t = kind_table(|k| k.0);
    for (row, &l) in t.iter_mut().zip(Lang::ALL.iter()) {
        for (id, v) in row.iter_mut().enumerate() {
            *v |= infer::role(l, id as u16) != 0;
        }
    }
    t
});

thread_local! {
    static PARSERS: RefCell<Vec<Option<Parser>>> = RefCell::new((0..Lang::ALL.len()).map(|_| None).collect());
}

static TS_ALLOC: std::sync::Once = std::sync::Once::new();

unsafe extern "C" fn mi_calloc(n: usize, size: usize) -> *mut std::ffi::c_void {
    match n.checked_mul(size) {
        /* SAFETY: plain zeroed allocation from the same mimalloc family */
        Some(total) => unsafe { libmimalloc_sys::mi_zalloc(total) },
        None => std::ptr::null_mut(),
    }
}

/// Route tree-sitter's C allocations through mimalloc (libc malloc contends badly across 12 threads).
/// Runs once, before the first parser exists; every tree-sitter entry point in this crate calls it.
pub fn init_allocator() {
    TS_ALLOC.call_once(|| {
        /* SAFETY: one allocator family (mimalloc), never null for non-zero sizes, malloc-compatible
         * alignment, and installed before any tree-sitter object is created (guarded by Once). */
        unsafe {
            tree_sitter::set_allocator(Some(tree_sitter::Allocator {
                malloc: libmimalloc_sys::mi_malloc,
                calloc: mi_calloc,
                realloc: libmimalloc_sys::mi_realloc,
                free: libmimalloc_sys::mi_free,
            }));
        }
    });
}

/// Run `f` with this thread's parser for `lang` (rayon workers reuse theirs).
fn with_parser<T>(lang: Lang, f: impl FnOnce(&mut Parser) -> Option<T>) -> Option<T> {
    init_allocator();
    PARSERS.with(|cell| {
        let mut parsers = cell.borrow_mut();
        let slot = &mut parsers[lang.slot()];
        if slot.is_none() {
            let mut p = Parser::new();
            p.set_language(&lang.ts_language()).ok()?;
            *slot = Some(p);
        }
        f(slot.as_mut().unwrap())
    })
}

/// Parse with a per-thread cached parser.
pub fn parse_tree(lang: Lang, src: &[u8]) -> Option<Tree> {
    with_parser(lang, |p| p.parse(src, None))
}

/// Re-parse `src` reusing `old`, already `edit`ed to match it: only the changed regions are parsed again.
pub fn reparse(lang: Lang, src: &[u8], old: &Tree) -> Option<Tree> {
    with_parser(lang, |p| p.parse(src, Some(old)))
}

/// Parse only `ranges` of `src`; nodes keep the byte offsets and positions of the whole file.
/// `old`, when given, is an earlier tree of the same ranges, already `edit`ed to match `src`.
pub fn parse_ranges(lang: Lang, src: &[u8], ranges: &[tree_sitter::Range], old: Option<&Tree>) -> Option<Tree> {
    with_parser(lang, |p| {
        p.set_included_ranges(ranges).ok()?;
        let t = p.parse(src, old);
        /* the parser is reused for whole files next: always clear the ranges */
        p.set_included_ranges(&[]).ok()?;
        t
    })
}

pub fn extract(lang: Lang, src: &[u8]) -> Extract {
    match parse_tree(lang, src) {
        Some(tree) => extract_tree(lang, src, &tree),
        None => Extract::default(),
    }
}

/// `extract` from a tree the caller already has for `src`.
pub fn extract_tree(lang: Lang, src: &[u8], tree: &Tree) -> Extract {
    let mut ex = Extractor { lang, src, out: Extract::default(), pkgs: Vec::new(), env: infer::Env::default(), sym_ends: Vec::new() };
    ex.out.lines = (memchr::memchr_iter(b'\n', src).count() + 1) as u32;
    ex.walk(tree.root_node());
    ex.infer_finish();
    ex.out
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Ctx {
    pub(crate) parent: Option<u32>,
    pub(crate) func: Option<u32>,
    pub(crate) exported: bool,
    /// inside a container whose members inherit visibility (trait, trait impl, exported class)
    pub(crate) member_public: bool,
    pub(crate) in_class: bool,
}

pub(crate) struct Extractor<'a> {
    pub(crate) lang: Lang,
    pub(crate) src: &'a [u8],
    pub(crate) out: Extract,
    /// Go: names the file's imports bind (`fmt`, an alias), so `fmt.Println()` is a package call
    pub(crate) pkgs: Vec<String>,
    env: infer::Env<'a>,
    /// end byte of each symbol
    sym_ends: Vec<usize>,
}

pub fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[node.byte_range()]).unwrap_or("")
}

pub(crate) fn collapse_ws(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
        if out.len() >= max {
            out.push('…');
            break;
        }
    }
    out.trim_end().to_string()
}

/// Header text of a definition: everything before its body.
pub fn signature_of(node: Node, src: &[u8]) -> String {
    let end = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or_else(|| {
            let r = node.byte_range();
            let nl = memchr::memchr(b'\n', &src[r.clone()]).map(|i| r.start + i).unwrap_or(r.end);
            nl
        });
    let s = std::str::from_utf8(&src[node.start_byte()..end.max(node.start_byte())]).unwrap_or("");
    collapse_ws(s.trim_end_matches(|c: char| c == '{' || c == ':' || c.is_whitespace()), 240)
}

impl<'a> Extractor<'a> {
    pub(crate) fn line(n: Node) -> u32 {
        n.start_position().row as u32 + 1
    }

    pub(crate) fn push_sym(&mut self, node: Node, name: &str, kind: SymKind, exported: bool, ctx: &Ctx, sig: String) -> u32 {
        let idx = self.out.symbols.len() as u32;
        self.out.symbols.push(Symbol {
            name: name.to_string(),
            kind,
            start_line: Self::line(node),
            end_line: node.end_position().row as u32 + 1,
            sig,
            exported,
            parent: ctx.parent,
        });
        self.sym_ends.push(node.end_byte());
        idx
    }

    /// Pre-order walk with one TreeCursor (no per-node allocation); `ctx` is the context of the current sibling level.
    fn walk(&mut self, root: Node<'a>) {
        let mut cur = root.walk();
        let mut saved: Vec<Ctx> = Vec::with_capacity(64);
        let mut ctx = Ctx::default();
        let mut child_ctx = self.visit(root, ctx);
        let opaque = &OPAQUE[self.lang.slot()];
        loop {
            let skip = opaque.get(cur.node().kind_id() as usize).copied().unwrap_or(false);
            if !skip && cur.goto_first_child() {
                saved.push(ctx);
                ctx = child_ctx;
            } else {
                loop {
                    if cur.goto_next_sibling() {
                        break;
                    }
                    if !cur.goto_parent() {
                        return;
                    }
                    ctx = saved.pop().unwrap_or_default();
                }
            }
            let n = cur.node();
            /* anonymous nodes are tokens: nothing to extract, nothing below */
            child_ctx = if n.is_named() { self.visit(n, ctx) } else { ctx };
        }
    }

    fn visit(&mut self, node: Node<'a>, ctx: Ctx) -> Ctx {
        /* most nodes are expressions we never look at: one table lookup instead of string matching */
        if !INTERESTING[self.lang.slot()].get(node.kind_id() as usize).copied().unwrap_or(false) {
            return ctx;
        }
        match infer::role(self.lang, node.kind_id()) {
            0 => {}
            1 => self.infer_bind(node, ctx.func, ctx.parent),
            _ => {
                self.infer_bind(node, ctx.func, ctx.parent);
                return ctx;
            }
        }
        let kind = node.kind();
        if kind.contains("comment") {
            self.comment(node);
            return ctx;
        }
        match self.lang {
            Lang::Rust => self.visit_rust(node, ctx),
            Lang::Python => self.visit_python(node, ctx),
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => self.visit_js(node, ctx),
            Lang::Go => self.visit_go(node, ctx),
            Lang::Java => self.visit_java(node, ctx),
            Lang::CSharp => self.visit_csharp(node, ctx),
            _ => self.visit_basic(node, ctx),
        }
    }

    fn comment(&mut self, node: Node) {
        let t = text(node, self.src);
        if let Some(pos) = memchr::memmem::find(t.as_bytes(), b"@agent-rule") {
            let rest = t[pos + "@agent-rule".len()..].trim_start_matches(':').trim();
            let rest = rest.trim_end_matches("*/").trim();
            self.out.directives.push(Directive {
                kind: DirectiveKind::Rule,
                line: Self::line(node),
                text: collapse_ws(rest, 400),
            });
        }
    }

    pub(crate) fn pattern(&mut self, node: Node, text: &str) {
        self.out.directives.push(Directive {
            kind: DirectiveKind::Pattern,
            line: Self::line(node),
            text: text.to_string(),
        });
    }

    pub(crate) fn call(&mut self, node: Node<'a>, name: &str, qual: Option<String>, ctx: &Ctx) {
        self.push_call(node, name, qual, false, ctx);
    }

    pub(crate) fn method_call(&mut self, node: Node<'a>, name: &str, qual: Option<String>, ctx: &Ctx) {
        self.push_call(node, name, qual, true, ctx);
    }

    pub(crate) fn push_call(&mut self, node: Node<'a>, name: &str, qual: Option<String>, recv: bool, ctx: &Ctx) {
        if name.is_empty() {
            return;
        }
        self.out.calls.push(CallSite {
            caller: ctx.func.or(ctx.parent),
            callee: name.to_string(),
            qual,
            line: Self::line(node),
            recv,
            rtype: None,
            rhint: None,
            in_macro: false,
        });
        self.infer_call(node, ctx.func, ctx.parent);
    }

    /* ---------------- Rust ---------------- */

    fn rust_pub(node: Node, src: &[u8]) -> bool {
        let mut c = node.walk();
        let r = node
            .named_children(&mut c)
            .any(|ch| ch.kind() == "visibility_modifier" && text(ch, src).starts_with("pub"));
        r
    }

    fn visit_rust(&mut self, node: Node<'a>, ctx: Ctx) -> Ctx {
        let src = self.src;
        let name_of = |n: Node| n.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("");
        let mut c = ctx;
        match node.kind() {
            "function_item" | "function_signature_item" => {
                let exported = Self::rust_pub(node, src) || ctx.member_public;
                let kind = if ctx.in_class { SymKind::Method } else { SymKind::Function };
                let sig = signature_of(node, src);
                if sig.starts_with("unsafe") || sig.contains(" unsafe fn") || sig.starts_with("pub unsafe") {
                    self.pattern(node, "unsafe fn: requires memory-safety verification of every caller contract");
                }
                let i = self.push_sym(node, name_of(node), kind, exported, &ctx, sig);
                c.parent = Some(i);
                c.func = Some(i);
                c.in_class = false;
                c.member_public = false;
            }
            "struct_item" | "enum_item" | "union_item" | "type_item" | "const_item" | "static_item" => {
                let kind = match node.kind() {
                    "struct_item" | "union_item" => SymKind::Struct,
                    "enum_item" => SymKind::Enum,
                    "type_item" => SymKind::TypeAlias,
                    _ => SymKind::Const,
                };
                if node.kind() == "static_item" && text(node, src).starts_with("static mut")
                    || text(node, src).contains("static mut ")
                {
                    self.pattern(node, "static mut: unsynchronized global state, data-race risk");
                }
                let exported = Self::rust_pub(node, src);
                let i = self.push_sym(node, name_of(node), kind, exported, &ctx, signature_of(node, src));
                c.parent = Some(i);
            }
            "trait_item" => {
                let exported = Self::rust_pub(node, src);
                let i = self.push_sym(node, name_of(node), SymKind::Trait, exported, &ctx, signature_of(node, src));
                c.parent = Some(i);
                c.in_class = true;
                c.member_public = exported;
            }
            "impl_item" => {
                let ty = node.child_by_field_name("type").map(|t| text(t, src)).unwrap_or("");
                let ty = ty.split('<').next().unwrap_or(ty).trim().to_string();
                let is_trait_impl = node.child_by_field_name("trait").is_some();
                let sig = signature_of(node, src);
                if sig.starts_with("unsafe impl") {
                    self.pattern(node, "unsafe impl: manual Send/Sync or unsafe trait contract must be verified");
                }
                let i = self.push_sym(node, &ty, SymKind::Impl, true, &ctx, sig);
                c.parent = Some(i);
                c.in_class = true;
                c.member_public = is_trait_impl;
            }
            "mod_item" => {
                let name = name_of(node);
                if node.child_by_field_name("body").is_none() {
                    self.out.imports.push(Import { raw: format!("mod {name}"), line: Self::line(node) });
                }
                let exported = Self::rust_pub(node, src);
                let i = self.push_sym(node, name, SymKind::Module, exported, &ctx, signature_of(node, src));
                c.parent = Some(i);
            }
            "macro_definition" => {
                self.push_sym(node, name_of(node), SymKind::Macro, true, &ctx, signature_of(node, src));
            }
            "use_declaration" => {
                if let Some(arg) = node.child_by_field_name("argument") {
                    self.out.imports.push(Import { raw: collapse_ws(text(arg, src), 300), line: Self::line(node) });
                }
            }
            "unsafe_block" => {
                self.pattern(node, "unsafe block: requires memory-safety verification");
            }
            "call_expression" => {
                if let Some(f) = node.child_by_field_name("function") {
                    let f = if f.kind() == "generic_function" { f.child_by_field_name("function").unwrap_or(f) } else { f };
                    match f.kind() {
                        "identifier" => self.call(node, text(f, src), None, &ctx),
                        "field_expression" => {
                            if let Some(fl) = f.child_by_field_name("field") {
                                let qual = f.child_by_field_name("value").filter(|v| v.kind() == "self").map(|_| "self".to_string());
                                self.method_call(node, text(fl, src), qual, &ctx)
                            }
                        }
                        "scoped_identifier" => {
                            let name = f.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("");
                            let qual = f.child_by_field_name("path").map(|p| {
                                let t = text(p, src);
                                t.rsplit("::").next().unwrap_or(t).to_string()
                            });
                            if name == "transmute" {
                                self.pattern(node, "mem::transmute: reinterprets bits, verify layout/lifetime invariants");
                            }
                            self.call(node, name, qual, &ctx)
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        c
    }

    /* ---------------- TypeScript / JavaScript ---------------- */

    fn js_private_member(node: Node, src: &[u8]) -> bool {
        let mut c = node.walk();
        let private = node.children(&mut c).any(|ch| {
            ch.kind() == "accessibility_modifier" && matches!(text(ch, src), "private" | "protected")
        });
        private
            || node
                .child_by_field_name("name")
                .map(|n| n.kind() == "private_property_identifier")
                .unwrap_or(false)
    }

    fn visit_js(&mut self, node: Node<'a>, ctx: Ctx) -> Ctx {
        let src = self.src;
        let name_of = |n: Node| n.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("");
        let mut c = ctx;
        match node.kind() {
            "export_statement" => {
                c.exported = true;
                if let Some(s) = node.child_by_field_name("source") {
                    self.out.imports.push(Import { raw: unquote(text(s, src)), line: Self::line(node) });
                }
            }
            "import_statement" => {
                if let Some(s) = node.child_by_field_name("source") {
                    self.out.imports.push(Import { raw: unquote(text(s, src)), line: Self::line(node) });
                }
            }
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                let exported = ctx.exported && ctx.func.is_none();
                let i = self.push_sym(node, name_of(node), SymKind::Function, exported, &ctx, signature_of(node, src));
                c = Ctx { parent: Some(i), func: Some(i), ..Ctx::default() };
            }
            "class_declaration" | "abstract_class_declaration" | "class" => {
                let name = name_of(node);
                if name.is_empty() {
                    return c;
                }
                let exported = ctx.exported && ctx.func.is_none();
                let i = self.push_sym(node, name, SymKind::Class, exported, &ctx, signature_of(node, src));
                c = Ctx { parent: Some(i), func: None, exported: false, member_public: exported, in_class: true };
            }
            "interface_declaration" => {
                let exported = ctx.exported;
                let i = self.push_sym(node, name_of(node), SymKind::Interface, exported, &ctx, signature_of(node, src));
                c = Ctx { parent: Some(i), member_public: exported, in_class: true, ..Ctx::default() };
            }
            "type_alias_declaration" | "enum_declaration" => {
                let kind = if node.kind() == "enum_declaration" { SymKind::Enum } else { SymKind::TypeAlias };
                self.push_sym(node, name_of(node), kind, ctx.exported, &ctx, signature_of(node, src));
            }
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                let exported = ctx.member_public && !Self::js_private_member(node, src);
                let i = self.push_sym(node, name_of(node), SymKind::Method, exported, &ctx, signature_of(node, src));
                c = Ctx { parent: Some(i), func: Some(i), ..Ctx::default() };
            }
            "variable_declarator" => {
                let value = node.child_by_field_name("value");
                if let Some(v) = value {
                    if matches!(v.kind(), "arrow_function" | "function_expression" | "function" | "generator_function") {
                        let name = name_of(node);
                        let kw = node.parent().map(|p| text(p, src).split_whitespace().next().unwrap_or("const")).unwrap_or("const");
                        let head_end = v.child_by_field_name("body").map(|b| b.start_byte()).unwrap_or(v.end_byte());
                        let head = std::str::from_utf8(&src[node.start_byte()..head_end]).unwrap_or("");
                        let sig = collapse_ws(&format!("{kw} {}", head.trim_end().trim_end_matches("=>").trim_end()), 240);
                        let exported = ctx.exported && ctx.func.is_none();
                        let i = self.push_sym(node, name, SymKind::Function, exported, &ctx, sig);
                        c = Ctx { parent: Some(i), func: Some(i), ..Ctx::default() };
                    } else if ctx.func.is_none() && !ctx.in_class && ctx.exported {
                        self.push_sym(node, name_of(node), SymKind::Const, true, &ctx, collapse_ws(text(node, src).lines().next().unwrap_or(""), 160));
                    }
                }
            }
            "call_expression" => {
                if let Some(f) = node.child_by_field_name("function") {
                    match f.kind() {
                        "identifier" => {
                            let name = text(f, src);
                            if name == "require" {
                                if let Some(arg) = node.child_by_field_name("arguments").and_then(|a| a.named_child(0)) {
                                    if arg.kind() == "string" {
                                        self.out.imports.push(Import { raw: unquote(text(arg, src)), line: Self::line(node) });
                                    }
                                }
                            }
                            if name == "eval" {
                                self.pattern(node, "eval(): executes arbitrary code, injection risk");
                            }
                            self.call(node, name, None, &ctx)
                        }
                        "import" => {
                            if let Some(arg) = node.child_by_field_name("arguments").and_then(|a| a.named_child(0)) {
                                self.out.imports.push(Import { raw: unquote(text(arg, src)), line: Self::line(node) });
                            }
                        }
                        "member_expression" => {
                            if let Some(p) = f.child_by_field_name("property") {
                                let name = text(p, src);
                                let obj = f.child_by_field_name("object").map(|o| text(o, src)).unwrap_or("");
                                if matches!(name, "exec" | "execSync") && obj.contains("child_process") {
                                    self.pattern(node, "child_process exec: shell command injection risk");
                                }
                                let qual = f.child_by_field_name("object").and_then(|o| match o.kind() {
                                    "identifier" => Some(text(o, src).to_string()),
                                    "this" => Some("this".to_string()),
                                    _ => None,
                                });
                                self.method_call(node, name, qual, &ctx)
                            }
                        }
                        _ => {}
                    }
                }
            }
            "new_expression" => {
                if let Some(cn) = node.child_by_field_name("constructor") {
                    if cn.kind() == "identifier" {
                        if text(cn, src) == "Function" {
                            self.pattern(node, "new Function(): executes arbitrary code, injection risk");
                        }
                        self.call(node, text(cn, src), None, &ctx)
                    }
                }
            }
            "assignment_expression" => {
                if let Some(l) = node.child_by_field_name("left") {
                    if l.kind() == "member_expression"
                        && l.child_by_field_name("property").map(|p| matches!(text(p, src), "innerHTML" | "outerHTML")).unwrap_or(false)
                    {
                        self.pattern(node, "innerHTML assignment: XSS risk, sanitize input");
                    }
                }
            }
            "jsx_attribute" => {
                if text(node, src).starts_with("dangerouslySetInnerHTML") {
                    self.pattern(node, "dangerouslySetInnerHTML: XSS risk, sanitize input");
                }
            }
            "arrow_function" | "function_expression" | "function" => {
                /* anonymous callbacks: calls inside still belong to the enclosing symbol */
            }
            _ => {}
        }
        c
    }

    /* ---------------- Python ---------------- */

    fn visit_python(&mut self, node: Node<'a>, ctx: Ctx) -> Ctx {
        let src = self.src;
        let name_of = |n: Node| n.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("");
        let mut c = ctx;
        match node.kind() {
            "function_definition" => {
                let name = name_of(node);
                let public = !name.starts_with('_') || (name.starts_with("__") && name.ends_with("__"));
                let exported = public && ctx.func.is_none() && (!ctx.in_class || ctx.member_public);
                let kind = if ctx.in_class { SymKind::Method } else { SymKind::Function };
                let i = self.push_sym(node, name, kind, exported, &ctx, signature_of(node, src));
                c = Ctx { parent: Some(i), func: Some(i), ..Ctx::default() };
            }
            "class_definition" => {
                let name = name_of(node);
                let exported = !name.starts_with('_') && ctx.func.is_none() && !ctx.in_class;
                let i = self.push_sym(node, name, SymKind::Class, exported, &ctx, signature_of(node, src));
                c = Ctx { parent: Some(i), func: None, exported: false, member_public: exported, in_class: true };
            }
            "import_statement" => {
                let mut cur = node.walk();
                for n in node.children_by_field_name("name", &mut cur) {
                    let n = if n.kind() == "aliased_import" { n.child_by_field_name("name").unwrap_or(n) } else { n };
                    self.out.imports.push(Import { raw: text(n, src).to_string(), line: Self::line(node) });
                }
            }
            "import_from_statement" => {
                if let Some(m) = node.child_by_field_name("module_name") {
                    self.out.imports.push(Import { raw: text(m, src).to_string(), line: Self::line(node) });
                }
            }
            "call" => {
                if let Some(f) = node.child_by_field_name("function") {
                    let (name, qual) = match f.kind() {
                        "identifier" => (text(f, src), None),
                        "attribute" => (
                            f.child_by_field_name("attribute").map(|a| text(a, src)).unwrap_or(""),
                            f.child_by_field_name("object").filter(|o| o.kind() == "identifier").map(|o| text(o, src).to_string()),
                        ),
                        _ => ("", None),
                    };
                    match (qual.as_deref(), name) {
                        (None, "eval" | "exec") => self.pattern(node, "eval/exec: executes arbitrary code, injection risk"),
                        (Some("pickle"), "loads" | "load") => self.pattern(node, "pickle.load: deserializing untrusted data executes code"),
                        (Some("os"), "system") => self.pattern(node, "os.system: shell command injection risk"),
                        (Some("yaml"), "load") => self.pattern(node, "yaml.load without SafeLoader: code execution risk"),
                        (Some("subprocess"), _) if text(node, src).contains("shell=True") => {
                            self.pattern(node, "subprocess shell=True: shell command injection risk")
                        }
                        _ => {}
                    }
                    let recv = f.kind() == "attribute";
                    self.push_call(node, name, qual, recv, &ctx);
                }
            }
            _ => {}
        }
        c
    }
}

pub(crate) fn unquote(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(ex: &Extract) -> Vec<(&str, SymKind, bool)> {
        ex.symbols.iter().map(|s| (s.name.as_str(), s.kind, s.exported)).collect()
    }

    #[test]
    fn rust_symbols_calls_imports_directives() {
        let src = br#"
use crate::db::repo::Repo;
mod util;
// @agent-rule: Do not modify this signature without updating schema.proto
pub struct UserService { repo: Repo }
impl UserService {
    /// Fetch profile
    pub async fn get_profile(&self, id: u64) -> Result<(), ()> {
        let u = self.repo.get_by_id(id);
        helper(u);
        Repo::new();
        Ok(())
    }
    fn private_one(&self) { unsafe { std::ptr::null::<u8>().read(); } }
}
fn helper<T>(_t: T) {}
"#;
        let ex = extract(Lang::Rust, src);
        let n = names(&ex);
        assert!(n.contains(&("UserService", SymKind::Struct, true)));
        assert!(n.contains(&("get_profile", SymKind::Method, true)));
        assert!(n.contains(&("private_one", SymKind::Method, false)));
        assert!(n.contains(&("helper", SymKind::Function, false)));
        let gp = ex.symbols.iter().find(|s| s.name == "get_profile").unwrap();
        assert_eq!(gp.sig, "pub async fn get_profile(&self, id: u64) -> Result<(), ()>");
        let callees: Vec<_> = ex.calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(callees.contains(&"get_by_id") && callees.contains(&"helper") && callees.contains(&"new"));
        let newc = ex.calls.iter().find(|c| c.callee == "new").unwrap();
        assert_eq!(newc.qual.as_deref(), Some("Repo"));
        assert_eq!(ex.symbols[newc.caller.unwrap() as usize].name, "get_profile");
        assert!(ex.imports.iter().any(|i| i.raw == "crate::db::repo::Repo"));
        assert!(ex.imports.iter().any(|i| i.raw == "mod util"));
        assert!(ex.directives.iter().any(|d| d.kind == DirectiveKind::Rule && d.text.starts_with("Do not modify")));
        assert!(ex.directives.iter().any(|d| d.kind == DirectiveKind::Pattern && d.text.contains("unsafe block")));
    }

    #[test]
    fn typescript_exports_and_calls() {
        let src = br#"
import { db } from "./db";
export interface User { id: string }
export class UserService {
  private cache = new Map();
  async getProfile(id: string): Promise<User> { return db.find(id); }
  private secret() { eval("x"); }
}
export const handler = async (req: Request) => { new UserService().getProfile("1"); };
function internal() {}
"#;
        let ex = extract(Lang::TypeScript, src);
        let n = names(&ex);
        assert!(n.contains(&("User", SymKind::Interface, true)));
        assert!(n.contains(&("UserService", SymKind::Class, true)));
        assert!(n.contains(&("getProfile", SymKind::Method, true)));
        assert!(n.contains(&("secret", SymKind::Method, false)));
        assert!(n.contains(&("handler", SymKind::Function, true)));
        assert!(n.contains(&("internal", SymKind::Function, false)));
        assert!(ex.imports.iter().any(|i| i.raw == "./db"));
        let callees: Vec<_> = ex.calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(callees.contains(&"find") && callees.contains(&"getProfile") && callees.contains(&"UserService"));
        assert!(ex.directives.iter().any(|d| d.text.starts_with("eval")));
        let h = ex.symbols.iter().find(|s| s.name == "handler").unwrap();
        assert_eq!(h.sig, "const handler = async (req: Request)");
    }

    #[test]
    fn tsx_and_javascript() {
        let ex = extract(Lang::Tsx, b"export function App() { return <div dangerouslySetInnerHTML={{__html: x}} /> }");
        assert_eq!(ex.symbols[0].name, "App");
        assert!(ex.directives.iter().any(|d| d.text.contains("dangerouslySetInnerHTML")));
        let ex = extract(Lang::JavaScript, b"const fs = require('fs');\nclass A { run() { helper(); } }\nfunction helper() {}");
        assert!(ex.imports.iter().any(|i| i.raw == "fs"));
        assert!(names(&ex).contains(&("run", SymKind::Method, false)));
        assert_eq!(ex.calls[1].callee, "helper");
    }

    #[test]
    fn python_symbols() {
        let src = b"import os\nfrom .models import User\n\nclass Svc:\n    \"\"\"doc\"\"\"\n    def get(self, id):\n        return fetch(id)\n    def _hidden(self):\n        os.system('ls')\n\ndef fetch(id):\n    return eval(id)\n";
        let ex = extract(Lang::Python, src);
        let n = names(&ex);
        assert!(n.contains(&("Svc", SymKind::Class, true)));
        assert!(n.contains(&("get", SymKind::Method, true)));
        assert!(n.contains(&("_hidden", SymKind::Method, false)));
        assert!(n.contains(&("fetch", SymKind::Function, true)));
        assert!(ex.imports.iter().any(|i| i.raw == "os") && ex.imports.iter().any(|i| i.raw == ".models"));
        assert_eq!(ex.directives.iter().filter(|d| d.kind == DirectiveKind::Pattern).count(), 2);
        assert_eq!(ex.symbols.iter().find(|s| s.name == "get").unwrap().sig, "def get(self, id)");
    }
}
