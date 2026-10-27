//! Symbol, call and import extraction for Java and C#.

use crate::lang::Lang;
use crate::parse::{KindLists, signature_of, text, Ctx, Extractor, Import, SymKind};
use tree_sitter::Node;

const JAVA_KINDS: &[&str] = &[
    "class_declaration", "record_declaration", "interface_declaration", "annotation_type_declaration",
    "enum_declaration", "method_declaration", "constructor_declaration", "import_declaration", "method_invocation",
    "object_creation_expression",
];
const JAVA_OPAQUE: &[&str] = &[
    "string_literal", "character_literal", "import_declaration", "package_declaration", "formal_parameters",
    "modifiers", "type_arguments", "type_parameters",
];
const CS_KINDS: &[&str] = &[
    "class_declaration", "record_declaration", "struct_declaration", "interface_declaration", "enum_declaration",
    "delegate_declaration", "method_declaration", "constructor_declaration", "local_function_statement",
    "using_directive", "invocation_expression", "object_creation_expression",
];
const CS_OPAQUE: &[&str] = &[
    "string_literal", "verbatim_string_literal", "raw_string_literal", "character_literal", "using_directive",
    "attribute_list", "parameter_list", "type_argument_list", "type_parameter_list", "modifier",
];


/// (kinds `visit` handles, kinds whose subtrees are skipped) for Java or C#.
pub(crate) fn kind_lists(l: Lang) -> KindLists {
    match l {
        Lang::Java => (JAVA_KINDS, JAVA_OPAQUE),
        _ => (CS_KINDS, CS_OPAQUE),
    }
}

/// Java/C# modifier keywords written on a declaration (`modifiers` node in Java, `modifier` children in C#).
fn modifiers<'a>(node: Node, src: &'a [u8]) -> Vec<&'a str> {
    let mut c = node.walk();
    let mut out = Vec::new();
    for ch in node.children(&mut c) {
        match ch.kind() {
            "modifiers" => out.extend(text(ch, src).split_whitespace().filter(|w| !w.starts_with('@'))),
            "modifier" => out.push(text(ch, src)),
            _ => {}
        }
    }
    out
}

/// The last identifier of a type name (`Foo`, `a.b.Foo`, `Foo<T>`).
fn type_name<'a>(n: Node, src: &'a [u8]) -> &'a str {
    let t = text(n, src);
    let t = t.split('<').next().unwrap_or(t);
    t.rsplit('.').next().unwrap_or(t).trim()
}

/// Header of a declaration without a trailing `;` (abstract and interface members).
fn sig(node: Node, src: &[u8]) -> String {
    signature_of(node, src).trim_end_matches(';').trim_end().to_string()
}


impl<'a> Extractor<'a> {
    /// A type declaration: its symbol, and members that inherit its visibility.
    fn jvm_type(&mut self, node: Node<'a>, ctx: Ctx, kind: SymKind) -> Ctx {
        let src = self.src;
        let name = node.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("");
        let m = modifiers(node, src);
        let public = m.iter().any(|w| matches!(*w, "public" | "protected"));
        /* members of an interface are public unless marked otherwise; a nested type follows its container */
        let exported = ctx.func.is_none() && (public || (ctx.in_class && ctx.member_public && !m.contains(&"private") && self.in_interface(ctx)));
        let i = self.push_sym(node, name, kind, exported, &ctx, sig(node, src));
        Ctx { parent: Some(i), func: None, exported: false, member_public: exported, in_class: true }
    }

    fn in_interface(&self, ctx: Ctx) -> bool {
        ctx.parent.is_some_and(|p| self.out.symbols[p as usize].kind == SymKind::Interface)
    }

    /// A method or constructor of a Java/C# type.
    fn jvm_method(&mut self, node: Node<'a>, ctx: Ctx, name: &str) -> Ctx {
        let src = self.src;
        let m = modifiers(node, src);
        let visible = m.iter().any(|w| matches!(*w, "public" | "protected")) || (self.in_interface(ctx) && !m.contains(&"private"));
        let exported = ctx.member_public && visible;
        let kind = if ctx.in_class { SymKind::Method } else { SymKind::Function };
        let i = self.push_sym(node, name, kind, exported, &ctx, sig(node, src));
        Ctx { parent: Some(i), func: Some(i), ..Ctx::default() }
    }

