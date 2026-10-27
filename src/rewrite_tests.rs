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

#[test]
fn ts_console_log_to_logger_debug() {
    let (d, mut e) = engine(&[
        ("src/a.ts", "console.log('hi');\nfunction f(x: number) {\n  console.log(x, 'y');\n  console.error(x);\n}\n"),
        ("src/b.js", "const s = \"console.log(x)\"; // console.log(x)\nconsole.log(1)\n"),
    ]);
    let out = e.rewrite("console.log($$$ARGS)", Some("logger.debug($$$ARGS)"), None, &[], false);
    assert!(out.contains("applied=\"true\" files=\"2\" sites=\"3\""), "{out}");
    assert!(out.contains("<verified>Re-parsed the 2 written files: 0 matches"), "{out}");
    assert_eq!(read(&d, "src/a.ts"), "logger.debug('hi');\nfunction f(x: number) {\n  logger.debug(x, 'y');\n  console.error(x);\n}\n");
    /* the string literal and the comment hold the pattern text but are not code */
    assert_eq!(read(&d, "src/b.js"), "const s = \"console.log(x)\"; // console.log(x)\nlogger.debug(1)\n");
}

#[test]
fn single_metavar_matches_exactly_one_argument() {
    let (d, mut e) = engine(&[("a.ts", "console.log(a);\nconsole.log(a, b);\nconsole.log();\n")]);
    let out = e.rewrite("console.log($MSG)", Some("logger.debug($MSG)"), Some("ts"), &[], false);
    assert!(out.contains("sites=\"1\""), "{out}");
    assert_eq!(read(&d, "a.ts"), "logger.debug(a);\nconsole.log(a, b);\nconsole.log();\n");
}

#[test]
fn swaps_arguments() {
    let (d, mut e) = engine(&[("a.ts", "foo(1, g(2));\nfoo(x);\nbar(1, 2);\n")]);
    let out = e.rewrite("foo($A, $B)", Some("foo($B, $A)"), None, &[], false);
    assert!(out.contains("sites=\"1\""), "{out}");
    /* the swapped call still has the pattern's shape: reported, not hidden */
    assert!(out.contains("<remaining matches=\"1\""), "{out}");
    assert_eq!(read(&d, "a.ts"), "foo(g(2), 1);\nfoo(x);\nbar(1, 2);\n");
}

#[test]
fn rest_args_pass_through_including_empty() {
    let (d, mut e) = engine(&[("a.py", "emit(ev)\nemit(ev, 1, k=2)\nemit()\n")]);
    let out = e.rewrite("emit($EV, $$$REST)", Some("bus.emit($EV, $$$REST)"), None, &[], false);
    assert!(out.contains("sites=\"2\""), "{out}");
    assert_eq!(read(&d, "a.py"), "bus.emit(ev)\nbus.emit(ev, 1, k=2)\nemit()\n");
    let (d, mut e) = engine(&[("a.ts", "f();\nf(1, 2);\n")]);
    e.rewrite("f($$$A)", Some("g($$$A, ctx)"), None, &[], false);
    assert_eq!(read(&d, "a.ts"), "g(ctx);\ng(1, 2, ctx);\n");
}

#[test]
fn repeated_metavar_must_be_equal() {
    let (d, mut e) = engine(&[("a.ts", "const a = x === x;\nconst b = x === y;\n")]);
    let out = e.rewrite("$A === $A", Some("true"), None, &[], false);
    assert!(out.contains("sites=\"1\""), "{out}");
    assert_eq!(read(&d, "a.ts"), "const a = true;\nconst b = x === y;\n");
}

#[test]
fn rust_unwrap_to_expect_including_macro_bodies() {
    let (d, mut e) = engine(&[(
        "src/lib.rs",
        "fn f(v: Option<u8>) -> u8 {\n    let a = v.unwrap();\n    println!(\"{}\", self.x.get().unwrap());\n    let s = \"v.unwrap()\";\n    a\n}\n",
    )]);
    let out = e.rewrite("$X.unwrap()", Some("$X.expect(\"TODO\")"), None, &[], false);
    assert!(out.contains("lang=\"rust\" applied=\"true\" files=\"1\" sites=\"2\""), "{out}");
    assert_eq!(
        read(&d, "src/lib.rs"),
        "fn f(v: Option<u8>) -> u8 {\n    let a = v.expect(\"TODO\");\n    println!(\"{}\", self.x.get().expect(\"TODO\"));\n    let s = \"v.unwrap()\";\n    a\n}\n"
    );
}

#[test]
fn python_call_rewrite() {
    let (d, mut e) = engine(&[("m.py", "import os\np = os.path.join(a, b)\nq = os.path.join(a)\n")]);
    let out = e.rewrite("os.path.join($A, $B)", Some("Path($A) / $B"), Some("python"), &[], false);
    assert!(out.contains("sites=\"1\""), "{out}");
    assert_eq!(read(&d, "m.py"), "import os\np = Path(a) / b\nq = os.path.join(a)\n");
}

