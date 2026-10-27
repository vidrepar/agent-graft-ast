# Baseline results: ast-grep and graft MCP

Measured 2026-10-27 on an Apple M3 Pro (12 cores), macOS 26.2. Machine-readable copy: `results.json`. Raw per-run data: `out/`.

**The machine was busy.** Another agent was compiling Rust at the same time. Load average went from about 3 to 15 on 12 cores during the final run (`out/load.txt`). Treat absolute numbers as noisy. All runs used the same conditions, so comparisons between tools are fair. A first 3-run pass gave higher graft build times on two runs (ts-monorepo took 11.45 s once, against 3.7 s normally). The final numbers come from a separate 5-run pass, made after graft was reinstalled with pnpm. In that pass, rust-analyzer's 5th build took 9.54 s, against 5.6–6.2 s for the others.

## Versions

| | |
|---|---|
| ast-grep | 0.42.1 (`/opt/homebrew/bin/ast-grep`) |
| graft | **0.25.0** (`@nanonets/graft`), installed with `pnpm add -g @nanonets/graft@0.25.0`. The bin is `~/Library/pnpm/bin/graft`, a shim that `exec`s node. The earlier npm install was removed with `npm rm -g`. All numbers below come from the pnpm install. `pnpm add -g @nanonets/graft@latest` resolves to 0.26.0, which the local pre-install hook blocks because it is under 24 h old. |
| node | v26.8.2 |
| tokenizer | tiktoken 0.14.0, **o200k_base** (`count_tokens.py`) |

## Repos

| repo | path benchmarked | commit | graft graph |
|---|---|---|---|
| ts-monorepo (a private 650-file TypeScript monorepo) | an rsync copy (excludes node_modules, dist, .git, graft, .herdr, .turbo, .wrangler, .astro, .next, .env*) | private | 6362 nodes / 9526 edges, 657 files |
| ts-service (a private small TypeScript service) | an rsync copy (same excludes) | private | 1254 nodes / 1421 edges, 14 files |
| rust-analyzer | `git clone --depth 1 https://github.com/rust-lang/rust-analyzer` (in place) | 6ee6a51 | 32179 nodes / 36357 edges, 1528 files |

The two private repos were benchmarked on copies; their originals were not touched.
All graft runs used the env `DO_NOT_TRACK=1 GRAFT_NO_GITIGNORE=1 GRAFT_NO_IGNORE=1 GRAFT_NO_BROWSER=1`.

## a) Cold index / full scan (median of 5, wall seconds)

| tool | ts-monorepo | ts-service | rust-analyzer |
|---|---|---|---|
| graft build (cold) | **4.55 s** | **0.81 s** | **5.94 s** |
| ast-grep scan, kind rule (all fn defs) | 0.07 s | 0.04 s | 0.23 s |
| ast-grep run, pattern | 0.05 s | 0.03 s | 0.22 s |

Commands (from `bench_index.sh`):
- graft: `rm -rf graft && graft build --no-reuse .`. This deletes the index and the extraction cache (`graft/.cache`) before each run.
- ast-grep kind scan, TS: `ast-grep scan --inline-rules "<rule>" --json=stream .` with two rules, TypeScript and Tsx. Each matches `any: [function_declaration, method_definition, arrow_function, function_expression]`. Matches: ts-monorepo 3094, ts-service 147.
- ast-grep kind scan, Rust: same command with `kind: function_item`. 24136 matches.
- ast-grep pattern: `ast-grep run -p 'function $F($$$) { $$$ }' -l ts --json=stream .` for TS. For Rust, `ast-grep run -p 'fn $F($$$) $$$' -l rs --json=stream .`. This TS pattern skips functions with return-type annotations: it found 0 matches in ts-service and 221 in ts-monorepo. The kind scan is the fairer "find all functions" number.

## b) Warm query latency (5 runs, each run = 100 calls per tool. Table shows the median of the per-run p50 / p99, in ms)

graft server: one `graft mcp <dir>` process over stdio. The client sends `initialize`, then `notifications/initialized`. Then 1 first call plus 3 warmup calls per tool, which are not counted. Then 100 timed calls per tool. Calls are sequential: send a request, wait for its response.

| tool / mode | ts-monorepo | ts-service | rust-analyzer |
|---|---|---|---|
| graft_trace_calls (default) | 19.93 / 28.29 | 6.34 / 8.14 | 47.97 / 81.47 |
| graft_file_api (default) | 18.42 / 22.87 | 6.59 / 12.87 | 41.15 / 54.92 |
| graft_trace_calls (`GRAFT_NO_REFRESH=1`) | 1.53 / 3.06 | 0.37 / 1.15 | 8.15 / 23.99 |
| graft_file_api (`GRAFT_NO_REFRESH=1`) | 0.28 / 0.70 | 0.17 / 0.26 | 0.68 / 3.61 |
| ast-grep run, fresh process per query (30 per run) | 42.05 / 58.25 | 29.87 / 35.49 | 160.47 / 204.86 |
| graft server startup (spawn → initialize reply) | 201 ms | 208 ms | 211 ms |