    pub(crate) fn visit_java(&mut self, node: Node<'a>, ctx: Ctx) -> Ctx {
        let src = self.src;
        let name_of = |n: Node| n.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("");
        match node.kind() {
            "class_declaration" | "record_declaration" => self.jvm_type(node, ctx, SymKind::Class),
            "interface_declaration" | "annotation_type_declaration" => self.jvm_type(node, ctx, SymKind::Interface),
            "enum_declaration" => self.jvm_type(node, ctx, SymKind::Enum),
            "method_declaration" | "constructor_declaration" => self.jvm_method(node, ctx, name_of(node)),
            "import_declaration" => {
                let mut cur = node.walk();
                let mut raw = String::new();
                for ch in node.named_children(&mut cur) {
                    match ch.kind() {
                        "scoped_identifier" | "identifier" => raw.push_str(text(ch, src)),
                        "asterisk" => raw.push_str(".*"),
                        _ => {}
                    }
                }
                if !raw.is_empty() {
                    self.out.imports.push(Import { raw, line: Self::line(node) });
                }
                ctx
            }
            "method_invocation" => {
                let name = name_of(node);
                let qual = match node.child_by_field_name("object") {
                    /* a bare `f()` is `this.f()` or a static of the same class */
                    None => Some("this".to_string()),
                    Some(o) if o.kind() == "this" => Some("this".to_string()),
                    Some(o) if o.kind() == "identifier" => Some(text(o, src).to_string()),
                    Some(o) => {
                        if name == "exec" && text(o, src).contains("getRuntime") {
                            self.pattern(node, "Runtime.exec: command injection risk");
                        }
                        None
                    }
                };
                self.method_call(node, name, qual, &ctx);
                ctx
            }
            "object_creation_expression" => {
                if let Some(t) = node.child_by_field_name("type") {
                    self.call(node, type_name(t, src), None, &ctx);
                }
                ctx
            }
            _ => ctx,
        }
    }

    pub(crate) fn visit_csharp(&mut self, node: Node<'a>, ctx: Ctx) -> Ctx {
        let src = self.src;
        let name_of = |n: Node| n.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("");
        match node.kind() {
            "class_declaration" | "record_declaration" => self.jvm_type(node, ctx, SymKind::Class),
            "struct_declaration" => self.jvm_type(node, ctx, SymKind::Struct),
            "interface_declaration" => self.jvm_type(node, ctx, SymKind::Interface),
            "enum_declaration" => self.jvm_type(node, ctx, SymKind::Enum),
            "delegate_declaration" => {
                let m = modifiers(node, src);
                let exported = ctx.func.is_none() && m.iter().any(|w| matches!(*w, "public" | "protected"));
                self.push_sym(node, name_of(node), SymKind::TypeAlias, exported, &ctx, sig(node, src));
                ctx
            }
            "method_declaration" | "constructor_declaration" => self.jvm_method(node, ctx, name_of(node)),
            "local_function_statement" => {
                let i = self.push_sym(node, name_of(node), SymKind::Function, false, &ctx, sig(node, src));
                Ctx { parent: Some(i), func: Some(i), ..Ctx::default() }
            }
            "using_directive" => {
                /* `using X = A.B;` binds an alias: the imported name is the last named child */
                let mut cur = node.walk();
                if let Some(n) = node.named_children(&mut cur).filter(|n| matches!(n.kind(), "identifier" | "qualified_name")).last() {
                    self.out.imports.push(Import { raw: text(n, src).to_string(), line: Self::line(node) });
                }
                ctx
            }
            "invocation_expression" => {
                let Some(f) = node.child_by_field_name("function") else { return ctx };
                let simple = |n: Node| -> &'a str {
                    /* `Foo<T>` is a generic_name whose first child is the identifier */
                    if n.kind() == "generic_name" { n.named_child(0).map(|x| text(x, src)).unwrap_or("") } else { text(n, src) }
                };
                match f.kind() {
                    "identifier" | "generic_name" => self.method_call(node, simple(f), Some("this".to_string()), &ctx),
                    "member_access_expression" => {
                        let name = f.child_by_field_name("name").map(simple).unwrap_or("");
                        let qual = f.child_by_field_name("expression").and_then(|o| match o.kind() {
                            "this" => Some("this".to_string()),
                            "identifier" => Some(text(o, src).to_string()),
                            _ => None,
                        });
                        if name == "Start" && qual.as_deref() == Some("Process") {
                            self.pattern(node, "Process.Start: command injection risk if arguments come from input");
                        }
                        self.method_call(node, name, qual, &ctx)
                    }
                    _ => {}
                }
                ctx
            }
            "object_creation_expression" => {
                if let Some(t) = node.child_by_field_name("type") {
                    self.call(node, type_name(t, src), None, &ctx);
                }
                ctx
            }
            _ => ctx,
        }
    }
}


