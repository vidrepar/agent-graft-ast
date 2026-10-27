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
fn rust_struct_and_its_literals() {
    let (d, mut e) = engine(&[
        ("src/a.rs", "pub struct Cfg {\n    pub name: String,\n    pub port: u16,\n}\n\nimpl Cfg {\n    pub fn new() -> Self {\n        Self { name: String::new(), port: 0 }\n    }\n}\n\nimpl Default for Cfg {\n    fn default() -> Self { Cfg::new() }\n}\n"),
        (
            "src/main.rs",
            "mod a;\nuse a::Cfg;\nfn main() {\n    let x = Cfg {\n        name: \"x\".into(),\n        port: 1, // web\n    };\n    let port = 2;\n    let name = String::new();\n    let y = Cfg { name, port };\n    let z = Cfg {\n        port: 3,\n        ..Default::default()\n    };\n    let w = a::Cfg { name: String::new(), port: 4 };\n}\n",
        ),
    ]);
    let out = e.add_field("Cfg", "pub retries: u32", "3", false);
    assert!(out.contains("applied=\"true\" files=\"2\" literals=\"4\""), "{out}");
    assert!(out.contains("<unchanged") && out.contains("src/main.rs lines 11"), "{out}");
    assert_eq!(
        read(&d, "src/a.rs"),
        "pub struct Cfg {\n    pub name: String,\n    pub port: u16,\n    pub retries: u32,\n}\n\nimpl Cfg {\n    pub fn new() -> Self {\n        Self { name: String::new(), port: 0, retries: 3 }\n    }\n}\n\nimpl Default for Cfg {\n    fn default() -> Self { Cfg::new() }\n}\n"
    );
    assert_eq!(
        read(&d, "src/main.rs"),
        "mod a;\nuse a::Cfg;\nfn main() {\n    let x = Cfg {\n        name: \"x\".into(),\n        port: 1, // web\n        retries: 3,\n    };\n    let port = 2;\n    let name = String::new();\n    let y = Cfg { name, port, retries: 3 };\n    let z = Cfg {\n        port: 3,\n        ..Default::default()\n    };\n    let w = a::Cfg { name: String::new(), port: 4, retries: 3 };\n}\n"
    );
}

#[test]
fn rust_generic_paths_and_refusals() {
    let (d, mut e) = engine(&[
        ("lib.rs", "pub struct Wrap<T> { inner: T }\npub struct Pair(u8, u8);\nfn f() { let w = crate::m::Wrap::<u8> { inner: 1 }; let v = Wrap { inner: 2 }; }\n"),
    ]);
    let out = e.add_field("Wrap", "tag: &'static str", "\"t\"", false);
    assert!(out.contains("literals=\"2\""), "{out}");
    assert_eq!(
        read(&d, "lib.rs"),
        "pub struct Wrap<T> { inner: T, tag: &'static str }\npub struct Pair(u8, u8);\nfn f() { let w = crate::m::Wrap::<u8> { inner: 1, tag: \"t\" }; let v = Wrap { inner: 2, tag: \"t\" }; }\n"
    );
    assert!(e.add_field("Pair", "c: u8", "0", false).contains("error=\"not_a_named_struct\""));
    assert!(e.add_field("Wrap", "tag: u8", "0", false).contains("error=\"field_exists\""));
}

#[test]
fn typescript_interface_and_typed_literals() {
    let (d, mut e) = engine(&[
        ("cfg.ts", "export interface Cfg {\n  host: string;\n  port: number;\n}\n"),
        (
            "use.ts",
            "import { Cfg } from './cfg';\nconst c: Cfg = { host: 'a', port: 1 };\nconst d = { host: 'b', port: 2 } as Cfg;\nfunction make(): Cfg {\n  return {\n    host: 'c',\n    port: 3,\n  };\n}\nconst list: Cfg[] = [{ host: 'd', port: 4 }];\nconst s = { ...c } satisfies Cfg;\nexport function start(cfg: Cfg) { return cfg; }\nconst loose = { host: 'e', port: 5 };\n",
        ),
    ]);
    let out = e.add_field("Cfg", "retries: number", "3", false);
    assert!(out.contains("applied=\"true\" files=\"2\" literals=\"4\""), "{out}");
    assert!(out.contains("<check") && out.contains("use.ts lines 12"), "{out}");
    assert!(out.contains("<unchanged") && out.contains("use.ts lines 11"), "{out}");
    assert_eq!(read(&d, "cfg.ts"), "export interface Cfg {\n  host: string;\n  port: number;\n  retries: number;\n}\n");
    assert_eq!(
        read(&d, "use.ts"),
        "import { Cfg } from './cfg';\nconst c: Cfg = { host: 'a', port: 1, retries: 3 };\nconst d = { host: 'b', port: 2, retries: 3 } as Cfg;\nfunction make(): Cfg {\n  return {\n    host: 'c',\n    port: 3,\n    retries: 3,\n  };\n}\nconst list: Cfg[] = [{ host: 'd', port: 4, retries: 3 }];\nconst s = { ...c } satisfies Cfg;\nexport function start(cfg: Cfg) { return cfg; }\nconst loose = { host: 'e', port: 5 };\n"
    );
}

