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

const UTIL: &str = "import { clamp } from './math';\n\nconst LIMIT = 10;\n\n/** Format a price. */\nexport function formatPrice(n: number): string {\n  return clamp(n, LIMIT).toFixed(2);\n}\n\nexport function total(xs: number[]): string {\n  return formatPrice(xs.reduce((a, b) => a + b, 0));\n}\n";

fn ts_fixture() -> (tempfile::TempDir, Engine) {
    engine(&[
        ("src/util.ts", UTIL),
        ("src/math.ts", "export function clamp(n: number, max: number) { return Math.min(n, max); }\n"),
        ("src/a.ts", "import { formatPrice as fp } from './util';\nfp(1);\n"),
        ("src/b.ts", "import { total, formatPrice } from './util';\ntotal([1]); formatPrice(2);\n"),
        ("src/index.ts", "export { formatPrice, total } from './util';\n"),
    ])
}

#[test]
fn moves_a_ts_function_and_fixes_every_import() {
    let (d, mut e) = ts_fixture();
    let out = e.move_symbol("formatPrice", "src/format/price.ts", false);
    assert!(out.contains("applied=\"true\" files=\"5\""), "{out}");
    assert!(out.contains("<exported") && out.contains("LIMIT"), "{out}");
    assert!(out.contains("import cycle"), "{out}");
    assert_eq!(
        read(&d, "src/format/price.ts"),
        "import { clamp } from '../math';\nimport { LIMIT } from '../util';\n\n/** Format a price. */\nexport function formatPrice(n: number): string {\n  return clamp(n, LIMIT).toFixed(2);\n}\n"
    );
    assert_eq!(
        read(&d, "src/util.ts"),
        "import { formatPrice } from './format/price';\n\nexport const LIMIT = 10;\n\nexport function total(xs: number[]): string {\n  return formatPrice(xs.reduce((a, b) => a + b, 0));\n}\n"
    );
    assert_eq!(read(&d, "src/a.ts"), "import { formatPrice as fp } from './format/price';\nfp(1);\n");
    assert_eq!(read(&d, "src/b.ts"), "import { total } from './util';\nimport { formatPrice } from './format/price';\ntotal([1]); formatPrice(2);\n");
    assert_eq!(read(&d, "src/index.ts"), "export { total } from './util';\nexport { formatPrice } from './format/price';\n");
    /* re-indexed: the definition now lives in the new file */
    let defs = e.find_symbols("formatPrice", None);
    assert_eq!(defs.len(), 1);
    assert_eq!(e.files[defs[0].0 as usize].path, "src/format/price.ts");
}

#[test]
fn dry_run_writes_nothing() {
    let (d, mut e) = ts_fixture();
    let out = e.move_symbol("formatPrice", "src/format/price.ts", true);
    assert!(out.contains("applied=\"false\" files=\"5\""), "{out}");
    assert!(!d.path().join("src/format").exists());
    assert_eq!(read(&d, "src/util.ts"), UTIL);
}

#[test]
fn destination_clash_gets_a_card() {
    let (d, mut e) = engine(&[
        ("a.ts", "export function go() { return 1; }\n"),
        ("b.ts", "export function go() { return 2; }\n"),
    ]);
    let out = e.move_symbol("a.ts:go", "b.ts", false);
    assert!(out.contains("applied=\"false\"") && out.contains("<choose>") && out.contains("b.ts:1"), "{out}");
    assert_eq!(read(&d, "a.ts"), "export function go() { return 1; }\n");
}

#[test]
fn refuses_methods_and_other_languages_and_escapes() {
    let (_d, mut e) = engine(&[("a.ts", "export class A { run() {} }\nexport function go() {}\n")]);
    assert!(e.move_symbol("run", "b.ts", false).contains("error=\"not_top_level\""));
    assert!(e.move_symbol("go", "b.py", false).contains("error=\"language_mismatch\""));
    assert!(e.move_symbol("go", "../b.ts", false).contains("REFUSED_OUTSIDE_ROOT"));
    assert!(e.move_symbol("go", ".git/b.ts", false).contains("REFUSED_HIDDEN"));
}

#[test]
fn python_from_imports_follow_the_move() {
    let (d, mut e) = engine(&[
        ("pkg/__init__.py", ""),
        ("pkg/old.py", "import os\n\n\ndef helper(p):\n    return os.path.join(p, 'x')\n\n\ndef other():\n    return 1\n"),
        ("app.py", "from pkg.old import helper, other\nimport pkg.old\n\nhelper('a')\npkg.old.helper('b')\nother()\n"),
    ]);
    let out = e.move_symbol("helper", "pkg/new.py", false);
    assert!(out.contains("applied=\"true\""), "{out}");
    assert!(out.contains("<check") && out.contains("pkg.old.helper"), "{out}");
    assert_eq!(read(&d, "pkg/new.py"), "import os\n\n\ndef helper(p):\n    return os.path.join(p, 'x')\n");
    assert_eq!(read(&d, "pkg/old.py"), "def other():\n    return 1\n");
    assert_eq!(read(&d, "app.py"), "from pkg.old import other\nfrom pkg.new import helper\nimport pkg.old\n\nhelper('a')\npkg.old.helper('b')\nother()\n");
}
