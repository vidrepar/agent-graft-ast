//! Symbol, call and import extraction for Go (the first five languages live in `parse.rs`).

use crate::parse::{KindLists, collapse_ws, signature_of, text, unquote, Ctx, Extractor, Import, SymKind};
use tree_sitter::Node;

const GO_KINDS: &[&str] = &[
    "function_declaration", "method_declaration", "type_spec", "type_alias", "method_elem", "const_spec",
    "import_spec", "call_expression",
];
const GO_OPAQUE: &[&str] = &[
    "interpreted_string_literal", "raw_string_literal", "rune_literal", "package_clause",
    "parameter_list", "field_declaration_list", "type_arguments", "type_parameter_list",
];

/// (kinds `visit` handles, kinds whose subtrees are skipped).
pub(crate) fn kind_lists() -> KindLists {
    (GO_KINDS, GO_OPAQUE)
}

/// Go exports by capitalization.
fn go_exported(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_uppercase())
}

/// The package name an import path binds: its last segment, skipping a `/v2`-style major version.
pub(crate) fn go_package_name(path: &str) -> &str {
    let mut segs = path.rsplit('/');
    let last = segs.next().unwrap_or(path);
    let is_version = last.len() > 1 && last.starts_with('v') && last[1..].bytes().all(|b| b.is_ascii_digit());
    if is_version { segs.next().unwrap_or(last) } else { last }
}


