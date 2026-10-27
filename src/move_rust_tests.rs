use crate::engine::Engine;
use std::fs;
use std::process::Command;

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

/// `cargo check --all-targets` of the fixture: Ok, or the compiler's errors.
fn cargo_check(d: &tempfile::TempDir) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["check", "--all-targets", "--offline", "--quiet", "--message-format=short"])
        .current_dir(d.path())
        .env("CARGO_TARGET_DIR", d.path().join("target"))
        .env_remove("RUSTFLAGS")
        .output()
        .unwrap();
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

const TOML: &str = "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";

const A: &str = "use std::collections::HashMap;

const SCALE: u32 = 3;

/// A square.
#[derive(Debug, Clone)]
pub struct Shape {
    side: u32,
}

impl Shape {
    pub fn new(side: u32) -> Self {
        Shape { side }
    }

    fn raw(&self) -> u32 {
        self.side
    }
}

pub fn area(s: &Shape) -> u32 {
    let mut m: HashMap<u32, u32> = HashMap::new();
    m.insert(1, s.raw() * SCALE);
    m[&1] + s.side
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t() {
        assert_eq!(area(&Shape::new(1)), 4);
    }
}
";

fn fixture(c_vis: &str) -> (tempfile::TempDir, Engine) {
    let lib = format!("pub mod a;\n{c_vis}mod c;\npub use a::Shape;\n\npub fn root() -> u32 {{\n    a::area(&a::Shape::new(2)) + c::twice()\n}}\n");
    engine(&[
        ("Cargo.toml", TOML),
        ("src/lib.rs", &lib),
        ("src/a.rs", A),
        (
            "src/c.rs",
            "use crate::a::{area, Shape};\n\npub fn twice() -> u32 {\n    area(&Shape::new(1)) * 2 + crate::a::area(&super::a::Shape::new(0))\n}\n",
        ),
        ("tests/it.rs", "use fx::a::Shape;\n\n#[test]\nfn x() {\n    assert_eq!(fx::a::area(&Shape::new(1)), 4);\n}\n"),
    ])
}

#[test]
fn moves_a_fn_to_a_new_module_and_it_compiles() {
    let (d, mut e) = fixture("");
    let out = e.move_symbol("area", "src/geo.rs", false);
    assert!(out.contains("applied=\"true\""), "{out}");
    assert_eq!(
        read(&d, "src/geo.rs"),
        "use crate::a::{SCALE, Shape};\nuse std::collections::HashMap;\n\npub fn area(s: &Shape) -> u32 {\n    let mut m: HashMap<u32, u32> = HashMap::new();\n    m.insert(1, s.raw() * SCALE);\n    m[&1] + s.side\n}\n"
    );
    /* used from tests/it.rs as fx::a::area: the new module is public */
    assert!(read(&d, "src/lib.rs").starts_with("pub mod a;\nmod c;\npub mod geo;\n"), "{}", read(&d, "src/lib.rs"));
    assert!(read(&d, "src/lib.rs").contains("geo::area(&a::Shape::new(2))"));
    assert_eq!(
        read(&d, "src/c.rs"),
        "use crate::a::Shape;\nuse crate::geo::area;\n\npub fn twice() -> u32 {\n    area(&Shape::new(1)) * 2 + crate::geo::area(&super::a::Shape::new(0))\n}\n"
    );
    assert!(read(&d, "tests/it.rs").contains("fx::geo::area(&Shape::new(1))"));
    let a = read(&d, "src/a.rs");
    /* only the test module still calls it (through `use super::*`) */
    assert!(a.contains("    use super::*;\n    use crate::geo::area;\n"), "{a}");
    assert!(a.contains("pub(crate) const SCALE") && a.contains("pub(crate) side: u32") && a.contains("pub(crate) fn raw"), "{a}");
    assert!(!a.contains("pub fn area") && !a.contains("HashMap"), "{a}");
    cargo_check(&d).unwrap();
}