- By default graft checks whether the index is stale before **every** query (`graph/refresh.ts`), and most warm latency is that check. `GRAFT_NO_REFRESH=1` turns it off and shows the raw lookup cost. The default row is what an agent actually gets. The ts-monorepo and ts-service copies have no `.git`, but rust-analyzer does, so the cost of the check may differ between them.
- Inputs: `symbols_<repo>.txt` holds 100 function/method names, each with at least 1 incoming `calls` edge in graft's graph, sampled with seed 42. `apifiles_<repo>.txt` holds 100 source files, sampled with replacement for ts-service. ast-grep uses the first 30 symbols.
- ast-grep query command: `ast-grep run -p '<SYM>($$$)' -l <ts|rs> --json=stream .`, run from the repo root. For ts-monorepo, `-l ts` skips .tsx files.
- graft_file_api returned `isError: true` for 17 of the 100 ts-monorepo files in each run (85 over 5 runs), always with "no definitions indexed for this file". Those files only export consts or routers. These are real responses, and they are counted in the latency. There were no other errors.
- Mean response size: trace_calls was 648 / 450 / 1437 bytes, file_api 541 / 1254 / 2957 bytes (ts-monorepo / ts-service / rust-analyzer).

## c) Peak RSS

| what | ts-monorepo | ts-service | rust-analyzer |
|---|---|---|---|
| graft build (median, `/usr/bin/time -l`) | 512 MB | 220 MB | 1071 MB |
| graft mcp server during query run, default (median) | 202 MB | 136 MB | 380 MB |
| graft mcp server, process tree peak sampled every 50 ms (max) | 210 MB | 142 MB | 387 MB |
| ast-grep kind scan (median) | 44 MB | 18 MB | 47 MB |
| ast-grep single query (max over runs) | 43 MB | 18 MB | 46 MB |

MB = 2^20 bytes. Exact byte counts are in `results.json`. Server RSS comes from `/usr/bin/time -l graft mcp` after the driver closes stdin, which makes the server exit cleanly.

## d) Tokens per read (tiktoken o200k_base)

Files are in `files_<repo>.txt`. For ts-monorepo and rust-analyzer they are 5 files evenly spaced across the 60th–90th size percentile of non-test source files. ts-service has only 11 such files, so its list is the 5 largest. `file_api` is the full tool output. That output includes graft's own `[graft] tokens saved …` banner line, which costs about 80 tokens per call. `no banner` is the same output with that line removed.

| repo | file (private repo paths withheld) | lines | raw | file_api | no banner |
|---|---|---|---|---|---|
| ts-monorepo | ts-monorepo file 1 | 46 | 337 | 137 | 58 |
| ts-monorepo | ts-monorepo file 2 | 57 | 634 | 109 | 30 |
| ts-monorepo | ts-monorepo file 3 | 76 | 878 | 247 | 168 |
| ts-monorepo | ts-monorepo file 4 | 101 | 1187 | 146 | 63 |
| ts-monorepo | ts-monorepo file 5 | 161 | 1852 | 362 | 279 |
| ts-service | ts-service file 1 | 87 | 833 | 632 | 553 |
| ts-service | ts-service file 2 | 111 | 1424 | 173 | 90 |
| ts-service | ts-service file 3 | 116 | 1568 | 427 | 346 |
| ts-service | ts-service file 4 | 238 | 2811 | 535 | 452 |
| ts-service | ts-service file 5 | 241 | 2815 | 978 | 895 |
| rust-analyzer | crates/ide-db/src/source_change.rs | 406 | 3251 | 1150 | 1067 |
| rust-analyzer | crates/ide-assists/src/handlers/generate_enum_projection_method.rs | 483 | 2786 | 499 | 416 |
| rust-analyzer | crates/hir-ty/src/lib.rs | 627 | 5544 | 1469 | 1384 |
| rust-analyzer | crates/ide/src/expand_macro.rs | 867 | 5410 | 804 | 721 |
| rust-analyzer | crates/ide-assists/src/handlers/expand_glob_import.rs | 1260 | 6186 | 1090 | 1007 |

| repo | raw total | file_api total | no-banner total | raw ÷ file_api |
|---|---|---|---|---|
| ts-monorepo | 4888 | 1001 | 598 | 4.88× |
| ts-service | 9451 | 2745 | 2336 | 3.44× |
| rust-analyzer | 23177 | 5012 | 4595 | 4.62× |

ast-grep has no file-summary feature, so it has no tokens-per-read number.

## Not measured / caveats

- graft 0.26.0 was not tested (see Versions).
- graft `--deep` (the LLM pass) and `--lsp` were not measured. Only the default, no-key build was.
- ast-grep TS queries cover one language per process, so `-l ts` excludes .tsx.
- Latencies are client-side round-trip times, measured with Node `performance.now()` in the driver. They include JSON-RPC serialization.

## Rerun

bench/baselines has no package.json and no JS dependencies (`mcp_bench.mjs` uses only Node built-ins), so `pnpm audit` does not apply. The scripts put `~/Library/pnpm/bin` on PATH themselves.

```
export REPO_TS_MONOREPO=/path/to/copy REPO_TS_SERVICE=/path/to/copy REPO_RUST_ANALYZER=/path/to/rust-analyzer   # unset = repo skipped
bash bench/baselines/run_all.sh 5     # index → select_inputs → mcp/astgrep/tokens → aggregate.py
```
Scripts: `bench_index.sh`, `select_inputs.py`, `mcp_bench.mjs`, `astgrep_query_bench.py`, `tokens_bench.py`, `count_tokens.py`, `aggregate.py`.