impl<'a> Extractor<'a> {
    pub(crate) fn visit_go(&mut self, node: Node<'a>, ctx: Ctx) -> Ctx {
        let src = self.src;
        let name_of = |n: Node| n.child_by_field_name("name").map(|x| text(x, src)).unwrap_or("");
        let mut c = ctx;
        match node.kind() {
            "function_declaration" | "method_declaration" => {
                let name = name_of(node);
                let kind = if node.kind() == "method_declaration" { SymKind::Method } else { SymKind::Function };
                let i = self.push_sym(node, name, kind, go_exported(name), &ctx, signature_of(node, src));
                c = Ctx { parent: Some(i), func: Some(i), ..Ctx::default() };
            }
            "type_spec" | "type_alias" => {
                let name = name_of(node);
                let ty = node.child_by_field_name("type").map(|t| t.kind()).unwrap_or("");
                let kind = match (node.kind(), ty) {
                    ("type_spec", "struct_type") => SymKind::Struct,
                    ("type_spec", "interface_type") => SymKind::Interface,
                    _ => SymKind::TypeAlias,
                };
                let exported = go_exported(name) && ctx.func.is_none();
                let head = text(node, src).lines().next().unwrap_or("");
                let head = collapse_ws(head.trim_end_matches(|ch: char| ch == '{' || ch.is_whitespace()), 240);
                let i = self.push_sym(node, name, kind, exported, &ctx, format!("type {head}"));
                if kind == SymKind::Interface {
                    c = Ctx { parent: Some(i), member_public: exported, in_class: true, ..ctx };
                }
            }
            "method_elem" if ctx.in_class => {
                let name = name_of(node);
                let exported = ctx.member_public && go_exported(name);
                self.push_sym(node, name, SymKind::Method, exported, &ctx, collapse_ws(text(node, src), 240));
            }
            "const_spec" if ctx.func.is_none() => {
                let mut cur = node.walk();
                for n in node.children_by_field_name("name", &mut cur) {
                    let name = text(n, src);
                    if go_exported(name) {
                        let line = collapse_ws(text(node, src).lines().next().unwrap_or(""), 160);
                        self.push_sym(node, name, SymKind::Const, true, &ctx, format!("const {line}"));
                    }
                }
            }
            "import_spec" => {
                if let Some(p) = node.child_by_field_name("path") {
                    let path = unquote(text(p, src));
                    let bound = match node.child_by_field_name("name") {
                        Some(n) => text(n, src).to_string(),
                        None => go_package_name(&path).to_string(),
                    };
                    self.pkgs.push(bound);
                    self.out.imports.push(Import { raw: path, line: Self::line(node) });
                }
            }
            "call_expression" => {
                let Some(f) = node.child_by_field_name("function") else { return c };
                match f.kind() {
                    "identifier" => self.call(node, text(f, src), None, &ctx),
                    "selector_expression" => {
                        let name = f.child_by_field_name("field").map(|x| text(x, src)).unwrap_or("");
                        let operand = f.child_by_field_name("operand").filter(|o| o.kind() == "identifier").map(|o| text(o, src));
                        match operand {
                            /* `pkg.Func()`: a package-level function, not a method */
                            Some(pkg) if self.pkgs.iter().any(|p| p == pkg) => {
                                if pkg == "unsafe" && name == "Pointer" {
                                    self.pattern(node, "unsafe.Pointer: bypasses type safety, verify layout and lifetime");
                                }
                                if pkg == "exec" && name == "Command" && Self::shell_arg(node, src) {
                                    self.pattern(node, "exec.Command via a shell: command injection risk");
                                }
                                self.call(node, name, None, &ctx)
                            }
                            q => self.method_call(node, name, q.map(str::to_string), &ctx),
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        c
    }

    /// `exec.Command("sh", ...)`: the first argument names a shell.
    fn shell_arg(call: Node, src: &[u8]) -> bool {
        call.child_by_field_name("arguments")
            .and_then(|a| a.named_child(0))
            .is_some_and(|a| matches!(unquote(text(a, src)).as_str(), "sh" | "bash" | "/bin/sh" | "/bin/bash" | "cmd"))
    }
}

#[cfg(test)]
mod tests {
    use crate::lang::Lang;
    use crate::parse::{extract, Extract, SymKind};

    fn names(ex: &Extract) -> Vec<(&str, SymKind, bool)> {
        ex.symbols.iter().map(|s| (s.name.as_str(), s.kind, s.exported)).collect()
    }

    fn call<'e>(ex: &'e Extract, name: &str) -> &'e crate::parse::CallSite {
        ex.calls.iter().find(|c| c.callee == name).unwrap_or_else(|| panic!("no call {name}"))
    }


    #[test]
    fn go_symbols_calls_imports() {
        let src = br#"package svc

import (
	"fmt"
	db "github.com/x/db"
	"github.com/spf13/cobra/v2"
)

// Server serves.
type Server struct {
	Name string
}

type Getter interface {
	Get(id int) error
	hidden()
}

const Max = 3

func (s *Server) Start(id int) error {
	x := helper(id)
	s.stop()
	db.Open("a")
	cobra.Run()
	fmt.Println(x)
	return nil
}

func helper(n int) int { return n }
"#;
        let ex = extract(Lang::Go, src);
        let n = names(&ex);
        assert!(n.contains(&("Server", SymKind::Struct, true)), "{n:?}");
        assert!(n.contains(&("Getter", SymKind::Interface, true)));
        assert!(n.contains(&("Get", SymKind::Method, true)));
        assert!(n.contains(&("hidden", SymKind::Method, false)));
        assert!(n.contains(&("Max", SymKind::Const, true)));
        assert!(n.contains(&("Start", SymKind::Method, true)));
        assert!(n.contains(&("helper", SymKind::Function, false)));
        let start = ex.symbols.iter().find(|s| s.name == "Start").unwrap();
        assert_eq!(start.sig, "func (s *Server) Start(id int) error");
        assert_eq!(ex.symbols.iter().find(|s| s.name == "Server").unwrap().sig, "type Server struct");
        let get = ex.symbols.iter().find(|s| s.name == "Get").unwrap();
        assert_eq!(ex.symbols[get.parent.unwrap() as usize].name, "Getter");
        let raws: Vec<&str> = ex.imports.iter().map(|i| i.raw.as_str()).collect();
        assert_eq!(raws, ["fmt", "github.com/x/db", "github.com/spf13/cobra/v2"]);
        assert!(!call(&ex, "helper").recv);
        assert!(call(&ex, "stop").recv && call(&ex, "stop").qual.as_deref() == Some("s"));
        /* package-qualified calls are free functions: `db` is an alias, `cobra` drops its /v2 */
        assert!(!call(&ex, "Open").recv && !call(&ex, "Run").recv && !call(&ex, "Println").recv);
        assert_eq!(ex.symbols[call(&ex, "helper").caller.unwrap() as usize].name, "Start");
    }

}