#[test]
fn moves_a_struct_with_its_impls_into_an_existing_module() {
    let (d, mut e) = fixture("pub ");
    let out = e.move_symbol("src/a.rs:Shape", "src/c.rs", false);
    assert!(out.contains("applied=\"true\""), "{out}");
    let c = read(&d, "src/c.rs");
    assert!(c.starts_with("use crate::a::area;\n"), "{c}");
    assert!(c.contains("self::Shape::new(0)") && c.contains("pub struct Shape") && c.contains("impl Shape"), "{c}");
    let a = read(&d, "src/a.rs");
    assert!(!a.contains("struct Shape") && !a.contains("impl Shape"), "{a}");
    assert!(a.contains("use crate::c::Shape;"), "{a}");
    assert!(read(&d, "src/lib.rs").contains("pub use c::Shape;"));
    assert!(read(&d, "tests/it.rs").starts_with("use fx::c::Shape;"));
    cargo_check(&d).unwrap();
}

#[test]
fn private_destination_refused_when_other_crates_use_it() {
    let (d, mut e) = fixture("");
    let out = e.move_symbol("src/a.rs:Shape", "src/c.rs", false);
    assert!(out.contains("error=\"dest_private\""), "{out}");
    assert_eq!(read(&d, "src/a.rs"), A);
}

#[test]
fn moves_out_of_the_crate_root_and_into_a_nested_module() {
    let (d, mut e) = engine(&[
        ("Cargo.toml", TOML),
        ("src/main.rs", "mod util;\n\nfn helper(x: u32) -> u32 {\n    x + 1\n}\n\nfn main() {\n    println!(\"{}\", helper(util::two()));\n}\n"),
        ("src/util.rs", "pub fn two() -> u32 {\n    super::helper(1)\n}\n"),
    ]);
    let out = e.move_symbol("helper", "src/util/math.rs", false);
    assert!(out.contains("applied=\"true\""), "{out}");
    assert_eq!(read(&d, "src/util/math.rs"), "pub(crate) fn helper(x: u32) -> u32 {\n    x + 1\n}\n");
    assert_eq!(read(&d, "src/util.rs"), "pub(crate) mod math;\n\npub fn two() -> u32 {\n    self::math::helper(1)\n}\n");
    assert_eq!(read(&d, "src/main.rs"), "mod util;\nuse crate::util::math::helper;\n\nfn main() {\n    println!(\"{}\", helper(util::two()));\n}\n");
    cargo_check(&d).unwrap();
}

#[test]
fn trait_moves_and_glob_importers_and_macros_follow() {
    let (d, mut e) = engine(&[
        ("Cargo.toml", TOML),
        ("src/lib.rs", "mod a;\nmod b;\nmod user;\n\n#[macro_export]\nmacro_rules! area_of {\n    ($x:expr) => {\n        $crate::a::Area::area(&$x)\n    };\n}\n"),
        ("src/a.rs", "pub trait Area {\n    fn area(&self) -> u32;\n}\n\npub struct Sq(pub u32);\n\nimpl Area for Sq {\n    fn area(&self) -> u32 {\n        self.0 * self.0\n    }\n}\n\npub fn big(s: &Sq) -> bool {\n    s.area() > 10\n}\n"),
        ("src/b.rs", "pub fn nothing() {}\n"),
        ("src/user.rs", "use crate::a::*;\n\npub fn f(s: &dyn Area) -> u32 {\n    s.area() + crate::area_of!(Sq(2)) + big(&Sq(1)) as u32\n}\n"),
    ]);
    let out = e.move_symbol("Area", "src/b.rs", false);
    assert!(out.contains("applied=\"true\""), "{out}");
    assert!(read(&d, "src/lib.rs").contains("$crate::b::Area::area(&$x)"));
    assert!(read(&d, "src/user.rs").starts_with("use crate::a::*;\nuse crate::b::Area;\n"), "{}", read(&d, "src/user.rs"));
    assert!(read(&d, "src/a.rs").starts_with("use crate::b::Area;\n\npub struct Sq"), "{}", read(&d, "src/a.rs"));
    cargo_check(&d).unwrap();
}

