//! `agent-graft-ast init`: index the repo, register the MCP server, the hooks, the tool permission and the
//! CLAUDE.md routing block for Claude Code. Every write is a merge that keeps what is already there; running it
//! twice changes nothing.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const SERVER_NAME: &str = "agent-graft-ast";
const EDIT_MATCHER: &str = "Edit|Write|MultiEdit";
const SEARCH_MATCHER: &str = "Grep|Glob|Bash";
/// The grant an earlier init put in the shared settings.json; dropped there, since it approved the edit tools too.
const OLD_WILDCARD: &str = "mcp__agent-graft-ast__*";
/// Tools that only read: allowed without a prompt in the per-user settings.local.json.
pub const READ_TOOLS: &[&str] = &["find_symbol", "context", "read_file"];
/// Tools that write files: they prompt unless `init --allow-edits`.
pub const EDIT_TOOLS: &[&str] = &["rename", "change_signature", "add_field", "move_symbol", "rewrite", "batch"];

/// What `init` sets up beyond the server and the two context hooks.
#[derive(Clone, Copy)]
pub struct Options {
    /// PreToolUse search redirect and the CLAUDE.md block
    pub redirect: bool,
    /// allow the edit tools without a prompt (per user)
    pub allow_edits: bool,
}

fn rule(tool: &str) -> String {
    format!("mcp__{SERVER_NAME}__{tool}")
}
pub const BLOCK_START: &str = "<!-- agent-graft-ast:start -->";
pub const BLOCK_END: &str = "<!-- agent-graft-ast:end -->";

/// The routing rules `init` keeps in the repo's CLAUDE.md, between the markers.
pub const CLAUDE_BLOCK: &str = "<!-- agent-graft-ast:start -->
## Code navigation: agent-graft-ast

This repo is indexed by agent-graft-ast (MCP tools `mcp__agent-graft-ast__*`).

- Mechanical edits go through the engine, not by hand: `rename`, `change_signature` (add a parameter and its call args), `add_field`, `move_symbol`, `rewrite` (repo-wide pattern -> replacement).
- Before changing a symbol, call `context(target)`: definition, every reference, Read windows.
- Looking for a symbol: `find_symbol` before Grep/Glob. A Grep/Glob for a defined symbol is answered from the index.
- Big file: `read_file(path, level=2)` (public API) first.
- Read whole files whenever you need more context; nothing here limits Read.
<!-- agent-graft-ast:end -->
";

/// One file `init` would create or change, with its new content.
pub struct Change {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: String,
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+:@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The hook command lines for this binary.
pub fn hook_commands(exe: &Path) -> (String, String) {
    let q = shell_quote(&exe.to_string_lossy());
    (format!("{q} hook prompt"), format!("{q} hook post-edit"))
}

fn pre_tool_command(exe: &Path) -> String {
    format!("{} hook pre-tool", shell_quote(&exe.to_string_lossy()))
}

fn read_json(path: &Path) -> Result<(Option<String>, Map<String, Value>)> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok((Some(s), Map::new())),
        Ok(s) => {
            let v: Value = serde_json::from_str(&s).with_context(|| format!("{} is not valid JSON; fix it and rerun init", path.display()))?;
            match v {
                Value::Object(m) => Ok((Some(s), m)),
                _ => anyhow::bail!("{} is not a JSON object", path.display()),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((None, Map::new())),
        Err(e) => Err(e.into()),
    }
}

fn obj<'a>(m: &'a mut Map<String, Value>, key: &str) -> Result<&'a mut Map<String, Value>> {
    m.entry(key).or_insert_with(|| json!({})).as_object_mut().with_context(|| format!("\"{key}\" is not a JSON object"))
}

fn finish(path: PathBuf, before: Option<String>, m: Map<String, Value>) -> Option<Change> {
    let after = serde_json::to_string_pretty(&Value::Object(m)).unwrap_or_default() + "\n";
    let unchanged = before.as_deref().and_then(|b| serde_json::from_str::<Value>(b).ok()) == serde_json::from_str::<Value>(&after).ok();
    (!unchanged).then_some(Change { path, before, after })
}

