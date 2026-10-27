# Benchmarks: agent-graft-ast vs ast-grep + graft MCP

Measured 2026-10-27 on an Apple M3 Pro (6 performance + 6 efficiency cores), macOS 26.2. Release build (`lto = "thin"`).
Tokens are counted with the o200k_base BPE everywhere (engine: `bpe-openai`, identical counts to tiktoken on all 2,198 benchmark files; baseline: Python `tiktoken`).
Baseline tools: ast-grep 0.42.1 and graft (`@nanonets/graft`) 0.25.0 installed with `pnpm add -g`, node v26.8.2. Baseline details: [`bench/baselines/RESULTS.md`](bench/baselines/RESULTS.md). Task suite details: [`bench/tasks/RESULTS.md`](bench/tasks/RESULTS.md).

## Speed targets

All end to end over stdio unless noted; medians of 5 runs; repos ts-monorepo / ts-service / rust-analyzer. Final run at load average ~3.

| Target | Measured | Result |
|---|---|---|
| Every query and read, p99 < 1 ms (warm) | Median p99: `callers` 0.13 / 0.12 / 0.23 ms, `find_symbol` 0.18 / 0.18 / 0.50, `context` 0.66 / 0.23 / 0.79, `read_file` L1 0.14 / 0.16 / 0.33, L2 0.10 / 0.12 / 0.24. Worst single run: 0.68 / 0.24 / **0.79** ms. | **Pass** on every run (75 of 75 run × tool p99 values under 1 ms) |
| Re-index after a one-file edit < 10 ms | Engine patch, p50 / p99: 1.8 / 3.7, 1.8 / 3.3, 4.8 / 7.9 ms. Edit to visible over stdio (write the file, poll `find_symbol` until it sees the new function), p50 / p99: 2.8 / 5.6, 2.9 / 5.2, 7.9 / **13.4** ms. | **Pass** for the engine; end to end **fail** at rust-analyzer p99 |
| Cold index of rust-analyzer < 150 ms | **157 ms** in the final run (169–176 ms in earlier runs), from 285 ms at the start of this phase. ts-monorepo 25 ms, ts-service 24 ms. | **Fail, accepted:** tree-sitter's own parsing takes ~140 ms of it, so 150 ms is at the floor for full parsing |
| Server startup with an existing index < 20 ms | Spawn → `initialize` reply: 6.1 / 7.0 / 6.2 ms. Spawn → first real answer (a `callers` graph query): 8.1 / 8.2 / **17.1** ms. | **Pass** |

What changed:

- **Cold index (285 → 169 ms).** samply showed only ~5 of 12 cores busy: rayon handed out coarse chunks of a size-sorted file list. One file per task fixed it (−100 ms). One Aho-Corasick pass replaced 8 secret-scan passes per file (13% of CPU). Small files are `read()`, not memory-mapped. The walker skips subtrees that hold no definitions or calls (macro token trees, string literals, parameter lists, attributes; extraction output identical: 35,695 symbols, 134,961 call sites). A new index file is created while parsing runs, records commit while the graph builds, the call-site index only edits need is deferred, and commits use `fsync` rather than macOS `F_FULLFSYNC` (the index is a rebuildable cache). Tiktoken was never on this path and xxh3 hashing is under 1%. What remains: tree-sitter's own parsing is ~70% of ~1.5 s CPU spread over 12 cores (~140 ms wall), plus a ~14 ms graph build and ~3 ms of commit. Parsing files as the walk finds them and tuning the C build for Apple Silicon (`-mcpu=apple-m1`) gave no measurable gain.
- **Re-index.** kqueue watches every indexed file (FSEvents delivers ~10 ms after a write even at zero latency); FSEvents still catches new files. A file seen empty (the truncate step of a save) waits up to 50 ms for its write instead of being indexed twice. The graph is patched in place; a query sees the patched state as soon as the patch releases the engine lock.
- **Startup.** `initialize` is answered before the index loads. The graph is restored from what the last index run resolved (persisted, bounds-checked, rebuilt if implausible) instead of re-resolving 135k calls. Warm-up runs without holding the engine. The tokenizer loads on its own thread, so a receipt written in the first ~50 ms says `estimated="true"`; every first answer above was such a receipt. An unclean redb shutdown made each next start pay ~15 ms of recovery, so the server closes it on exit.
- **`context` p99.** Hub symbols have hundreds of callers (`filter_map`: 561): callers are now grouped with a map and their path prefix computed once, and token counting is ~1.7× faster after the switch to `bpe-openai`.

