//! Graph build time over an indexed repo: `cargo run --release --example graph_bench -- <root> <db> [rounds]`.
//! Run with RAYON_NUM_THREADS=1 for a number that holds steady on a busy machine.
use agent_graft_ast::{engine::Engine, graph::Graph};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let rounds: usize = a.get(3).and_then(|r| r.parse().ok()).unwrap_or(5);
    let (e, _, _) = Engine::open(std::path::Path::new(&a[1]), Some(std::path::Path::new(&a[2]))).unwrap();
    let mut best = f64::MAX;
    for _ in 0..rounds {
        let t = std::time::Instant::now();
        let g = Graph::build(&e.files);
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        std::hint::black_box(g);
    }
    println!("graph best_ms={best:.1}");
}
