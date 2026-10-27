//! MCP server over stdio (JSON-RPC 2.0, newline-delimited), on tokio.

use crate::engine::{esc, Engine, DEFAULT_CALL_BUDGET};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL: &str = "2025-06-18";

/// Returned with `initialize`: how to use the tools so whole files are never read.
pub const INSTRUCTIONS: &str = "This repo is indexed by agent-graft-ast. Let it do mechanical edits and decide from its verdicts: \
- Rename a symbol: call `rename(target, new_name)`. It edits every reference and writes the files; no Read or Edit needed. \
- Add a parameter to a function and its calls: call `change_signature(target, add_param, call_arg)`. Its reply shows the updated definition with line numbers, so do not call `context` for it; Edit the body from that reply if it must use the parameter. Several functions? One `change_signature` each, together in one `batch`. \
- Add a field to a struct/interface/dataclass and every literal building it: call `add_field(target, field, value)`. \
- Move a top-level function/class/const/type to another file (TS/JS/Python; Rust within one crate): call `move_symbol(target, to_file)`. It moves the code and fixes imports everywhere. \
- A repo-wide mechanical edit that is not a rename (wrap or swap call arguments, replace one API with another): call `rewrite(pattern, replacement)`; pattern is code with `$X` (one node) and `$$$ARGS` (any siblings). Without replacement it lists the matches. \
- Anything else: call `context(target)`. It returns the definition and every line naming it, per file, with Read windows \
`[Read offset=N limit=M]` (or `[Read whole file]` for small files). Start with those windows; read more, or whole files, whenever you need more context to be sure. \
- Every reply has impact=low|medium|high with the reason. If a reply has <choose>, A is the engine's guess (most callers); check it against the task before picking. \
If the `check` tool is listed, pass `check: true` to rename/change_signature (or call `check(paths)`) to get compiler errors in the same reply. \
Issue independent Reads and Edits in one turn. Several independent edits or lookups? Send them in one `batch` call. Use `find_symbol` if unsure of a name.";

/// Instructions without the edit tools (`AGENT_GRAFT_NO_REFACTOR=1`, for A/B benchmarks).
pub const INSTRUCTIONS_NO_REFACTOR: &str = "This repo is indexed by agent-graft-ast. For any change, call `context(target)` first. \
It returns the definition and every line naming it, per file, with Read windows `[Read offset=N limit=M]` \
(or `[Read whole file]` for small files). Start with those windows; read more, or whole files, whenever you need more context to be sure. \
Every reply has impact=low|medium|high with the reason. If a reply has <choose>, A is the engine's guess (most callers); check it against the task before picking. \
To find every place shaped like some code (e.g. `$X.unwrap()`, `console.log($$$ARGS)`), call `rewrite(pattern)` with no replacement. \
Issue independent Reads and Edits in one turn. Several independent edits or lookups? Send them in one `batch` call. Use `find_symbol` if unsure of a name.";

fn refactor_off() -> bool {
    std::env::var_os("AGENT_GRAFT_NO_REFACTOR").is_some_and(|v| v == "1")
}

/// Listed tools (others stay callable but are not advertised: every listed schema costs tokens on every turn).
pub fn tool_list() -> Value {
    let (no_refactor, no_check) = (refactor_off(), !crate::verify::check_enabled());
    let keep: Vec<Value> = all_tools()
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| match t["name"].as_str() {
            Some("rename" | "change_signature" | "add_field" | "move_symbol" | "rewrite") => !no_refactor,
            Some("check") => !no_check,
            _ => true,
        })
        .cloned()
        .collect();
    Value::Array(keep)
}

