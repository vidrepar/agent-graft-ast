//! `context`: what an agent needs to change one symbol, in one call, trimmed to the budget.
//!
//! The reply is built so the agent never reads whole files: the definition's code, then every line
//! where the name occurs (definition, calls, imports, type uses), grouped per file with the exact
//! `Read offset/limit` window that covers them plus one line on each side. The agent reads those
//! windows (Claude Code's `Edit` only needs a prior `Read` of the file, not a full one) and edits.

use crate::engine::{cdata, esc, Engine};
use crate::guard::{self, Refusal};
use crate::index;
use crate::lang::Lang;
use crate::parse::DirectiveKind;
use crate::summary;
use crate::refactor::Target;
use crate::tokens::count;
use crate::ts_exact::Fallback;
use crate::ts_refactor::Exact;
use rayon::prelude::*;

/// The target's code may take this share of the budget before it is folded to its signature.
const CODE_SHARE_PCT: usize = 50;
/// Occurrences closer than this many lines share one Read window.
const WINDOW_GAP: u32 = 6;
/// Files shorter than this are read whole.
const WHOLE_FILE_LINES: u32 = 150;
/// Files whose windows cover more than this share of their lines are read whole.
const WHOLE_FILE_COVER_PCT: u32 = 60;
/// Paths listed for files beyond the budget.
const MAX_LISTED_PATHS: usize = 20;
/// Lines of surrounding context on each side of an occurrence.
const AROUND: u32 = 1;

/// The token budget left while building a reply, kept without loading the tokenizer (~30 ms in a cold process)
/// as long as no decision needs exact counts. A token is at least one byte, so text whose byte length fits
/// the lower bound (exact spend plus the bytes of what is not counted yet) fits exactly too; only a close
/// call counts the pending text. Every decision equals the one exact counting makes.
struct Left {
    exact: usize,
    pending: Vec<String>,
    pending_bytes: usize,
    /// count as late as possible (the tokenizer was not loaded when the reply started)
    lazy: bool,
}

impl Left {
    fn new(n: usize) -> Left {
        Left { exact: n, pending: Vec::new(), pending_bytes: 0, lazy: !crate::tokens::ready() }
    }
    fn spend(&mut self, text: &str, extra: usize) {
        self.exact = self.exact.saturating_sub(extra);
        if !self.lazy {
            self.exact = self.exact.saturating_sub(count(text));
        } else {
            self.pending_bytes += text.len();
            self.pending.push(text.to_string());
        }
    }
    fn settle(&mut self) -> usize {
        for t in self.pending.drain(..) {
            self.exact = self.exact.saturating_sub(count(&t));
        }
        self.pending_bytes = 0;
        self.exact
    }
    /// `count(text) * scale <= left * pct`, counting only when the byte bound cannot decide.
    fn fits(&mut self, text: &str, scale: usize, pct: usize) -> bool {
        let lo = self.exact.saturating_sub(self.pending_bytes);
        if text.len() * scale <= lo * pct {
            return true;
        }
        let left = self.settle();
        count(text) * scale <= left * pct
    }
}

/// Lines `a..=b` (1-based) of a file, without scanning past them.
fn line_range(bytes: &[u8], a: u32, b: u32) -> String {
    let mut start = 0usize;
    for _ in 1..a.max(1) {
        match memchr::memchr(b'\n', &bytes[start..]) {
            Some(i) => start += i + 1,
            None => return String::new(),
        }
    }
    let mut end = start;
    for _ in a.max(1)..=b.max(a) {
        match memchr::memchr(b'\n', &bytes[end..]) {
            Some(i) => end += i + 1,
            None => {
                end = bytes.len();
                break;
            }
        }
    }
    String::from_utf8_lossy(&bytes[start..end]).trim_end_matches('\n').to_string()
}

pub(crate) fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Lines (1-based, ascending, one per line) with a whole-word occurrence of `name` in `bytes`.
fn occurrences(bytes: &[u8], name: &str) -> Vec<u32> {
    let finder = memchr::memmem::Finder::new(name.as_bytes());
    let mut out: Vec<u32> = Vec::new();
    let (mut line, mut scanned) = (1u32, 0usize);
    for pos in finder.find_iter(bytes) {
        let end = pos + name.len();
        let left_ok = pos == 0 || !is_ident(bytes[pos - 1]);
        let right_ok = end >= bytes.len() || !is_ident(bytes[end]);
        if !(left_ok && right_ok) {
            continue;
        }
        line += memchr::memchr_iter(b'\n', &bytes[scanned..pos]).count() as u32;
        scanned = pos;
        if out.last() != Some(&line) {
            out.push(line);
        }
    }
    out
}