#[test]
fn typescript_optional_field_only_touches_the_type() {
    let (d, mut e) = engine(&[("t.ts", "type Opts = { a: string }\nconst o: Opts = { a: 'x' };\n")]);
    let out = e.add_field("Opts", "verbose?: boolean", "", false);
    assert!(out.contains("applied=\"true\" files=\"1\" literals=\"0\""), "{out}");
    assert_eq!(read(&d, "t.ts"), "type Opts = { a: string; verbose?: boolean }\nconst o: Opts = { a: 'x' };\n");
}

#[test]
fn python_dataclass_and_keyword_calls() {
    let (d, mut e) = engine(&[
        ("cfg.py", "from dataclasses import dataclass\n\n@dataclass\nclass Cfg:\n    host: str\n    port: int = 80\n\n    def url(self):\n        return self.host\n"),
        ("use.py", "from cfg import Cfg\nimport cfg\n\na = Cfg('h')\nb = cfg.Cfg(host='h', port=1)\nc = Cfg(**opts)\n"),
    ]);
    let out = e.add_field("Cfg", "retries: int", "3", false);
    assert!(out.contains("applied=\"true\" files=\"2\" literals=\"2\""), "{out}");
    assert_eq!(
        read(&d, "cfg.py"),
        "from dataclasses import dataclass\n\n@dataclass\nclass Cfg:\n    host: str\n    port: int = 80\n    retries: int = 3\n\n    def url(self):\n        return self.host\n"
    );
    assert_eq!(read(&d, "use.py"), "from cfg import Cfg\nimport cfg\n\na = Cfg('h', retries=3)\nb = cfg.Cfg(host='h', port=1, retries=3)\nc = Cfg(**opts)\n");
}

#[test]
fn dry_run_writes_nothing() {
    let src = "struct P { x: i32 }\nfn f() -> P { P { x: 1 } }\n";
    let (d, mut e) = engine(&[("p.rs", src)]);
    let out = e.add_field("P", "y: i32", "0", true);
    assert!(out.contains("applied=\"false\" files=\"1\" literals=\"1\""), "{out}");
    assert_eq!(read(&d, "p.rs"), src);
}

#[test]
fn ambiguous_struct_gets_a_choice_card() {
    let (d, mut e) = engine(&[
        ("src/a.rs", "pub struct Cfg { a: u8 }\n"),
        ("src/b.rs", "pub struct Cfg { b: u8 }\nimpl Cfg { fn n() -> Cfg { Cfg { b: 1 } } }\n"),
    ]);
    let out = e.add_field("Cfg", "c: u8", "0", false);
    assert!(out.contains("ambiguous=\"true\"") && out.contains("add_field(target="), "{out}");
    assert_eq!(read(&d, "src/b.rs"), "pub struct Cfg { b: u8 }\nimpl Cfg { fn n() -> Cfg { Cfg { b: 1 } } }\n");
    let out = e.add_field("src/b.rs:Cfg", "c: u8", "0", false);
    assert!(out.contains("applied=\"true\""), "{out}");
    assert_eq!(read(&d, "src/b.rs"), "pub struct Cfg { b: u8, c: u8 }\nimpl Cfg { fn n() -> Cfg { Cfg { b: 1, c: 0 } } }\n");
    assert_eq!(read(&d, "src/a.rs"), "pub struct Cfg { a: u8 }\n");
}
