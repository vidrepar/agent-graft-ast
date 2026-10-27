# agent-graft-ast

**A code-intelligence server for AI coding agents.** One Rust binary that indexes your repo in milliseconds, answers "where is this used?" in under a millisecond, and performs renames and signature changes itself, so the agent doesn't have to edit every call site by hand.

It speaks [MCP](https://modelcontextprotocol.io) over stdio, so it plugs into Claude Code, Cursor, Codex or any other MCP client. Runs on Linux and macOS. Languages: Rust, TypeScript, TSX, JavaScript, Python, Go, Java, C# (full support), and C, C++, Ruby, PHP, Kotlin, Swift, Bash, Lua, Scala (indexing and structural rewrite).

On real refactoring tasks, an agent using it finished in **half the time at about a third of the cost**, with the same pass rate ([HopCount](#hopcount-the-benchmark) below).

---

## Why

Watch an agent rename a function. It greps for the name, reads ten files, then makes twenty separate edits. Every one of those steps is a tool call, and every tool call is a full model round trip that re-sends the conversation. Most of the time and money goes on mechanical steps that need no intelligence.

Better search doesn't fix that. We measured it: giving the agent a perfect "here is every line that uses this symbol" tool saved nothing, because it still read and edited each site one call at a time. Removing the round trips is what saves time and money.

So this engine does three things:

1. **Does the mechanical edit itself.** `rename` and `change_signature` find every reference, rewrite the files and re-index them in one call. The agent's job becomes deciding *what* to change, not typing it 20 times.
2. **Hands back decisions, not raw text.** Each answer comes with an impact verdict (`impact="high"`: 12 callers in 6 files across 5 packages). When a name is ambiguous, the reply is a short list of options with a suggested pick, instead of a guess.
3. **Answers instantly.** Queries take well under a millisecond, so the engine never becomes the bottleneck.

## Where it comes from

Two tools inspired this one:

- **[ast-grep](https://ast-grep.github.io)** showed that tree-sitter makes fast, structural, multi-language code search practical. But it is built for humans at a terminal: one process per query, colored diffs, and no memory between calls.
- **[graft](https://www.npmjs.com/package/@nanonets/graft)** showed that an agent works better with a symbol graph (who calls what) served over MCP than with grep. But it is a Node.js server that takes seconds to build its index and tens of milliseconds to answer a query.

agent-graft-ast combines the two ideas into a single native process: tree-sitter parsing on every core, a call/import graph kept in memory and on disk, and an API designed for an agent's token budget rather than a human's terminal. On top of that it adds the part neither one has: tools that apply the edit.

## How much better

Measured on three repos (a 650-file TypeScript monorepo, a small TypeScript service, and rust-analyzer with 1,500 files), Apple M3 Pro. Full method and raw numbers: [`BENCHMARKS.md`](BENCHMARKS.md).

| | agent-graft-ast | graft | ast-grep |
|---|---|---|---|
| Index rust-analyzer from scratch | **157 ms** | 5.9 s | no index (230 ms per scan) |
| "Who calls X?" (warm, median) | **0.08 ms** | 48 ms | 160 ms (one process per query) |
| Memory while serving rust-analyzer | **148 MB** | 380 MB | n/a |
| Re-index after you save a file | **2–8 ms** | re-scans on each query | n/a |

And the number that matters most, from 6 real refactoring tasks run by Claude Code (`claude -p`, 3 runs each per setup):

| Agent setup | Passed | Time per task | Tool calls | Cost per task |
|---|---|---|---|---|
| **With agent-graft-ast** | 18/18 | **13 s** | **3.2** | **$0.020** |
| Same engine, edit tools turned off | 18/18 | 28 s | 15.2 | $0.060 |
| Built-in tools only | 18/18 | 25 s | 14.6 | $0.057 |

The middle row is the control: the same engine and search, minus the tools that apply edits. It is no faster than having no engine at all. Applying the edits is where the gain comes from.

Structural search and rewrite against ast-grep 0.42, on rust-analyzer (`$X.unwrap()` → `$X.expect(..)`, 1,549 sites in 321 files), through the MCP server with 1 s between calls, as an agent would:

| | agent-graft-ast | ast-grep |
|---|---|---|
| Search (median of 10 patterns) | **75 ms** | 185 ms |
| Rewrite | **~120 ms** | 263 ms |
| Search, then rewrite | **~155 ms** | 460 ms |
| Dry run, then apply | **~110 ms** | ~450 ms |

Both rewrites compile with zero errors, and every rewrite re-parses the files it wrote to prove the pattern is gone.

## Where it pays off

The gain grows with the number of sites one edit touches and the size of the repo. Measured with HopCount (`claude -p`, Sonnet; per-task numbers in [`bench/tasks/RESULTS.md`](bench/tasks/RESULTS.md)):

| task | built-in tools only | with agent-graft-ast |
|---|---|---|
| rename across 100 files of rust-analyzer (133 sites) | 156 s, 202 tool calls, $0.56 | **19 s, 4 tool calls, $0.033** |
| rename a function in rust-analyzer (7 files) | 30 s, 16.7 calls, $0.057 | **9 s, 2.0 calls, $0.019** |
| add a field to a struct and its 4 literals | 16 s, 9.7 calls, $0.034 | **7 s, 1.0 call, $0.008** |
| add a parameter to a helper used 7× in a small TS service | 19 s, 10 calls, $0.043 | 12 s, 4 calls, $0.020 |
| add a flag to a Rust function, then rewrite its body by hand | 33 s, 10.3 calls, $0.064 | 26 s, 7.0 calls, $0.046 |

Repo-wide mechanical edits (rename, add a parameter or field, move a symbol, rewrite a call pattern) are where it pays most. When most of the work is reasoning about one function body, the agent still reads and edits by hand and the saving is small. One-line fixes in a single file: not measured; expect little, as there are no round trips to remove.

## Install

**Prebuilt binary** (Linux x86_64/arm64, macOS Intel/Apple Silicon):

```bash
curl -fsSL https://raw.githubusercontent.com/vidrepar/agent-graft-ast/main/install.sh | sh
```

It installs to `~/.local/bin` (set `AGENT_GRAFT_INSTALL_DIR` to change that) and falls back to building from source when no prebuilt binary matches your system.

**With Cargo** (Rust 1.85+; needs a C compiler for tree-sitter):

```bash
cargo install --git https://github.com/vidrepar/agent-graft-ast --locked
```

**From a clone:**

```bash
git clone https://github.com/vidrepar/agent-graft-ast && cd agent-graft-ast
cargo build --release      # binary at target/release/agent-graft-ast
```

Check it works:

```bash
cd ~/code/your-repo
agent-graft-ast index      # writes .agent-graft/index.redb
agent-graft-ast find main  # lists symbols whose name contains "main"
```

Add `.agent-graft/` to your `.gitignore`. It is a cache; delete it at any time.

## Use it with your agent

**If you are an AI agent setting this up for your user**, run these from the root of the user's repo:

```bash
curl -fsSL https://raw.githubusercontent.com/vidrepar/agent-graft-ast/main/install.sh | sh   # installs to ~/.local/bin
agent-graft-ast init --dry-run   # show what will change
agent-graft-ast init             # index the repo and write the config below
```

Then tell the user to restart Claude Code in that repo and approve the `agent-graft-ast` server when asked. The edit tools (`rename`, `change_signature`, `add_field`, `move_symbol`, `rewrite`, `batch`) still ask before each use; if the user wants them to run without asking, run `agent-graft-ast init --allow-edits` (their own settings only, never the committed ones).

`init` writes, merging into existing files and keeping everything else in them (a rerun changes nothing):

| file | what |
|---|---|
| `.mcp.json` | the `agent-graft-ast` MCP server for this repo |
| `.claude/settings.json` (committed) | hooks: `UserPromptSubmit` (symbols the prompt names), `PostToolUse` on edits (files that depend on the edited file), `PreToolUse` on `Grep\|Glob\|Bash` (the search redirect). No permissions. |
| `.claude/settings.local.json` (per user) | `permissions.allow` for the read-only tools `find_symbol`, `context`, `read_file`; with `--allow-edits`, also the edit tools |
| `CLAUDE.md` | routing rules between `<!-- agent-graft-ast:start -->` and `<!-- agent-graft-ast:end -->`: edit tools for mechanical edits, `context` before changing a symbol, `find_symbol` before Grep, `read_file` level 2 for big files, whole-file reads whenever more context is needed |
| `.gitignore` | `.agent-graft/` (the index) and `.claude/settings.local.json` |

**The search redirect.** When the agent runs Grep, Glob or a plain `grep`/`rg`/`find` for a symbol the index defines (`get_profile`, `fn get_profile`, `class UserRepo`, `**/UserRepo*`), the hook answers it from the index instead: the call is denied with the definition and every reference, with Read windows, as the reason, so the agent gets it in the same turn. Text and regex searches, plain words, names defined in more than 3 places, and bare-name searches that want the matching lines (Grep content mode, grep without `-l`) run as usual. Read, Edit and Write are never touched. An answer takes ~23 ms on rust-analyzer (1,528 files; ~36 ms before the server has run once and saved its identifier index) and ~8 ms on a small TypeScript service; a pass-through ~5 ms. TS/JS answers use the syntax index (no tsserver start).

**Undo.** `agent-graft-ast init --no-redirect` removes the `PreToolUse` hook and the `CLAUDE.md` block and keeps the rest. To remove everything: delete the `agent-graft-ast` entries from `.mcp.json`, `.claude/settings.json` (hooks) and `.claude/settings.local.json` (allow rules), the marked block from `CLAUDE.md`, and the `.agent-graft/` directory.

**Claude Code by hand**, from inside the repo:

```bash
claude mcp add agent-graft-ast -- agent-graft-ast --root "$PWD" serve
```

**Any other MCP client** (Cursor, Codex, Windsurf, …), add a stdio server:

```json
{
  "mcpServers": {
    "agent-graft-ast": {
      "command": "agent-graft-ast",
      "args": ["--root", "/absolute/path/to/your/repo", "serve"]
    }
  }
}
```

The server builds or refreshes the index when it starts and watches the repo while it runs. On startup it tells the model how to use the tools, so no prompt is needed.

## Tools

| Tool | What the agent gets |
|---|---|
| `rename(target, new_name)` | Every reference renamed and written to disk, plus the files and lines that changed. Lists mentions left in docs and config (Markdown, YAML, JSON); `text: "replace"` updates those too. Asks first if the new name is already in use there. |
| `change_signature(target, add_param, call_arg)` | The parameter added to the definition and the argument added at every call. Lists references that aren't calls (callbacks, re-exports) for the agent to check. |
| `add_field(target, field, value)` | The field added to a Rust struct, TypeScript interface or Python dataclass, and to every literal or constructor call that builds it. |
| `move_symbol(target, to_file)` | A top-level TS/JS/Python declaration, or a Rust item (with its impls) within one crate, moved to another file, with every import and path fixed. |
| `rewrite(pattern, replacement)` | A structural search-and-replace across the repo, in ast-grep syntax (`$X` one node, `$$$ARGS` any run of siblings): `console.log($$$A)` → `logger.debug($$$A)`. Without `replacement` it lists the matches. Strings and comments never match. Sites where the replacement would drop code the pattern doesn't mention (a comment, an `async`, an `else`) are listed, not rewritten. |
| `batch(ops)` | Up to 20 tool calls in one round trip. |
| `check(paths)` | Compiler errors for the projects that own these files (`cargo check`, `tsc`, `py_compile`). Off by default, see below. |
| `context(target)` | The definition, plus every line in the repo that names it, grouped by file as exact `Read offset/limit` windows. |
| `find_symbol(query)` | Symbols matching a name, with file and line. |
| `read_file(path, level)` | A file at three detail levels: `0` full, `1` bodies folded, `2` public API only (3–10× fewer tokens). |

Every reply carries an `impact` verdict and a `<budget_receipt>` with the tokens it cost. A target can be `name`, `path:name`, or `path:name@line`. Every edit tool accepts `dry_run: true` to preview the change without writing anything.

**Compile checks are opt-in.** `cargo check` runs a crate's build scripts and proc macros, and `tsc` comes from the repo's own `node_modules`, so checking means running code from the repo. Start the server with `AGENT_GRAFT_ENABLE_CHECK=1` to turn it on for repos you trust. The `check` tool then appears, and `rename` and `change_signature` accept `check: true` to return compiler errors in the same reply.

**TypeScript/JavaScript edits are exact.** `rename`, `change_signature` and `context` resolve TS/JS names through `tsserver`, the TypeScript language service editors use, so they edit exactly the locations the compiler links to the symbol: a method on one class and not another class's method of the same name, interface methods with their implementations, overloads, re-exports, JSX, shorthand properties (`{ hello }` becomes `{ hello: hi }`). After the edit the reply lists the compiler errors the edited files gained (`<diagnostics new="0">`), so a broken edit shows up without running `tsc`. One `tsserver` runs per repo, started with the server and loaded while the index warms up; warm renames on a 650-file monorepo take about 30 ms. It needs Node.js and a TypeScript with `tsserver`: `agent-graft-ast init --typescript` installs TypeScript 6.0.3 with pnpm into `~/.cache/agent-graft-ast/typescript/` (TypeScript 7 is native and has no `tsserver`), or set `AGENT_GRAFT_TSSERVER=/path/to/tsserver.js`. The repo's own `node_modules/typescript` is used only with `AGENT_GRAFT_ENABLE_CHECK=1`, since it is code from the repo. Without `tsserver` the syntax engine answers and every TS/JS reply says `exact="false"` with how to turn exact mode on. `AGENT_GRAFT_TS_EXACT=0` turns it off; `AGENT_GRAFT_TS_DIAG_MS` caps the time spent on diagnostics (default 10000, 0 = off).

The same operations work from the shell, which is handy for scripts and for checking what the agent sees:

```bash
agent-graft-ast context getUser
agent-graft-ast rename load_config read_config --dry-run
agent-graft-ast change-signature truncate "suffix: string" '"…"'
agent-graft-ast read src/lib.rs --level 2
```

## How it works

1. **Index.** Walk the repo (respecting `.gitignore`) and parse every file with tree-sitter on all cores. Extract symbols, imports and call sites into a graph, and store it in an embedded [redb](https://www.redb.org) database.
2. **Serve.** Load the graph in a few milliseconds and answer queries from memory. A file watcher (kqueue on macOS, inotify on Linux) re-parses only the file you saved. While idle, the server also keeps syntax trees of recently changed files for `rewrite` (256 MB by default; set `AGENT_GRAFT_TREE_CACHE_MB`, `0` turns it off).
3. **Edit.** For `rename` and `change_signature`, re-parse the affected files and edit syntax nodes, not text. String literals are never touched. `obj.name` only counts when the target is a method, and local variables that shadow the name are left alone. TypeScript overloads, and a Rust trait method together with its impls, change as one unit. Files are written atomically and re-indexed right away. `rewrite` parses only the functions that can hold a match (the index knows where every function starts and ends) and falls back to the whole file whenever that isn't provably enough.

Guardrails: the engine refuses to read lockfiles, binaries, files that look like secrets (`.env`, keys, embedded tokens), and anything outside the repo root. It also won't write through symlinks.

## Limits

These are the cases where it gets things wrong, so you know where to check its work:

- **No type checker.** Names are resolved by syntax, imports and the call graph, plus receiver types the code states (`self`/`this`, `let x: T`, `T::new()`, `new C()`, typed parameters and fields, `x = C()` in Python), followed up `impl Trait`/`extends`/base classes. On rust-analyzer that links 35% more same-name method calls (hand-checked sample: 0 wrong). Receivers whose type is not written down, dynamic dispatch, duck typing and `any`-typed calls can still be missed. Ambiguous cases come back as a choice instead of a guess, and you should still run your compiler or tests after a big refactor.
- **Docs are listed, not renamed, by default.** `rename` reports mentions in Markdown, YAML and other text files; pass `text: "replace"` to update them too.
- **`change_signature` appends a parameter.** It doesn't reorder or remove parameters, and it refuses functions that end in a rest/variadic parameter.
- **`add_field` finds literals by syntax.** Rust struct literals inside macros, and TypeScript object literals with no type annotation, `as` or `satisfies`, are missed (the TypeScript ones are listed for the agent to check).
- **`move_symbol` for Rust stays inside one crate.** It refuses moves to another crate, `#[path]` modules, items whose code uses a `macro_rules!` macro of the old file, and items a `pub use a::*` re-export would silently lose. It widens visibility to `pub(crate)` where the new module needs it, so check that the result is what you want. For TS/JS and Python, namespace imports (`ns.sym`) are listed rather than rewritten.
- **Three tiers of languages.** Rust, TypeScript, TSX, JavaScript and Python get every tool. Go, Java and C# get indexing, `context`, `read_file`, callers, `rename` and `rewrite` (plus `change_signature` for Go); `add_field` and `move_symbol` refuse them. C, C++, Ruby, PHP, Kotlin, Swift, Bash, Lua and Scala get indexing (functions, types, methods, calls), `context`, `read_file`, callers and `rewrite`; `rename` refuses them. Java/C# calls to overloaded methods are not linked (no argument-count matching). In PHP and Bash, `$NAME` in an uppercase pattern is a metavariable, not a variable. Adding a language means a tree-sitter grammar plus an extractor (see `src/lang.rs` and `src/parse*.rs`).

## HopCount: the benchmark

**HopCount** measures how many hops (tool round trips) an agent needs to complete a real refactor, and what those hops cost. Each task is a plain-English request (rename a helper across a crate, add a required parameter and update every caller), run by `claude -p` on a throwaway copy of a real repo, and checked with a script. Three setups run side by side, so you can see where a gain comes from:

- `baseline`: built-in tools only
- `ctxonly`: the engine, with the edit tools turned off
- `treatment`: the full engine

```bash
bench/tasks/run.sh 3                                    # 3 runs per task per setup
ARMS="baseline treatment" bench/tasks/run.sh 1 <task>   # a subset
```

A task is an entry in [`bench/tasks/tasks.json`](bench/tasks/tasks.json) (a repo and a prompt) plus a check function in [`bench/tasks/bench.py`](bench/tasks/bench.py) that fails on the untouched code. Add your own repo and task, run it, and send a PR with the results. Spending is capped per run and per suite (`MAX_BUDGET_USD`, `SUITE_BUDGET_USD`).

## Contributing

Issues and PRs are welcome. Before you open a PR:

```bash
cargo test
scripts/audit.sh       # cargo audit: zero known vulnerabilities
```

Benchmarks are measured, never estimated: if you change a number in the docs, include the command that produced it.

## License

MIT OR Apache-2.0, at your option.