/// `.mcp.json`: our server under `mcpServers`, pointing at this binary and this root. An existing entry of
/// that name that runs anything else is rewritten (and shows up in the plan), never trusted as-is.
pub fn plan_mcp(root: &Path, exe: &Path) -> Result<Option<Change>> {
    let path = root.join(".mcp.json");
    let (before, mut m) = read_json(&path)?;
    let servers = obj(&mut m, "mcpServers")?;
    let want = json!({"command": exe.to_string_lossy(), "args": ["--root", root.to_string_lossy(), "serve"]});
    /* the whole entry must match: an extra env, type or url would change what runs */
    if servers.get(SERVER_NAME) != Some(&want) {
        servers.insert(SERVER_NAME.into(), want);
    }
    Ok(finish(path, before, m))
}

/// True when some handler under `hooks[event]` runs exactly `cmd`.
fn has_hook(list: &[Value], cmd: &str) -> bool {
    list.iter().any(|g| g["hooks"].as_array().into_iter().flatten().any(|h| h["command"].as_str() == Some(cmd)))
}

/// Drop our handlers for `needle` that run a different command (an old or foreign binary), and groups left empty.
fn drop_stale(list: &mut Vec<Value>, needle: &str, cmd: &str) {
    for g in list.iter_mut() {
        if let Some(hs) = g.get_mut("hooks").and_then(|h| h.as_array_mut()) {
            hs.retain(|h| !h["command"].as_str().is_some_and(|c| c.contains(SERVER_NAME) && c.ends_with(needle) && c != cmd));
        }
    }
    list.retain(|g| g.get("hooks").and_then(|h| h.as_array()).is_none_or(|hs| !hs.is_empty()));
}

/// `.claude/settings.json` (shared, committed): append our hooks, keeping every other key and hook. It grants no
/// permissions. Without `redirect` the PreToolUse search hook is left out, and removed if an earlier init added it.
pub fn plan_settings(root: &Path, exe: &Path, redirect: bool) -> Result<Option<Change>> {
    let path = root.join(".claude").join("settings.json");
    let (before, mut m) = read_json(&path)?;
    if let Some(allow) = m.get_mut("permissions").and_then(|p| p.get_mut("allow")).and_then(Value::as_array_mut) {
        allow.retain(|v| v.as_str() != Some(OLD_WILDCARD));
    }
    let (prompt_cmd, edit_cmd) = hook_commands(exe);
    let hooks = obj(&mut m, "hooks")?;
    if !redirect {
        if let Some(arr) = hooks.get_mut("PreToolUse").and_then(Value::as_array_mut) {
            drop_stale(arr, "hook pre-tool", "");
        }
    }
    let mut wanted = vec![("UserPromptSubmit", None, prompt_cmd, "hook prompt"), ("PostToolUse", Some(EDIT_MATCHER), edit_cmd, "hook post-edit")];
    if redirect {
        wanted.push(("PreToolUse", Some(SEARCH_MATCHER), pre_tool_command(exe), "hook pre-tool"));
    }
    for (event, matcher, cmd, needle) in wanted {
        let list = hooks.entry(event).or_insert_with(|| json!([]));
        let arr = list.as_array_mut().with_context(|| format!("hooks.{event} is not a JSON array"))?;
        drop_stale(arr, needle, &cmd);
        if has_hook(arr, &cmd) {
            continue;
        }
        let mut group = json!({"hooks": [{"type": "command", "command": cmd, "timeout": 10}]});
        if let Some(mt) = matcher {
            group["matcher"] = json!(mt);
        }
        arr.push(group);
    }
    Ok(finish(path, before, m))
}

