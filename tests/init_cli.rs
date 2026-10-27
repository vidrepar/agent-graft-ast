//! `init --dry-run` writes nothing; `init` writes the config and is idempotent.
use std::fs;
use std::process::Command;

fn run(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_agent-graft-ast")).arg("--root").arg(root).args(args).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn listing(root: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = walk(root).into_iter().map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        }
        out.push(p);
    }
    out
}

#[test]
fn dry_run_writes_nothing_then_init_is_idempotent() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    fs::create_dir_all(r.join("src")).unwrap();
    fs::write(r.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    let before = listing(r);
    let out = run(r, &["init", "--dry-run"]);
    assert!(out.contains("would create .mcp.json"), "{out}");
    assert!(out.contains("would create .claude/settings.json"), "{out}");
    assert_eq!(before, listing(r), "dry run must not write");

    let out = run(r, &["init"]);
    assert!(out.contains("created .claude/settings.json"), "{out}");
    assert!(r.join(".agent-graft/index.redb").exists());
    let settings = fs::read_to_string(r.join(".claude/settings.json")).unwrap();
    let out = run(r, &["init"]);
    assert!(out.contains("Already set up"), "{out}");
    assert_eq!(settings, fs::read_to_string(r.join(".claude/settings.json")).unwrap());

    /* the installed hook command runs from the repo and stays quiet on an unrelated prompt */
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-graft-ast"))
        .args(["hook", "prompt"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    let input = serde_json::json!({"prompt": "hello there", "cwd": r}).to_string();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
}