fn all_tools() -> Value {
    json!([
        {
            "name": "context",
            "description": "Start here for any change. Returns a symbol's code and every line in the repo where its name occurs, per file, with exact Read offset/limit windows (1 line around each). Read only those windows, then Edit.",
            "inputSchema": {"type": "object", "properties": {"target": {"type": "string", "description": "symbol name or path:name"}, "token_budget": {"type": "integer"}}, "required": ["target"]}
        },
        {
            "name": "rename",
            "description": "Rename a function/class/type/variable everywhere it is referenced and write the files (re-indexed). One call replaces Read+Edit on every file. Returns files and lines changed, an impact verdict, or a <choose> card if the name is ambiguous or the new name is taken.",
            "inputSchema": {"type": "object", "properties": {"target": {"type": "string", "description": "symbol name or path:name"}, "new_name": {"type": "string"}, "dry_run": {"type": "boolean"}, "force": {"type": "boolean"}, "check": {"type": "boolean", "description": "also compile the edited projects and return errors"}, "text": {"type": "string", "enum": ["report", "replace", "skip"], "description": "mentions in docs/config (md, yaml, json...): report (default), replace, or skip"}}, "required": ["target", "new_name"]}
        },
        {
            "name": "change_signature",
            "description": "Append a parameter to a function's definition and the matching argument to every call site, and write the files. Lists lines that name the function without calling it (callbacks) for you to check.",
            "inputSchema": {"type": "object", "properties": {"target": {"type": "string"}, "add_param": {"type": "string", "description": "parameter as written in the definition, e.g. 'suffix: string'"}, "call_arg": {"type": "string", "description": "argument to pass at every call, e.g. '\\\"…\\\"' or 'true'"}, "dry_run": {"type": "boolean"}, "check": {"type": "boolean", "description": "also compile the edited projects and return errors"}}, "required": ["target", "add_param", "call_arg"]}
        },
        {
            "name": "check",
            "description": "Compile the projects owning these files (cargo check, the project's tsc --noEmit, python py_compile) and return error lines. Use after edits.",
            "inputSchema": {"type": "object", "properties": {"paths": {"type": "array", "items": {"type": "string"}}}, "required": ["paths"]}
        },
        {
            "name": "add_field",
            "description": "Add a field to a Rust struct, TypeScript interface/object type or Python dataclass, and `name: value` to every literal that builds it (Rust struct expressions, TS literals typed as it, Python constructor calls), and write the files. Spread literals are left alone; other uses are listed for you to check.",
            "inputSchema": {"type": "object", "properties": {"target": {"type": "string", "description": "type name or path:name"}, "field": {"type": "string", "description": "field as declared, e.g. 'pub retries: u32', 'retries?: number', 'retries: int'"}, "value": {"type": "string", "description": "value written into every literal, e.g. '3' (not needed for optional TS fields)"}, "dry_run": {"type": "boolean"}}, "required": ["target", "field"]}
        },
        {
            "name": "move_symbol",
            "description": "Move a top-level function/class/const/type to another file (TS/JS/Python; Rust within one crate, with the impls of a moved type; created if missing) and fix imports: carries the imports it needs, rewrites every importer, re-export and Rust path, adds `mod` and widens Rust visibility where needed, and writes the files. Lists what it could not rewrite in <check>; refuses with the reason what it cannot be sure of.",
            "inputSchema": {"type": "object", "properties": {"target": {"type": "string", "description": "symbol name or path:name"}, "to_file": {"type": "string", "description": "repo-relative destination path"}, "dry_run": {"type": "boolean"}}, "required": ["target", "to_file"]}
        },
        {
            "name": "rewrite",
            "description": "Structural search and replace (ast-grep style) over every indexed file of the language, and write the files (re-indexed). pattern is code with metavariables: $X matches one node, $$$ARGS any run of siblings (arguments, statements); the same name twice must match the same text. replacement may use them: `console.log($$$A)` -> `logger.debug($$$A)`, `foo($A, $B)` -> `foo($B, $A)`. Strings and comments never match. Without replacement: lists the matches.",
            "inputSchema": {"type": "object", "properties": {"pattern": {"type": "string"}, "replacement": {"type": "string"}, "lang": {"type": "string", "enum": ["rust", "typescript", "javascript", "python", "go", "java", "csharp", "c", "cpp", "ruby", "php", "kotlin", "swift", "bash", "lua", "scala"], "description": "needed only when the pattern matches in several languages"}, "paths": {"type": "array", "items": {"type": "string"}, "description": "files or directories to limit it to"}, "dry_run": {"type": "boolean"}}, "required": ["pattern"]}
        },
        {
            "name": "find_symbol",
            "description": "Find symbol definitions by name substring (path:line and signature).",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}
        },
        {
            "name": "read_file",
            "description": "A file's outline (level 1) or public API (level 2) under a token budget, without reading the whole file.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}, "level": {"type": "integer"}}, "required": ["path"]}
        },
        {
            "name": "batch",
            "description": "Run up to 20 tool calls in order in one round trip; each reply is returned, a failing op does not stop the rest.",
            "inputSchema": {"type": "object", "properties": {"ops": {"type": "array", "maxItems": MAX_BATCH, "items": {"type": "object", "properties": {"tool": {"type": "string"}, "args": {"type": "object"}}, "required": ["tool"]}}}, "required": ["ops"]}
        }
    ])
}

