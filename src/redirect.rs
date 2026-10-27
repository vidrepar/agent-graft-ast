//! `hook pre-tool` (PreToolUse on Grep/Glob/Bash): a search for an identifier the index defines is answered
//! from the index instead of running. The hook denies the tool call with `context(name)` as the reason, so the
//! model gets the definition and every reference with Read windows in the same turn. Every other search (text,
//! regex, plain words, names defined in many places) passes through untouched. A search for a bare name that asks
//! for the matching lines themselves (Grep content mode, grep/rg without -l) also passes: the context lists sites
//! by line number, not their text. Read/Edit/Write are never matched.

use crate::engine::Engine;
use crate::hooks::{code_shaped, fixture, locate, open_engine_with};
use crate::parse::SymKind;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// A name defined in more places than this is too generic to answer with one context.
const MAX_DEFS: usize = 3;
/// Token budget for the context put in the deny reason (the MCP tool's default).
const CONTEXT_BUDGET: usize = 4000;

/// Words that may precede the name in a definition search: `fn foo`, `pub async fn foo`, `class Foo`.
const DEF_KEYWORDS: &[&str] = &[
    "pub", "pub(crate)", "export", "default", "async", "unsafe", "static", "abstract", "final", "public", "private", "protected",
    "fn", "func", "def", "function", "class", "struct", "enum", "trait", "interface", "type", "impl", "const", "let", "var", "val",
    "mod", "union", "record", "object", "macro_rules!",
];

/// What a search asks for: one identifier, and whether the pattern names it as a definition (`fn name`).
#[derive(Debug, PartialEq)]
pub struct Target {
    pub name: String,
    pub definition: bool,
    /// Glob searches look for files: only type-shaped names are answered.
    pub file_search: bool,
}

fn is_ident(s: &str) -> bool {
    s.len() >= 3 && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The identifier a grep regex searches for, or None for anything that is more than a name.
pub fn regex_target(pattern: &str) -> Option<Target> {
    let mut p = pattern.trim();
    for pre in ["^", "\\s*", "\\b", "\\<"] {
        p = p.strip_prefix(pre).unwrap_or(p);
    }
    for suf in ["$", "\\s*", "\\(", "(", "\\b", "\\>"] {
        p = p.strip_suffix(suf).unwrap_or(p);
    }
    let words: Vec<&str> = p.split(|c: char| c.is_whitespace()).flat_map(|w| w.split("\\s+")).flat_map(|w| w.split("\\s*")).filter(|w| !w.is_empty()).collect();
    let (&last, keywords) = words.split_last()?;
    if !is_ident(last) || !keywords.iter().all(|k| DEF_KEYWORDS.contains(k)) {
        return None;
    }
    Some(Target { name: last.to_string(), definition: !keywords.is_empty(), file_search: false })
}

/// The identifier a file glob looks for (`**/HoverConfig*.ts` -> `HoverConfig`).
pub fn glob_target(glob: &str) -> Option<Target> {
    let last = glob.rsplit('/').next()?;
    let stem = last.split('.').next()?.trim_matches(|c| c == '*' || c == '?');
    (is_ident(stem) && stem.starts_with(|c: char| c.is_ascii_uppercase())).then(|| Target { name: stem.to_string(), definition: false, file_search: true })
}

/// Shell words of a simple command; None when it has pipes, redirects, substitutions or several commands.
fn shell_words(cmd: &str) -> Option<Vec<String>> {
    let (mut words, mut cur, mut quote, mut any) = (Vec::new(), String::new(), None::<char>, false);
    let mut it = cmd.trim().chars();
    while let Some(c) = it.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '$' | '`') => return None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => (quote, any) = (Some(c), true),
            (None, '\\') => cur.push(it.next()?),
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            (None, '|' | ';' | '&' | '<' | '>' | '(' | ')' | '`' | '$') => return None,
            (None, c) => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if any || !cur.is_empty() {
        words.push(cur);
    }
    Some(words)
}

/// `grep`/`rg`/`git grep` with one pattern, or `find -name`, as a plain command.
pub fn bash_target(cmd: &str) -> Option<Target> {
    let words = shell_words(cmd)?;
    let args: &[String] = match words.first()?.as_str() {
        "grep" | "egrep" | "rg" => &words[1..],
        "git" if words.get(1)? == "grep" => &words[2..],
        "find" => {
            let i = words.iter().position(|w| w == "-name" || w == "-iname")?;
            return glob_target(words.get(i + 1)?);
        }
        _ => return None,
    };
    /* flags that take the next word as their value */
    const WITH_VALUE: &[&str] = &["-A", "-B", "-C", "-m", "-g", "-t", "-T", "-M", "-j", "--type", "--glob", "--max-count", "--context"];
    let (mut flagged, mut positional, mut files_only) = (Vec::new(), None, false);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-e" || a == "--regexp" {
            flagged.push(args.get(i + 1)?.as_str());
            i += 2;
        } else if matches!(a, "-f" | "--file" | "-U" | "--multiline") {
            return None;
        } else if WITH_VALUE.contains(&a) {
            i += 2;
        } else {
            if matches!(a, "-l" | "--files-with-matches" | "-c" | "--count") {
                files_only = true;
            }
            /* the first positional word is the pattern unless -e gave one; the rest are paths */
            if !a.starts_with('-') && positional.is_none() {
                positional = Some(a);
            }
            i += 1;
        }
    }
    let t = match flagged.as_slice() {
        [] => regex_target(positional?),
        [one] => regex_target(one),
        _ => None,
    }?;
    (t.definition || files_only).then_some(t)
}

