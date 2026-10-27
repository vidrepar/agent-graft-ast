//! `rewrite`: one call for a repo-wide mechanical edit (or a structural search when no replacement is given).
//! The matcher lives in `rewrite.rs`; this file picks the files, applies the edits and writes the reply.

use crate::engine::{esc, Engine};
use crate::guard::Refusal;
use crate::index::FileRecord;
use crate::lang::Lang;
use crate::parse::SymKind;
use crate::tree_cache::TreeCache;
use crate::refactor::{apply_edits, checked_bytes, line_of, package_of, write_all};
use crate::rewrite::{outermost, render, reparse_edited, var_names, Match, Pattern};
use rayon::prelude::*;
use std::collections::HashMap;

/// Matches listed in search mode.
const MAX_LISTED: usize = 50;
/// Files listed with their lines in a rewrite reply.
const MAX_FILES: usize = 30;
/// Before/after pairs shown for a rewrite.
const MAX_PREVIEW: usize = 5;

/// Language families a pattern can target, in a fixed order.
const FAMILIES: [(&str, &[Lang]); 15] = [
    ("rust", &[Lang::Rust]),
    ("typescript", &[Lang::TypeScript, Lang::Tsx, Lang::JavaScript]),
    ("python", &[Lang::Python]),
    ("go", &[Lang::Go]),
    ("java", &[Lang::Java]),
    ("csharp", &[Lang::CSharp]),
    ("c", &[Lang::C]),
    ("cpp", &[Lang::Cpp]),
    ("ruby", &[Lang::Ruby]),
    ("php", &[Lang::Php]),
    ("kotlin", &[Lang::Kotlin]),
    ("swift", &[Lang::Swift]),
    ("bash", &[Lang::Bash]),
    ("lua", &[Lang::Lua]),
    ("scala", &[Lang::Scala]),
];

fn family_of(lang: Lang) -> usize {
    FAMILIES.iter().position(|(_, ls)| ls.contains(&lang)).unwrap_or(0)
}

fn family_named(s: &str) -> Option<usize> {
    let lang = match s.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Lang::Rust,
        "typescript" | "ts" | "tsx" | "javascript" | "js" | "jsx" => Lang::TypeScript,
        "python" | "py" => Lang::Python,
        "go" | "golang" => Lang::Go,
        "java" => Lang::Java,
        "csharp" | "cs" | "c#" => Lang::CSharp,
        "c" => Lang::C,
        "cpp" | "c++" | "cxx" => Lang::Cpp,
        "ruby" | "rb" => Lang::Ruby,
        "php" => Lang::Php,
        "kotlin" | "kt" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "bash" | "sh" | "shell" => Lang::Bash,
        "lua" => Lang::Lua,
        "scala" => Lang::Scala,
        _ => return None,
    };
    Some(family_of(lang))
}

/// The last search or dry run, so the rewrite that usually follows need not parse every file again.
pub(crate) struct ScanCache {
    /// pattern, language families, path prefixes
    key: (String, Vec<usize>, Vec<String>),
    /// every candidate file with the (mtime, size) it had when scanned; any difference voids the cache
    files: Vec<(String, u64, u64)>,
    hits: Vec<Hits>,
    refused: Vec<(String, Refusal)>,
    /// a dry run's rendered and checked plan, applied as is by the same rewrite
    prepared: Option<Prepared>,
}

/// A rewrite planned by a dry run, checked after its reply (`Engine::after_reply`).
pub(crate) struct Prepared {
    template: String,
    plan: Plan,
    /// None until checked
    post: Option<Vec<Post>>,
    compiled: HashMap<Lang, Pattern>,
}

/// Per written file: lines where the pattern still matches, and the new whole-file tree for the re-index.
type Post = (Vec<u32>, Option<(Lang, tree_sitter::Tree)>);

/// A rewrite plan: new texts and edits per file, changed lines, preview, site count, and the sites left
/// alone because the replacement would drop code the pattern does not mention.
pub(crate) struct Plan {
    writes: Vec<(String, String)>,
    file_edits: Vec<Vec<(usize, usize, String)>>,
    changed: Vec<(String, Vec<u32>)>,
    preview: String,
    sites: usize,
    kept: Vec<(String, Vec<u32>)>,
    /// index in the hits of each written file
    idx: Vec<usize>,
}