/// `.claude/settings.local.json` (per user, not committed): allow the read-only tools, and the edit tools with
/// `allow_edits`. Only adds: a rerun without `allow_edits` keeps a grant the user made earlier.
pub fn plan_local_settings(root: &Path, allow_edits: bool) -> Result<Option<Change>> {
    let path = root.join(".claude").join("settings.local.json");
    let (before, mut m) = read_json(&path)?;
    let allow = obj(&mut m, "permissions")?.entry("allow").or_insert_with(|| json!([]));
    let allow = allow.as_array_mut().context("permissions.allow is not a JSON array")?;
    let tools = READ_TOOLS.iter().chain(if allow_edits { EDIT_TOOLS } else { &[] });
    for r in tools.map(|t| rule(t)) {
        if !allow.iter().any(|v| v.as_str() == Some(r.as_str())) {
            allow.push(json!(r));
        }
    }
    Ok(finish(path, before, m))
}

/// True when a .gitignore line ignores `name` (a root-relative path) as written by hand or by us.
fn ignores(text: &str, name: &str) -> bool {
    let dir = name.split('/').next().unwrap_or(name);
    text.lines().map(str::trim).any(|l| {
        let l = l.strip_prefix('/').unwrap_or(l);
        l == name || l.trim_end_matches('/') == dir || (name.contains('/') && l == name.rsplit('/').next().unwrap_or(name))
    })
}

/// `.gitignore`: append `.agent-graft/` and `.claude/settings.local.json` unless lines already ignore them.
pub fn plan_gitignore(root: &Path) -> Result<Option<Change>> {
    let path = root.join(".gitignore");
    let before = match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let text = before.clone().unwrap_or_default();
    let missing: Vec<&str> = [".agent-graft/", ".claude/settings.local.json"].into_iter().filter(|n| !ignores(&text, n)).collect();
    if missing.is_empty() {
        return Ok(None);
    }
    let mut after = text;
    if !after.is_empty() && !after.ends_with('\n') {
        after.push('\n');
    }
    for n in missing {
        after.push_str(n);
        after.push('\n');
    }
    Ok(Some(Change { path, before, after }))
}

fn is_symlink(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// `CLAUDE.md`: our routing block between the markers, replaced in place or appended; nothing else is touched.
/// Without `redirect` an existing block is removed. A symlinked CLAUDE.md is left alone (`describe` says so).
pub fn plan_claude_md(root: &Path, redirect: bool) -> Result<Option<Change>> {
    let path = root.join("CLAUDE.md");
    if is_symlink(&path) {
        return Ok(None);
    }
    let before = match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let text = before.clone().unwrap_or_default();
    let after = match (text.find(BLOCK_START), text.find(BLOCK_END)) {
        (Some(s), Some(e)) if s < e => {
            let mut end = e + BLOCK_END.len();
            if text[end..].starts_with('\n') {
                end += 1;
            }
            /* drop the blank line init put before an appended block */
            let head = if text[end..].is_empty() { text[..s].strip_suffix('\n').filter(|h| h.ends_with('\n')).unwrap_or(&text[..s]) } else { &text[..s] };
            let rest = format!("{head}{}", &text[end..]);
            if redirect {
                format!("{}{CLAUDE_BLOCK}{}", &text[..s], &text[end..])
            } else if rest.trim().is_empty() {
                String::new()
            } else {
                rest
            }
        }
        (None, None) if redirect => {
            let sep = match text.as_str() {
                "" => "",
                t if t.ends_with("\n\n") => "",
                t if t.ends_with('\n') => "\n",
                _ => "\n\n",
            };
            format!("{text}{sep}{CLAUDE_BLOCK}")
        }
        (None, None) => return Ok(None),
        _ => anyhow::bail!("{} has an unmatched {BLOCK_START} / {BLOCK_END} marker; fix it and rerun init", path.display()),
    };
    if after == text || (before.is_none() && after.is_empty()) {
        return Ok(None);
    }
    Ok(Some(Change { path, before, after }))
}

/// All file changes `init` would make, in the order they are written.
pub fn plan(root: &Path, exe: &Path, o: Options) -> Result<Vec<Change>> {
    Ok([
        plan_mcp(root, exe)?,
        plan_settings(root, exe, o.redirect)?,
        plan_local_settings(root, o.allow_edits)?,
        plan_gitignore(root)?,
        plan_claude_md(root, o.redirect)?,
    ]
        .into_iter()
        .flatten()
        .collect())
}

/// Writes go through a fresh temp file renamed into place, and a symlink at a target is refused, never followed.
pub fn apply(changes: &[Change]) -> Result<()> {
    for c in changes {
        let dir = c.path.parent().unwrap_or(Path::new("."));
        if is_symlink(&c.path) || is_symlink(dir) {
            anyhow::bail!("{} is a symlink; init does not write through symlinks", c.path.display());
        }
        std::fs::create_dir_all(dir)?;
        let mut f = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut f, c.after.as_bytes())?;
        f.persist(&c.path).with_context(|| format!("writing {}", c.path.display()))?;
    }
    Ok(())
}

