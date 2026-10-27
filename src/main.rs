use agent_graft_ast::engine::{default_db, Engine, DEFAULT_CALL_BUDGET};
use agent_graft_ast::{hooks, init, mcp, redirect, skeleton, tokens, ts_exact};
use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use std::time::Instant;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "agent-graft-ast", version, about = "Agent-native AST skeletons, call graph and context governance")]
struct Cli {
    /// Repository root
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,
    /// Index database path (default: <root>/.agent-graft/index.redb)
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build or incrementally update the index
    Index {
        /// Delete the existing index first (cold build)
        #[arg(long)]
        fresh: bool,
    },
    /// Read a file as a budgeted skeleton
    Read {
        path: String,
        #[arg(long, default_value_t = 1)]
        level: u8,
        #[arg(long, default_value_t = DEFAULT_CALL_BUDGET)]
        budget: usize,
    },
    /// Read one symbol with its callers/callees
    Symbol {
        name: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long, default_value_t = 0)]
        level: u8,
        #[arg(long, default_value_t = DEFAULT_CALL_BUDGET)]
        budget: usize,
    },
    /// Everything needed to change a symbol, in one reply
    Context {
        target: String,
        #[arg(long, default_value_t = 4000)]
        budget: usize,
    },
    /// Rename a symbol everywhere and write the files
    Rename {
        target: String,
        new_name: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
    },
    /// Append a parameter to a function and an argument to every call
    ChangeSignature {
        target: String,
        add_param: String,
        call_arg: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Compile the projects owning these files and print their errors
    Check {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Add a field to a struct/interface/dataclass and to every literal that builds it
    AddField {
        target: String,
        field: String,
        /// value written into every literal (omit for optional TypeScript fields)
        #[arg(default_value = "")]
        value: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Move a top-level symbol to another file and fix imports
    MoveSymbol {
        target: String,
        to_file: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Structural search (no replacement) or search-and-replace: `$X` one node, `$$$ARGS` any siblings
    Rewrite {
        pattern: String,
        replacement: Option<String>,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long)]
        paths: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// 1-hop call graph for a symbol
    Callers {
        name: String,
        #[arg(long)]
        path: Option<String>,
    },
    /// Find symbols by name substring
    Find {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Literal text search across indexed files
    Grep {
        pattern: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Serve MCP over stdio
    Serve,
    /// Claude Code hook: reads the hook JSON on stdin, prints added context (or nothing)
    Hook {
        #[command(subcommand)]
        kind: HookKind,
    },
    /// Set up this repo for Claude Code: index, .mcp.json, hooks in .claude/settings.json, read-only tool permission in .claude/settings.local.json, .gitignore, CLAUDE.md block
    Init {
        /// Print the planned changes and write nothing
        #[arg(long)]
        dry_run: bool,
        /// Leave out the PreToolUse search redirect and the CLAUDE.md block (and remove them if present)
        #[arg(long)]
        no_redirect: bool,
        /// Also allow the edit tools (rename, change_signature, ...) without a prompt, in your .claude/settings.local.json
        #[arg(long)]
        allow_edits: bool,
        /// Also install the pinned TypeScript (with pnpm) that exact TS/JS edits run as tsserver
        #[arg(long)]
        typescript: bool,
    },
    /// Measure warm query latency and token reduction; prints JSON
    Bench {
        #[arg(long, default_value_t = 2000)]
        iters: usize,
        /// File with repo-relative paths (one per line) for read/token measurements
        #[arg(long)]
        files: Option<PathBuf>,
    },
}

#[derive(Subcommand, Clone, Copy)]
enum HookKind {
    /// UserPromptSubmit: point at the symbols the prompt names
    Prompt,
    /// PostToolUse (Edit/Write/MultiEdit): list the files that depend on the edited file
    PostEdit,
    /// PreToolUse (Grep/Glob/Bash): answer a search for a defined symbol from the index instead of running it
    PreTool,
}

/// Hooks never fail loudly: any error or panic means exit 0 with no output.
fn run_hook(kind: HookKind, root: PathBuf, db: Option<PathBuf>) {
    let explicit = db.map(|db| (root.canonicalize().unwrap_or(root), db));
    let out = std::panic::catch_unwind(move || {
        let mut stdin = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut stdin).ok()?;
        match kind {
            HookKind::Prompt => hooks::prompt_hook(&stdin, explicit),
            HookKind::PostEdit => hooks::post_edit_hook(&stdin, explicit),
            HookKind::PreTool => redirect::pre_tool_hook(&stdin, explicit),
        }
    });
    if let Ok(Some(text)) = out {
        println!("{text}");
    }
}

fn percentile(sorted_ns: &[u128], p: f64) -> f64 {
    if sorted_ns.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_ns.len() as f64 - 1.0) * p).round() as usize;
    sorted_ns[idx] as f64 / 1e6
}

fn lat(mut v: Vec<u128>) -> serde_json::Value {
    v.sort_unstable();
    json!({"n": v.len(), "p50_ms": percentile(&v, 0.50), "p99_ms": percentile(&v, 0.99), "max_ms": percentile(&v, 1.0)})
}

fn main() -> Result<()> {
    agent_graft_ast::qos::init_pool();
    let cli = Cli::parse();
    if let Cmd::Hook { kind } = cli.cmd {
        std::panic::set_hook(Box::new(|_| {}));
        run_hook(kind, cli.root, cli.db);
        return Ok(());
    }
    let root = cli.root.canonicalize()?;
    let db = cli.db.clone().unwrap_or_else(|| default_db(&root));
    let open = |root: &PathBuf| Engine::open(root, Some(&db));

    match cli.cmd {
        Cmd::Index { fresh } => {
            if fresh {
                let _ = std::fs::remove_file(&db);
            }
            let t = Instant::now();
            let (e, stats, tm) = open(&root)?;
            let total = t.elapsed().as_secs_f64() * 1000.0;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "root": root, "db": db, "threads": rayon::current_num_threads(),
                    "total_ms": total, "load_ms": tm.load_ms, "parse_ms": tm.index_ms, "graph_ms": tm.graph_ms, "save_ms": tm.save_ms,
                    "files": e.files.len(), "stats": stats,
                    "graph_nodes": e.graph.g.node_count(), "graph_edges": e.graph.g.edge_count(),
                }))?
            );
        }
        Cmd::Read { path, level, budget } => println!("{}", open(&root)?.0.read_file(&path, level, budget)),
        Cmd::Symbol { name, path, level, budget } => println!("{}", open(&root)?.0.read_symbol(&name, path.as_deref(), level, budget)),
        Cmd::Context { target, budget } => println!("{}", open(&root)?.0.context(&target, budget)),
        Cmd::Rename { target, new_name, dry_run, force } => println!("{}", open(&root)?.0.rename(&target, &new_name, dry_run, force)),
        Cmd::ChangeSignature { target, add_param, call_arg, dry_run } => {
            println!("{}", open(&root)?.0.change_signature(&target, &add_param, &call_arg, dry_run))
        }
        Cmd::Check { paths } => println!("{}", open(&root)?.0.check(&paths)),
        Cmd::AddField { target, field, value, dry_run } => println!("{}", open(&root)?.0.add_field(&target, &field, &value, dry_run)),
        Cmd::MoveSymbol { target, to_file, dry_run } => println!("{}", open(&root)?.0.move_symbol_checked(&target, &to_file, dry_run)),
        Cmd::Rewrite { pattern, replacement, lang, paths, dry_run } => {
            println!("{}", open(&root)?.0.rewrite(&pattern, replacement.as_deref(), lang.as_deref(), &paths, dry_run))
        }
        Cmd::Callers { name, path } => println!("{}", open(&root)?.0.callers(&name, path.as_deref())),
        Cmd::Find { query, limit } => println!("{}", open(&root)?.0.find(&query, limit)),
        Cmd::Grep { pattern, limit } => println!("{}", open(&root)?.0.grep(&pattern, limit)),
        Cmd::Serve => {
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(mcp::serve(root.clone(), Some(db.clone())))?;
        }
        Cmd::Hook { .. } => unreachable!("handled before the root is resolved"),
        Cmd::Init { dry_run, no_redirect, allow_edits, typescript } => {
            let opts = init::Options { redirect: !no_redirect, allow_edits };
            let exe = std::env::current_exe()?.canonicalize()?;
            let index_line = if dry_run {
                format!("would build the index at {}", db.display())
            } else {
                let t = Instant::now();
                let (e, _, _) = open(&root)?;
                format!("indexed {} files into {} in {:.0} ms", e.files.len(), db.display(), t.elapsed().as_secs_f64() * 1000.0)
            };
            let changes = init::plan(&root, &exe, opts)?;
            if !dry_run {
                init::apply(&changes)?;
            }
            print!("{}", init::describe(&root, &exe, &changes, dry_run, opts, &index_line));
            /* exact TS/JS edits: shown, and installed only when asked for */
            if let Some(dir) = ts_exact::pinned_dir() {
                let cmd = ts_exact::install_command(&dir);
                if typescript && !dry_run {
                    println!("installing TypeScript {} for exact TS/JS edits: {cmd}", ts_exact::PINNED);
                    let ok = std::process::Command::new("sh").arg("-c").arg(&cmd).status()?.success();
                    println!("{}", if ok { "TypeScript installed: rename, change_signature and context resolve TS/JS through tsserver." } else { "TypeScript install failed; TS/JS edits use the syntax engine." });
                } else if ts_exact::locate(&root).is_err() {
                    println!("exact TS/JS edits (tsserver) are off: run `agent-graft-ast init --typescript`, which runs: {cmd}");
                }
            }
        }
        Cmd::Bench { iters, files } => {
            /* 1. warm open from the persisted index */
            let t = Instant::now();
            let (mut e, stats, tm) = open(&root)?;
            let open_ms = t.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            e.warm();
            let warm_ms = t.elapsed().as_secs_f64() * 1000.0;

            /* 2. pure graph queries: callers + callees of pseudo-random symbols */
            let syms = e.all_symbols();
            let mut rng = 0x9E3779B97F4A7C15u64;
            let mut next = |n: usize| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                (rng % n.max(1) as u64) as usize
            };
            let mut graph_ns = Vec::with_capacity(iters);
            let mut edges_seen = 0usize;
            for _ in 0..iters {
                let (f, s) = syms[next(syms.len())];
                let t = Instant::now();
                let a = e.graph.callers(f, s);
                let b = e.graph.callees(f, Some(s));
                graph_ns.push(t.elapsed().as_nanos());
                edges_seen += a.len() + b.len();
            }

            /* 3. full MCP tool path for `callers` (lookup + XML + receipt token count) */
            let mut tool_ns = Vec::new();
            for _ in 0..(iters / 4).max(1) {
                let (f, s) = syms[next(syms.len())];
                let name = e.files[f as usize].ex.symbols[s as usize].name.clone();
                let t = Instant::now();
                let out = e.callers(&name, None);
                tool_ns.push(t.elapsed().as_nanos());
                std::hint::black_box(out);
            }

            /* 4. read_file latency + token reduction on the listed files */
            let list: Vec<String> = files
                .map(|p| std::fs::read_to_string(p).unwrap_or_default())
                .unwrap_or_default()
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            let mut per_file = Vec::new();
            let mut read_ns = Vec::new();
            for rel in &list {
                let abs = root.join(rel);
                let Ok(bytes) = std::fs::read(&abs) else { continue };
                let raw = tokens::count(&String::from_utf8_lossy(&bytes));
                let Some(&fi) = e.graph.path_idx.get(rel) else {
                    per_file.push(json!({"path": rel, "raw_tokens": raw, "indexed": false}));
                    continue;
                };
                let f = &e.files[fi as usize];
                let l1 = tokens::count(&skeleton::render(f.lang, &bytes, &f.ex, 1));
                let l2 = tokens::count(&skeleton::render(f.lang, &bytes, &f.ex, 2));
                let resp1 = tokens::count(&e.read_file(rel, 1, DEFAULT_CALL_BUDGET));
                let resp2 = tokens::count(&e.read_file(rel, 2, DEFAULT_CALL_BUDGET));
                for _ in 0..20 {
                    let t = Instant::now();
                    std::hint::black_box(e.read_file(rel, 1, DEFAULT_CALL_BUDGET));
                    read_ns.push(t.elapsed().as_nanos());
                }
                per_file.push(json!({
                    "path": rel, "raw_tokens": raw, "level1_tokens": l1, "level2_tokens": l2,
                    "read_file_l1_response_tokens": resp1, "read_file_l2_response_tokens": resp2,
                    "ratio_raw_over_l1_response": raw as f64 / resp1.max(1) as f64,
                }));
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "root": root, "tokenizer": tokens::TOKENIZER, "files_indexed": e.files.len(), "symbols": syms.len(),
                    "graph_nodes": e.graph.g.node_count(), "graph_edges": e.graph.g.edge_count(),
                    "open_ms": open_ms, "open_breakdown": {"load_ms": tm.load_ms, "check_ms": tm.index_ms, "graph_ms": tm.graph_ms, "save_ms": tm.save_ms},
                    "reparsed_on_open": stats.parsed, "warm_lazy_state_ms": warm_ms,
                    "graph_query": lat(graph_ns), "avg_edges_per_query": edges_seen as f64 / iters.max(1) as f64,
                    "callers_tool": lat(tool_ns), "read_file_tool_l1": lat(read_ns), "files": per_file,
                }))?
            );
        }
    }
    Ok(())
}
