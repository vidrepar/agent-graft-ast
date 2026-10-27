use crate::engine::Engine;
use crate::ts_exact::TsService;
use std::fs;
use std::sync::Arc;

/// A repo from `files`, indexed, with tsserver on; None when this machine has no tsserver.
fn repo(files: &[(&str, &str)]) -> Option<(tempfile::TempDir, Engine)> {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    fs::write(r.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"noEmit":true,"jsx":"react-jsx","target":"es2022","module":"esnext","moduleResolution":"bundler"}}"#).unwrap();
    for (p, text) in files {
        if let Some(dir) = r.join(p).parent() {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(r.join(p), text).unwrap();
    }
    let (mut e, _, _) = Engine::open(r, Some(&r.join("i.redb"))).unwrap();
    crate::ts_exact::tests::live(&e.root)?;
    e.ts = Arc::new(TsService::enabled(&e.root));
    Some((d, e))
}

fn read(d: &tempfile::TempDir, p: &str) -> String {
    fs::read_to_string(d.path().join(p)).unwrap()
}

#[test]
fn rename_follows_types_not_names() {
    let Some((d, mut e)) = repo(&[
        ("a.ts", "export interface Store { save(x: number): void }\nexport class Db implements Store { save(x: number) {} }\nexport class Log { save(x: number) {} }\n"),
        ("b.ts", "import { Db, Log, type Store } from './a';\nconst s: Store = new Db();\ns.save(1);\nnew Db().save(2);\nnew Log().save(3);\nconst o = { save: 1 };\nconst { save } = o;\n"),
    ]) else { return };
    let out = e.rename("a.ts:Store.save", "persist", false, false);
    assert!(out.contains("exact=\"true\"") && out.contains("new=\"0\""), "{out}");
    assert_eq!(read(&d, "a.ts"), "export interface Store { persist(x: number): void }\nexport class Db implements Store { persist(x: number) {} }\nexport class Log { save(x: number) {} }\n");
    /* the unrelated `Log.save` and the object key stay */
    assert_eq!(
        read(&d, "b.ts"),
        "import { Db, Log, type Store } from './a';\nconst s: Store = new Db();\ns.persist(1);\nnew Db().persist(2);\nnew Log().save(3);\nconst o = { save: 1 };\nconst { save } = o;\n"
    );
}

#[test]
fn rename_keeps_shorthand_keys_and_reports_new_errors() {
    let Some((d, mut e)) = repo(&[
        ("a.ts", "export function hello() { return 1; }\nexport const greet = 2;\n"),
        ("b.tsx", "import { hello } from './a';\nexport const o = { hello };\nexport const n: number = o.hello();\n"),
    ]) else { return };
    let out = e.rename("hello", "hi", false, false);
    assert!(out.contains("exact=\"true\""), "{out}");
    assert_eq!(read(&d, "b.tsx"), "import { hi } from './a';\nexport const o = { hello: hi };\nexport const n: number = o.hello();\n");
    /* a rename onto an existing export compiles to a duplicate: the reply shows it (forced past the clash card) */
    let out = e.rename("a.ts:hi", "greet", false, true);
    assert!(out.contains("new=\"2\"") && out.contains("a.ts:2 TS2300 Duplicate identifier"), "{out}");
}

#[test]
fn change_signature_updates_exact_calls_only() {
    let Some((d, mut e)) = repo(&[
        ("a.ts", "export class A { run(x: number) { return x; } }\nexport class B { run(x: number) { return x; } }\n"),
        ("b.ts", "import { A, B } from './a';\nconst a = new A();\na.run(1);\nnew B().run(2);\nconst f = a.run;\n"),
    ]) else { return };
    let out = e.change_signature("A.run", "y: number", "0", false);
    assert!(out.contains("exact=\"true\"") && out.contains("calls=\"1\""), "{out}");
    assert!(out.contains("<check reason") && out.contains("b.ts lines 5"), "{out}");
    assert_eq!(read(&d, "b.ts"), "import { A, B } from './a';\nconst a = new A();\na.run(1, 0);\nnew B().run(2);\nconst f = a.run;\n");
    assert!(read(&d, "a.ts").starts_with("export class A { run(x: number, y: number)"));
}

#[test]
fn context_lists_references_not_name_matches() {
    let Some((_d, mut e)) = repo(&[
        ("a.ts", "export function load() {}\n// load is mentioned here\nconst s = 'load';\n"),
        ("b.ts", "import { load } from './a';\nload();\nfunction other() { const load = 1; return load; }\n"),
    ]) else { return };
    let out = e.context("a.ts:load", 4000);
    assert!(out.contains("exact=\"true\""), "{out}");
    assert!(out.contains("<sites n=\"3\" files=\"2\">\na.ts [Read whole file] lines 1\nb.ts [Read whole file] lines 1,2\n</sites>"), "{out}");
}

#[test]
fn without_tsserver_replies_fall_back_and_say_so() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("a.ts"), "export function hello() {}\nhello();\n").unwrap();
    let (mut e, _, _) = Engine::open(d.path(), Some(&d.path().join("i.redb"))).unwrap();
    e.ts = Arc::new(TsService::unavailable(&e.root, "node is not on PATH"));
    let out = e.rename("hello", "hi", false, false);
    assert!(out.contains("applied=\"true\"") && out.contains("exact=\"false\""), "{out}");
    assert!(out.contains("<exact_off reason=\"node is not on PATH\">") && out.contains("init --typescript"), "{out}");
    assert_eq!(fs::read_to_string(d.path().join("a.ts")).unwrap(), "export function hi() {}\nhi();\n");
}

#[test]
fn unresolved_package_imports_fall_back_to_the_syntax_engine() {
    /* `@pkg/core` resolves only through node_modules, which is not installed: tsserver sees the definition
    alone, and an edit from its answer would skip the caller */
    let Some((d, mut e)) = repo(&[
        ("packages/core/package.json", r#"{"name":"@pkg/core","main":"src/index.ts"}"#),
        ("packages/core/src/index.ts", "export function durationOf(h: number) { return h * 60; }\n"),
        ("apps/web/package.json", r#"{"name":"web","dependencies":{"@pkg/core":"workspace:*"}}"#),
        ("apps/web/src/page.ts", "import { durationOf } from '@pkg/core';\nexport const m = durationOf(2);\n"),
    ]) else { return };
    let out = e.rename("durationOf", "minutesOf", false, false);
    assert!(out.contains("exact=\"false\"") && out.contains("tsserver did not see 1 file(s)"), "{out}");
    assert_eq!(read(&d, "apps/web/src/page.ts"), "import { minutesOf } from '@pkg/core';\nexport const m = minutesOf(2);\n");
}