#[cfg(test)]
mod tests {
    use crate::lang::Lang;
    use crate::parse::{extract, DirectiveKind, Extract, SymKind};

    fn names(ex: &Extract) -> Vec<(&str, SymKind, bool)> {
        ex.symbols.iter().map(|s| (s.name.as_str(), s.kind, s.exported)).collect()
    }

    fn call<'e>(ex: &'e Extract, name: &str) -> &'e crate::parse::CallSite {
        ex.calls.iter().find(|c| c.callee == name).unwrap_or_else(|| panic!("no call {name}"))
    }


    #[test]
    fn java_symbols_calls_imports() {
        let src = br#"package com.x.svc;

import java.util.List;
import static com.x.Util.go;
import com.x.other.*;

/** A service. */
public class UserService extends Base {
    private final int repo;
    public UserService(int r) { this.repo = r; }
    public int getProfile(int id) {
        int u = helper(id);
        this.helper(u);
        Util.go();
        new Foo<String>(1);
        Runtime.getRuntime().exec("ls");
        return u;
    }
    private static int helper(int x) { return x; }
    public interface Inner { void run(); }
    enum Color { RED }
}
class Hidden { public void x() {} }
"#;
        let ex = extract(Lang::Java, src);
        let n = names(&ex);
        assert!(n.contains(&("UserService", SymKind::Class, true)), "{n:?}");
        assert!(n.contains(&("getProfile", SymKind::Method, true)));
        assert!(n.contains(&("helper", SymKind::Method, false)));
        assert!(n.contains(&("Inner", SymKind::Interface, true)));
        assert!(n.contains(&("run", SymKind::Method, true)));
        assert!(n.contains(&("Color", SymKind::Enum, false)));
        assert!(n.contains(&("Hidden", SymKind::Class, false)));
        assert!(n.contains(&("x", SymKind::Method, false)));
        let gp = ex.symbols.iter().find(|s| s.name == "getProfile").unwrap();
        assert_eq!(gp.sig, "public int getProfile(int id)");
        assert_eq!(ex.symbols.iter().find(|s| s.name == "run").unwrap().sig, "void run()");
        let raws: Vec<&str> = ex.imports.iter().map(|i| i.raw.as_str()).collect();
        assert_eq!(raws, ["java.util.List", "com.x.Util.go", "com.x.other.*"]);
        let h = call(&ex, "helper");
        assert!(h.recv && h.qual.as_deref() == Some("this"));
        assert_eq!(ex.symbols[h.caller.unwrap() as usize].name, "getProfile");
        assert_eq!(call(&ex, "go").qual.as_deref(), Some("Util"));
        assert!(!call(&ex, "Foo").recv);
        assert!(ex.directives.iter().any(|d| d.kind == DirectiveKind::Pattern && d.text.starts_with("Runtime.exec")));
    }

    #[test]
    fn csharp_symbols_calls_imports() {
        let src = br#"using System;
using Foo.Bar;

namespace Svc.Core
{
    /// <summary>A service.</summary>
    public class UserService : IService
    {
        private int repo;
        public UserService(int r) { repo = r; }
        public int GetProfile(int id)
        {
            var u = Helper(id);
            this.Helper(u);
            Util.Go<int>();
            new Foo(1);
            return u;
        }
        private static int Helper(int x) => x;
    }
    internal interface IService { int GetProfile(int id); }
    public struct Pt { public int X; }
    public delegate void Handler(int x);
}
"#;
        let ex = extract(Lang::CSharp, src);
        let n = names(&ex);
        assert!(n.contains(&("UserService", SymKind::Class, true)), "{n:?}");
        assert!(n.contains(&("GetProfile", SymKind::Method, true)));
        assert!(n.contains(&("Helper", SymKind::Method, false)));
        assert!(n.contains(&("IService", SymKind::Interface, false)));
        assert!(n.contains(&("Pt", SymKind::Struct, true)));
        assert!(n.contains(&("Handler", SymKind::TypeAlias, true)));
        assert_eq!(ex.symbols.iter().find(|s| s.name == "Helper").unwrap().sig, "private static int Helper(int x)");
        let raws: Vec<&str> = ex.imports.iter().map(|i| i.raw.as_str()).collect();
        assert_eq!(raws, ["System", "Foo.Bar"]);
        let h = call(&ex, "Helper");
        assert!(h.recv && h.qual.as_deref() == Some("this"));
        assert_eq!(ex.symbols[h.caller.unwrap() as usize].name, "GetProfile");
        assert_eq!(call(&ex, "Go").qual.as_deref(), Some("Util"));
        assert!(!call(&ex, "Foo").recv);
    }
}
