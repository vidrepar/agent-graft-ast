//! Go, Java and C# through the engine: callers, read_file skeletons, rename across files, rewrite.

use crate::engine::Engine;
use std::fs;

fn engine(files: &[(&str, &str)]) -> (tempfile::TempDir, Engine) {
    let d = tempfile::tempdir().unwrap();
    for (p, s) in files {
        let path = d.path().join(p);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, s).unwrap();
    }
    let (e, _, _) = Engine::open(d.path(), Some(&d.path().join("i.redb"))).unwrap();
    (d, e)
}

fn read(d: &tempfile::TempDir, p: &str) -> String {
    fs::read_to_string(d.path().join(p)).unwrap()
}

/// Names of the symbols whose bodies call `name` (first definition found).
fn callers_of(e: &Engine, name: &str) -> Vec<String> {
    let (f, s) = e.graph.by_name[name][0];
    let mut v: Vec<String> = e
        .graph
        .callers(f, s)
        .iter()
        .filter_map(|r| r.sym.map(|cs| e.files[r.file as usize].ex.symbols[cs as usize].name.clone()))
        .collect();
    v.sort();
    v
}

const GO_SVC: &str = "package svc\n\n// Server serves.\ntype Server struct {\n\tName string\n}\n\n// Start runs it.\nfunc (s *Server) Start(id int) error {\n\tx := helper(id)\n\ts.stop()\n\treturn nil\n}\n\nfunc (s *Server) stop() {}\n";
const GO_UTIL: &str = "package svc\n\nfunc helper(n int) int {\n\t// helper is unexported but package-wide\n\treturn n\n}\n\nfunc other() { helper(1) }\n";
const GO_MAIN: &str = "package main\n\nimport \"example.com/m/svc\"\n\nfunc main() {\n\ts := &svc.Server{}\n\ts.Start(1)\n\thelper := 2\n\t_ = helper\n}\n";

#[test]
fn go_callers_skeleton_rename_rewrite() {
    let (d, mut e) = engine(&[("svc/server.go", GO_SVC), ("svc/util.go", GO_UTIL), ("main.go", GO_MAIN)]);
    /* an unexported function in a sibling file of the same package is linked */
    assert_eq!(callers_of(&e, "helper"), ["Start", "other"]);
    assert_eq!(callers_of(&e, "Start"), ["main"]);
    let l1 = e.read_file("svc/server.go", 1, 4000);
    assert!(l1.contains("// Start runs it.\nfunc (s *Server) Start(id int) error { /* 3 lines hidden */ }"), "{l1}");
    let l2 = e.read_file("svc/server.go", 2, 4000);
    assert!(l2.contains("type Server struct { Name string }") && l2.contains("func (s *Server) Start(id int) error;"), "{l2}");
    assert!(!l2.contains("stop"), "{l2}");

    let out = e.rename("helper", "clamp", false, false);
    assert!(out.contains("applied=\"true\" files=\"2\""), "{out}");
    assert_eq!(read(&d, "svc/util.go"), GO_UTIL.replace("helper", "clamp"));
    assert!(read(&d, "svc/server.go").contains("x := clamp(id)"));
    /* main's local `helper` is another variable */
    assert_eq!(read(&d, "main.go"), GO_MAIN);

    let out = e.rename("Start", "Run", false, false);
    assert!(out.contains("applied=\"true\" files=\"2\""), "{out}");
    assert!(read(&d, "main.go").contains("s.Run(1)") && read(&d, "svc/server.go").contains("// Run runs it.\nfunc (s *Server) Run("));

    let out = e.rewrite("$S.stop()", Some("$S.halt()"), Some("go"), &[], false);
    assert!(out.contains("applied=\"true\" files=\"1\" sites=\"1\""), "{out}");
    assert!(read(&d, "svc/server.go").contains("\ts.halt()\n"));
}

#[test]
fn go_change_signature_follows_package_calls() {
    let (d, mut e) = engine(&[
        ("lib/lib.go", "package lib\n\nfunc Load(path string) error { return nil }\n"),
        ("main.go", "package main\n\nimport l \"example.com/m/lib\"\n\nfunc main() {\n\tl.Load(\"a\")\n\tx.Load(\"b\")\n}\n"),
    ]);
    let out = e.change_signature("Load", "ctx context.Context", "ctx", false);
    assert!(out.contains("applied=\"true\""), "{out}");
    assert_eq!(read(&d, "lib/lib.go"), "package lib\n\nfunc Load(path string, ctx context.Context) error { return nil }\n");
    /* `l` is the aliased package; `x.Load` is a method on something else */
    assert!(read(&d, "main.go").contains("\tl.Load(\"a\", ctx)\n\tx.Load(\"b\")\n"));
}

