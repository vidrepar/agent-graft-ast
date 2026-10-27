//! Tier-2 languages: symbols and calls per language, then through the engine (structural rewrite in each,
//! skeletons, callers, rename refused).

use crate::engine::Engine;
use crate::lang::Lang;
use crate::parse::{extract, SymKind};
use std::fs;

/// (name, kind, exported) of every symbol, and every callee with `.` for method calls.
fn summary(lang: Lang, src: &str) -> (Vec<(String, SymKind, bool)>, Vec<String>) {
    let ex = extract(lang, src.as_bytes());
    let syms = ex.symbols.iter().map(|s| (s.name.clone(), s.kind, s.exported)).collect();
    let calls = ex.calls.iter().map(|c| format!("{}{}", if c.recv { "." } else { "" }, c.callee)).collect();
    (syms, calls)
}

fn has(syms: &[(String, SymKind, bool)], name: &str, kind: SymKind, exported: bool) -> bool {
    syms.iter().any(|s| s.0 == name && s.1 == kind && s.2 == exported)
}

#[test]
fn c_and_cpp() {
    let (s, c) = summary(Lang::C, "static int helper(int x) { return x; }\nstruct Point { int x; };\nint *make(int n) { helper(n); p->run(1); return 0; }\n");
    assert!(has(&s, "helper", SymKind::Function, false) && has(&s, "make", SymKind::Function, true) && has(&s, "Point", SymKind::Struct, true), "{s:?}");
    assert_eq!(c, ["helper", ".run"]);
    let src = "class Svc : public Base {\n public:\n  int get(int id) { return helper(id); }\n private:\n  void hidden();\n};\nint Svc::run() { this->get(1); Svc::stat(); obj.go<int>(2); return 0; }\n";
    let (s, c) = summary(Lang::Cpp, src);
    assert!(has(&s, "Svc", SymKind::Class, true) && has(&s, "get", SymKind::Method, true) && has(&s, "hidden", SymKind::Method, false), "{s:?}");
    assert!(has(&s, "run", SymKind::Method, true), "{s:?}");
    assert_eq!(c, ["helper", ".get", "stat", ".go"]);
}

#[test]
fn ruby_php_lua_bash() {
    let rb = "module Billing\n  class Invoice < Base\n    def total(x)\n      helper(x)\n      self.tax(1)\n    end\n    private\n    def hidden; end\n  end\nend\n";
    let (s, c) = summary(Lang::Ruby, rb);
    assert!(has(&s, "Billing", SymKind::Module, true) && has(&s, "Invoice", SymKind::Class, true), "{s:?}");
    assert!(has(&s, "total", SymKind::Method, true) && has(&s, "hidden", SymKind::Method, false), "{s:?}");
    assert_eq!(c, ["helper", ".tax"]);
    let php = "<?php\nfunction helper($x) { return $x; }\nclass Svc extends Base {\n    public function get($id) { helper($id); $this->run(1); Util::go(); new Foo(); }\n    private function hidden() {}\n}\n";
    let (s, c) = summary(Lang::Php, php);
    assert!(has(&s, "helper", SymKind::Function, true) && has(&s, "get", SymKind::Method, true) && has(&s, "hidden", SymKind::Method, false), "{s:?}");
    assert_eq!(c, ["helper", ".run", ".go", "Foo"]);
    let lua = "local function helper(x) return x end\nfunction M.get(id) helper(id) obj:run(1) return M.go() end\n";
    let (s, c) = summary(Lang::Lua, lua);
    assert!(has(&s, "helper", SymKind::Function, false) && has(&s, "get", SymKind::Method, true), "{s:?}");
    assert_eq!(c, ["helper", ".run", "go"]);
    let sh = "helper() { echo \"$1\"; }\nfunction main {\n  helper x\n  ls | grep foo\n}\nmain \"$@\"\n";
    let (s, c) = summary(Lang::Bash, sh);
    assert!(has(&s, "helper", SymKind::Function, true) && has(&s, "main", SymKind::Function, true), "{s:?}");
    assert_eq!(c, ["echo", "helper", "ls", "grep", "main"]);
}