## Verdict per claim (spec section 4)

| Spec claim | Claimed | Measured (ts-monorepo / ts-service / rust-analyzer) | Verdict |
|---|---|---|---|
| Token payload / read | 80,000 → 8,500 tokens, **9.4× smaller** | Raw ÷ full `read_file` reply (code + 1-hop graph + directives + receipt): level 2 **4.9× / 3.6× / 7.9×**, level 1 **2.2× / 2.2× / 2.7×**. Graft `file_api`: 4.9× / 3.4× / 4.6×. | **Not confirmed.** Best full reply 7.9× misses 9.4× by **1.2×**; ts-monorepo by 1.9×, ts-service by 2.6×. |
| Runtime RAM | **~10× lower** | Indexing peak RSS 33 / 28 / 104 MB vs graft build 512 / 220 / 1,071 MB: **16× / 7.9× / 10×**. MCP server 107 / 96 / 148 MB vs graft server 202 / 136 / 380 MB: **1.9× / 1.4× / 2.6×**. | **Partly confirmed** (indexing on 2 of 3; server no) |
| Cold indexing | 180–320 ms vs 3.5–8.0 s, **~20× faster** | 22 / 24 / 157 ms vs graft 4.55 / 0.81 / 5.94 s: **207× / 34× / 38× faster** | **Confirmed** |
| Warm graph query | < 1.5 ms vs 120–350 ms, **~100× faster** | `callers` p50 / p99: 0.07/0.15, 0.06/0.11, 0.08/0.27 ms. Graft `trace_calls` p50 / p99: 19.9/28.3, 6.3/8.1, 48.0/81.5 ms. p50 ratio **302× / 113× / 571×**. | **Confirmed.** Graft took 6–81 ms, not the claimed 120–350 ms. |
| LLM prefill latency | 2.4 s → 0.25 s / turn | `claude -p` does not report per-turn prefill. Input tokens per task (the proxy): **−20%** with the edit tools, +21% with search alone. | **Not measured** directly |
| Task completion time | 75 s → 15 s (5×) | HopCount, 6 tasks × 3 runs: 25 s → **13 s (1.9×)** with the edit tools; search alone: 25 s → 28 s | **Partly confirmed** (1.9×, not 5×) |
| API cost / task | $3.60 → $0.23 (−94%) | Same runs: $0.057 → **$0.020 (−65%)** | **Partly confirmed** (−65%, not −94%) |

## HopCount: task-level benchmark (claude -p)