#[test]
fn nested_matches_outermost_wins() {
    let (d, mut e) = engine(&[("src/lib.rs", "fn f() { let x = a.unwrap().b().unwrap(); }\n")]);
    let out = e.rewrite("$X.unwrap()", Some("$X.expect(\"e\")"), None, &[], false);
    assert!(out.contains("sites=\"1\""), "{out}");
    assert!(out.contains("<remaining matches=\"1\""), "{out}");
    assert_eq!(read(&d, "src/lib.rs"), "fn f() { let x = a.unwrap().b().expect(\"e\"); }\n");
}

#[test]
fn dry_run_writes_nothing() {
    let src = "console.log(1);\n";
    let (d, mut e) = engine(&[("a.ts", src)]);
    let out = e.rewrite("console.log($A)", Some("logger.debug($A)"), None, &[], true);
    assert!(out.contains("applied=\"false\" files=\"1\" sites=\"1\""), "{out}");
    assert!(out.contains("a.ts:1: console.log(1) -&gt; logger.debug(1)"), "{out}");
    assert_eq!(read(&d, "a.ts"), src);
}

#[test]
fn search_mode_lists_matches_and_caps() {
    let many: String = (0..60).map(|i| format!("fn f{i}() {{ x.unwrap(); }}\n")).collect();
    let (d, mut e) = engine(&[("src/a.rs", &many), ("src/b.rs", "fn g() { let y = z\n    .unwrap(); }\n")]);
    let out = e.rewrite("$X.unwrap()", None, None, &[], false);
    assert!(out.contains("mode=\"search\" matches=\"61\" files=\"2\""), "{out}");
    assert!(out.contains("src/a.rs:1: x.unwrap()\n"), "{out}");
    assert!(out.contains("... 11 more"), "{out}");
    let only_b = e.rewrite("$X.unwrap()", None, None, &["src/b.rs".to_string()], false);
    assert!(only_b.contains("matches=\"1\"") && only_b.contains("src/b.rs:1: z\n"), "{only_b}");
    assert_eq!(read(&d, "src/b.rs"), "fn g() { let y = z\n    .unwrap(); }\n");
}

#[test]
fn ambiguous_language_and_bad_input_are_errors() {
    let (_d, mut e) = engine(&[("a.py", "foo(1)\n"), ("b.ts", "foo(2);\n")]);
    let out = e.rewrite("foo($A)", Some("bar($A)"), None, &[], false);
    assert!(out.contains("error=\"ambiguous_lang\"") && out.contains("python: 1"), "{out}");
    assert!(e.rewrite("foo($A)", Some("bar($B)"), None, &[], false).contains("error=\"invalid_replacement\""));
    assert!(e.rewrite("$A", None, Some("ts"), &[], false).contains("error=\"invalid_pattern\""));
    assert!(e.rewrite("foo($A)", None, None, &["../x".to_string()], false).contains("refused=\"true\""));
}

#[test]
fn search_then_rewrite_rescans_files_changed_in_between() {
    let (d, mut e) = engine(&[("a.ts", "console.log(1);\n"), ("b.ts", "let x = 1;\n")]);
    assert!(e.rewrite("console.log($A)", None, None, &[], false).contains("matches=\"1\""));
    fs::write(d.path().join("b.ts"), "let x = 1;\nconsole.log(x);\n").unwrap();
    let out = e.rewrite("console.log($A)", Some("logger.debug($A)"), None, &[], false);
    assert!(out.contains("files=\"2\" sites=\"2\""), "{out}");
    assert_eq!(read(&d, "b.ts"), "let x = 1;\nlogger.debug(x);\n");
}

#[test]
fn dry_run_then_apply_writes_the_planned_edits() {
    let (d, mut e) = engine(&[("a.rs", "fn f() { a.unwrap(); b.unwrap(); }\n")]);
    assert!(e.rewrite("$X.unwrap()", Some("$X?"), None, &[], true).contains("sites=\"2\""));
    let out = e.rewrite("$X.unwrap()", Some("$X?"), None, &[], false);
    assert!(out.contains("applied=\"true\"") && out.contains("<verified>"), "{out}");
    assert_eq!(read(&d, "a.rs"), "fn f() { a?; b?; }\n");
}

