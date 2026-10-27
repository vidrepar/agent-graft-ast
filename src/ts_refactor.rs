//! TS/JS edits resolved by tsserver: rename locations, references for `change_signature` and `context`,
//! and the compiler errors an edit adds. The syntax engine answers instead when tsserver cannot run,
//! and the reply says so (`exact="false"` plus how to turn exact mode on).

use crate::engine::{esc, Engine};
use crate::lang::Lang;
use crate::parse::{parse_tree, text};
use crate::refactor::{Target, IDENT_KINDS};
use crate::ts_exact::{to_byte, to_loc, Diag, Fallback, Loc, ENABLE_HINT};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tree_sitter::Node;

/// Default time one diagnostics pass (before or after an edit) may take.
const DEFAULT_DIAG_MS: u64 = 10_000;

/// Time budget per diagnostics pass; None when turned off (AGENT_GRAFT_TS_DIAG_MS=0).
fn diag_budget() -> Option<std::time::Duration> {
    let ms = std::env::var("AGENT_GRAFT_TS_DIAG_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(DEFAULT_DIAG_MS);
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// Projects loaded ahead of the first request.
const MAX_PREWARM_PROJECTS: usize = 32;
/// New errors listed in a reply.
const MAX_LISTED_ERRORS: usize = 10;

/// Whether a TS/JS answer came from tsserver.
pub(crate) enum Exact {
    /// tsserver resolved it
    Yes,
    /// the syntax engine did: why tsserver could not
    No(String),
    /// not a TS/JS target, or exact mode is off without a note (unit tests)
    NotApplicable,
}

impl Exact {
    /// Attribute for the reply's opening tag.
    pub(crate) fn attr(&self) -> &'static str {
        match self {
            Exact::Yes => " exact=\"true\"",
            Exact::No(_) => " exact=\"false\"",
            Exact::NotApplicable => "",
        }
    }

    /// Body line saying the answer is the syntax engine's, and how to get tsserver's.
    pub(crate) fn note(&self) -> String {
        match self {
            Exact::No(why) => format!("<exact_off reason=\"{}\">{}</exact_off>\n", esc(why), esc(ENABLE_HINT)),
            _ => String::new(),
        }
    }
}

/// A file's planned rename edits: path, its text, (start, end, replacement) spans.
pub(crate) type FileEdits = (String, String, Vec<(usize, usize, String)>);

/// Exact references grouped per indexed file: (file, reference name starts, definition lines).
pub(crate) type FileRefs = (u32, Vec<usize>, Vec<u32>);

/// `(start, end)` byte spans of tsserver locations in `src`.
fn spans(src: &str, locs: &[&Loc]) -> Option<Vec<(usize, usize)>> {
    locs.iter().map(|l| Some((to_byte(src, l.start.0, l.start.1)?, to_byte(src, l.end.0, l.end.1)?))).collect()
}

impl Engine {
    /// Whether a target in `lang` is first tried with tsserver (`Yes` until it falls back).
    pub(crate) fn ts_mode(&self, lang: Lang) -> Exact {
        if !lang.is_js_family() || self.ts.is_quiet_off() {
            Exact::NotApplicable
        } else {
            Exact::Yes
        }
    }

    /// Every TS/JS project as (its `tsconfig.json`/`jsconfig.json`, up to 5 source files it most likely
    /// includes); one group without a config when the repo has none. Empty when the repo has no TS/JS.
    pub fn ts_projects(&self) -> Vec<(Option<PathBuf>, Vec<PathBuf>)> {
        let mut sources: Vec<&str> = self
            .files
            .iter()
            .filter(|f| f.lang.is_js_family() && f.size > 0 && !f.path.ends_with(".d.ts"))
            .map(|f| f.path.as_str())
            .collect();
        if sources.is_empty() || self.ts.is_quiet_off() {
            return Vec::new();
        }
        /* a tsconfig's `include` is most surely the TypeScript under `src/`; `.mjs` configs often sit outside it */
        sources.sort_by_key(|p| (!(p.ends_with(".ts") || p.ends_with(".tsx")), !p.contains("/src/") && !p.starts_with("src/"), *p));
        let mut configs: Vec<(String, PathBuf)> = ignore::WalkBuilder::new(&self.root)
            .filter_entry(|e| e.file_name() != "node_modules")
            .build()
            .flatten()
            .filter(|e| matches!(e.file_name().to_str(), Some("tsconfig.json" | "jsconfig.json")))
            .filter_map(|e| {
                let rel = e.path().parent()?.strip_prefix(&self.root).ok()?.to_string_lossy().replace('\\', "/");
                let dir = if rel.is_empty() { rel } else { format!("{rel}/") };
                Some((dir, self.ts.root().join(e.path().strip_prefix(&self.root).ok()?)))
            })
            .collect();
        configs.sort();
        configs.dedup_by(|a, b| a.0 == b.0);
        let dirs: Vec<&str> = configs.iter().map(|c| c.0.as_str()).collect();
        /* a file belongs to its nearest config's directory */
        let owner = |p: &str| dirs.iter().filter(|d| p.starts_with(**d)).max_by_key(|d| d.len()).copied();
        let mut out: Vec<(Option<PathBuf>, Vec<PathBuf>)> = Vec::new();
        for (dir, cfg) in &configs {
            let cands: Vec<PathBuf> = sources.iter().filter(|p| owner(p) == Some(dir.as_str())).take(5).map(|p| self.ts.root().join(p)).collect();
            if !cands.is_empty() {
                out.push((Some(cfg.clone()), cands));
            }
        }
        if out.is_empty() {
            out.push((None, vec![self.ts.root().join(sources[0])]));
        }
        out.truncate(MAX_PREWARM_PROJECTS);
        out
    }

    /// tsserver position of `target`'s (primary) definition name; None when it does not resolve to one.
    pub fn ts_target_at(&mut self, target: &str) -> Option<(PathBuf, (u32, u32))> {
        let t = self.resolve_target(target, "rename", 4000).ok()?;
        self.ts_def_at(t.defs[0].0, t.defs[0].1)
    }

    /// Repo-relative path of a tsserver file, when it is inside the repo and not a dependency.
    fn ts_rel(&self, abs: &Path) -> Option<String> {
        let rel = abs.strip_prefix(self.ts.root()).or_else(|_| abs.strip_prefix(&self.root)).ok()?;
        if rel.components().any(|c| c.as_os_str() == "node_modules") {
            return None;
        }
        Some(rel.to_string_lossy().replace('\\', "/"))
    }

    /// File and tsserver position of the name in definition `(f, s)`: the first identifier spelling the
    /// name inside the declaration.
    pub(crate) fn ts_def_at(&self, f: u32, s: u32) -> Option<(PathBuf, (u32, u32))> {
        if !self.ts.has_projects() {
            let _ = self.ts.load_projects(&self.ts_projects());
        }
        let fr = &self.files[f as usize];
        let sym = &fr.ex.symbols[s as usize];
        let src = self.checked_source(f).ok()?;
        let tree = parse_tree(fr.lang, src.as_bytes())?;
        let mut best: Option<usize> = None;
        let mut stack = vec![tree.root_node()];
        while let Some(n) = stack.pop() {
            let (a, b) = (n.start_position().row as u32 + 1, n.end_position().row as u32 + 1);
            if b < sym.start_line || a > sym.end_line {
                continue;
            }
            if n.child_count() == 0 && IDENT_KINDS.contains(&n.kind()) && a >= sym.start_line && text(n, src.as_bytes()) == sym.name {
                best = Some(best.map_or(n.start_byte(), |x| x.min(n.start_byte())));
            }
            let mut cur = n.walk();
            stack.extend(n.children(&mut cur));
        }
        Some((self.ts.root().join(&fr.path), to_loc(&src, best?)))
    }

    /// The text tsserver would edit in `abs`: an indexed file's checked text, else the file as on disk.
    fn ts_source(&self, rel: &str) -> Result<String, String> {
        match self.graph.path_idx.get(rel) {
            Some(&f) => self.checked_source(f).map_err(|r| format!("{rel}: {}", r.code())),
            None => {
                let rel = self.resolve_rel(rel).map_err(|r| format!("{rel}: {}", r.code()))?;
                std::fs::read_to_string(self.root.join(rel)).map_err(|e| e.to_string())
            }
        }
    }

    /// tsserver's rename of `t` as per-file edits. Err(Fallback) when tsserver cannot answer;
    /// Ok(Err(why)) when it refuses (a library symbol, a location outside the repo).
    pub(crate) fn ts_rename_plan(&self, t: &Target, new_name: &str, comments: bool) -> Result<Result<Vec<FileEdits>, String>, Fallback> {
        let (f, s) = t.defs[0];
        let Some((file, at)) = self.ts_def_at(f, s) else { return Err(Fallback("definition name not found in its file".into())) };
        for attempt in 0..2 {
            let locs = match self.ts.rename(&file, at, comments)? {
                Ok(l) => l,
                Err(why) => return Ok(Err(why)),
            };
            let mut by_file: HashMap<PathBuf, Vec<&Loc>> = HashMap::new();
            for l in &locs {
                by_file.entry(l.file.clone()).or_default().push(l);
            }
            let mut plan = Vec::new();
            let mut stale = Vec::new();
            for (abs, locs) in by_file {
                let Some(rel) = self.ts_rel(&abs) else {
                    return Ok(Err(format!("tsserver would also rename {} (outside the repo or in node_modules)", abs.display())));
                };
                let src = match self.ts_source(&rel) {
                    Ok(s) => s,
                    Err(e) => return Ok(Err(format!("cannot edit {e}"))),
                };
                /* every span must still spell the old name: anything else means tsserver saw older text */
                match spans(&src, &locs) {
                    Some(sp) if sp.iter().all(|&(a, b)| src.get(a..b) == Some(t.name.as_str())) => {
                        let edits = sp.iter().zip(&locs).map(|(&(a, b), l)| (a, b, format!("{}{new_name}{}", l.prefix, l.suffix))).collect();
                        plan.push((rel, src, edits));
                    }
                    _ => stale.push(abs),
                }
            }
            if stale.is_empty() {
                plan.sort_by(|a, b| a.0.cmp(&b.0));
                let seen: HashSet<u32> = plan.iter().filter_map(|p| self.graph.path_idx.get(&p.0).copied()).collect();
                self.ts_complete(t, &seen)?;
                return Ok(Ok(plan));
            }
            if attempt == 1 {
                return Err(Fallback(format!("tsserver answered from outdated text of {}", stale[0].display())));
            }
            for abs in stale {
                self.ts.touch(abs);
            }
        }
        unreachable!()
    }

    /// tsserver's references to `t`, per indexed file in path order.
    pub(crate) fn ts_refs(&self, t: &Target) -> Result<Vec<FileRefs>, Fallback> {
        let (f, s) = t.defs[0];
        let Some((file, at)) = self.ts_def_at(f, s) else { return Err(Fallback("definition name not found in its file".into())) };
        let locs = self.ts.references(&file, at)?;
        let mut by_file: HashMap<u32, (Vec<usize>, Vec<u32>)> = HashMap::new();
        let mut texts: HashMap<u32, String> = HashMap::new();
        for l in &locs {
            let Some(fi) = self.ts_rel(&l.file).and_then(|r| self.graph.path_idx.get(&r).copied()) else { continue };
            if !texts.contains_key(&fi) {
                let Ok(src) = self.checked_source(fi) else { continue };
                texts.insert(fi, src);
            }
            let Some(a) = to_byte(&texts[&fi], l.start.0, l.start.1) else { continue };
            let e = by_file.entry(fi).or_default();
            e.0.push(a);
            if l.is_def {
                e.1.push(l.start.0);
            }
        }
        let seen: HashSet<u32> = by_file.keys().copied().collect();
        self.ts_complete(t, &seen)?;
        let mut out: Vec<FileRefs> = by_file.into_iter().map(|(f, (a, d))| (f, a, d)).collect();
        out.sort_by(|a, b| self.files[a.0 as usize].path.cmp(&self.files[b.0 as usize].path));
        Ok(out)
    }

    /// tsserver's answer must cover every file the index links to `t` by its calls. A file it leaves out
    /// means tsserver could not resolve an import there (most often: dependencies not installed), and an
    /// edit from that answer would be silently incomplete; the syntax engine answers instead.
    fn ts_complete(&self, t: &Target, seen: &HashSet<u32>) -> Result<(), Fallback> {
        let mut missed: Vec<&str> = t
            .defs
            .iter()
            .flat_map(|&(f, s)| self.graph.callers(f, s))
            .map(|r| r.file)
            .filter(|f| !seen.contains(f))
            .map(|f| self.files[f as usize].path.as_str())
            .collect();
        missed.sort_unstable();
        missed.dedup();
        match missed.first() {
            None => Ok(()),
            Some(first) => Err(Fallback(format!(
                "tsserver did not see {} file(s) that call it (e.g. {first}); its imports there do not resolve, most often because dependencies are not installed (run the repo's install)",
                missed.len()
            ))),
        }
    }

    /// Lines naming `t` per file, from tsserver (for `context`).
    pub(crate) fn ts_sites(&self, t: &Target) -> Result<Vec<(u32, Vec<u32>)>, Fallback> {
        Ok(self
            .ts_refs(t)?
            .into_iter()
            .filter_map(|(f, starts, _)| {
                let src = self.checked_source(f).ok()?;
                let mut lines: Vec<u32> = starts.iter().map(|&a| crate::refactor::line_of(&src, a)).collect();
                lines.sort_unstable();
                lines.dedup();
                Some((f, lines))
            })
            .collect())
    }

    /// tsserver's errors in these repo files now, within the diagnostics time budget: (errors, files
    /// left unchecked). None when tsserver cannot answer or diagnostics are off.
    pub(crate) fn ts_errors(&self, rels: &[String]) -> Option<(Vec<Diag>, Vec<PathBuf>)> {
        let budget = diag_budget()?;
        let files: Vec<PathBuf> = rels.iter().map(|r| self.ts.root().join(r)).collect();
        self.ts.errors(&files, std::time::Instant::now() + budget).ok()
    }

    /// `<diagnostics>`: errors tsserver reports in the edited files after the edit that it did not
    /// report before. A message naming the new name counts as the old one renamed, not as new.
    pub(crate) fn diagnostics_block(&self, before: (Vec<Diag>, Vec<PathBuf>), after: (Vec<Diag>, Vec<PathBuf>), renamed: Option<(&str, &str)>) -> String {
        /* only files checked both times compare */
        let unchecked: HashSet<PathBuf> = before.1.iter().chain(&after.1).cloned().collect();
        let (before, after): (Vec<Diag>, Vec<Diag>) =
            (before.0.into_iter().filter(|d| !unchecked.contains(&d.file)).collect(), after.0.into_iter().filter(|d| !unchecked.contains(&d.file)).collect());
        let key = |d: &Diag, norm: bool| {
            let mut text = d.text.clone();
            if let (true, Some((old, new))) = (norm, renamed) {
                text = text.replace(&format!("'{new}'"), &format!("'{old}'"));
            }
            (d.file.clone(), d.code, text)
        };
        let mut seen: HashMap<(PathBuf, i64, String), usize> = HashMap::new();
        for d in &before {
            *seen.entry(key(d, false)).or_default() += 1;
        }
        let mut new = Vec::new();
        for d in &after {
            match seen.get_mut(&key(d, true)) {
                Some(n) if *n > 0 => *n -= 1,
                _ => new.push(d),
            }
        }
        let skipped = if unchecked.is_empty() { String::new() } else { format!(" unchecked=\"{}\"", unchecked.len()) };
        let open = format!("<diagnostics source=\"tsserver\" before=\"{}\" after=\"{}\" new=\"{}\"{skipped}>", before.len(), after.len(), new.len());
        let mut s = format!("{open}\n");
        if new.is_empty() {
            s.push_str("No new compiler errors in the edited files.\n");
        }
        for d in new.iter().take(MAX_LISTED_ERRORS) {
            let rel = self.ts_rel(&d.file).unwrap_or_else(|| d.file.display().to_string());
            s.push_str(&esc(&format!("{rel}:{} TS{} {}\n", d.line, d.code, d.text.lines().next().unwrap_or(""))));
        }
        if new.len() > MAX_LISTED_ERRORS {
            s.push_str(&format!("... {} more\n", new.len() - MAX_LISTED_ERRORS));
        }
        if !unchecked.is_empty() {
            let mut names: Vec<String> = unchecked.iter().map(|f| self.ts_rel(f).unwrap_or_else(|| f.display().to_string())).collect();
            names.sort();
            s.push_str(&esc(&format!("not checked within the time budget (AGENT_GRAFT_TS_DIAG_MS): {}\n", names.join(", "))));
        }
        s.push_str("</diagnostics>\n");
        s
    }
}

/// `<kept_name>`: re-exports and imports where tsserver keeps the old public name (`export { new as
/// old }`), as TypeScript's rename does; code importing that name is unchanged and still compiles.
pub(crate) fn alias_note(plan: &[FileEdits], old: &str) -> String {
    let (head, tail) = (format!("{old} as "), format!(" as {old}"));
    let sites: Vec<String> = plan
        .iter()
        .flat_map(|(p, src, edits)| {
            edits.iter().filter(|e| e.2.starts_with(&head) || e.2.ends_with(&tail)).map(move |e| format!("{p}:{}", crate::refactor::line_of(src, e.0)))
        })
        .collect();
    if sites.is_empty() {
        return String::new();
    }
    format!(
        "<kept_name sites=\"{}\">{}</kept_name>\n",
        sites.len(),
        esc(&format!("these re-exports/imports keep the old public name `{old}` (tsserver's rename keeps importers working): {}", sites.join(", ")))
    )
}

/// The name starting at byte `at` is the name of a function or method declaration (or of a variable
/// holding a function): its parameter list belongs to the target.
pub(crate) fn ts_declares(root: Node, at: usize) -> bool {
    let Some(n) = root.descendant_for_byte_range(at, at) else { return false };
    let Some(p) = n.parent() else { return false };
    let named = p.child_by_field_name("name") == Some(n);
    match p.kind() {
        "function_declaration" | "generator_function_declaration" | "function_signature" | "method_definition" | "method_signature"
        | "abstract_method_signature" => named,
        "variable_declarator" | "public_field_definition" => {
            named && p.child_by_field_name("value").is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression" | "function"))
        }
        _ => false,
    }
}

/// Argument lists of the calls whose callee name starts at one of `starts`: (list start, last argument end, None).
pub(crate) fn ts_call_sites(root: Node, starts: &HashSet<usize>) -> Vec<(usize, Option<usize>, Option<usize>)> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), "call_expression" | "new_expression") {
            let f = n.child_by_field_name("function").or_else(|| n.child_by_field_name("constructor"));
            let callee = f.and_then(|f| match f.kind() {
                "identifier" => Some(f),
                "member_expression" => f.child_by_field_name("property"),
                _ => None,
            });
            if let (Some(c), Some(args)) = (callee, n.child_by_field_name("arguments")) {
                if starts.contains(&c.start_byte()) && args.kind() == "arguments" {
                    let mut cur = args.walk();
                    let last = args.named_children(&mut cur).filter(|c| !c.kind().contains("comment")).last().map(|c| c.end_byte());
                    out.push((args.start_byte(), last, None));
                }
            }
        }
        let mut cur = n.walk();
        stack.extend(n.named_children(&mut cur));
    }
    out
}

#[cfg(test)]
#[path = "ts_refactor_tests.rs"]
mod tests;
