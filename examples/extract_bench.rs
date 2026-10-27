//! Single-threaded extraction time over a repo: `cargo run --release --example extract_bench -- <root> [rounds]`.
//! Prints the fastest round, which is steadier than a parallel index under load.
use agent_graft_ast::{lang::Lang, parse};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let rounds: usize = a.get(2).and_then(|r| r.parse().ok()).unwrap_or(5);
    let files: Vec<(Lang, Vec<u8>)> = ignore::Walk::new(&a[1])
        .filter_map(Result::ok)
        .filter_map(|e| Some((Lang::from_path(e.path())?, std::fs::read(e.path()).ok()?)))
        .collect();
    let mut best = f64::MAX;
    let mut calls = 0;
    for _ in 0..rounds {
        let t = std::time::Instant::now();
        calls = files.iter().map(|(l, b)| parse::extract(*l, b).calls.len()).sum::<usize>();
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
    }
    println!("files={} calls={calls} best_ms={best:.1}", files.len());
}