fn arg_str<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(|v| v.as_str())
}

fn arg_bool(a: &Value, k: &str) -> bool {
    a.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn arg_usize(a: &Value, k: &str, d: usize) -> usize {
    a.get(k).and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(d)
}

/// Dispatch one tool call; Err = invalid arguments.
pub fn call_tool(e: &mut Engine, name: &str, a: &Value) -> Result<String, String> {
    let level = |d: u8| arg_usize(a, "level", d as usize).min(2) as u8;
    /* what an earlier edit left to re-index lands first, so this call sees it */
    e.reindex_deferred();
    let need = |k: &str| arg_str(a, k).ok_or_else(|| format!("missing required argument `{k}`"));
    Ok(match name {
        "context" => e.context(need("target")?, arg_usize(a, "token_budget", 4000)),
        "rewrite" => {
            let paths: Vec<String> = a.get("paths").and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
            let replacement = arg_str(a, "replacement");
            if replacement.is_some() && refactor_off() {
                return Err("rewrite with a replacement is disabled (AGENT_GRAFT_NO_REFACTOR=1); omit replacement to search".into());
            }
            e.rewrite(need("pattern")?, replacement, arg_str(a, "lang"), &paths, arg_bool(a, "dry_run"))
        }
        "rename" | "change_signature" | "add_field" | "move_symbol" if refactor_off() => return Err(format!("unknown tool {name}")),
        "rename" => e.rename_opts(need("target")?, need("new_name")?, arg_bool(a, "dry_run"), arg_bool(a, "force"), arg_str(a, "text").unwrap_or("report"), arg_bool(a, "check")),
        "change_signature" => {
            e.change_signature_and_check(need("target")?, need("add_param")?, need("call_arg")?, arg_bool(a, "dry_run"), arg_bool(a, "check"))
        }
        "check" => {
            let paths: Vec<String> = a.get("paths").and_then(|v| v.as_array()).ok_or("missing required argument `paths`")?.iter().filter_map(|v| v.as_str().map(String::from)).collect();
            e.check(&paths)
        }
        "add_field" => e.add_field(need("target")?, need("field")?, arg_str(a, "value").unwrap_or(""), arg_bool(a, "dry_run")),
        "move_symbol" => e.move_symbol_checked(need("target")?, need("to_file")?, arg_bool(a, "dry_run")),
        "read_file" => e.read_file(need("path")?, level(1), arg_usize(a, "token_budget", DEFAULT_CALL_BUDGET)),
        "read_symbol" => e.read_symbol(need("name")?, arg_str(a, "path"), level(0), arg_usize(a, "token_budget", DEFAULT_CALL_BUDGET)),
        "callers" => e.callers(need("name")?, arg_str(a, "path")),
        "find_symbol" => e.find(need("query")?, arg_usize(a, "limit", 20)),
        "search_text" => e.grep(need("pattern")?, arg_usize(a, "limit", 50)),
        "index_status" => e.status_xml(),
        "batch" => batch(e, a)?,
        other => return Err(format!("unknown tool `{other}`")),
    })
}

/// Most tool calls one `batch` runs.
const MAX_BATCH: usize = 20;

/// `batch`: run each op through `call_tool` in order; a failing op is reported and the rest still run.
fn batch(e: &mut Engine, a: &Value) -> Result<String, String> {
    let ops = a.get("ops").and_then(|v| v.as_array()).ok_or("missing required argument `ops` (array)")?;
    if ops.len() > MAX_BATCH {
        return Err(format!("batch takes at most {MAX_BATCH} ops, got {}", ops.len()));
    }
    let mut out = format!("<batch ops=\"{}\">\n", ops.len());
    for (i, op) in ops.iter().enumerate() {
        let tool = op.get("tool").and_then(|v| v.as_str()).unwrap_or("");
        let args = op.get("args").cloned().unwrap_or(json!({}));
        let reply = if tool == "batch" { Err("batch cannot be nested".to_string()) } else { call_tool(e, tool, &args) };
        match reply {
            Ok(text) => out.push_str(&text),
            Err(err) => out.push_str(&format!("<error op=\"{i}\" tool=\"{}\">{}</error>", esc(tool), esc(&err))),
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str("</batch>");
    Ok(out)
}

/// The engine, filled in by a background loader; tool calls wait until it is there.
pub struct Slot {
    engine: Mutex<Option<Engine>>,
    ready: Condvar,
}

impl Slot {
    pub fn empty() -> Slot {
        Slot { engine: Mutex::new(None), ready: Condvar::new() }
    }

    pub fn with_engine(e: Engine) -> Slot {
        Slot { engine: Mutex::new(Some(e)), ready: Condvar::new() }
    }

    pub fn fill(&self, e: Engine) {
        *self.engine.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
        self.ready.notify_all();
    }

    /// Take the engine out (shutdown): dropping it closes the index database cleanly.
    pub fn take(&self) -> Option<Engine> {
        self.engine.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Engine) -> R) -> R {
        let mut g = self.engine.lock().unwrap_or_else(|p| p.into_inner());
        while g.is_none() {
            g = self.ready.wait(g).unwrap_or_else(|p| p.into_inner());
        }
        f(g.as_mut().expect("engine loaded"))
    }

    /// `with`, but None instead of waiting when the engine is not loaded yet.
    pub fn try_with<R>(&self, f: impl FnOnce(&mut Engine) -> R) -> Option<R> {
        self.engine.lock().unwrap_or_else(|p| p.into_inner()).as_mut().map(f)
    }
}

/// Handle one JSON-RPC message; None for notifications.
pub fn handle(engine: &Slot, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = id?;
    let result = match method {
        "initialize" => {
            let v = msg.pointer("/params/protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL);
            Ok(json!({
                "protocolVersion": v,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "agent-graft-ast", "version": env!("CARGO_PKG_VERSION")},
                "instructions": if refactor_off() { INSTRUCTIONS_NO_REFACTOR } else { INSTRUCTIONS }
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_list()})),
        "tools/call" => {
            let name = msg.pointer("/params/name").and_then(|v| v.as_str()).unwrap_or("");
            let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
            let run = |e: &mut Engine| {
                /* the idle parser pauses while a call runs */
                e.trees.busy.store(true, std::sync::atomic::Ordering::Release);
                let r = call_tool(e, name, &args);
                e.trees.busy.store(false, std::sync::atomic::Ordering::Release);
                r
            };
            match engine.with(run) {
                Ok(text) => Ok(json!({"content": [{"type": "text", "text": text}], "isError": false})),
                Err(err) => Ok(json!({"content": [{"type": "text", "text": err}], "isError": true})),
            }
        }
        _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

/// Start loading the index in the background, then serve stdio at once: `initialize` never waits on the index.
pub async fn serve(root: PathBuf, db: Option<PathBuf>) -> anyhow::Result<()> {
    crate::tokens::start_loading();
    let slot = Arc::new(Slot::empty());
    {
        let slot = slot.clone();
        std::thread::spawn(move || {
            /* 1. serve the persisted index as is (no walk, no stat), or build one if there is none */
            let mut e = match Engine::open_fast(&root, db.as_deref()) {
                Ok(Some(e)) => e,
                _ => match Engine::open(&root, db.as_deref()) {
                    Ok((e, _, _)) => e,
                    Err(_) => return,
                },
            };
            let root = e.root.clone();
            e.defer_reindex = true;
            slot.fill(e);
            let snapshot = slot.with(|e| e.stat_snapshot());
            let indexed: Vec<String> = snapshot.keys().cloned().collect();
            {
                let slot = slot.clone();
                std::thread::spawn(move || warm(&slot));
            }
            /* 2. catch up on edits made while no server ran */
            let stale = Engine::stale_paths(&root, &snapshot);
            if !stale.is_empty() {
                slot.with(|e| {
                    for r in &stale {
                        e.refresh(r);
                    }
                });
            }
            /* 3. the call-site index (deferred by the graph restore) is only needed by edits: build it now, off the first query */
            slot.with(|e| e.graph.ensure_calls_index(&e.files, None));
            crate::watch::run(root, slot, indexed);
        });
    }
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => {
                let is_call = msg.get("method").and_then(|m| m.as_str()) == Some("tools/call");
                if is_call {
                    let slot = slot.clone();
                    tokio::task::spawn_blocking(move || handle(&slot, &msg)).await?
                } else {
                    handle(&slot, &msg)
                }
            }
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        if let Some(r) = reply {
            let mut s = serde_json::to_string(&r)?;
            s.push('\n');
            out.write_all(s.as_bytes()).await?;
            out.flush().await?;
        }
        /* work that can wait for the reply (re-index, checking a dry run's plan) runs while the model reads it */
        let s = slot.clone();
        tokio::task::spawn_blocking(move || {
            let _ = s.try_with(|e| e.after_reply());
        });
    }
    /* stdin closed: close redb cleanly, or the next start pays for crash recovery */
    drop(slot.take());
    Ok(())
}

/// Background warm-up without holding the engine: summaries are computed from small batches of copied
/// records, so queries never wait behind it.
fn warm(slot: &Slot) {
    crate::tokens::warm();
    let (root, store, cache, sigs, warmed, n) =
        slot.with(|e| (e.root.clone(), e.store.clone(), e.cache.clone(), e.sigs.clone(), e.warmed.clone(), e.files.len()));
    let mut persisted = store.load_summaries().unwrap_or_default();
    for start in (0..n).step_by(WARM_BATCH) {
        let batch: Vec<crate::index::FileRecord> = slot.with(|e| e.files[start..(start + WARM_BATCH).min(e.files.len())].to_vec());
        /* save each batch's new summaries right away: nothing accumulates */
        let fresh = crate::summary::warm_batch(&root, &batch, &mut persisted, &cache);
        let _ = store.save_summaries(&fresh);
    }
    drop(persisted);
    sigs.get_or_init(|| crate::gitsig::scan(&root, crate::engine::GIT_COMMITS_SCANNED));
    warmed.store(true, std::sync::atomic::Ordering::Release);
    /* load the TS/JS projects into tsserver now, so the first exact edit does not wait for it */
    let (ts, ts_projects) = slot.with(|e| (e.ts.clone(), e.ts_projects()));
    if !ts_projects.is_empty() {
        let _ = ts.prewarm(&ts_projects);
    }
    /* then keep syntax trees for structural search, most recently changed files first */
    let (trees, mut files) =
        slot.with(|e| (e.trees.clone(), e.files.iter().filter(|f| f.size > 0).map(|f| (f.path.clone(), f.lang, f.hash, f.mtime_ns)).collect::<Vec<_>>()));
    files.sort_by_key(|f| std::cmp::Reverse(f.3));
    let files: Vec<(String, crate::lang::Lang, u64)> = files.into_iter().map(|f| (f.0, f.1, f.2)).collect();
    trees.preparse(&root, &files);
}

/// Records copied per lock while warming.
const WARM_BATCH: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> (tempfile::TempDir, Slot) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.py"), "def hello():\n    return world()\n\ndef world():\n    return 1\n").unwrap();
        let (e, _, _) = Engine::open(d.path(), Some(&d.path().join("i.redb"))).unwrap();
        (d, Slot::with_engine(e))
    }

    #[test]
    fn initialize_list_and_call() {
        let (_d, e) = engine();
        let init = handle(&e, &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(handle(&e, &json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        let list = handle(&e, &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 10);
        assert!(init["result"]["instructions"].as_str().unwrap().contains("rename(target, new_name)"));
        let call = handle(&e, &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"callers","arguments":{"name":"world"}}})).unwrap();
        let text = call["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("at=\"hello:2\"") && text.contains("<budget_receipt"), "{text}");
        let bad = handle(&e, &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_file","arguments":{}}})).unwrap();
        assert_eq!(bad["result"]["isError"], true);
        let unknown = handle(&e, &json!({"jsonrpc":"2.0","id":5,"method":"nope"})).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn batch_runs_every_op_and_reports_failures() {
        let (_d, s) = engine();
        let ops = json!({"ops": [
            {"tool": "callers", "args": {"name": "world"}},
            {"tool": "read_file", "args": {}},
            {"tool": "find_symbol", "args": {"query": "hello"}}
        ]});
        let out = s.with(|e| call_tool(e, "batch", &ops)).unwrap();
        assert!(out.starts_with("<batch ops=\"3\">") && out.ends_with("</batch>"), "{out}");
        assert!(out.contains("at=\"hello:2\""), "{out}");
        assert!(out.contains("<error op=\"1\" tool=\"read_file\">missing required argument `path`</error>"), "{out}");
        assert!(out.contains("<match path=\"a.py\" line=\"1\""), "{out}");
    }

    #[test]
    fn batch_rejects_nesting_and_oversize() {
        let (_d, s) = engine();
        let nested = json!({"ops": [{"tool": "batch", "args": {"ops": []}}]});
        let out = s.with(|e| call_tool(e, "batch", &nested)).unwrap();
        assert!(out.contains("batch cannot be nested"), "{out}");
        let many = json!({"ops": vec![json!({"tool": "index_status"}); 21]});
        assert!(s.with(|e| call_tool(e, "batch", &many)).is_err());
    }
}