/// Read windows (offset = first line, limit = line count) covering `lines` with `AROUND` lines of context.
fn windows(lines: &[u32]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for &l in lines {
        let (a, b) = (l.saturating_sub(AROUND).max(1), l + AROUND);
        match out.last_mut() {
            Some((_, e)) if a <= *e + WINDOW_GAP => *e = (*e).max(b),
            _ => out.push((a, b)),
        }
    }
    out.into_iter().map(|(a, b)| (a, b - a + 1)).collect()
}

pub(crate) fn same_family(a: Lang, b: Lang) -> bool {
    a == b || (a.is_js_family() && b.is_js_family())
}

impl Engine {
    /// Every line that names `name`, per file in path order, from the summary cache's identifier index
    /// (no file reads). Files without a cached summary yet (warm-up still running) are scanned directly.
    pub(crate) fn sites(&self, name: &str, lang: Lang) -> Vec<(u32, Vec<u32>)> {
        summary::load_persisted(&self.files, &self.store, &self.cache);
        let h = summary::ident_hash(name.as_bytes());
        let mut out: Vec<(u32, Vec<u32>)> = Vec::new();
        let mut scan: Vec<u32> = Vec::new();
        for (i, f) in self.files.iter().enumerate() {
            if f.hash == 0 || !same_family(f.lang, lang) {
                continue;
            }
            match summary::get(&self.cache, f) {
                Some(s) => {
                    let lines = s.lines_of(h);
                    if !lines.is_empty() {
                        out.push((i as u32, lines));
                    }
                }
                None => scan.push(i as u32),
            }
        }
        let root = &self.root;
        let scanned: Vec<(u32, Vec<u32>)> = scan
            .par_iter()
            .filter_map(|&i| {
                let f = &self.files[i as usize];
                let lines = index::with_bytes(&root.join(&f.path), |b| {
                    /* bytes that changed since parsing must pass the content guard again */
                    if xxhash_rust::xxh3::xxh3_64(b) != f.hash && guard::check_content(b).is_some() {
                        return Vec::new();
                    }
                    occurrences(b, name)
                })
                .ok()?;
                (!lines.is_empty()).then_some((i, lines))
            })
            .collect();
        out.extend(scanned);
        out.sort_by(|a, b| self.files[a.0 as usize].path.cmp(&self.files[b.0 as usize].path));
        out
    }

    /// `target` = `name`, `path:name`, or a file path (falls back to a level-1 file read).
    pub fn context(&mut self, target: &str, budget: usize) -> String {
        self.context_with(target, budget, false)
    }

    /// `context` for a cold one-shot process (a hook), in ~1 ms instead of ~30-200: the budget receipt may be a
    /// flagged estimate (the tokenizer loads only when trimming to the budget needs exact counts), and TS/JS
    /// sites come from the syntax index instead of starting tsserver. Rust/Python/Go/... replies are the same.
    pub fn context_quick(&mut self, target: &str, budget: usize) -> String {
        self.context_with(target, budget, true)
    }

