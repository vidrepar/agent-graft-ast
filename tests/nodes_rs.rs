//! Real-world guard against call fan-out: rust-analyzer's generated `nodes.rs` has ~1,000 `fn syntax(&self)`
//! methods. Runs only when REPO_RUST_ANALYZER points at a rust-analyzer checkout; skipped otherwise.
use agent_graft_ast::graph::{Edge, Graph};
use agent_graft_ast::index::{self, IndexStats};
use std::collections::HashMap;
use std::path::PathBuf;

#[test]
fn nodes_rs_calls_link_to_at_most_one_target() {
    let Ok(repo) = std::env::var("REPO_RUST_ANALYZER").map(PathBuf::from) else {
        eprintln!("skipped: REPO_RUST_ANALYZER not set");
        return;
    };
    let rel = "crates/syntax/src/ast/generated/nodes.rs";
    if !repo.join(rel).exists() {
        eprintln!("skipped: {} not found", repo.join(rel).display());
        return;
    }
    let files = index::build(&repo, HashMap::new(), &mut IndexStats::default());
    let g = Graph::build(&files);
    let fi = g.path_idx[rel];
    let calls = files[fi as usize].ex.calls.len();
    /* edges leaving nodes.rs (from its symbols or its module scope) */
    let out: usize = g
        .g
        .edge_indices()
        .filter(|&e| matches!(g.g[e], Edge::Calls { .. }))
        .filter(|&e| {
            let (src, _) = g.g.edge_endpoints(e).unwrap();
            matches!(g.g[src], agent_graft_ast::graph::Node::File(f) | agent_graft_ast::graph::Node::Sym(f, _) if f == fi)
        })
        .count();
    eprintln!("nodes.rs: {calls} call sites -> {out} call edges; graph total {}", g.g.edge_count());
    assert!(calls > 2_000, "fixture changed: {calls} calls");
    assert!(out <= calls, "fan-out: {out} edges from {calls} calls");
    assert!(g.g.edge_count() < 150_000, "graph total {}", g.g.edge_count());
}
