//! Method-call resolution counts for one repo: `cargo run --release --example call_stats -- <root> <db> <out.tsv>`.
//! Counts `x.m()` call sites by how many same-family methods are named `m`, and whether the graph linked them.
//! `out.tsv` lists every linked call whose name has 2+ method definitions: site, then target.
use agent_graft_ast::engine::Engine;
use agent_graft_ast::graph::{Edge, Node};
use agent_graft_ast::parse::SymKind;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let t = std::time::Instant::now();
    let (e, _, _) = Engine::open(Path::new(&a[1]), Some(Path::new(&a[2]))).unwrap();
    eprintln!("open {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    let fam = |l: agent_graft_ast::lang::Lang| if l.is_js_family() { None } else { Some(l) };
    let mut out = std::fs::File::create(&a[3]).unwrap();
    let mut n: HashMap<&str, usize> = HashMap::new();
    /* every call edge by (file, caller, line, target name) */
    let mut edges: HashMap<(u32, Option<u32>, u32, &str), Vec<(u32, u32)>> = HashMap::new();
    for ei in e.graph.g.edge_indices() {
        let Edge::Calls { line } = e.graph.g[ei] else { continue };
        let (a, b) = e.graph.g.edge_endpoints(ei).unwrap();
        let (fi, caller) = match e.graph.g[a] { Node::File(f) => (f, None), Node::Sym(f, s) => (f, Some(s)) };
        let Node::Sym(tf, ts) = e.graph.g[b] else { continue };
        let name = e.files[tf as usize].ex.symbols[ts as usize].name.as_str();
        edges.entry((fi, caller, line, name)).or_default().push((tf, ts));
    }
    for (fi, f) in e.files.iter().enumerate() {
        for c in f.ex.calls.iter().filter(|c| c.recv) {
            let cands: Vec<(u32, u32)> = e.graph.by_name.get(&c.callee).into_iter().flatten().copied()
                .filter(|&(cf, cs)| e.files[cf as usize].ex.symbols[cs as usize].kind == SymKind::Method && fam(e.files[cf as usize].lang) == fam(f.lang))
                .collect();
            let hit = edges.get(&(fi as u32, c.caller, c.line, c.callee.as_str())).and_then(|v| v.iter().find(|t| cands.contains(t)));
            let bucket = match (cands.len(), hit.is_some()) {
                (0, _) => "0 cands",
                (1, true) => "1 cand, linked",
                (1, false) => "1 cand, unlinked",
                (_, true) => "2+ cands, linked",
                (_, false) => "2+ cands, unlinked",
            };
            *n.entry(bucket).or_default() += 1;
            let known = e.graph.receiver_type(&e.files, fi as u32, c).is_some();
            if known {
                *n.entry("receiver type known").or_default() += 1;
            }
            if cands.len() > 1 && !known {
                *n.entry("2+ cands, receiver untyped").or_default() += 1;
            }
            if cands.len() > 1 && hit.is_none() && std::env::var("UNLINKED").is_ok() {
                writeln!(out, "UNLINKED\t{}:{}\t{}\t{:?}\t{:?}\t{:?}", f.path, c.line, c.callee, c.qual, c.rtype, c.rhint).unwrap();
            }
            if let (true, Some(&(rf, rs))) = (cands.len() > 1, hit) {
                let tf = &e.files[rf as usize];
                let ts = &tf.ex.symbols[rs as usize];
                let owner = ts.parent.map(|p| tf.ex.symbols[p as usize].name.as_str()).unwrap_or("");
                writeln!(out, "{}:{}\t{}\t{}:{}\t{}", f.path, c.line, c.callee, tf.path, ts.start_line, owner).unwrap();
            }
        }
    }
    let mut v: Vec<_> = n.into_iter().collect();
    v.sort();
    for (k, c) in v {
        println!("{k}\t{c}");
    }
}