    fn context_with(&mut self, target: &str, budget: usize, quick: bool) -> String {
        /* 1. resolve the target */
        let (path, name) = match target.rsplit_once(':') {
            Some((p, n)) if p.contains('.') || p.contains('/') => (Some(p), n),
            _ => (None, target),
        };
        if let Some(p) = path {
            match self.resolve_rel(p) {
                Ok(rel) => {
                    if let Err(r) = self.ensure_fresh(&rel) {
                        return self.refusal(&rel, r);
                    }
                }
                Err(r) => return self.refusal(p, r),
            }
        }
        let found = self.find_symbols(name, path);
        let Some(&(fi, _)) = found.first() else {
            if path.is_none() && (target.contains('/') || target.contains('.')) {
                return self.read_file(target, 1, budget);
            }
            let open = format!("<context target=\"{}\" found=\"false\">", esc(target));
            let hint = self.near_names(name);
            return self.finish(&open, "</context>", &hint, budget, "");
        };
        let rel = self.files[fi as usize].path.clone();
        if let Err(r) = self.ensure_fresh(&rel) {
            return self.refusal(&rel, r);
        }
        /* the refresh may have moved symbols; look again */
        let found = self.find_symbols(name, Some(&rel));
        let Some(&(fi, si)) = found.first() else {
            return self.refusal(&rel, Refusal::NotFound);
        };
        let f = &self.files[fi as usize];
        let sym = f.ex.symbols[si as usize].clone();
        let lang = f.lang;
        /* serve only bytes identical to what was checked at parse time (a write between stat and read is refused) */
        let hash = f.hash;
        let read = index::with_bytes(&self.root.join(&rel), |b| {
            (xxhash_rust::xxh3::xxh3_64(b) == hash).then(|| line_range(b, sym.start_line, sym.end_line))
        });
        let Ok(Some(full)) = read else {
            return self.refusal(&rel, Refusal::NotFound);
        };

        let mut left = Left::new(budget.saturating_sub(120));
        let mut body = String::new();

        /* 2. rules and hints on the target, and recent changes to it */
        let mut dirs = Vec::new();
        for d in &f.ex.directives {
            if d.line >= sym.start_line.saturating_sub(3) && d.line <= sym.end_line {
                let kind = if d.kind == DirectiveKind::Rule { "rule" } else { "hint" };
                dirs.push(format!("{kind} L{}: {}", d.line, d.text));
            }
        }
        if let Some(cs) = self.sig_changes().get(&rel) {
            for c in cs.iter().filter(|c| c.names.iter().any(|n| *n == sym.name)) {
                dirs.push(format!("changed in {}: {}", c.commit, c.summary));
            }
        }
        if !dirs.is_empty() {
            let d = format!("<directives>\n{}\n</directives>\n", esc(&dirs.join("\n")));
            left.spend(&d, 0);
            body.push_str(&d);
        }

        /* 3. the definition: full code, or its signature when it would eat the budget */
        let (code, folded) = if left.fits(&full, 100, CODE_SHARE_PCT) { (full, false) } else { (sym.sig.clone(), true) };
        left.spend(&code, 10);
        let attr = if folded { " folded=\"true\"" } else { "" };
        body.push_str(&format!("<code{attr}>{}</code>\n", cdata(&code)));

        /* 4. every line that names the symbol, per file: the Read windows and the line numbers (the agent
         * reads the windows anyway, so the lines' text is not repeated here) */
        let mut exact = if quick { Exact::NotApplicable } else { self.ts_mode(lang) };
        let sites = match exact {
            Exact::Yes => {
                let t = Target { name: sym.name.clone(), lang, defs: vec![(fi, si)], method: sym.kind == crate::parse::SymKind::Method };
                match self.ts_sites(&t) {
                    Ok(s) => s,
                    Err(Fallback(why)) => {
                        exact = Exact::No(why);
                        self.sites(&sym.name, lang)
                    }
                }
            }
            _ => self.sites(&sym.name, lang),
        };
        body.push_str(&exact.note());
        let total: usize = sites.iter().map(|s| s.1.len()).sum();
        let mut block = String::new();
        let mut shown = 0;
        for (i, lines) in &sites {
            let p = &self.files[*i as usize].path;
            let wins = windows(lines);
            let covered: u32 = wins.iter().map(|w| w.1).sum();
            let file_lines = self.files[*i as usize].ex.lines.max(1);
            /* small files, or windows covering most of a file: one whole read is cheaper than several windows */
            let read = if file_lines < WHOLE_FILE_LINES || covered * 100 > file_lines * WHOLE_FILE_COVER_PCT {
                "whole file".to_string()
            } else {
                wins.iter().map(|(o, l)| format!("offset={o} limit={l}")).collect::<Vec<_>>().join("; ")
            };
            let ls: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            let part = esc(&format!("{p} [Read {read}] lines {}\n", ls.join(",")));
            if !left.fits(&part, 1, 1) {
                break;
            }
            left.spend(&part, 0);
            block.push_str(&part);
            shown += 1;
        }
        if shown < sites.len() {
            let rest: Vec<&str> = sites[shown..].iter().take(MAX_LISTED_PATHS).map(|(i, _)| self.files[*i as usize].path.as_str()).collect();
            block.push_str(&esc(&format!("+{} more files: {}\n", sites.len() - shown, rest.join(", "))));
        }
        body.push_str(&format!("<sites n=\"{total}\" files=\"{}\">\n{block}</sites>\n", sites.len()));

        /* other definitions with this name: a choice card, not a silent pick */
        if found.len() > 1 && path.is_none() {
            body.push_str(&self.choice_card(&sym.name, &found, "context"));
        }
        let impact = self.impact(fi, si);
        let open = format!(
            "<context target=\"{}\" path=\"{}\" lines=\"{}-{}\" kind=\"{}\" impact=\"{}\" why=\"{}\" root=\"{}\"{}>",
            esc(&sym.name),
            esc(&rel),
            sym.start_line,
            sym.end_line,
            sym.kind.as_str(),
            impact.level,
            esc(&impact.reason),
            esc(&self.root.to_string_lossy()),
            exact.attr()
        );
        if !quick {
            crate::tokens::count("");
        }
        self.finish(&open, "</context>", &body, budget, "")
    }