#[test]
fn partial_parse_finds_matches_inside_and_outside_functions() {
    let rs = "const A: u8 = x.unwrap();\nfn f() { y.unwrap(); }\nimpl S {\n    fn g(&self) { z.unwrap(); }\n}\n";
    let py = "import os\nclass C:\n    def m(self):\n        return os.path.join(a, b)\n\ndef top():\n    return os.path.join(c, d)\n";
    let ts = "class K {\n  run() {\n    console.log(1);\n  }\n}\nfunction g() { console.log(2); }\nconsole.log(3);\n";
    let filler = "fn pad() {}\n".repeat(40);
    let (_d, mut e) = engine(&[("a.rs", rs), ("b.rs", &format!("{filler}fn only() {{ q.unwrap(); }}\n")), ("c.py", py), ("d.ts", ts)]);
    assert!(e.rewrite("$X.unwrap()", None, Some("rust"), &[], false).contains("matches=\"4\""));
    assert!(e.rewrite("os.path.join($A, $B)", None, Some("python"), &[], false).contains("matches=\"2\""));
    assert!(e.rewrite("console.log($A)", None, Some("ts"), &[], false).contains("matches=\"3\""));
    let out = e.rewrite("$X.unwrap()", Some("$X?"), Some("rust"), &["b.rs".to_string()], false);
    assert!(out.contains("sites=\"1\"") && out.contains("<verified>"), "{out}");
}

#[test]
fn dry_run_checked_after_reply_then_applied() {
    let (d, mut e) = engine(&[("a.ts", "f(g(1));\nf(2);\n")]);
    assert!(e.rewrite("f($A)", Some("h($A)"), None, &[], true).contains("sites=\"2\""));
    e.after_reply();
    let out = e.rewrite("f($A)", Some("h($A)"), None, &[], false);
    assert!(out.contains("applied=\"true\"") && out.contains("<verified>"), "{out}");
    assert_eq!(read(&d, "a.ts"), "h(g(1));\nh(2);\n");
    /* a different replacement is planned afresh, not taken from the dry run */
    assert!(e.rewrite("h($A)", Some("k($A)"), None, &[], true).contains("sites=\"2\""));
    e.after_reply();
    e.rewrite("h($A)", Some("m($A)"), None, &[], false);
    assert_eq!(read(&d, "a.ts"), "m(g(1));\nm(2);\n");
}

#[test]
fn punctuation_and_keywords_the_pattern_omits_are_skipped() {
    let ts = "function f(e) {\n  throw e;\n}\nasync function g() { throw new Error('x') }\nconst a = x === y;\nconst b = x !== y;\n";
    let rs = "fn f() -> u8 { return 1; }\nfn g(v: &mut Vec<u8>) { h(v, 2,); }\n";
    let (_d, mut e) = engine(&[("a.ts", ts), ("b.rs", rs)]);
    assert!(e.rewrite("throw $X", None, Some("ts"), &[], false).contains("matches=\"2\""));
    assert!(e.rewrite("$A === $B", None, Some("ts"), &[], false).contains("matches=\"1\""));
    assert!(e.rewrite("return $X", None, Some("rust"), &[], false).contains("matches=\"1\""));
    let out = e.rewrite("h($$$A)", Some("k($$$A)"), Some("rust"), &[], true);
    assert!(out.contains("h(v, 2,) -&gt; k(v, 2)"), "{out}");
}

#[test]
fn sites_that_would_lose_code_are_skipped_and_listed() {
    let ts = "const a = x /* why */ && y;\nconst b = p && q;\nfunction f() { throw e; }\nasync function g() {}\nfunction h() {}\n";
    let (d, mut e) = engine(&[("a.ts", ts)]);
    let out = e.rewrite("$A && $B", Some("$B && $A"), None, &[], false);
    assert!(out.contains("sites=\"1\"") && out.contains("<skipped") && out.contains("a.ts lines 1"), "{out}");
    let out = e.rewrite("throw $X", Some("throw wrap($X)"), None, &[], false);
    assert!(out.contains("sites=\"1\""), "{out}");
    let out = e.rewrite("function $F() {}", Some("const $F = () => {}"), None, &[], false);
    assert!(out.contains("sites=\"1\"") && out.contains("a.ts lines 4"), "{out}");
    assert_eq!(
        read(&d, "a.ts"),
        "const a = x /* why */ && y;\nconst b = q && p;\nfunction f() { throw wrap(e); }\nasync function g() {}\nconst h = () => {}\n"
    );
}

#[test]
fn metavariables_where_the_grammar_wants_a_string() {
    let ts = "import React from 'react';\nimport { a } from \"./a\";\nimport x from './x';\n";
    let py = "from os import path\nimport json\n";
    let (d, mut e) = engine(&[("a.ts", ts), ("b.py", py)]);
    assert!(e.rewrite("import $X from $Y", None, Some("ts"), &[], false).contains("matches=\"3\""));
    let out = e.rewrite("import $X from './x'", Some("import $X from './y'"), Some("ts"), &[], false);
    assert!(out.contains("sites=\"1\""), "{out}");
    assert!(read(&d, "a.ts").ends_with("import x from './y';\n"));
    assert!(e.rewrite("from $M import $N", None, Some("python"), &[], false).contains("matches=\"1\""));
}