#[test]
fn kotlin_swift_scala() {
    let kt = "class Svc(val r: Int) : Base() {\n    fun get(id: Int): Int { helper(id); this.run(1); return 0 }\n    private fun hidden() {}\n}\n\ninterface I {\n    fun x()\n}\n\nfun helper(x: Int) = x\n";
    let (s, c) = summary(Lang::Kotlin, kt);
    assert!(has(&s, "Svc", SymKind::Class, true) && has(&s, "get", SymKind::Method, true) && has(&s, "hidden", SymKind::Method, false), "{s:?}");
    assert!(has(&s, "I", SymKind::Interface, true) && has(&s, "helper", SymKind::Function, true), "{s:?}");
    assert_eq!(c, ["helper", ".run"]);
    let sw = "class Svc: Base {\n    func get(id: Int) -> Int { helper(id); self.run(1); return 0 }\n    private func hidden() {}\n}\nstruct Pt { func m() {} }\nprotocol P { func x() }\nfunc helper(_ x: Int) -> Int { return x }\n";
    let (s, c) = summary(Lang::Swift, sw);
    assert!(has(&s, "Svc", SymKind::Class, true) && has(&s, "get", SymKind::Method, true) && has(&s, "hidden", SymKind::Method, false), "{s:?}");
    assert!(has(&s, "Pt", SymKind::Struct, true) && has(&s, "P", SymKind::Interface, true) && has(&s, "helper", SymKind::Function, true), "{s:?}");
    assert_eq!(c, ["helper", ".run"]);
    let sc = "class Svc(r: Int) extends Base {\n  def get(id: Int): Int = { helper(id); this.run(1); new Foo(1); 0 }\n  private def hidden(): Unit = {}\n}\ntrait T { def x(): Int }\n";
    let (s, c) = summary(Lang::Scala, sc);
    assert!(has(&s, "Svc", SymKind::Class, true) && has(&s, "get", SymKind::Method, true) && has(&s, "hidden", SymKind::Method, false), "{s:?}");
    assert!(has(&s, "T", SymKind::Trait, true) && has(&s, "x", SymKind::Method, true), "{s:?}");
    assert_eq!(c, ["helper", ".run", "Foo"]);
}

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

/// (file, source, pattern, replacement, lang, expected file after the rewrite)
const CASES: &[(&str, &str, &str, &str, &str, &str)] = &[
    ("a.c", "int f(int x) {\n  log_msg(\"a\", x);\n  return x;\n}\n", "log_msg($A, $B)", "log_msg2($B, $A)", "c",
        "int f(int x) {\n  log_msg2(x, \"a\");\n  return x;\n}\n"),
    ("a.cpp", "void g() { std::cout << v.size(); }\n", "$V.size()", "std::size($V)", "cpp", "void g() { std::cout << std::size(v); }\n"),
    ("a.rb", "def f(x)\n  puts x.inspect\nend\n", "puts $X", "logger.info $X", "ruby", "def f(x)\n  logger.info x.inspect\nend\n"),
    ("a.php", "<?php\nfunction f($x) { return array_key_exists('a', $x); }\n", "array_key_exists($K, $A)", "isset($A[$K])", "php",
        "<?php\nfunction f($x) { return isset($x['a']); }\n"),
    ("a.kt", "fun f(xs: List<Int>) {\n    println(xs.size)\n}\n", "println($X)", "log.info($X)", "kotlin", "fun f(xs: List<Int>) {\n    log.info(xs.size)\n}\n"),
    ("a.swift", "func f() {\n    print(\"hi\", 1)\n}\n", "print($$$A)", "log($$$A)", "swift", "func f() {\n    log(\"hi\", 1)\n}\n"),
    ("a.sh", "#!/bin/sh\necho start\nrm -rf /tmp/x\n", "rm -rf $P", "rm -r $P", "bash", "#!/bin/sh\necho start\nrm -r /tmp/x\n"),
    ("a.lua", "local t = {}\ntable.insert(t, 1)\n", "table.insert($T, $V)", "$T[#$T + 1] = $V", "lua", "local t = {}\nt[#t + 1] = 1\n"),
    ("a.scala", "object A {\n  def f(xs: List[Int]) = println(xs.length)\n}\n", "println($X)", "log.info($X)", "scala",
        "object A {\n  def f(xs: List[Int]) = log.info(xs.length)\n}\n"),
];

#[test]
fn structural_rewrite_in_every_tier2_language() {
    for (file, src, pat, rep, lang, want) in CASES {
        let (d, mut e) = engine(&[(file, src)]);
        let out = e.rewrite(pat, Some(rep), Some(lang), &[], false);
        assert!(out.contains("applied=\"true\" files=\"1\" sites=\"1\""), "{lang}: {out}");
        assert_eq!(fs::read_to_string(d.path().join(file)).unwrap(), *want, "{lang}");
    }
}

#[test]
fn tier2_skeletons_callers_and_rename_refusal() {
    let (_d, mut e) = engine(&[
        ("svc.rb", "class Svc\n  def run(x)\n    a = helper(x)\n    a + 1\n  end\n  private\n  def secret; end\nend\n\ndef helper(x)\n  x\nend\n"),
        ("lib.cpp", "class Box {\n public:\n  int size() const {\n    int n = count();\n    return n;\n  }\n private:\n  int count() const;\n};\n"),
    ]);
    let l1 = e.read_file("svc.rb", 1, 4000);
    assert!(l1.contains("def run(x)\n    # 2 lines hidden\n  end"), "{l1}");
    let l2 = e.read_file("svc.rb", 2, 4000);
    assert!(l2.contains("class Svc\n  def run(x);\nend") && !l2.contains("secret"), "{l2}");
    let l2 = e.read_file("lib.cpp", 2, 4000);
    assert!(l2.contains("class Box {\n  int size() const;\n}") && !l2.contains("count"), "{l2}");
    let callers = e.callers("helper", None);
    assert!(callers.contains("<caller path=\"svc.rb\" at=\"run:3\"/>"), "{callers}");
    assert!(e.rename("helper", "assist", false, false).contains("error=\"unsupported_language\""));
}