    /// Up to 5 symbol names close to a missing one.
    pub(crate) fn near_names(&self, name: &str) -> String {
        let q = name.to_lowercase();
        let mut v: Vec<&String> = self.graph.by_name.keys().filter(|k| k.to_lowercase().contains(&q)).collect();
        v.sort_by_key(|k| (k.len(), k.as_str()));
        if v.is_empty() {
            return String::new();
        }
        let names: Vec<&str> = v.iter().take(5).map(|s| s.as_str()).collect();
        format!("<did_you_mean>{}</did_you_mean>\n", esc(&names.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn repo() -> (tempfile::TempDir, Engine) {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("src")).unwrap();
        fs::write(r.join("src/db.ts"), "export function find(id: string): Row {\n  return q(id);\n}\nexport function q(s: string) {}\n").unwrap();
        fs::write(
            r.join("src/svc.ts"),
            "import { find } from './db';\n// @agent-rule: keep getUser pure\nexport function getUser(id: string): User {\n  const row = find(id);\n  return toUser(row);\n}\nexport function toUser(r: Row): User { return r; }\n",
        )
        .unwrap();
        let mut api = String::from("import { getUser } from './svc';\nexport function handler(req: Req) {\n  return getUser(req.id);\n}\n");
        for i in 0..20 {
            api.push_str(&format!("// filler {i}\n"));
        }
        api.push_str("export function other() { getUser('x'); getUser('y'); getUsers(); }\n");
        fs::write(r.join("src/api.ts"), api).unwrap();
        fs::write(r.join("lib.rs"), "fn getUser() {}\n").unwrap();
        let (e, _, _) = Engine::open(r, Some(&r.join("i.redb"))).unwrap();
        (d, e)
    }

    #[test]
    fn lazy_budget_decides_like_exact_counting() {
        let parts: Vec<String> = (0..60)
            .map(|i| match i % 4 {
                0 => format!("src/very/long/path/number_{i}/file_{i}.rs [Read offset={i} limit=3] lines {i},{}\n", i * 7),
                1 => "fn f() { a.b().c(); } // ünïcödé ✓ 漢字\n".repeat(i % 9 + 1),
                2 => " ".repeat(i * 13),
                _ => format!("x{i}\n"),
            })
            .collect();
        for budget in [0, 5, 40, 300, 900, 2500, 4000] {
            for pct in [1, 50] {
                /* exact: the original arithmetic */
                let (mut left, mut kept_exact) = (budget, Vec::new());
                for p in &parts {
                    let n = count(p);
                    kept_exact.push(n * 100 <= left * pct);
                    if n > left {
                        break;
                    }
                    left -= n;
                }
                let mut l = Left { exact: budget, pending: Vec::new(), pending_bytes: 0, lazy: true };
                let mut kept_lazy = Vec::new();
                for p in &parts {
                    kept_lazy.push(l.fits(p, 100, pct));
                    if !l.fits(p, 1, 1) {
                        break;
                    }
                    l.spend(p, 0);
                }
                assert_eq!(kept_exact, kept_lazy, "budget {budget} pct {pct}");
                assert_eq!(l.settle(), left, "budget {budget}");
            }
        }
    }

    #[test]
    fn quick_context_matches_context_outside_ts() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("src")).unwrap();
        fs::write(r.join("src/a.rs"), "pub struct Repo;\nimpl Repo {\n    pub fn get_by_id(&self) -> u64 { 1 }\n}\n").unwrap();
        fs::write(r.join("src/b.rs"), "use crate::a::Repo;\n// get_by_id is cheap\npub fn f(r: &Repo) -> u64 { r.get_by_id() }\n").unwrap();
        let db = r.join("i.redb");
        let (e, _, _) = Engine::open(r, Some(&db)).unwrap();
        e.warm();
        drop(e);
        let strip = |s: &str| s.lines().filter(|l| !l.starts_with("<budget_receipt")).collect::<Vec<_>>().join("\n");
        let mut cold = Engine::open_fast_with(r, Some(&db), true).unwrap().unwrap();
        assert!(!cold.cache.read().unwrap().is_empty(), "persisted summaries are loaded with the index");
        let quick = cold.context_quick("get_by_id", 4000);
        drop(cold);
        let (mut full, _, _) = Engine::open(r, Some(&db)).unwrap();
        assert_eq!(strip(&quick), strip(&full.context("get_by_id", 4000)));
        assert!(quick.contains("src/b.rs [Read whole file] lines 2,3"), "{quick}");
    }