HopCount counts the hops (tool round trips) an agent needs to land a real refactor, and what they cost. 5 tasks across the 3 repos: add a required parameter and update every caller (ts-service, ts-monorepo), rename a function across a package or crate (ts-monorepo, rust-analyzer `find_struct_impl`), and add a flag to a Rust function and its 7 call sites (rust-analyzer `invert_boolean_expression`). Each run uses a reset throwaway copy of the repo, model `claude-sonnet-5-5`, no user settings, hooks, plugins or other MCP servers, and `--permission-mode acceptEdits` with an allowlist (Read, Edit, Write, Grep, Glob, plus the engine's MCP tools; no Bash). A scripted check, which fails on the untouched code, decides pass or fail.

Three setups, so each gain can be attributed:

- **baseline**: built-in tools only
- **ctxonly**: the engine with `rename` and `change_signature` hidden (search, `context` edit windows, `read_file`)
- **treatment**: the full engine

### Result (54 runs, 3 per task per setup, 54/54 passed)

6 tasks: the 5 above plus adding a field to rust-analyzer's `GotoImplementationConfig` and its 4 struct literals. Mean per task:

| metric | baseline | ctxonly | treatment |
|---|---|---|---|
| pass rate | 18/18 | 18/18 | 18/18 |
| wall time | 25 s | 28 s (+11%) | **13 s (−48%)** |
| turns | 15.6 | 16.2 (+4%) | **4.2 (−73%)** |
| tool calls | 14.6 | 15.2 (+5%) | **3.2 (−78%)** |
| input tokens | 36.2k | 43.7k (+21%) | **28.9k (−20%)** |
| output tokens | 2,920 | 2,965 (+2%) | **830 (−72%)** |
| cost | $0.057 | $0.060 (+6%) | **$0.020 (−65%)** |

The add-field task alone: 16 s → **7 s**, 9.7 → **1.0** tool calls, $0.034 → **$0.008**, using `add_field`.

The previous run (45 runs, 5 tasks, before `add_field`, `move_symbol`, `batch` and doc-mention reports existed) gave the same picture: treatment −51% time, −72% cost; ctxonly no faster than baseline.

### One huge edit: a 133-site rename

Rename rust-analyzer's `SourceChangeBuilder::add_file_edits` to `push_file_edits`: 134 lines in 100 files. Two runs per setup, each result then compiled (`cargo +nightly check` on ide-db, ide, ide-assists, ide-completion, ide-diagnostics; a deliberately broken rename gives 132 errors, so the check is real).

| setup | wall time | tool calls | cost | compiler errors |
|---|---|---|---|---|
| treatment | **26 s, 11 s** | **4, 4** | **$0.036, $0.030** | **0** |
| baseline | 158 s, 154 s | 202, 202 | $0.58, $0.54 | 0 |

Both setups got it right; the engine did it 6–14× faster and about 16× cheaper. The engine's rename itself takes 1.3 s on a cold start, index build included.

### Tightening loop (treatment only, 18 runs each, 18/18 passed every round)

Each round read the transcripts, found the hop the agent still made, removed its cause, and re-measured:

| round | change | wall time | tool calls | cost |
|---|---|---|---|---|
| 0 | the 54-run result above | 13 s | 3.2 | $0.020 |
| 1 | `rename` re-scans and states nothing is left; `change_signature` echoes the new definition | 15 s | 2.7 | $0.014 |
| 2 | proof wording matches what rename does; definition echoed in Read's `N<tab>line` format (the agent had copied wrong indentation) | 11 s | 2.6 | $0.013 |
| 3 | instructions: don't call `context` next to `change_signature`; batch several signature changes | 9 s | 2.2 | $0.016 |
| 4 | `change_signature` re-parses and proves every call has the new argument | 9 s | 2.5 | $0.013 |

Renames and `add_field` now take 1 call. Adding a parameter takes 2 (the tool, then the agent's own edit of the body). The floor for this task set is about 1.7 calls per task; what remains is the reasoning work itself.

Per-task numbers: [`bench/tasks/RESULTS.md`](bench/tasks/RESULTS.md).

What this shows:

- **Search alone doesn't save hops.** `ctxonly` finds every site in one call but is no faster than the baseline: the agent still makes one Read and one Edit per site, and each tool call is a model round trip.
- **Applying the edit does.** In the treatment, one `rename` or `change_signature` call replaces 10–20 Reads and Edits. The agent still checks the result (a Grep afterwards; it fixed Markdown mentions the code-only rename leaves alone) and edits the function body by hand when it needs to use the new parameter.
- **The model is not boxed in.** A rerun of the treatment (15 runs) with instructions that let the model read whole files whenever it wants, and treat the suggested option on a choice card as a guess to check, gave the same result: 15/15 passed, 13 s and $0.020 per task.

### Refactors checked by the compiler

The same edit tools, applied directly (no agent) to copies of the repos, then type-checked (`tsc` via each repo's typecheck script; `cargo check -p ide-assists -p ide`):

| repo | refactor | files / sites | new compiler errors |
|---|---|---|---|
| ts-service | add a parameter to a helper used 7×; rename it | 2 / 8, 2 / 10 | 0, 0 |
| ts-monorepo | rename a helper across 5 packages; add a parameter | 8 / 20, 5 / 6 | 0, 0 |
| ts-monorepo | rename a React component used in 23 files | 23 / 44 | 0 |
| ts-monorepo | rename `get` (3 definitions: choice card, picked A) | 3 / 4 | 0 |
| rust-analyzer | 2 renames, 2 signature changes in ide-assists | 5–7 / 7–16 | 0 |

One rename was refused rather than applied: renaming the ts-monorepo `cn` helper to `classNames` would have collided with an existing `classNames` prop in one component, so the tool asked first. Two earlier bugs this run found (that collision, and calls through `import * as ns`) are fixed and covered by tests.

### Earlier runs (for the record)

| version | runs | wall time | input tokens | cost |
|---|---|---|---|---|
| edit windows only (no small-file rule) | 3 per arm | −12% | −14% | −11% |
| `context` with caller/callee/sibling lists, 7 tools | 3 per arm | +2% | +36% | +11% |
| same, first single run | 1 per arm | −15% | +4% | −4% |
| same, before the permission lockdown | 1 per arm | +86% | +121% | +72% |

## Repos

| repo | files indexed | symbols | call + import edges | source |
|---|---|---|---|---|
| ts-monorepo (private TypeScript monorepo) | 652 | 1,783 | 4,316 | private |
| ts-service (private small TypeScript service) | 18 | 2,102 | 362 | private (working tree; it changed since the baseline run) |
| rust-analyzer (Rust) | 1,527 | 35,695 | 61,632 | `git clone --depth 1` @ 6ee6a51 |

Graft ran on rsync copies (657 / 14 / 1,528 files). Each call links to at most one target (or none when ambiguous), so rust-analyzer's graph went from 403k edges to 61.6k.

## 1. Cold indexing (no index on disk, median of 5)

| tool | ts-monorepo | ts-service | rust-analyzer |
|---|---|---|---|
| **agent-graft-ast** `index --fresh` | **22 ms** | **24 ms** | **157 ms** |
| graft `build --no-reuse` | 4,550 ms | 810 ms | 5,940 ms |
| ast-grep full scan (no index, finds all functions) | 70 ms | 40 ms | 230 ms |

Covers discover + parse + graph + persisted records and resolved graph. The OS file cache was warm.

## 2. Warm query latency (stdio round trip, 100 calls × 5 runs, median of per-run p50 / p99)

| tool | ts-monorepo | ts-service | rust-analyzer |
|---|---|---|---|
| **ours `callers`** | **0.07 / 0.15 ms** | **0.06 / 0.11 ms** | **0.08 / 0.27 ms** |
| ours `find_symbol` | 0.06 / 0.22 ms | 0.06 / 0.19 ms | 0.26 / 0.52 ms |
| ours `context` (edit sites) | 0.14 / 0.66 ms | 0.09 / 0.23 ms | 0.21 / 0.79 ms |
| ours `read_file` level 1 | 0.08 / 0.17 ms | 0.09 / 0.15 ms | 0.15 / 0.35 ms |
| ours `read_file` level 2 | 0.07 / 0.13 ms | 0.08 / 0.12 ms | 0.10 / 0.26 ms |
| graft `trace_calls` (default) | 19.93 / 28.29 ms | 6.34 / 8.14 ms | 47.97 / 81.47 ms |
| graft `file_api` (default) | 18.42 / 22.87 ms | 6.59 / 12.87 ms | 41.15 / 54.92 ms |
| graft `trace_calls` (`GRAFT_NO_REFRESH=1`) | 1.53 / 3.06 ms | 0.37 / 1.15 ms | 8.15 / 23.99 ms |
| ast-grep, one process per query | 42.05 / 58.25 ms | 29.87 / 35.49 ms | 160.47 / 204.86 ms |

Reads serve folded text and token counts from a per-file cache keyed by content hash, built in the background after startup. Timed queries start once the server reports `warm="true"` (51–131 ms after spawn).

## 3. Peak RSS (`/usr/bin/time -l`, median)

| process | ts-monorepo | ts-service | rust-analyzer |
|---|---|---|---|
| **ours, cold index** | **33 MB** | **28 MB** | **104 MB** |
| graft build | 512 MB | 220 MB | 1,071 MB |
| **ours, MCP server during query run** | **107 MB** | **96 MB** | **148 MB** |
| graft MCP server during query run | 202 MB | 136 MB | 380 MB |

The server holds the o200k tokenizer tables (exact receipts; `bpe-openai`'s faster counter costs ~25 MB more than tiktoken's), the summary cache (including every identifier's line numbers as varint deltas, so `context` never reads a file) and the git-signature scan; the indexer holds none of them. Server memory on rust-analyzer went 206 → 148 MB this round: compact line index, summaries moved instead of copied during warm-up, and redb's page cache capped at 8 MB. The ~126 MB target was missed by ~22 MB, about the tokenizer's extra tables.

## 4. Tokens per read (o200k_base, 5 files per repo, totals)

| repo | raw file | ours: L1 reply | ours: L2 reply | ours: L2 code only | graft `file_api` (without its banner) |
|---|---|---|---|---|---|
| ts-monorepo | 4,888 | 2,252 (2.2×) | **988 (4.9×)** | 178 (27×) | 1,001 (4.9×) / 598 (8.2×) |
| ts-service | 9,661 | 4,468 (2.2×) | **2,683 (3.6×)** | 1,400 (6.9×) | 2,745 (3.4×) / 2,336 (4.0×) |
| rust-analyzer | 23,177 | 8,569 (2.7×) | **2,920 (7.9×)** | 1,727 (13.4×) | 5,012 (4.6×) / 4,595 (5.0×) |

ts-service's files grew since the baseline run (raw 9,451 then, 9,661 now). With graft's ~80-token banner stripped, we are smaller on rust-analyzer only: our reply also carries the 1-hop graph and directives, which graft serves through a separate `trace_calls` call.

## 5. Structural rewrite vs ast-grep 0.42

rust-analyzer, `$X.unwrap()` -> `$X.expect("checked")`: 1,549 sites in 321 files (ast-grep finds 1,441: it does not
look inside macro bodies). Our calls go through the MCP server, ast-grep runs as a process; both sleep 1 s before
each call, as an agent's calls are spaced, so the CPU has idled down for both. Medians of 3.

| Flow | agent-graft-ast | ast-grep |
|---|---|---|
| search | 75 ms (median of 10 patterns) | 185 ms |
| rewrite alone | ~120 ms | 263 ms |
| search, then rewrite | ~155 ms | 460 ms |
| dry run, then apply | ~110 ms | ~450 ms (preview + apply) |

Both rewrites compile (`cargo +nightly check --workspace --tests`: 0 errors). Where the time went, in order of gain:
parsing only the functions that can hold a match (the index has every function's span; full parse when that is not
provably enough), reusing the scan's trees from a search or dry run, re-parsing only the edited regions to verify,
writing files in parallel, and moving re-indexing after the reply. Match counts equal ast-grep's on 14 of 16 TS and
19 of 20 Python patterns; the differences are matches ast-grep misses (imports inside try blocks, a comment between
operands). Reproduce: `python3 -I bench/rewrite/flows.py target/release/agent-graft-ast <ra>` and
`bench/rewrite/ast_grep.sh <ra>`.

## Caveats

- Load average was 3–11 during the various runs (other agents, browser, the task suite); the final engine run started at 3.2. The task suite ran while engine changes landed (the treatment arm used the binary current at each run's start; tool behaviour was unchanged).
- graft 0.25.0 was tested, not 0.26.0 (a local install policy blocks packages under 24 h old).
- Call resolution is name-based with receiver and owner hints, never type-aware; ambiguous calls stay unresolved.
- The index database is a cache: commits use `fsync`, not `F_FULLFSYNC`, so a power cut can lose the last commit (redb's checksums detect it; the engine re-indexes).

## Reproduce

```bash
cargo build --release
bash bench/baselines/run_all.sh 5          # graft + ast-grep
bash bench/engine/run.sh                   # ours: cold index, startup, warm queries, edit->visible
python3 -I bench/engine/aggregate.py       # medians -> bench/engine/results.json
ARMS="baseline ctxonly treatment" SUITE_BUDGET_USD=10 bash bench/tasks/run.sh 3   # HopCount (claude -p; ~$0.01-0.10 per run)
```
