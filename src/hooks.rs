//! Claude Code hooks: push index context to the agent instead of waiting for it to ask.
//!
//! `hook prompt` (UserPromptSubmit) lists the symbols a prompt names; `hook post-edit` (PostToolUse on
//! Edit/Write/MultiEdit) lists the files that depend on the file just edited. Both read the hook JSON on
//! stdin, print `{"hookSpecificOutput": {...additionalContext}}` or nothing, and never fail loudly.

use crate::engine::{default_db, Engine};
use crate::parse::SymKind;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Lines of symbol pointers in the prompt hook.
const MAX_PROMPT_LINES: usize = 8;
/// Definitions shown per matched name.
const MAX_DEFS_PER_NAME: usize = 2;
/// A plain English word that names more definitions than this is too generic to point at.
const MAX_DEFS_PLAIN_WORD: usize = 3;
/// Dependent files listed by the post-edit hook.
const MAX_DEPENDENTS: usize = 10;
/// Symbol names listed per dependent file.
const MAX_NAMES: usize = 4;

/// Plain words of 4+ letters that are symbol names in most repos but carry no pointer in a prompt.
const STOPWORDS: &[&str] = &[
    "this", "that", "with", "from", "into", "have", "make", "when", "then", "than", "them", "they", "what", "which", "where", "there",
    "these", "those", "also", "just", "only", "some", "more", "most", "such", "each", "every", "both", "same", "other", "after",
    "before", "should", "would", "could", "must", "will", "does", "done", "need", "needs", "want", "like", "keep", "sure", "please",
    "check", "test", "tests", "file", "files", "code", "name", "names", "call", "calls", "type", "types", "list", "find", "read",
    "write", "update", "change", "rename", "field", "fields", "param", "parameter", "function", "method", "struct", "class", "value",
    "data", "item", "items", "line", "lines", "path", "text", "string", "error", "errors", "result", "run", "build", "main", "self",
    "default", "new", "get", "set", "add", "remove", "delete", "fix", "bug", "work", "works", "used", "uses", "using", "here", "about",
    "again", "below", "above", "first", "last", "next", "good", "fine", "true", "false", "none", "null", "some", "into", "over",
];

/// Repo root + index path for a hook: an explicit `--db` wins, else the nearest ancestor of `start`
/// holding `.agent-graft/index.redb`. None means "no index yet": the hook stays silent.
pub fn locate(start: &Path, explicit: Option<(PathBuf, PathBuf)>) -> Option<(PathBuf, PathBuf)> {
    if let Some((root, db)) = explicit {
        return db.exists().then_some((root, db));
    }
    start.ancestors().find(|d| default_db(d).exists()).map(|d| (d.to_path_buf(), default_db(d)))
}

fn wrap(event: &str, text: String) -> String {
    json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}}).to_string()
}

fn start_dir(input: &Value, key_path: Option<&str>) -> PathBuf {
    if let Some(p) = key_path {
        if let Some(dir) = Path::new(p).parent() {
            return dir.to_path_buf();
        }
    }
    input["cwd"].as_str().map(PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_default()
}

/// Identifier-shaped tokens in order of appearance, deduplicated.
pub fn tokens(prompt: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    prompt
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| t.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_'))
        .filter(|t| seen.insert(t.to_string()))
        .map(str::to_string)
        .collect()
}

/// snake_case, camelCase or multi-hump PascalCase. A single leading capital (`Engine`) is not enough on its
/// own: it is indistinguishable from a sentence-initial word, so such words go through the plain-word rules.
pub(crate) fn code_shaped(t: &str) -> bool {
    let inner_upper = t.chars().skip(1).any(|c| c.is_ascii_uppercase());
    let has_lower = t.chars().any(|c| c.is_ascii_lowercase());
    (t.contains('_') && t.len() > 2) || (inner_upper && has_lower)
}

fn loose(s: &str) -> String {
    s.chars().filter(|&c| c != '_').map(|c| c.to_ascii_lowercase()).collect()
}

/// The text the prompt hook adds, or None when the prompt names nothing in the index.
pub fn prompt_context(e: &Engine, prompt: &str) -> Option<String> {
    let mut picks: Vec<(u8, String)> = Vec::new();
    let mut lc: Option<std::collections::HashMap<String, String>> = None;
    for t in tokens(prompt) {
        let shaped = code_shaped(&t);
        let defs = e.graph.by_name.get(&t).map(Vec::len).unwrap_or(0);
        if defs > 0 {
            if shaped {
                picks.push((0, t));
            } else if t.len() >= 4 && defs <= MAX_DEFS_PLAIN_WORD && !STOPWORDS.contains(&t.to_lowercase().as_str()) {
                picks.push((if t.chars().next().is_some_and(|c| c.is_ascii_uppercase()) { 1 } else { 2 }, t));
            }
        } else if shaped {
            /* `get_profile` typed as `getProfile`/`GetProfile`: one lookup ignoring case and underscores */
            let map = lc.get_or_insert_with(|| e.graph.by_name.keys().map(|k| (loose(k), k.clone())).collect());
            if let Some(name) = map.get(&loose(&t)) {
                picks.push((1, name.clone()));
            }
        }
    }
    picks.sort_by_key(|p| p.0);
    let mut seen = HashSet::new();
    picks.retain(|p| seen.insert(p.1.clone()));
    let mut lines = Vec::new();
    'names: for (_, name) in &picks {
        let mut defs = e.graph.by_name[name].clone();
        defs.retain(|&(f, _)| !fixture(&e.files[f as usize].path));
        defs.sort_by_key(|&(f, s)| {
            let sym = &e.files[f as usize].ex.symbols[s as usize];
            (!sym.exported, std::cmp::Reverse(e.graph.callers(f, s).len()), f, s)
        });
        let n_defs = defs.len();
        for (i, &(f, s)) in defs.iter().take(MAX_DEFS_PER_NAME).enumerate() {
            if lines.len() >= MAX_PROMPT_LINES {
                break 'names;
            }
            let fr = &e.files[f as usize];
            let sym = &fr.ex.symbols[s as usize];
            let callers = e.graph.callers(f, s).len();
            let more = if i + 1 == MAX_DEFS_PER_NAME && n_defs > MAX_DEFS_PER_NAME { format!(" +{} more", n_defs - MAX_DEFS_PER_NAME) } else { String::new() };
            lines.push(format!("{} {} {}:{} ({callers} callers){more}", sym.name, sym.kind.as_str(), fr.path, sym.start_line));
        }
    }
    if lines.is_empty() {
        return None;
    }
    let mut out = String::from("agent-graft-ast: symbols named in this prompt (call context(target) before changing one):\n");
    out.push_str(&lines.join("\n"));
    if let Some(tip) = edit_tip(prompt) {
        out.push('\n');
        out.push_str(tip);
    }
    Some(out)
}

