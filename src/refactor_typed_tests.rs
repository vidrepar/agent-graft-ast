//! rename / change_signature on methods whose calls have a known receiver type.
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

const SHAPES: &str = "pub struct Circle;\nimpl Circle { pub fn area(&self) -> f64 { 1.0 } }\npub struct Square;\nimpl Square { pub fn area(&self) -> f64 { 2.0 } }\n";

#[test]
fn rename_by_type_skips_the_other_types_calls() {
    let (d, mut e) = engine(&[
        ("src/shapes.rs", SHAPES),
        ("src/use.rs", "fn total(c: &Circle, s: &Square, x: Thing) -> f64 {\n    let y = make();\n    c.area()\n        + s.area()\n        + x.area()\n        + y.area()\n}\n"),
    ]);
    /* the bare name is ambiguous: a card */
    assert!(e.rename("area", "surface", true, false).contains("<choose"));
    let out = e.rename("Circle::area", "surface", false, false);
    assert!(!out.contains("<choose"), "{out}");
    assert_eq!(
        read(&d, "src/shapes.rs"),
        "pub struct Circle;\nimpl Circle { pub fn surface(&self) -> f64 { 1.0 } }\npub struct Square;\nimpl Square { pub fn area(&self) -> f64 { 2.0 } }\n"
    );
    /* Square's and Thing's (not the repo's) calls keep their name; so does `y.area()`, whose receiver
     * could be either type: it is listed for the model instead */
    assert_eq!(
        read(&d, "src/use.rs"),
        "fn total(c: &Circle, s: &Square, x: Thing) -> f64 {\n    let y = make();\n    c.surface()\n        + s.area()\n        + x.area()\n        + y.area()\n}\n"
    );
    assert!(out.contains("<check reason=\"receiver type unknown and Circle, Square define `area`") && out.contains("src/use.rs lines 6"), "{out}");
}

#[test]
fn untyped_calls_follow_the_only_type_or_wait_for_a_decision() {
    /* one type defines `push_edit`: an untyped receiver can only mean it */
    let (d, mut e) = engine(&[("lib.rs", "pub struct B;\nimpl B { pub fn push_edit(&self, a: u8) {} }\nfn f(x: Vec<B>) { x[0].push_edit(1); }\n")]);
    e.change_signature("push_edit", "b: u8", "2", false);
    assert_eq!(read(&d, "lib.rs"), "pub struct B;\nimpl B { pub fn push_edit(&self, a: u8, b: u8) {} }\nfn f(x: Vec<B>) { x[0].push_edit(1, 2); }\n");
    /* two do: the untyped call keeps its arguments and is listed */
    let (d, mut e) = engine(&[("lib.rs", &format!("{SHAPES}fn f(c: &Circle, v: Vec<Square>) {{ c.area(); v[0].area(); }}\n"))]);
    let out = e.change_signature("Circle::area", "k: u8", "1", false);
    assert!(read(&d, "lib.rs").ends_with("fn f(c: &Circle, v: Vec<Square>) { c.area(1); v[0].area(); }\n"), "{}", read(&d, "lib.rs"));
    assert!(out.contains("sites=\"1\">\nlib.rs lines 5"), "{out}");
}

#[test]
fn type_dot_method_target_in_typescript_and_python() {
    let (d, mut e) = engine(&[
        ("a.ts", "export class Repo { save(x: number) {} }\nexport class Cache { save(x: number) {} }\nexport function f(r: Repo, c: Cache) { r.save(1); c.save(2); }\n"),
        ("m.py", "class Repo:\n    def get(self): pass\n\nclass Cache:\n    def get(self): pass\n\ndef f(r: Repo):\n    c = Cache()\n    r.get()\n    c.get()\n"),
    ]);
    let out = e.change_signature("Cache.save", "y: number", "0", false);
    assert!(out.contains("applied=\"true\""), "{out}");
    assert_eq!(
        read(&d, "a.ts"),
        "export class Repo { save(x: number) {} }\nexport class Cache { save(x: number, y: number) {} }\nexport function f(r: Repo, c: Cache) { r.save(1); c.save(2, 0); }\n"
    );
    e.rename("Repo.get", "fetch", false, false);
    assert_eq!(
        read(&d, "m.py"),
        "class Repo:\n    def fetch(self): pass\n\nclass Cache:\n    def get(self): pass\n\ndef f(r: Repo):\n    c = Cache()\n    r.fetch()\n    c.get()\n"
    );
}

#[test]
fn a_types_trait_method_renames_with_its_trait() {
    let (d, mut e) = engine(&[(
        "lib.rs",
        "pub trait Named { fn name(&self); }\npub struct Dog;\nimpl Named for Dog { fn name(&self) {} }\npub struct Cat;\nimpl Named for Cat { fn name(&self) {} }\npub struct Car;\nimpl Car { pub fn name(&self) {} }\nfn f(d: Dog, c: Car) { d.name(); c.name(); }\n",
    )]);
    let out = e.rename("Dog.name", "label", false, false);
    assert!(!out.contains("<choose"), "{out}");
    assert_eq!(
        read(&d, "lib.rs"),
        "pub trait Named { fn label(&self); }\npub struct Dog;\nimpl Named for Dog { fn label(&self) {} }\npub struct Cat;\nimpl Named for Cat { fn label(&self) {} }\npub struct Car;\nimpl Car { pub fn name(&self) {} }\nfn f(d: Dog, c: Car) { d.label(); c.name(); }\n"
    );
}

#[test]
fn a_trait_implemented_for_a_foreign_type_keeps_its_calls() {
    let (d, mut e) = engine(&[(
        "lib.rs",
        "pub trait Sum { fn total(&self) -> u8; }\nimpl Sum for Vec<u8> { fn total(&self) -> u8 { 0 } }\npub struct Bill;\nimpl Bill { pub fn total(&self) -> u8 { 1 } }\nfn f(v: Vec<u8>, s: String) { v.total(); s.total(); }\n",
    )]);
    e.rename("Sum.total", "sum", false, false);
    assert_eq!(
        read(&d, "lib.rs"),
        "pub trait Sum { fn sum(&self) -> u8; }\nimpl Sum for Vec<u8> { fn sum(&self) -> u8 { 0 } }\npub struct Bill;\nimpl Bill { pub fn total(&self) -> u8 { 1 } }\nfn f(v: Vec<u8>, s: String) { v.sum(); s.total(); }\n"
    );
}

#[test]
fn calls_in_macros_follow_their_receiver_type() {
    let (d, mut e) = engine(&[("lib.rs", &format!("{SHAPES}fn f(c: &Circle, s: &Square) {{ let v = vec![c.area(), s.area()]; }}\n"))]);
    e.change_signature("Circle::area", "k: u8", "1", false);
    assert!(read(&d, "lib.rs").ends_with("{ let v = vec![c.area(1), s.area()]; }\n"), "{}", read(&d, "lib.rs"));
}
