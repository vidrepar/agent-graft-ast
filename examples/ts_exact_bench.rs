//! Exact TS/JS edits on a real repo: latency of tsserver-backed rename / change_signature, tsserver
//! memory, and every edited file saved for an accuracy check against TypeScript's own language service.
//!
//! cargo run --release --example ts_exact_bench -- <git repo> <cases.json> <out dir> [positions|run|cold]
//!
//! cases.json: [{"id": "r1", "op": "rename", "target": "...", "new_name": "..."},
//!              {"id": "s1", "op": "sig", "target": "...", "add_param": "...", "call_arg": "..."}]
//! `positions` writes positions.json (each target's definition as tsserver line/offset) and stops.
//! `run` pre-warms, then per case: resets the repo (`git checkout -- .`), applies the edit, saves the
//! changed files to <out>/<id>/ and the reply to <out>/<id>.xml; then times warm requests. Prints JSON.
//! `cold` times the first rename with no pre-warm (server start + project load in the call).
use agent_graft_ast::engine::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn p50(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().expect("git");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Put tracked files back and tell the engine (and so tsserver) what changed.
fn reset(e: &mut Engine, root: &Path) {
    let changed: Vec<String> = git(root, &["diff", "--name-only"]).lines().map(String::from).collect();
    git(root, &["checkout", "--", "."]);
    for rel in changed {
        e.refresh(&rel);
    }
}

fn apply(e: &mut Engine, c: &Value) -> String {
    let s = |k: &str| c[k].as_str().unwrap_or("").to_string();
    match c["op"].as_str() {
        Some("rename") => e.rename_opts(&s("target"), &s("new_name"), false, true, "skip", false),
        Some("sig") => e.change_signature(&s("target"), &s("add_param"), &s("call_arg"), false),
        _ => panic!("unknown op {c}"),
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(&a[1]).canonicalize().unwrap();
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&a[2]).unwrap()).unwrap();
    let out = PathBuf::from(&a[3]);
    std::fs::create_dir_all(&out).unwrap();
    let mode = a.get(4).map(String::as_str).unwrap_or("run");
    let t = Instant::now();
    let (mut e, _, _) = Engine::open(&root, None).unwrap();
    let open_ms = ms(t);

    if mode == "positions" {
        let pos: Vec<Value> = cases
            .iter()
            .map(|c| {
                let (f, (l, o)) = e.ts_target_at(c["target"].as_str().unwrap()).unwrap_or_else(|| panic!("no target {c}"));
                json!({"id": c["id"], "file": f, "line": l, "offset": o})
            })
            .collect();
        std::fs::write(out.join("positions.json"), serde_json::to_string_pretty(&pos).unwrap()).unwrap();
        return;
    }
    if mode == "cold" {
        let c = &cases[0];
        let t = Instant::now();
        let r = e.rename_opts(c["target"].as_str().unwrap(), c["new_name"].as_str().unwrap(), true, true, "skip", false);
        println!("{}", json!({"cold_first_rename_dry_run_ms": ms(t), "exact": r.contains("exact=\"true\""), "tsserver_rss_mb": e.ts.rss_kb().map(|k| k / 1024)}));
        return;
    }

    /* 1. pre-warm, as the MCP server does after its own warm-up */
    let t = Instant::now();
    let projects = e.ts_projects();
    e.ts.prewarm(&projects).expect("tsserver");
    let prewarm_ms = ms(t);
    let rss_after_prewarm = e.ts.rss_kb();
    let first = cases.iter().find(|c| c["op"] == "rename").expect("a rename case");
    let t = Instant::now();
    e.rename_opts(first["target"].as_str().unwrap(), first["new_name"].as_str().unwrap(), true, true, "skip", false);
    let first_dry_ms = ms(t);

    /* 2. every case, applied on a clean tree */
    let mut per_case = Vec::new();
    for c in &cases {
        reset(&mut e, &root);
        let id = c["id"].as_str().unwrap();
        let t = Instant::now();
        let reply = apply(&mut e, c);
        let took = ms(t);
        std::fs::write(out.join(format!("{id}.xml")), &reply).unwrap();
        let dir = out.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let changed: Vec<String> = git(&root, &["diff", "--name-only"]).lines().map(String::from).collect();
        for rel in &changed {
            let dst = dir.join(rel);
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::copy(root.join(rel), dst).unwrap();
        }
        per_case.push(json!({"id": id, "ms": took, "files": changed, "exact": reply.contains("exact=\"true\""),
            "new_errors_reported": reply.split("new=\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("-")}));
    }
    reset(&mut e, &root);

    /* 3. warm latency: raw tsserver rename/references, and the engine's whole rename (dry run) */
    let (mut ren, mut refs, mut dry) = (Vec::new(), Vec::new(), Vec::new());
    for c in cases.iter().filter(|c| c["op"] == "rename") {
        let target = c["target"].as_str().unwrap();
        let (f, at) = e.ts_target_at(target).unwrap();
        for _ in 0..5 {
            let t = Instant::now();
            e.ts.rename(&f, at, false).unwrap().unwrap();
            ren.push(ms(t));
            let t = Instant::now();
            e.ts.references(&f, at).unwrap();
            refs.push(ms(t));
            let t = Instant::now();
            e.rename_opts(target, c["new_name"].as_str().unwrap(), true, true, "skip", false);
            dry.push(ms(t));
        }
    }
    let applied: Vec<f64> = per_case.iter().map(|c| c["ms"].as_f64().unwrap()).collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "files_indexed": e.files.len(), "open_ms": open_ms, "projects": projects.len(), "prewarm_ms": prewarm_ms,
            "first_rename_dry_run_after_prewarm_ms": first_dry_ms, "first_applied_edit_after_prewarm_ms": applied.first(),
            "p50_applied_edit_with_diagnostics_ms": p50(applied.clone()),
            "p50_tsserver_rename_ms": p50(ren), "p50_tsserver_references_ms": p50(refs), "p50_engine_rename_dry_run_ms": p50(dry),
            "tsserver_rss_mb_after_prewarm": rss_after_prewarm.map(|k| k / 1024), "tsserver_rss_mb_end": e.ts.rss_kb().map(|k| k / 1024),
            "cases": per_case,
        }))
        .unwrap()
    );
}