/// Parser/test fixtures define throwaway symbols with everyday names; they are never what a prompt means.
pub(crate) fn fixture(path: &str) -> bool {
    ["test_data/", "testdata/", "fixtures/", "__fixtures__/", "__snapshots__/"].iter().any(|d| path.contains(d))
}

fn edit_tip(prompt: &str) -> Option<&'static str> {
    let p = prompt.to_lowercase();
    let any = |ws: &[&str]| ws.iter().any(|w| p.contains(w));
    if any(&["rename"]) {
        Some("Renaming? Use the rename tool: it edits every reference in one call.")
    } else if any(&["add parameter", "add a parameter", "add param", "add an argument", "add argument", "new parameter", "change signature", "change the signature"]) {
        Some("Adding a parameter? Use change_signature: it updates the definition and every call.")
    } else if any(&["add field", "add a field", "new field", "add a member", "add property", "add a property"]) {
        Some("Adding a field? Use add_field: it updates the type and every literal that builds it.")
    } else {
        None
    }
}

/// Files that import `rel` or call symbols defined in it, with the names involved.
pub fn dependents(e: &Engine, rel: &str) -> Option<String> {
    let &fi = e.graph.path_idx.get(rel)?;
    let file = &e.files[fi as usize];
    /* file -> (call sites, names it calls from `rel`) */
    let mut deps: BTreeMap<u32, (usize, Vec<String>)> = BTreeMap::new();
    for (si, s) in file.ex.symbols.iter().enumerate() {
        if matches!(s.kind, SymKind::Impl | SymKind::Module) {
            continue;
        }
        for c in e.graph.callers(fi, si as u32) {
            if c.file == fi {
                continue;
            }
            let d = deps.entry(c.file).or_default();
            d.0 += 1;
            if !d.1.contains(&s.name) {
                d.1.push(s.name.clone());
            }
        }
    }
    for imp in e.graph.importers(fi) {
        if imp != fi {
            deps.entry(imp).or_default();
        }
    }
    if deps.is_empty() {
        return None;
    }
    let mut ranked: Vec<(u32, (usize, Vec<String>))> = deps.into_iter().collect();
    ranked.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| e.files[a.0 as usize].path.cmp(&e.files[b.0 as usize].path)));
    let mut out = format!("Files that depend on {rel}; check them if you changed a signature or behaviour:\n");
    for (f, (_, names)) in ranked.iter().take(MAX_DEPENDENTS) {
        let what = if names.is_empty() {
            "imports it".to_string()
        } else {
            let mut v: Vec<&str> = names.iter().take(MAX_NAMES).map(String::as_str).collect();
            let extra = format!("+{}", names.len().saturating_sub(MAX_NAMES));
            if names.len() > MAX_NAMES {
                v.push(&extra);
            }
            format!("uses {}", v.join(", "))
        };
        out.push_str(&format!("{} ({what})\n", e.files[*f as usize].path));
    }
    if ranked.len() > MAX_DEPENDENTS {
        out.push_str(&format!("+{} more files\n", ranked.len() - MAX_DEPENDENTS));
    }
    Some(out.trim_end().to_string())
}