/// The target of one PreToolUse call; None passes it through.
pub fn tool_target(tool: &str, input: &Value) -> Option<Target> {
    match tool {
        "Grep" if input["multiline"].as_bool() != Some(true) => {
            let t = regex_target(input["pattern"].as_str()?)?;
            (t.definition || input["output_mode"].as_str() != Some("content")).then_some(t)
        }
        "Glob" => glob_target(input["pattern"].as_str()?),
        "Bash" => bash_target(input["command"].as_str()?),
        _ => None,
    }
}

/// Grep's `glob`/`type` filters: the definition's file must be one the search would look at.
fn filter_allows(input: &Value, def_path: &str) -> bool {
    let ext = def_path.rsplit('.').next().unwrap_or("");
    if let Some(g) = input["glob"].as_str() {
        if !g.split(|c| c == ',' || c == '{' || c == '}' || c == '.').any(|e| e == ext) {
            return false;
        }
    }
    if let Some(t) = input["type"].as_str() {
        let exts: &[&str] = match t {
            "rust" | "rs" => &["rs"],
            "ts" | "typescript" => &["ts", "tsx", "mts", "cts"],
            "js" | "javascript" => &["js", "jsx", "mjs", "cjs"],
            "py" | "python" => &["py", "pyi"],
            "go" => &["go"],
            "java" => &["java"],
            "kotlin" => &["kt", "kts"],
            _ => return false,
        };
        return exts.contains(&ext);
    }
    true
}

/// The deny reason for `t`, or None when the index cannot answer it with one definition.
pub fn answer(e: &mut Engine, t: &Target, input: &Value) -> Option<String> {
    if !t.definition && !code_shaped(&t.name) && !t.name.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    let defs: Vec<(u32, u32)> = e.graph.by_name.get(&t.name)?.iter().copied().filter(|&(f, s)| {
        let fr = &e.files[f as usize];
        !fixture(&fr.path) && fr.ex.symbols[s as usize].kind != SymKind::Impl
    }).collect();
    if defs.is_empty() || defs.len() > MAX_DEFS {
        return None;
    }
    let types = [SymKind::Class, SymKind::Struct, SymKind::Enum, SymKind::Trait, SymKind::Interface, SymKind::TypeAlias];
    if t.file_search && !defs.iter().any(|&(f, s)| types.contains(&e.files[f as usize].ex.symbols[s as usize].kind)) {
        return None;
    }
    if !defs.iter().any(|&(f, _)| filter_allows(input, &e.files[f as usize].path)) {
        return None;
    }
    let ctx = e.context_quick(&t.name, CONTEXT_BUDGET);
    let head = ctx.lines().next().unwrap_or("");
    if !head.starts_with("<context ") || !head.contains(" path=\"") {
        return None;
    }
    let body: Vec<&str> = ctx.lines().filter(|l| !l.starts_with("<budget_receipt")).collect();
    let more = if defs.len() > 1 { format!(" It is defined {} times; this is the first, call context(path:{}) for another.", defs.len(), t.name) } else { String::new() };
    Some(format!(
        "Not an error: agent-graft-ast answered this search from its index, which matches the files on disk. Below is the definition of `{}` and every line naming it, with Read windows; use it as the search result. context(name) and find_symbol do this directly; Read any file in full whenever you need more.{more}\n{}",
        t.name,
        body.join("\n")
    ))
}

fn start_dir(input: &Value) -> PathBuf {
    let cwd = input["cwd"].as_str().map(PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    match input["tool_input"]["path"].as_str() {
        Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
        Some(p) => cwd.join(p),
        None => cwd,
    }
}

/// `hook pre-tool`: stdin JSON in, a deny decision with the answer (or nothing) out.
pub fn pre_tool_hook(stdin: &str, explicit: Option<(PathBuf, PathBuf)>) -> Option<String> {
    let input: Value = serde_json::from_str(stdin).ok()?;
    let t = tool_target(input["tool_name"].as_str()?, &input["tool_input"])?;
    let (root, db) = locate(&start_dir(&input), explicit)?;
    let mut o = open_engine_with(&root, &db, true)?;
    let reason = answer(o.engine.as_mut()?, &t, &input["tool_input"]);
    o.close_for_exit();
    let reason = reason?;
    Some(
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}})
            .to_string(),
    )
}

#[cfg(test)]
#[path = "redirect_tests.rs"]
mod tests;