#[test]
fn refuses_what_it_cannot_be_sure_of() {
    let (d, mut e) = engine(&[
        ("Cargo.toml", TOML),
        ("src/lib.rs", "pub mod a;\npub mod b;\n#[path = \"odd.rs\"]\nmod p;\npub use a::*;\n"),
        ("src/a.rs", "macro_rules! twice {\n    ($x:expr) => {\n        $x * 2\n    };\n}\n\npub fn go() -> u32 {\n    twice!(2)\n}\n\npub fn stay() {}\n"),
        ("src/b.rs", "pub fn other() {}\n"),
        ("src/odd.rs", "pub fn odd() {}\n"),
        ("other/Cargo.toml", "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        ("other/src/lib.rs", "pub fn x() {}\n"),
    ]);
    assert!(e.move_symbol("go", "src/b.rs", false).contains("error=\"macro_rules\""));
    assert!(e.move_symbol("stay", "src/b.rs", false).contains("error=\"glob_reexport\""));
    assert!(e.move_symbol("odd", "src/b.rs", false).contains("error=\"path_attribute\""));
    assert!(e.move_symbol("other", "other/src/lib.rs", false).contains("error=\"cross_crate\""));
    assert!(e.move_symbol("other", "src/z/deep.rs", false).contains("error=\"no_parent_module\""));
    let clash = e.move_symbol("other", "src/odd.rs", false);
    assert!(clash.contains("error=\"path_attribute\""), "{clash}");
    assert_eq!(read(&d, "src/b.rs"), "pub fn other() {}\n");
}

#[test]
fn dry_run_and_clash() {
    let (d, mut e) = engine(&[
        ("Cargo.toml", TOML),
        ("src/lib.rs", "mod a;\nmod b;\n"),
        ("src/a.rs", "pub fn go() {}\n"),
        ("src/b.rs", "pub fn go() {}\n"),
    ]);
    let out = e.move_symbol("src/a.rs:go", "src/b.rs", false);
    assert!(out.contains("<choose>") && out.contains("already defined"), "{out}");
    let out = e.move_symbol("src/a.rs:go", "src/c.rs", true);
    assert!(out.contains("applied=\"false\""), "{out}");
    assert!(!d.path().join("src/c.rs").exists());
    assert_eq!(read(&d, "src/lib.rs"), "mod a;\nmod b;\n");
}

#[test]
fn path_cycles_and_deep_nesting_end() {
    /* a.rs and b.rs load each other through #[path]: each file is loaded once */
    let (d, mut e) = engine(&[
        ("Cargo.toml", TOML),
        ("src/lib.rs", "pub mod a;\npub mod c;\n"),
        ("src/a.rs", "#[path = \"b.rs\"]\npub mod b;\n\npub fn go() -> u32 {\n    1\n}\n"),
        ("src/b.rs", "#[path = \"a.rs\"]\npub mod a;\n"),
        ("src/c.rs", "pub fn other() {}\n"),
    ]);
    let t = std::time::Instant::now();
    let out = e.move_symbol("src/a.rs:go", "src/c.rs", true);
    assert!(t.elapsed().as_secs() < 5, "{out}");
    assert!(out.contains("applied=\"false\" files="), "{out}");
    let _ = d;
    /* 70 nested inline modules: past the depth bound, refused */
    let deep = format!("{}{}\nmod x;\npub fn go() {{}}\n", "mod m {\n".repeat(70), "}\n".repeat(70));
    let (_d2, mut e2) = engine(&[("Cargo.toml", TOML), ("src/lib.rs", &deep), ("src/x.rs", "pub fn y() {}\n")]);
    let out = e2.move_symbol("src/lib.rs:go", "src/x.rs", true);
    assert!(out.contains("error=\"module_limit\""), "{out}");
}