/// The index for one hook run. A running MCP server holds the redb file lock, so the hook then reads
/// `hook-snapshot.redb`, a copy kept next to the index. A copy of a live database must be repaired on its first
/// open (~45 ms on rust-analyzer), so the repaired copy is kept and reused until the index file changes.
pub struct Opened {
    pub engine: Option<Engine>,
    /// (fresh copy, snapshot path, stamp path, stamp): installed once the engine is closed cleanly
    install: Option<(PathBuf, PathBuf, PathBuf, String)>,
}

impl Opened {
    /// End of a one-shot hook: close the index cleanly but leave the in-memory files, graph and summaries for
    /// process exit to reclaim; freeing them piece by piece takes ~4 ms on a large repo.
    pub fn close_for_exit(mut self) {
        if let Some(e) = self.engine.take() {
            let Engine { files, graph, cache, .. } = e;
            std::mem::forget((files, graph, cache));
        }
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        drop(self.engine.take());
        if let Some((tmp, snap, stamp_path, stamp)) = self.install.take() {
            /* rename replaces a planted symlink itself rather than following it */
            if std::fs::rename(&tmp, &snap).is_ok() {
                let _ = write_fresh(&stamp_path, stamp.as_bytes());
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// Write through a new, exclusively created temp file renamed into place: never follows a symlink at `path`.
fn write_fresh(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    std::io::Write::write_all(&mut f, bytes)?;
    f.persist(path).map(|_| ()).map_err(|e| e.error)
}

fn is_symlink(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

fn stamp_of(db: &Path) -> Option<String> {
    let m = std::fs::metadata(db).ok()?;
    Some(format!("{} {}", crate::index::mtime_ns(&m), m.len()))
}

pub fn open_engine(root: &Path, db: &Path) -> Option<Opened> {
    open_engine_with(root, db, false)
}

/// `open_engine`; `summaries` also loads the persisted read summaries (see `Engine::open_fast_with`).
pub fn open_engine_with(root: &Path, db: &Path, summaries: bool) -> Option<Opened> {
    match Engine::open_fast_with(root, Some(db), summaries) {
        Ok(e) => return e.map(|e| Opened { engine: Some(e), install: None }),
        Err(_) if !db.exists() => return None,
        Err(_) => {}
    }
    let snap = db.with_file_name("hook-snapshot.redb");
    let stamp_path = db.with_file_name("hook-snapshot.stamp");
    let stamp = stamp_of(db)?;
    if [db, snap.as_path(), stamp_path.as_path()].iter().any(|p| is_symlink(p)) {
        return None;
    }
    if std::fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str()) {
        if let Ok(Some(e)) = Engine::open_fast_with(root, Some(&snap), summaries) {
            return Some(Opened { engine: Some(e), install: None });
        }
    }
    /* an exclusively created temp file: a symlink planted at a guessable name cannot redirect the copy */
    let mut fresh = tempfile::Builder::new().prefix("hook-snapshot.").suffix(".tmp").tempfile_in(db.parent()?).ok()?;
    std::io::copy(&mut std::fs::File::open(db).ok()?, fresh.as_file_mut()).ok()?;
    let tmp = fresh.into_temp_path().keep().ok()?;
    match Engine::open_fast_with(root, Some(&tmp), summaries) {
        Ok(Some(e)) => Some(Opened { engine: Some(e), install: Some((tmp, snap, stamp_path, stamp)) }),
        _ => {
            let _ = std::fs::remove_file(&tmp);
            None
        }
    }
}

/// `hook prompt`: stdin JSON in, hook JSON (or nothing) out.
pub fn prompt_hook(stdin: &str, explicit: Option<(PathBuf, PathBuf)>) -> Option<String> {
    let input: Value = serde_json::from_str(stdin).ok()?;
    let prompt = input["prompt"].as_str()?;
    let (root, db) = locate(&start_dir(&input, None), explicit)?;
    let o = open_engine(&root, &db)?;
    prompt_context(o.engine.as_ref()?, prompt).map(|t| wrap("UserPromptSubmit", t))
}

/// `hook post-edit`: refreshes the edited file in memory (the watcher may lag), then lists its dependents.
pub fn post_edit_hook(stdin: &str, explicit: Option<(PathBuf, PathBuf)>) -> Option<String> {
    let input: Value = serde_json::from_str(stdin).ok()?;
    let path = input["tool_input"]["file_path"].as_str().or_else(|| input["tool_response"]["filePath"].as_str())?;
    let (root, db) = locate(&start_dir(&input, Some(path)), explicit)?;
    let mut o = open_engine(&root, &db)?;
    let e = o.engine.as_mut()?;
    let abs = if Path::new(path).is_absolute() { PathBuf::from(path) } else { e.root.join(path) };
    let rel = abs.canonicalize().ok()?.strip_prefix(&e.root).ok()?.to_string_lossy().replace('\\', "/");
    e.refresh(&rel);
    dependents(e, &rel).map(|t| wrap("PostToolUse", t))
}

#[cfg(test)]
#[path = "hooks_tests.rs"]
mod tests;