const JAVA_SVC: &str = "package com.x;\n\n/** A service. */\npublic class UserService {\n    public UserService() {}\n    public UserService(int r) { this(); }\n    public int getProfile(int id) {\n        int u = helper(id);\n        return this.helper(u);\n    }\n    private static int helper(int x) {\n        int y = x;\n        return y;\n    }\n}\n";
const JAVA_USE: &str = "package com.x;\n\nimport com.x.UserService;\n\nclass Api {\n    int run() {\n        UserService s = new UserService(1);\n        return s.getProfile(2);\n    }\n}\n";

#[test]
fn java_callers_skeleton_rename_rewrite() {
    let (d, mut e) = engine(&[("src/com/x/UserService.java", JAVA_SVC), ("src/com/x/Api.java", JAVA_USE)]);
    assert_eq!(callers_of(&e, "helper"), ["getProfile", "getProfile"]);
    assert_eq!(callers_of(&e, "getProfile"), ["run"]);
    let l2 = e.read_file("src/com/x/UserService.java", 2, 4000);
    assert!(l2.contains("public class UserService {\n  public UserService();\n  public UserService(int r);\n  public int getProfile(int id);\n}"), "{l2}");
    let l1 = e.read_file("src/com/x/UserService.java", 1, 4000);
    assert!(l1.contains("/** A service. */") && l1.contains("private static int helper(int x) { /* 2 lines hidden */ }"), "{l1}");

    /* a class rename takes its constructors along */
    let out = e.rename("UserService", "Accounts", false, false);
    assert!(out.contains("applied=\"true\" files=\"2\""), "{out}");
    assert!(read(&d, "src/com/x/UserService.java").contains("public class Accounts {\n    public Accounts() {}\n    public Accounts(int r)"));
    assert!(read(&d, "src/com/x/Api.java").contains("Accounts s = new Accounts(1);"));

    let out = e.rename("getProfile", "profile", false, false);
    assert!(out.contains("applied=\"true\" files=\"2\""), "{out}");
    assert!(read(&d, "src/com/x/Api.java").contains("return s.profile(2);"));

    /* `helper($X)` has no receiver, so `this.helper(u)` is another shape */
    let out = e.rewrite("helper($X)", Some("clamp($X)"), Some("java"), &[], false);
    assert!(out.contains("sites=\"1\""), "{out}");
    assert!(read(&d, "src/com/x/UserService.java").contains("int u = clamp(id);\n        return this.helper(u);"));
}

#[test]
fn java_overloads_rename_together() {
    let (d, mut e) = engine(&[("A.java", "class A {\n    void put(int a) {}\n    void put(String s) { put(1); }\n}\n")]);
    let out = e.rename("put", "store", false, false);
    assert!(out.contains("applied=\"true\""), "{out}");
    assert_eq!(read(&d, "A.java"), "class A {\n    void store(int a) {}\n    void store(String s) { store(1); }\n}\n");
}

const CS_SVC: &str = "namespace Svc\n{\n    /// <summary>A service.</summary>\n    public class UserService\n    {\n        public int GetProfile(int id)\n        {\n            var u = Helper(id);\n            return this.Helper(u);\n        }\n        private static int Helper(int x) => x;\n    }\n}\n";
const CS_USE: &str = "using Svc;\n\nclass Api\n{\n    int Run() => new UserService().GetProfile(2);\n}\n";

#[test]
fn csharp_callers_skeleton_rename_rewrite() {
    let (d, mut e) = engine(&[("Svc/UserService.cs", CS_SVC), ("Api.cs", CS_USE)]);
    assert_eq!(callers_of(&e, "Helper"), ["GetProfile", "GetProfile"]);
    assert_eq!(callers_of(&e, "GetProfile"), ["Run"]);
    let l2 = e.read_file("Svc/UserService.cs", 2, 4000);
    assert!(l2.contains("public class UserService {\n  public int GetProfile(int id);\n}"), "{l2}");

    let out = e.rename("Helper", "Clamp", false, false);
    assert!(out.contains("applied=\"true\" files=\"1\" sites=\"3\""), "{out}");
    assert!(read(&d, "Svc/UserService.cs").contains("var u = Clamp(id);\n            return this.Clamp(u);"));

    let out = e.rename("GetProfile", "Profile", false, false);
    assert!(out.contains("applied=\"true\" files=\"2\""), "{out}");
    assert!(read(&d, "Api.cs").contains("new UserService().Profile(2)"));

    let out = e.rewrite("new UserService()", Some("UserService.Create()"), Some("csharp"), &[], false);
    assert!(out.contains("sites=\"1\""), "{out}");
    assert!(read(&d, "Api.cs").contains("int Run() => UserService.Create().Profile(2);"));
}

#[test]
fn refactors_without_new_language_rules_refuse() {
    let (_d, mut e) = engine(&[("A.java", "public class A { public int x; }\n"), ("B.cs", "public class B { public void Go() {} }\n")]);
    assert!(e.add_field("A", "y: int", "0", false).contains("error=\"unsupported_language\""));
    assert!(e.change_signature("Go", "int x", "1", false).contains("error=\"unsupported_language\""));
    assert!(e.move_symbol_checked("A", "C.java", false).contains("error=\"unsupported_language\""));
}