/// What `init` keeps in each file, for the plan output.
fn what(rel: &str, o: Options) -> &'static str {
    match rel {
        ".mcp.json" => ": the agent-graft-ast MCP server",
        ".gitignore" => ": ignore .agent-graft/ (the index) and .claude/settings.local.json",
        "CLAUDE.md" => ": routing rules between the agent-graft-ast markers",
        _ if rel.ends_with("settings.local.json") && o.allow_edits => ": allow find_symbol, context, read_file and the edit tools without a prompt (your settings only)",
        _ if rel.ends_with("settings.local.json") => ": allow find_symbol, context, read_file without a prompt (your settings only)",
        _ if rel.ends_with("settings.json") && o.redirect => ": hooks (prompt, post-edit, pre-tool search redirect)",
        _ if rel.ends_with("settings.json") => ": hooks (prompt, post-edit)",
        _ => "",
    }
}

/// Human-readable report of the plan; `dry_run` changes the verbs only.
pub fn describe(root: &Path, exe: &Path, changes: &[Change], dry_run: bool, o: Options, index_line: &str) -> String {
    let mut out = String::new();
    out.push_str(index_line);
    out.push('\n');
    if changes.is_empty() {
        out.push_str("Already set up: .mcp.json, .claude/settings.json, .claude/settings.local.json, .gitignore and CLAUDE.md need no changes.\n");
    }
    if is_symlink(&root.join("CLAUDE.md")) {
        out.push_str("CLAUDE.md is a symlink, so init left it alone; paste the agent-graft-ast block from the README into it yourself.\n");
    }
    for c in changes {
        let rel = c.path.strip_prefix(root).unwrap_or(&c.path).display();
        let verb = match (dry_run, c.before.is_some()) {
            (true, true) => "would update",
            (true, false) => "would create",
            (false, true) => "updated",
            (false, false) => "created",
        };
        out.push_str(&format!("{verb} {rel}{}\n", what(&rel.to_string(), o)));
        if dry_run {
            for l in c.after.lines() {
                out.push_str(&format!("    {l}\n"));
            }
        }
    }
    let edits = EDIT_TOOLS.join(", ");
    if o.allow_edits {
        out.push_str(&format!("Edit tools ({edits}) run without a prompt for you: allowed in .claude/settings.local.json, which is not committed.\n"));
    } else {
        out.push_str(&format!("Edit tools ({edits}) ask before each use. To allow them for you only: agent-graft-ast init --allow-edits\n"));
    }
    out.push_str("Restart Claude Code in this repo and approve the agent-graft-ast server when asked.\n");
    out.push_str(&format!(
        "Rather not commit .mcp.json? Delete our entry from it and run: claude mcp add {SERVER_NAME} -- {} --root {} serve\n",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&root.to_string_lossy())
    ));
    out
}

#[cfg(test)]
#[path = "init_tests.rs"]
mod tests;