    #[test]
    fn sites_cover_every_occurrence_with_read_windows() {
        let (_d, mut e) = repo();
        let out = e.context("getUser", 4000);
        assert!(out.starts_with("<context target=\"getUser\" path=\"src/svc.ts\" lines=\"3-6\" kind=\"function\" impact=\"low\" why=\"2 callers in 1 files across 1 packages; public\" root=\""), "{out}");
        assert!(out.contains("rule L2: keep getUser pure"), "{out}");
        assert!(out.contains("<code>\nexport function getUser(id: string): User {\n  const row = find(id);"), "{out}");
        /* import + call are 2 lines apart: one window; the far call gets its own; `getUsers` is not a match */
        assert!(out.contains("<sites n=\"5\" files=\"2\">\nsrc/api.ts [Read whole file] lines 1,3,25\nsrc/svc.ts [Read whole file] lines 2,3\n</sites>"), "{out}");
        /* other languages are not searched */
        assert!(!out.contains("lib.rs"), "{out}");
        assert!(!out.contains("\n  <"), "no indentation: {out}");
    }

    #[test]
    fn tight_budget_folds_code_and_lists_remaining_files() {
        /* exact token counts: a busy test run can still be loading the tokenizer, and estimates run high */
        crate::tokens::warm();
        let (_d, mut e) = repo();
        let out = e.context("src/svc.ts:getUser", 150);
        let used: usize = out.split("tokens_used_this_call=\"").nth(1).unwrap().split('"').next().unwrap().parse().unwrap();
        /* the random temp dir name in root="..." tokenizes to a varying count: leave it out */
        let root = out.split("root=\"").nth(1).unwrap().split('"').next().unwrap();
        let used = used - crate::tokens::count(root);
        assert!(used <= 150, "{used}: {out}");
        assert!(out.contains("more files"), "{out}");
    }

    #[test]
    fn missing_symbol_suggests_names_and_files_fall_back_to_read() {
        let (_d, mut e) = repo();
        let out = e.context("getUsr", 1000);
        assert!(out.contains("found=\"false\""));
        let out = e.context("User", 1000);
        assert!(out.contains("<did_you_mean>toUser, getUser</did_you_mean>"), "{out}");
        assert!(e.context("src/db.ts", 1000).starts_with("<code_intelligence_context path=\"src/db.ts\""));
    }

    #[test]
    fn large_files_get_windows() {
        let d = tempfile::tempdir().unwrap();
        let mut big = String::from("export function target() {}\n");
        for i in 0..300 {
            big.push_str(&format!("export const v{i} = {i};\n"));
        }
        big.push_str("target();\n");
        fs::write(d.path().join("big.ts"), big).unwrap();
        let (mut e, _, _) = Engine::open(d.path(), Some(&d.path().join("i.redb"))).unwrap();
        let out = e.context("target", 4000);
        assert!(out.contains("big.ts [Read offset=1 limit=2; offset=301 limit=3] lines 1,302"), "{out}");
    }

    #[test]
    fn windows_merge_nearby_lines() {
        assert_eq!(windows(&[1, 3]), vec![(1, 4)]);
        assert_eq!(windows(&[10, 30]), vec![(9, 3), (29, 3)]);
        assert_eq!(occurrences(b"a ab ba a_b\nab a", "a"), vec![1, 2]);
    }
}