/// The replacement for one match, or None when it would drop code the pattern leaves out (a comment,
/// a keyword such as `async`, an `else` after the pattern): commas may go, a trailing `;` is carried over.
fn render_site(template: &str, m: &Match, src: &str) -> Result<Option<String>, String> {
    let mut out = render(template, m)?;
    for &(a, b) in &m.dropped {
        match &src[a..b] {
            "," => {}
            ";" if b == m.end => {
                if !out.trim_end().ends_with(';') {
                    out.push(';');
                }
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(out))
}

/// The plan for every file: outermost matches only, each file rendered and applied in parallel.
/// Err names a metavariable the template uses but the pattern lacks.
fn plan(hits: &[Hits], template: &str) -> Result<Plan, String> {
    type FilePlan = (Vec<(usize, usize, String)>, Vec<u32>, String, Vec<u32>);
    let planned: Vec<FilePlan> = hits
        .par_iter()
        .map(|h| {
            let outer = outermost(&h.matches);
            let lines = lines_at(&h.src, outer.iter().map(|m| m.start));
            let (mut edits, mut done, mut kept) = (Vec::new(), Vec::new(), Vec::new());
            for (m, line) in outer.iter().zip(lines) {
                match render_site(template, m, &h.src)? {
                    Some(t) => {
                        edits.push((m.start, m.end, t));
                        done.push(line);
                    }
                    None => kept.push(line),
                }
            }
            let new = apply_edits(&h.src, edits.clone());
            Ok((edits, done, new, kept))
        })
        .collect::<Result<_, String>>()?;
    let mut p = Plan { writes: Vec::new(), file_edits: Vec::new(), changed: Vec::new(), preview: String::new(), sites: 0, kept: Vec::new(), idx: Vec::new() };
    for (i, (h, (edits, lines, new, kept))) in hits.iter().zip(planned).enumerate() {
        if !kept.is_empty() {
            p.kept.push((h.path.clone(), kept));
        }
        if edits.is_empty() {
            continue;
        }
        p.idx.push(i);
        let (writes, file_edits, changed, preview, sites) = (&mut p.writes, &mut p.file_edits, &mut p.changed, &mut p.preview, &mut p.sites);
        for ((s, e, t), line) in edits.iter().zip(&lines).take(MAX_PREVIEW.saturating_sub(*sites)) {
            preview.push_str(&format!("{}:{line}: {} -> {}\n", h.path, first_line(&h.src[*s..*e]), first_line(t)));
        }
        *sites += edits.len();
        changed.push((h.path.clone(), lines));
        writes.push((h.path.clone(), new));
        file_edits.push(edits);
    }
    Ok(p)
}

/// What the pattern still matches in each new text: the scan's tree re-parsed only where the edits landed
/// (a tree of some functions proves the pattern is gone but cannot feed the re-index).
fn check_planned(hits: &[Hits], idx: &[usize], file_edits: &[Vec<(usize, usize, String)>], news: &[(String, String)], compiled: &HashMap<Lang, Pattern>) -> Vec<Post> {
    idx.par_iter()
        .map(|&i| &hits[i])
        .zip(file_edits)
        .zip(news)
        .map(|((h, edits), (_, new))| {
            let pat = &compiled[&h.lang];
            match h.tree.clone().and_then(|t| reparse_edited(h.lang, t, &h.src, edits, new, h.ranges.as_deref())) {
                Some(t) => (lines_of(new, &pat.matches_in(&t, new)), h.ranges.is_none().then_some((h.lang, t))),
                None => (lines_of(new, &pat.find_all(new)), None),
            }
        })
        .collect()
}

/// Files where the pattern still matches after the rewrite, with those lines.
fn remaining(news: &[(String, String)], post: &[Post]) -> Vec<(String, Vec<u32>)> {
    news.iter().zip(post).filter(|(_, p)| !p.0.is_empty()).map(|((path, _), p)| (path.clone(), p.0.clone())).collect()
}

/// Scans whose hits span more files than this are not kept (memory: each keeps its text and tree).
const MAX_CACHED_FILES: usize = 5000;

/// (mtime_ns, size) of a file, (0, 0) when it cannot be read.
fn stamp(abs: &std::path::Path) -> (u64, u64) {
    std::fs::metadata(abs).map(|m| (crate::index::mtime_ns(&m), m.len())).unwrap_or((0, 0))
}

/// A file to scan: path, language, indexed hash, and its index in `Engine::files`.
type Candidate = (String, Lang, u64, usize);

/// One file's scan: its hits, or the reason it was refused.
type FileScan = Result<Hits, (String, Refusal)>;
/// Hits, refused files, and the (mtime, size) of every candidate as it was read.
type Scanned = (Vec<Hits>, Vec<(String, Refusal)>, Vec<(u64, u64)>);

/// Read and match every candidate in parallel: one read per file, skipped before parsing when it lacks
/// the pattern's words (see `Pattern::may_match`).
fn scan(root: &std::path::Path, files: &[Candidate], recs: &[FileRecord], trees: &TreeCache, compiled: &HashMap<Lang, Pattern>) -> Scanned {
    let scanned: Vec<((u64, u64), Option<FileScan>)> = files
        .par_iter()
        .map(|f| {
            let st = stamp(&root.join(&f.0));
            (st, (st != (0, 0)).then(|| scan_file(root, f, recs, trees, compiled)).flatten())
        })
        .collect();
    let mut hits = Vec::new();
    let mut refused = Vec::new();
    let mut stamps = Vec::with_capacity(scanned.len());
    for (st, r) in scanned {
        stamps.push(st);
        match r {
            Some(Ok(h)) => hits.push(h),
            Some(Err(x)) => refused.push(x),
            None => {}
        }
    }
    (hits, refused, stamps)
}

/// One candidate file's matches; None when it has none.
fn scan_file(root: &std::path::Path, (path, l, hash, rec): &Candidate, recs: &[FileRecord], trees: &TreeCache, compiled: &HashMap<Lang, Pattern>) -> Option<FileScan> {
    let pat = &compiled[l];
    let bytes = crate::index::with_bytes(&root.join(path), |b| pat.may_match(b).then(|| b.to_vec())).ok()??;
    /* the index's function spans describe this text only if it is what was indexed */
    let indexed = xxhash_rust::xxh3::xxh3_64(&bytes) == *hash;
    let src = match checked_bytes(bytes, *hash) {
        Ok(s) => s,
        Err(r) => return Some(Err((path.clone(), r))),
    };
    /* 1. a kept tree of this exact text: walk it, no parse */
    if let Some(t) = indexed.then(|| trees.get(path, *hash)).flatten() {
        let matches = pat.matches_in(&t, &src);
        return (!matches.is_empty()).then(|| Ok(Hits { path: path.clone(), src, family: family_of(*l), lang: *l, matches, tree: Some(t), ranges: None }));
    }
    /* 2. parse only the functions that can hold a match; 3. the whole file when that is not provably enough */
    let ranges = (indexed && pat.fits_in_functions()).then(|| pat.regions(src.as_bytes(), &function_spans(&recs[*rec]))).flatten();
    let partial = ranges.as_ref().and_then(|r| pat.find_in_ranges(&src, r));
    let (matches, tree, ranges) = match partial {
        Some((m, t)) => (m, Some(t), ranges),
        None => {
            let (m, t) = pat.find_all_tree(&src);
            if let (true, Some(t)) = (indexed, &t) {
                trees.put(path, *hash, t.clone(), src.len());
            }
            (m, t, None)
        }
    };
    (!matches.is_empty()).then(|| Ok(Hits { path: path.clone(), src, family: family_of(*l), lang: *l, matches, tree, ranges }))
}

/// Line spans of the outermost functions and methods of a file, sorted.
fn function_spans(rec: &FileRecord) -> Vec<(u32, u32)> {
    let mut fns: Vec<(u32, u32)> =
        rec.ex.symbols.iter().filter(|s| matches!(s.kind, SymKind::Function | SymKind::Method)).map(|s| (s.start_line, s.end_line)).collect();
    fns.sort_unstable_by_key(|&(a, b)| (a, std::cmp::Reverse(b)));
    let mut out: Vec<(u32, u32)> = Vec::new();
    for f in fns {
        if out.last().is_none_or(|o| f.0 > o.1) {
            out.push(f);
        }
    }
    out
}

/// One file's matches, with the text they were found in.
pub(crate) struct Hits {
    path: String,
    src: String,
    family: usize,
    lang: Lang,
    matches: Vec<Match>,
    tree: Option<tree_sitter::Tree>,
    /// the tree covers only these ranges (functions that can hold a match); None = the whole file
    ranges: Option<Vec<tree_sitter::Range>>,
}

/// Line numbers of matches, counting newlines once through the file.
fn lines_of(src: &str, ms: &[Match]) -> Vec<u32> {
    lines_at(src, ms.iter().map(|m| m.start))
}

/// Line numbers of ascending byte offsets, counting newlines once through the file.
fn lines_at(src: &str, starts: impl Iterator<Item = usize>) -> Vec<u32> {
    let (mut line, mut at) = (1u32, 0usize);
    starts
        .map(|s| {
            line += memchr::memchr_iter(b'\n', &src.as_bytes()[at..s.max(at)]).count() as u32;
            at = at.max(s);
            line
        })
        .collect()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// `path lines a,b` per file, the first MAX_FILES files only.
fn capped_lines(files: &[(String, Vec<u32>)]) -> String {
    let mut s = Engine::lines_block(&files[..files.len().min(MAX_FILES)]);
    if files.len() > MAX_FILES {
        let rest = &files[MAX_FILES..];
        s.push_str(&format!("... {} more files, {} lines\n", rest.len(), rest.iter().map(|f| f.1.len()).sum::<usize>()));
    }
    s
}

impl Engine {
    /// `rewrite(pattern, replacement?, lang?, paths?, dry_run?)`: structural search, or search and replace,
    /// over every indexed file of the pattern's language (or under `paths`).
    pub fn rewrite(&mut self, pattern: &str, replacement: Option<&str>, lang: Option<&str>, paths: &[String], dry_run: bool) -> String {
        let budget = 4000;
        let err = |e: &mut Engine, code: &str, msg: &str| {
            let open = format!("<rewrite pattern=\"{}\" error=\"{code}\">", esc(pattern));
            e.finish(&open, "</rewrite>", &format!("{}\n", esc(msg)), budget, "")
        };

        /* 1. scope: paths (refused like any read), and the language families to try */
        let mut prefixes: Vec<String> = Vec::new();
        let mut path_families: Vec<usize> = Vec::new();
        for p in paths {
            let rel = match self.resolve_rel(p) {
                Ok(r) => r,
                Err(r) => return self.refusal(p, r),
            };
            if let Some(l) = Lang::from_path(std::path::Path::new(&rel)) {
                if let Err(r) = self.ensure_fresh(&rel) {
                    return self.refusal(&rel, r);
                }
                path_families.push(family_of(l));
            }
            prefixes.push(if rel == "." { String::new() } else { rel });
        }
        path_families.sort_unstable();
        path_families.dedup();
        let families: Vec<usize> = match lang {
            Some(l) => match family_named(l) {
                Some(f) => vec![f],
                None => return err(self, "invalid_lang", "lang must be one of rust, typescript, javascript, python, go, java, csharp, c, cpp, ruby, php, kotlin, swift, bash, lua, scala"),
            },
            None if !path_families.is_empty() => path_families,
            None => (0..FAMILIES.len()).collect(),
        };
        let mut compiled: HashMap<Lang, Pattern> = HashMap::new();
        let mut why = String::new();
        for &f in &families {
            for &l in FAMILIES[f].1 {
                match Pattern::compile(pattern, l) {
                    Ok(p) => {
                        compiled.insert(l, p);
                    }
                    Err(e) if why.is_empty() => why = e,
                    Err(_) => {}
                }
            }
        }
        if compiled.is_empty() {
            return err(self, "invalid_pattern", &why);
        }
        if let Some(t) = replacement {
            let bound = var_names(pattern);
            if let Some(n) = var_names(t).into_iter().find(|n| !bound.contains(n)) {
                return err(self, "invalid_replacement", &format!("replacement uses ${n}, which the pattern does not bind"));
            }
        }

        /* 2. match every candidate file in parallel, or reuse the last scan of this pattern if no file changed */
        let under = |path: &str| prefixes.is_empty() || prefixes.iter().any(|p| p.is_empty() || path == p || path.starts_with(&format!("{p}/")));
        let files: Vec<Candidate> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| compiled.contains_key(&f.lang) && under(&f.path))
            .map(|(i, f)| (f.path.clone(), f.lang, f.hash, i))
            .collect();
        let root = self.root.clone();
        let key = (pattern.to_string(), families.clone(), prefixes.clone());
        /* stamps are taken only to check a cached scan of this key; a fresh scan takes them as it reads */
        let reuse = self.scan_cache.take().and_then(|c| {
            let now: Vec<(u64, u64)> =
                if c.key == key && c.files.len() == files.len() { files.par_iter().map(|(p, _, _, _)| stamp(&root.join(p))).collect() } else { Vec::new() };
            if !now.is_empty() && c.files.iter().zip(&files).zip(&now).all(|((c, f), n)| c.0 == f.0 && (c.1, c.2) == *n) {
                return Some((c, now));
            }
            /* a stale scan holds hundreds of trees: free them off the reply path */
            rayon::spawn(move || drop(c));
            None
        });
        let (mut hits, refused, now, prepared) = match reuse {
            Some((c, now)) => (c.hits, c.refused, now, c.prepared),
            None => {
                let (h, r, n) = scan(&root, &files, &self.files, &self.trees, &compiled);
                (h, r, n, None)
            }
        };
        hits.sort_by(|a, b| a.path.cmp(&b.path));

        /* 3. the pattern parses in several languages and matches in more than one: ask */
        let mut per_family = [0usize; FAMILIES.len()];
        for h in &hits {
            per_family[h.family] += h.matches.len();
        }
        if per_family.iter().filter(|n| **n > 0).count() > 1 {
            let counts: Vec<String> = per_family.iter().enumerate().filter(|(_, n)| **n > 0).map(|(f, n)| format!("{}: {n}", FAMILIES[f].0)).collect();
            return err(self, "ambiguous_lang", &format!("pattern matches in several languages ({}); pass lang", counts.join(", ")));
        }
        let lang_name = per_family.iter().position(|n| *n > 0).map(|f| FAMILIES[f].0).unwrap_or(FAMILIES[families[0]].0);
        let total: usize = hits.iter().map(|h| h.matches.len()).sum();

        let stamps: Vec<(String, u64, u64)> = files.iter().zip(&now).map(|((p, _, _, _), n)| (p.clone(), n.0, n.1)).collect();
        let Some(template) = replacement else {
            let r = self.search_reply(pattern, lang_name, &hits, total, &refused);
            self.keep_scan(ScanCache { key, files: stamps, hits, refused, prepared: None });
            return r;
        };
        if let Some((path, r)) = refused.first().cloned() {
            return self.refusal(&path, r);
        }

        /* 4. the plan: a dry run of this exact rewrite made it already, else render it now */
        let prepared = prepared.filter(|p| p.template == template);
        let (planned, post) = match prepared {
            Some(p) => (p.plan, p.post),
            None => match plan(&hits, template) {
                Ok(p) => (p, None),
                Err(e) => return err(self, "invalid_replacement", &e),
            },
        };
        let Plan { writes, file_edits, changed, preview, sites, kept, idx } = planned;
        let n_files = writes.len();
        let written: Vec<String> = writes.iter().map(|w| w.0.clone()).collect();
        let applied = !dry_run && !writes.is_empty();
        let mut verified = String::new();
        if applied {
            let resolved = match self.resolve_writes(writes) {
                Ok(r) => r,
                Err(e) => return err(self, "write_failed", &e),
            };
            let root = self.root.clone();
            /* checked by the dry run: only write; else write while checking the new text */
            let (wrote, post) = match post {
                Some(p) => (write_all(&root, &resolved), p),
                None => rayon::join(|| write_all(&root, &resolved), || check_planned(&hits, &idx, &file_edits, &resolved, &compiled)),
            };
            verified = verify_reply(&remaining(&resolved, &post), n_files, false);
            if let Err(e) = self.after_write(resolved, wrote, post.into_iter().map(|p| p.1).collect()) {
                return err(self, "write_failed", &e);
            }
            /* freeing hundreds of syntax trees takes tens of ms: do it off the reply path */
            rayon::spawn(move || drop(hits));
        } else if dry_run {
            /* the usual next call applies this plan: keep it with the scan; `after_reply` checks it */
            let plan = Plan { writes, file_edits, changed: changed.clone(), preview: preview.clone(), sites, kept: kept.clone(), idx };
            let prepared = Prepared { template: template.to_string(), plan, post, compiled };
            self.keep_scan(ScanCache { key, files: stamps, hits, refused, prepared: Some(prepared) });
        }
        let mut body = capped_lines(&changed);
        if !preview.is_empty() {
            body.push_str(&format!("<preview>\n{}</preview>\n", esc(&preview)));
        }
        body.push_str(&verified);
        if !kept.is_empty() {
            body.push_str(&format!(
                "<skipped reason=\"the replacement would drop code the pattern does not mention (a comment, a keyword such as async, or what follows the pattern such as an else branch); edit these by hand or extend the pattern\">\n{}</skipped>\n",
                capped_lines(&kept)
            ));
        }
        if applied {
            body.push_str("<next>Done: files are written and re-indexed. No Read, Edit or Grep is needed for these sites.</next>\n");
        } else if sites > 0 {
            body.push_str("<next>Plan only: call again without dry_run to apply.</next>\n");
        } else if !kept.is_empty() {
            body.push_str("<next>Nothing to write: every match is listed in <skipped>.</next>\n");
        } else {
            body.push_str("<next>No match: check the pattern against a real line (find_symbol / context), or try without lang.</next>\n");
        }
        let mut pkgs: Vec<&str> = written.iter().map(|p| package_of(p)).collect();
        pkgs.sort_unstable();
        pkgs.dedup();
        let level = if sites > 10 || pkgs.len() > 2 {
            "high"
        } else if sites <= 3 && pkgs.len() <= 1 {
            "low"
        } else {
            "medium"
        };
        let open = format!(
            "<rewrite pattern=\"{}\" replacement=\"{}\" lang=\"{lang_name}\" applied=\"{applied}\" files=\"{n_files}\" sites=\"{sites}\" impact=\"{level}\" why=\"{sites} sites in {n_files} files across {} packages\">",
            esc(pattern),
            esc(template),
            pkgs.len()
        );
        self.finish(&open, "</rewrite>", &body, budget, "")
    }

    /// Work that waits until a reply is sent: re-index written files, and check a dry run's plan so
    /// applying it only writes.
    pub fn after_reply(&mut self) {
        self.reindex_deferred();
        if let Some(c) = self.scan_cache.as_mut() {
            if let Some(p) = c.prepared.as_mut().filter(|p| p.post.is_none()) {
                p.post = Some(check_planned(&c.hits, &p.plan.idx, &p.plan.file_edits, &p.plan.writes, &p.compiled));
            }
        }
    }

    /// Keep a scan for the next call; the one it replaces is freed off the reply path.
    fn keep_scan(&mut self, c: ScanCache) {
        let old = if c.hits.len() <= MAX_CACHED_FILES { self.scan_cache.replace(c) } else { self.scan_cache.take() };
        rayon::spawn(move || drop(old));
    }

    fn search_reply(&mut self, pattern: &str, lang: &str, hits: &[Hits], total: usize, refused: &[(String, Refusal)]) -> String {
        let mut body = String::new();
        for h in hits {
            for m in &h.matches {
                if body.lines().count() >= MAX_LISTED {
                    break;
                }
                body.push_str(&format!("{}:{}: {}\n", h.path, line_of(&h.src, m.start), first_line(&h.src[m.start..m.end])));
            }
        }
        let listed = body.lines().count();
        let mut body = esc(&body);
        if total > listed {
            body.push_str(&format!("... {} more\n", total - listed));
        }
        if !refused.is_empty() {
            let names: Vec<&str> = refused.iter().take(5).map(|r| r.0.as_str()).collect();
            body.push_str(&format!("<refused files=\"{}\">{}</refused>\n", refused.len(), esc(&names.join(", "))));
        }
        body.push_str(if total > 0 {
            "<next>To change them all, call rewrite again with the same pattern and a replacement.</next>\n"
        } else {
            "<next>No match: check the pattern against a real line, or pass lang.</next>\n"
        });
        let open = format!("<rewrite pattern=\"{}\" lang=\"{lang}\" mode=\"search\" matches=\"{total}\" files=\"{}\">", esc(pattern), hits.len());
        self.finish(&open, "</rewrite>", &body, 4000, "")
    }

}

/// What the pattern still matches in the new text of the written (or, for a dry run, planned) files.
fn verify_reply(left: &[(String, Vec<u32>)], written: usize, planned: bool) -> String {
    if left.is_empty() {
        let what = if planned { "planned" } else { "written" };
        return format!("<verified>Re-parsed the {written} {what} files: 0 matches of the pattern remain. No Grep needed.</verified>\n");
    }
    let n: usize = left.iter().map(|l| l.1.len()).sum();
    format!(
        "<remaining matches=\"{n}\" reason=\"a match nested inside a rewritten one, a site listed in skipped, or the replacement matches the pattern itself; call again if they must change\">\n{}</remaining>\n",
        capped_lines(left)
    )
}

#[cfg(test)]
#[path = "rewrite_tests.rs"]
mod tests;
