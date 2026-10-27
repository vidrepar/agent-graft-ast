//! The engine: owns the index + graph and renders section-3 XML payloads with budget receipts.

use crate::gitsig::{self, SigChange};
use crate::graph::{Graph, Ref};
use crate::guard::{self, Refusal};
use crate::index::{self, FileRecord, IndexStats};
use crate::lang::Lang;
use crate::parse::{self, DirectiveKind, SymKind};
use crate::skeleton;
use crate::store::{self, Store, StoreMsg};
use crate::tokens::count;
use anyhow::Result;
use once_cell::sync::OnceCell;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use crate::summary;
use std::time::Instant;

pub const DEFAULT_CALL_BUDGET: usize = 8_000;
pub const DEFAULT_SESSION_BUDGET: usize = 200_000;
const MAX_LISTED_EDGES: usize = 8;
const MAX_DIRECTIVES: usize = 12;
/// Callees listed with a full signature at levels 0-1; the rest are grouped names.
const MAX_SIGNATURES: usize = 5;
/// Caller/callee files listed per direction in a file read (level 0-1, level 2), ranked by call count.
const TOP_N: usize = 5;
const TOP_N_API: usize = 3;
/// Symbol names listed per caller/callee file before `+N`.
const MAX_NAMES: usize = 4;
/// Share of the call budget that directives + graph may take before they are trimmed.
const EXTRAS_SHARE_PCT: usize = 25;
/// Tags, CDATA wrapper and receipt.
const WRAPPER_TOKENS: usize = 70;
pub(crate) const GIT_COMMITS_SCANNED: usize = 200;

pub struct Timings {
    pub load_ms: f64,
    pub index_ms: f64,
    pub graph_ms: f64,
    pub save_ms: f64,
}

pub struct Engine {
    pub root: PathBuf,
    pub files: Vec<FileRecord>,
    pub graph: Graph,
    pub session_tokens: usize,
    pub session_budget: usize,
    pub(crate) sigs: Arc<OnceCell<HashMap<String, Vec<SigChange>>>>,
    pub(crate) warmed: Arc<AtomicBool>,
    /// wall time of the last re-index triggered by a file change
    pub last_refresh_ms: f64,
    /// (lowercase, name) for every symbol name; dropped whenever the graph changes
    names_lc: Option<Arc<Vec<(String, String)>>>,
    pub(crate) store: Arc<Store>,
    pub(crate) cache: summary::Cache,
    /// set by the MCP server: files an edit wrote are re-indexed after the reply is sent, not before
    pub defer_reindex: bool,
    /// written files waiting for that re-index
    pub(crate) deferred: Vec<Written>,
    /// the last structural search, reused by a rewrite of the same pattern while no file changed
    pub(crate) scan_cache: Option<crate::rewrite_tool::ScanCache>,
    /// whole-file syntax trees kept for structural search (see `tree_cache`)
    pub trees: Arc<crate::tree_cache::TreeCache>,
    /// the repo's tsserver: exact TS/JS rename, references and diagnostics (see `ts_exact`)
    pub ts: Arc<crate::ts_exact::TsService>,
}

/// A file an edit wrote: path, the text written, and its tree when the edit had one.
pub(crate) type Written = (String, String, Option<(Lang, tree_sitter::Tree)>);

pub fn default_db(root: &Path) -> PathBuf {
    root.join(".agent-graft").join("index.redb")
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Escape for XML attributes and text.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            _ => o.push(c),
        }
    }
    o
}

/// Code as element text: CDATA only when something would need escaping.
pub(crate) fn cdata(s: &str) -> String {
    if !s.contains('<') && !s.contains('&') {
        return format!("\n{s}\n");
    }
    format!("<![CDATA[\n{}\n]]>", s.replace("]]>", "]]]]><![CDATA[>"))
}

/// A repo-relative path the indexer could have produced: relative, no `..`, no hidden or skipped dirs, not refused.
fn plausible_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && p.split('/').all(|c| !c.is_empty() && c != ".." && c != "." && !c.starts_with('.') && !index::SKIP_DIRS.contains(&c))
        && guard::check_path(p).is_none()
        && Lang::from_path(Path::new(p)).is_some()
}

/// Tokenizer, git signature scan and every file's summary (persisted ones first).
fn warm_job(root: &Path, files: &[FileRecord], store: &Store, cache: &summary::Cache, sigs: &OnceCell<HashMap<String, Vec<SigChange>>>) {
    crate::tokens::warm();
    summary::warm_all(root, files, store, cache);
    sigs.get_or_init(|| gitsig::scan(root, GIT_COMMITS_SCANNED));
}

/// Tokens the `<structure>` wrapper (and CDATA markers) add around already-counted code.
fn element_overhead(text: &str) -> usize {
    static PLAIN: once_cell::sync::Lazy<usize> = once_cell::sync::Lazy::new(|| count("<structure>\nx\n</structure>\n") - count("x"));
    static CDATA: once_cell::sync::Lazy<usize> =
        once_cell::sync::Lazy::new(|| count("<structure><![CDATA[\nx\n]]></structure>\n") - count("x"));
    if text.contains('<') || text.contains('&') { *CDATA } else { *PLAIN }
}

/// Drop XML indentation (it costs tokens and carries nothing); code inside CDATA keeps its own.
fn dedent(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut in_code = false;
    for line in xml.split_inclusive('\n') {
        out.push_str(if in_code { line } else { line.trim_start() });
        if line.contains("<![CDATA[") || line.contains("<structure>") || line.contains("<source>") || line.starts_with("<code") {
            in_code = true;
        }
        if line.contains("]]>") || line.contains("</structure>") || line.contains("</source>") || line.contains("</code>") {
            in_code = false;
        }
    }
    out
}

/// Longest shared directory (with trailing `/`) of 2+ paths; empty when none or a single path.
pub(crate) fn common_dir(paths: &[&str]) -> String {
    if paths.len() < 2 {
        return String::new();
    }
    let mut prefix: &str = paths[0].rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    for p in &paths[1..] {
        while !prefix.is_empty() && !(p.starts_with(prefix) && p.as_bytes().get(prefix.len()) == Some(&b'/')) {
            prefix = prefix.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        }
    }
    if prefix.is_empty() { String::new() } else { format!("{prefix}/") }
}

fn base_attr(name: &str, base: &str) -> String {
    if base.is_empty() { String::new() } else { format!(" {name}=\"{}\"", esc(base)) }
}

/// `a,b,c,d,+3`: at most `n` names, then how many were left out.
fn cap_names<S: AsRef<str>>(names: &[S], n: usize) -> String {
    let mut v: Vec<&str> = names.iter().take(n).map(|x| x.as_ref()).collect();
    let extra = names.len().saturating_sub(n).to_string();
    let extra = format!("+{extra}");
    if names.len() > n {
        v.push(&extra);
    }
    v.join(",")
}

impl Engine {
    /// Load the on-disk index, re-parse whatever changed, rebuild the graph, persist.
    pub fn open(root: &Path, db: Option<&Path>) -> Result<(Engine, IndexStats, Timings)> {
        let root = root.canonicalize()?;
        let db_path = db.map(|p| p.to_path_buf()).unwrap_or_else(|| default_db(&root));
        let t = Instant::now();
        /* an existing index is loaded first (unchanged files are reused); a new one is created by the
         * writer thread while parsing already runs */
        let existing = if db_path.exists() {
            let st = Store::open(&db_path)?;
            let prev = st.load()?;
            Some((st, prev))
        } else {
            None
        };
        let (store, prev) = match existing {
            Some((st, prev)) => (Some(st), prev),
            None => (None, HashMap::new()),
        };
        let load_ms = ms(t);
        let prev_keys: HashMap<String, (u64, u64)> = prev.values().map(|r| (r.path.clone(), (r.mtime_ns, r.size))).collect();

        /* 1. parse in parallel; workers encode new records and stream them to the redb writer as they finish */
        let t = Instant::now();
        let mut stats = IndexStats::default();
        let (tx, rx) = std::sync::mpsc::channel::<StoreMsg>();
        let (files, index_ms, graph, graph_ms, saved, save_ms) = std::thread::scope(|sc| {
            let saver = sc.spawn(|| {
                let r = (|| {
                    let store = match store {
                        Some(st) => st,
                        None => Store::open(&db_path)?,
                    };
                    store.write_stream(rx, &prev_keys)?;
                    Ok::<Store, anyhow::Error>(store)
                })();
                (r, Instant::now())
            });
            let files = index::build_with(&root, prev, &mut stats, &|r| {
                let _ = tx.send(StoreMsg::Put(r.path.clone(), store::encode(r)));
            });
            let index_ms = ms(t);
            let _ = tx.send(StoreMsg::Done(files.iter().map(|f| f.path.clone()).collect()));
            /* 2. build the graph, then persist what it resolved so the next start skips resolution */
            let tg = Instant::now();
            let graph = Graph::build(&files);
            let graph_ms = ms(tg);
            if let Ok(bytes) = rmp_serde::to_vec(&graph.resolved()) {
                let _ = tx.send(StoreMsg::Graph(index::files_sig(&files), bytes));
            }
            drop(tx);
            let (saved, saved_at) = saver.join().expect("save thread");
            let save_ms = saved_at.duration_since(t).as_secs_f64() * 1000.0 - index_ms;
            (files, index_ms, graph, graph_ms, saved, save_ms.max(0.0))
        });
        let store = saved?;

        let ts = Arc::new(crate::ts_exact::TsService::new(&root));
        let e = Engine {
            root,
            files,
            graph,
            session_tokens: 0,
            session_budget: DEFAULT_SESSION_BUDGET,
            sigs: Arc::new(OnceCell::new()),
            warmed: Arc::new(AtomicBool::new(false)),
            last_refresh_ms: 0.0,
            defer_reindex: false,
            deferred: Vec::new(),
            scan_cache: None,
            trees: Arc::new(crate::tree_cache::TreeCache::default()),
            ts,
            names_lc: None,
            store: Arc::new(store),
            cache: summary::new_cache(),
        };
        Ok((e, stats, Timings { load_ms, index_ms, graph_ms, save_ms }))
    }

    /// Serve the persisted index without walking or stat-ing the repo; None when there is no index yet.
    /// Pair with `stale_paths` + `refresh` (in the background) to catch up on offline edits.
    pub fn open_fast(root: &Path, db: Option<&Path>) -> Result<Option<Engine>> {
        Self::open_fast_with(root, db, false)
    }

    /// `open_fast`; with `summaries`, the persisted read summaries are loaded too, while the graph is restored,
    /// for a one-shot process whose first request needs every file's identifier index.
    pub fn open_fast_with(root: &Path, db: Option<&Path>, summaries: bool) -> Result<Option<Engine>> {
        let root = root.canonicalize()?;
        let db_path = db.map(|p| p.to_path_buf()).unwrap_or_else(|| default_db(&root));
        if !db_path.exists() {
            return Ok(None);
        }
        let store = Store::open(&db_path)?;
        /* the persisted graph decodes while the records load; its signature is checked against them after */
        let (files, resolved) = rayon::join(|| store.load(), || store.load_graph_signed());
        let mut files: Vec<FileRecord> = files?.into_values().collect();
        if files.is_empty() {
            return Ok(None);
        }
        /* the index file lives in the repo: a record with a path the walk could never produce means a
         * tampered or foreign index, so rebuild from source instead of trusting it */
        if !files.iter().all(|f| plausible_path(&f.path)) {
            return Ok(None);
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let resolved = resolved?.and_then(|(sig, r)| (sig == index::files_sig(&files)).then_some(r));
        let cache = summary::new_cache();
        let (graph, _) = rayon::join(
            || match resolved {
                Some(r) => Graph::restore(&files, r),
                None => Graph::build(&files),
            },
            || summaries.then(|| summary::load_persisted(&files, &store, &cache)),
        );
        let ts = Arc::new(crate::ts_exact::TsService::new(&root));
        Ok(Some(Engine {
            root,
            files,
            graph,
            session_tokens: 0,
            session_budget: DEFAULT_SESSION_BUDGET,
            sigs: Arc::new(OnceCell::new()),
            warmed: Arc::new(AtomicBool::new(false)),
            last_refresh_ms: 0.0,
            defer_reindex: false,
            deferred: Vec::new(),
            scan_cache: None,
            trees: Arc::new(crate::tree_cache::TreeCache::default()),
            ts,
            names_lc: None,
            store: Arc::new(store),
            cache,
        }))
    }

    /// path -> (mtime, size) of every indexed file.
    pub fn stat_snapshot(&self) -> HashMap<String, (u64, u64)> {
        self.files.iter().filter(|f| f.hash != 0 || f.size != 0).map(|f| (f.path.clone(), (f.mtime_ns, f.size))).collect()
    }

    /// Files added, changed or deleted on disk relative to `snapshot` (walks the repo; no engine lock needed).
    pub fn stale_paths(root: &Path, snapshot: &HashMap<String, (u64, u64)>) -> Vec<String> {
        let mut stats = IndexStats::default();
        let cands = index::discover(root, &mut stats);
        let mut out: Vec<String> = cands
            .iter()
            .filter(|c| snapshot.get(&c.rel) != Some(&(c.mtime_ns, c.size)))
            .map(|c| c.rel.clone())
            .collect();
        let live: std::collections::HashSet<&str> = cands.iter().map(|c| c.rel.as_str()).collect();
        out.extend(snapshot.keys().filter(|k| !live.contains(k.as_str())).cloned());
        out
    }

    pub(crate) fn sig_changes(&self) -> &HashMap<String, Vec<SigChange>> {
        self.sigs.get_or_init(|| gitsig::scan(&self.root, GIT_COMMITS_SCANNED))
    }

    /// Precompute lazy state (git scan, tokenizer) so it does not land on the first request.
    pub fn warm(&self) {
        let (root, files, store, cache, sigs) = (self.root.clone(), self.files.clone(), self.store.clone(), self.cache.clone(), self.sigs.clone());
        warm_job(&root, &files, &store, &cache, &sigs);
        self.warmed.store(true, Ordering::Release);
    }

    /// Same as `warm`, on a background thread: requests are served meanwhile (a miss computes its own summary).
    pub fn spawn_warmer(&self) -> std::thread::JoinHandle<()> {
        let (root, files, store, cache, sigs) = (self.root.clone(), self.files.clone(), self.store.clone(), self.cache.clone(), self.sigs.clone());
        let warmed = self.warmed.clone();
        std::thread::spawn(move || {
            warm_job(&root, &files, &store, &cache, &sigs);
            warmed.store(true, Ordering::Release);
        })
    }

    pub fn is_warm(&self) -> bool {
        self.warmed.load(Ordering::Acquire)
    }

    /// Apply an on-disk change to one file (watcher path): re-parse if its content hash changed,
    /// patch the graph, refresh its summary. Returns true when the index changed.
    pub fn refresh(&mut self, rel: &str) -> bool {
        self.refresh_parsed(rel, None)
    }

    /// Re-index many written files: parsed in parallel (or from the caller's extract of the written
    /// text), then patched into the graph one by one.
    pub(crate) fn refresh_many(&mut self, items: Vec<(String, Option<(&str, parse::Extract)>)>) {
        use rayon::prelude::*;
        let root = self.root.clone();
        let parsed: Vec<(String, Option<index::ParseOutcome>)> = items
            .into_par_iter()
            .map(|(rel, pre)| {
                let abs = root.join(&rel);
                let (Ok(meta), Some(lang)) = (std::fs::metadata(&abs), Lang::from_path(&abs)) else { return (rel, None) };
                let cand = index::Candidate { rel: rel.clone(), lang, mtime_ns: index::mtime_ns(&meta), size: meta.len() };
                let out = match pre {
                    Some((text, ex)) => index::record_from(&cand, text.as_bytes(), ex),
                    None => index::parse_one(&root, &cand),
                };
                (rel, Some(out))
            })
            .collect();
        for (rel, pre) in parsed {
            let rel = rel.as_str();
            self.refresh_parsed(rel, pre);
        }
    }

    /// Re-index written files: their extract comes from the edit's tree when there is one, else a fresh parse.
    pub(crate) fn reindex_written(&mut self, written: Vec<Written>) {
        use rayon::prelude::*;
        let mut items: Vec<(String, String, Option<parse::Extract>)> = written
            .into_par_iter()
            .map(|(rel, text, tree)| {
                let ex = tree.map(|(lang, t)| {
                    let ex = parse::extract_tree(lang, text.as_bytes(), &t);
                    /* the next search of this file walks this tree instead of parsing it */
                    self.trees.put(&rel, xxhash_rust::xxh3::xxh3_64(text.as_bytes()), t, text.len());
                    ex
                });
                (rel, text, ex)
            })
            .collect();
        self.refresh_many(items.iter_mut().map(|(rel, text, ex)| (rel.clone(), ex.take().map(|ex| (text.as_str(), ex)))).collect());
    }

    /// Re-index what earlier edits wrote and left for later (see `defer_reindex`).
    pub fn reindex_deferred(&mut self) {
        if !self.deferred.is_empty() {
            let d = std::mem::take(&mut self.deferred);
            self.reindex_written(d);
        }
    }

    /// `refresh` with the file already parsed (`pre`), or parsed here when None.
    fn refresh_parsed(&mut self, rel: &str, pre: Option<index::ParseOutcome>) -> bool {
        /* same rules as the initial walk and as reads: inside root, not hidden/skipped, not a symlink, not gitignored */
        let abs = self.root.join(rel);
        let is_link = std::fs::symlink_metadata(&abs).map(|m| m.file_type().is_symlink()).unwrap_or(false);
        let known = self.graph.path_idx.contains_key(rel);
        if is_link || (abs.exists() && self.resolve_rel(rel).is_err()) || (!known && self.ignored(rel)) {
            return false;
        }
        let t = Instant::now();
        let changed = self.refresh_inner(rel, pre);
        if changed {
            self.last_refresh_ms = ms(t);
            if Lang::from_path(&abs).is_some_and(|l| l.is_js_family()) {
                self.ts.touch(abs);
            }
        }
        changed
    }

    /// Root `.gitignore` match for a path the initial walk never saw.
    fn ignored(&self, rel: &str) -> bool {
        let mut b = ignore::gitignore::GitignoreBuilder::new(&self.root);
        b.add(self.root.join(".gitignore"));
        b.build().map(|g| g.matched_path_or_any_parents(rel, false).is_ignore()).unwrap_or(false)
    }

    fn refresh_inner(&mut self, rel: &str, pre: Option<index::ParseOutcome>) -> bool {
        let before = self.graph.path_idx.get(rel).map(|&i| self.files[i as usize].hash);
        match self.ensure_fresh_parsed(rel, pre) {
            Ok(Some(fi)) => {
                /* the read summary is rebuilt by the next read of this file, off the re-index path */
                before != Some(self.files[fi as usize].hash)
            }
            Err(Refusal::NotFound) if before.is_some() => {
                /* deleted: keep the slot (indices stay stable), patch it to an empty file: its nodes and edges go */
                let fi = self.graph.path_idx[rel];
                let mut empty = self.files[fi as usize].clone();
                empty.ex = parse::Extract::default();
                empty.hash = 0;
                empty.size = 0;
                let old = std::mem::replace(&mut self.files[fi as usize], empty);
                self.graph.update_file(&self.files, fi, Some(&old));
                self.names_lc = None;
                true
            }
            _ => false,
        }
    }

    /* ---------- path handling + freshness ---------- */

    pub(crate) fn resolve_rel(&self, path: &str) -> Result<String, Refusal> {
        let p = Path::new(path);
        let abs = if p.is_absolute() { p.to_path_buf() } else { self.root.join(p) };
        let abs = abs.canonicalize().map_err(|_| Refusal::NotFound)?;
        let rel = abs.strip_prefix(&self.root).map_err(|_| Refusal::OutsideRoot)?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        /* same visibility rules as indexing: no hidden paths (.git, .ssh, .aws...) or skipped dirs */
        if let Some(r) = guard::check_path(&rel) {
            return Err(r);
        }
        /* only a dotenv template (already cleared by check_path) may be a hidden component, and only as the file itself */
        let parts: Vec<&str> = rel.split('/').collect();
        let hidden = parts.iter().enumerate().any(|(i, c)| {
            let template = i + 1 == parts.len() && c.starts_with(".env.");
            (c.starts_with('.') && *c != "." && !template) || index::SKIP_DIRS.contains(c)
        });
        if hidden {
            return Err(Refusal::Hidden);
        }
        Ok(rel)
    }

    /// Re-parse a file if it changed on disk since indexing; returns its index.
    pub(crate) fn ensure_fresh(&mut self, rel: &str) -> Result<Option<u32>, Refusal> {
        self.ensure_fresh_parsed(rel, None)
    }

    /// `ensure_fresh`, reusing `pre` when the caller already parsed the file as it is on disk now.
    fn ensure_fresh_parsed(&mut self, rel: &str, pre: Option<index::ParseOutcome>) -> Result<Option<u32>, Refusal> {
        if let Some(r) = guard::check_path(rel) {
            return Err(r);
        }
        let abs = self.root.join(rel);
        let meta = std::fs::metadata(&abs).map_err(|_| Refusal::NotFound)?;
        if meta.len() > guard::MAX_FILE_BYTES {
            return Err(Refusal::TooLarge(meta.len()));
        }
        let Some(lang) = Lang::from_path(&abs) else { return Ok(None) };
        let cand = index::Candidate { rel: rel.to_string(), lang, mtime_ns: index::mtime_ns(&meta), size: meta.len() };
        let existing = self.graph.path_idx.get(rel).copied();
        if let Some(i) = existing {
            let f = &self.files[i as usize];
            if f.mtime_ns == cand.mtime_ns && f.size == cand.size {
                return Ok(Some(i));
            }
            /* touched but identical content: only the stat key moves */
            if f.size == cand.size {
                let same = index::with_bytes(&abs, |b| xxhash_rust::xxh3::xxh3_64(b) == f.hash).unwrap_or(false);
                if same {
                    self.files[i as usize].mtime_ns = cand.mtime_ns;
                    return Ok(Some(i));
                }
            }
        }
        match pre.unwrap_or_else(|| index::parse_one(&self.root, &cand)) {
            index::ParseOutcome::Ok(rec) => {
                /* patch the graph in place: only this file's nodes and the calls that can see its names move */
                let i = match existing {
                    Some(i) => {
                        let old = std::mem::replace(&mut self.files[i as usize], rec);
                        self.graph.update_file(&self.files, i, Some(&old));
                        self.names_lc = None;
                        i
                    }
                    None => {
                        self.files.push(rec);
                        let i = (self.files.len() - 1) as u32;
                        self.graph.update_file(&self.files, i, None);
                        self.names_lc = None;
                        i
                    }
                };
                Ok(Some(i))
            }
            index::ParseOutcome::Refused(code) => Err(match code {
                "REFUSED_BINARY" => Refusal::Binary,
                "REFUSED_SECRET" => Refusal::Secret("embedded credential"),
                _ => Refusal::NotFound,
            }),
        }
    }

    /* ---------- receipts ---------- */

    fn status(&self) -> &'static str {
        let pct = self.session_tokens * 100 / self.session_budget.max(1);
        match pct {
            0..=74 => "nominal",
            75..=99 => "warning",
            _ => "exceeded",
        }
    }

    /// Wrap a body with its root element and a `<budget_receipt>` whose count covers the whole response.
    pub(crate) fn finish(&mut self, open_tag: &str, close_tag: &str, body: &str, budget: usize, extra: &str) -> String {
        self.finish_with_code(open_tag, close_tag, body, None, budget, extra)
    }

    /// Like `finish`, plus a code element whose token count is already known (from the summary cache),
    /// so a large read never runs the tokenizer over its code again.
    pub(crate) fn finish_with_code(
        &mut self,
        open_tag: &str,
        close_tag: &str,
        body: &str,
        code: Option<(&str, usize)>,
        budget: usize,
        extra: &str,
    ) -> String {
        let mut body = dedent(body);
        let code_tokens = code.map(|c| c.1).unwrap_or(0);
        /* exact once the tokenizer is loaded; before that (first ms of a server's life) a flagged estimate */
        let mut exact = true;
        let mut fast = |s: &str| {
            let (n, e) = crate::tokens::count_fast(s);
            exact &= e;
            n
        };
        let base = fast(open_tag) + fast(&body) + code_tokens + fast(close_tag) + 2;
        if let Some((el, _)) = code {
            body.push_str(el);
        }
        let mut total = base;
        let mut receipt = String::new();
        for _ in 0..3 {
            let cumulative = self.session_tokens + total;
            /* the caller knows its own budget; only an overrun is worth reporting */
            let over = if budget > 0 && total > budget { format!(" over_budget_by=\"{}\"", total - budget) } else { String::new() };
            let pct = cumulative * 100 / self.session_budget.max(1);
            let status = match pct {
                0..=74 => "nominal",
                75..=99 => "warning",
                _ => "exceeded",
            };
            receipt = format!(
                "<budget_receipt tokens_used_this_call=\"{total}\"{over} cumulative_session_tokens=\"{cumulative}\" budget_status=\"{status}\"{extra}/>"
            );
            let next = base + fast(&receipt);
            if next == total {
                break;
            }
            total = next;
        }
        if !exact {
            receipt = receipt.replacen("<budget_receipt ", "<budget_receipt estimated=\"true\" ", 1);
        }
        self.session_tokens += total;
        format!("{open_tag}\n{receipt}\n{body}{close_tag}")
    }

    pub(crate) fn refusal(&mut self, path: &str, r: Refusal) -> String {
        let open = format!("<code_intelligence_context path=\"{}\" refused=\"true\">", esc(path));
        let body = format!("  <refusal code=\"{}\" reason=\"{}\"/>\n", r.code(), esc(&r.reason()));
        self.finish(&open, "</code_intelligence_context>", &body, 0, "")
    }

    /* ---------- section renderers ---------- */

    pub(crate) fn sym_name(&self, r: &Ref) -> String {
        match r.sym {
            Some(s) => self.files[r.file as usize].ex.symbols[s as usize].name.clone(),
            None => "(module)".into(),
        }
    }

    /// Directives that apply to this file, most important first: (priority, xml).
    /// Git changes count only when they touch a name this file calls (dependency) or defines (self).
    pub(crate) fn directive_items(&self, fi: u32, deps: &[u32], outgoing: &[(Ref, u32)]) -> Vec<(u8, String)> {
        let f = &self.files[fi as usize];
        let mut items: Vec<(u8, String)> = Vec::new();
        /* 1. explicit rules, one each */
        for d in f.ex.directives.iter().filter(|d| d.kind == DirectiveKind::Rule) {
            items.push((0, format!("<directive priority=\"high\" kind=\"rule\" line=\"{}\">{}</directive>", d.line, esc(&d.text))));
        }
        /* 2. pattern hints, grouped by hint text */
        let mut hints: Vec<(&str, Vec<u32>)> = Vec::new();
        for d in f.ex.directives.iter().filter(|d| d.kind == DirectiveKind::Pattern) {
            match hints.iter_mut().find(|h| h.0 == d.text) {
                Some(h) => h.1.push(d.line),
                None => hints.push((&d.text, vec![d.line])),
            }
        }
        for (text, lines) in hints {
            let ls: Vec<String> = lines.iter().take(10).map(|l| l.to_string()).collect();
            items.push((2, format!("<agent_hint lines=\"{}\">{}</agent_hint>", ls.join(","), esc(text))));
        }
        /* 3. recent signature changes that touch this file's calls or definitions */
        let sigs = self.sig_changes();
        let called: HashSet<&str> = f.ex.calls.iter().map(|c| c.callee.as_str()).collect();
        let defined: HashSet<&str> = f.ex.symbols.iter().map(|s| s.name.as_str()).collect();
        let relevant = |path: &str, names: &HashSet<&str>| -> Vec<&SigChange> {
            sigs.get(path).into_iter().flatten().filter(|c| c.names.iter().any(|n| names.contains(n.as_str()))).take(4).collect()
        };
        /* dependencies: one directive per commit, listing each changed file */
        let mut by_commit: Vec<(&str, Vec<String>)> = Vec::new();
        /* a dependency change applies to names this file resolves into that file, or calls that no longer resolve */
        let resolved: Vec<(u32, String)> = outgoing.iter().map(|(r, _)| (r.file, self.sym_name(r))).collect();
        let unresolved: HashSet<&str> = called.iter().copied().filter(|n| !resolved.iter().any(|(_, m)| m == n)).collect();
        for &dep in deps {
            let path = &self.files[dep as usize].path;
            let mut names: HashSet<&str> = unresolved.clone();
            names.extend(resolved.iter().filter(|(f, _)| *f == dep).map(|(_, n)| n.as_str()));
            let cs = relevant(path, &names);
            if cs.is_empty() {
                continue;
            }
            let what: Vec<&str> = cs.iter().map(|c| c.summary.as_str()).collect();
            let line = format!("{path}: {}", what.join("; "));
            match by_commit.iter_mut().find(|b| b.0 == cs[0].commit) {
                Some(b) => b.1.push(line),
                None => by_commit.push((&cs[0].commit, vec![line])),
            }
        }
        for (commit, lines) in by_commit {
            items.push((1, format!(
                "<directive priority=\"high\" kind=\"dependency_changed\" commit=\"{commit}\">{}</directive>",
                esc(&lines.join(" | "))
            )));
        }
        let own = relevant(&f.path, &defined);
        if !own.is_empty() {
            let what: Vec<&str> = own.iter().map(|c| c.summary.as_str()).collect();
            items.push((3, format!(
                "<directive priority=\"medium\" kind=\"self_changed\" commit=\"{}\">{}; check callers</directive>",
                own[0].commit, esc(&what.join("; "))
            )));
        }
        items.sort_by_key(|i| i.0);
        items
    }

    pub(crate) fn directives_xml(items: &[(u8, String)], max: usize) -> String {
        if items.is_empty() || max == 0 {
            return String::new();
        }
        let mut s = String::from("  <agent_directives>\n");
        for (_, x) in items.iter().take(max) {
            let _ = writeln!(s, "    {x}");
        }
        if items.len() > max {
            let _ = writeln!(s, "    <more count=\"{}\"/>", items.len() - max);
        }
        s.push_str("  </agent_directives>\n");
        s
    }

    /// File-level 1-hop graph: caller and callee files ranked by call count, top `top_n` each.
    /// The first `sig_cap` callees carry a signature. `top_n == 0` emits counts only.
    fn file_graph_xml(&self, incoming: &[(Ref, String)], outgoing: &[(Ref, u32)], top_n: usize, sig_cap: usize) -> String {
        let n_in: usize = incoming.len();
        let n_out: u32 = outgoing.iter().map(|o| o.1).sum();
        if top_n == 0 {
            return format!("  <symbol_graph_1hop incoming_calls=\"{n_in}\" outgoing_calls=\"{n_out}\"/>\n");
        }
        /* 1. callers per file: call count + which of our symbols they use */
        let mut callers: Vec<(u32, usize, Vec<&str>)> = Vec::new();
        for (r, target) in incoming {
            let i = match callers.iter().position(|c| c.0 == r.file) {
                Some(i) => i,
                None => {
                    callers.push((r.file, 0, Vec::new()));
                    callers.len() - 1
                }
            };
            callers[i].1 += 1;
            if !callers[i].2.contains(&target.as_str()) {
                callers[i].2.push(target);
            }
        }
        callers.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        /* 2. callees per file, ranked the same way */
        let mut callees: Vec<(u32, u32, Vec<(Ref, u32)>)> = Vec::new();
        for (r, n) in outgoing {
            match callees.iter_mut().find(|c| c.0 == r.file) {
                Some(c) => {
                    c.1 += n;
                    c.2.push((r.clone(), *n));
                }
                None => callees.push((r.file, *n, vec![(r.clone(), *n)])),
            }
        }
        callees.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for c in &mut callees {
            c.2.sort_by(|a, b| b.1.cmp(&a.1));
        }

        /* paths in each list are relative to its shared directory (`in_base` / `out_base`) */
        let caller_paths: Vec<&str> = callers.iter().take(top_n).map(|c| self.files[c.0 as usize].path.as_str()).collect();
        let callee_paths: Vec<&str> = callees.iter().take(top_n).map(|c| self.files[c.0 as usize].path.as_str()).collect();
        let (cbase, obase) = (common_dir(&caller_paths), common_dir(&callee_paths));
        let mut s = format!(
            "  <symbol_graph_1hop incoming_calls=\"{n_in}\" outgoing_calls=\"{n_out}\"{}{}>\n",
            base_attr("in_base", &cbase),
            base_attr("out_base", &obase)
        );
        /* `calls` is omitted when it is 1 */
        let calls = |n: usize| if n == 1 { String::new() } else { format!(" calls=\"{n}\"") };
        for (path, (_, n, uses)) in caller_paths.iter().zip(callers.iter()) {
            let _ = writeln!(s, "    <caller path=\"{}\"{} uses=\"{}\"/>", esc(&path[cbase.len()..]), calls(*n), esc(&cap_names(uses, MAX_NAMES)));
        }
        if callers.len() > top_n {
            let rest: usize = callers[top_n..].iter().map(|c| c.1).sum();
            let _ = writeln!(s, "    <more_callers files=\"{}\" calls=\"{rest}\"/>", callers.len() - top_n);
        }
        let mut sigs_left = sig_cap;
        for (full, (_, n, syms)) in callee_paths.iter().zip(callees.iter()) {
            let names: Vec<String> = syms.iter().map(|(r, _)| self.sym_name(r)).collect();
            let _ = writeln!(s, "    <callee path=\"{}\"{} symbols=\"{}\"/>", esc(&full[obase.len()..]), calls(*n as usize), esc(&cap_names(&names, MAX_NAMES)));
            for (r, _) in syms.iter().take(sigs_left) {
                if let Some(si) = r.sym {
                    let sig = self.files[r.file as usize].ex.symbols[si as usize].sig.replace("( ", "(").replace(", )", ")");
                    let _ = writeln!(s, "    <signature>{}</signature>", esc(&sig));
                    sigs_left -= 1;
                }
            }
        }
        if callees.len() > top_n {
            let rest: u32 = callees[top_n..].iter().map(|c| c.1).sum();
            let _ = writeln!(s, "    <more_callees files=\"{}\" calls=\"{rest}\"/>", callees.len() - top_n);
        }
        s.push_str("  </symbol_graph_1hop>\n");
        s
    }

    /// 1-hop graph, grouped by file. The first `sig_cap` callees carry their signature; the rest are names only.
    fn graph_xml(&self, incoming: &[(Ref, Option<String>)], outgoing: &[Ref], sig_cap: usize) -> String {
        let mut s = String::from("  <symbol_graph_1hop>\n");
        /* callers grouped by file: `at="caller:line(target)"` */
        let mut groups: Vec<(u32, Vec<String>)> = Vec::new();
        for (r, target) in incoming {
            let site = match target {
                Some(t) => format!("{}:{}({t})", self.sym_name(r), r.line),
                None => format!("{}:{}", self.sym_name(r), r.line),
            };
            match groups.iter_mut().find(|g| g.0 == r.file) {
                Some(g) => g.1.push(site),
                None => groups.push((r.file, vec![site])),
            }
        }
        if groups.is_empty() {
            s.push_str("    <incoming_callers count=\"0\"/>\n");
        } else {
            let _ = writeln!(s, "    <incoming_callers count=\"{}\">", incoming.len());
            for (file, sites) in groups.iter().take(MAX_LISTED_EDGES) {
                let _ = writeln!(s, "      <caller path=\"{}\" at=\"{}\"/>", esc(&self.files[*file as usize].path), esc(&sites.join(",")));
            }
            if groups.len() > MAX_LISTED_EDGES {
                let _ = writeln!(s, "      <more files=\"{}\"/>", groups.len() - MAX_LISTED_EDGES);
            }
            s.push_str("    </incoming_callers>\n");
        }
        if outgoing.is_empty() {
            s.push_str("    <outgoing_calls count=\"0\"/>\n  </symbol_graph_1hop>\n");
            return s;
        }
        let _ = writeln!(s, "    <outgoing_calls count=\"{}\">", outgoing.len());
        for r in outgoing.iter().take(sig_cap) {
            let f = &self.files[r.file as usize];
            let sig = r.sym.map(|x| f.ex.symbols[x as usize].sig.replace("( ", "(").replace(", )", ")")).unwrap_or_default();
            let _ = writeln!(s, "      <callee path=\"{}\" symbol=\"{}\" signature=\"{}\"/>", esc(&f.path), esc(&self.sym_name(r)), esc(&sig));
        }
        let mut rest: Vec<(u32, Vec<String>)> = Vec::new();
        for r in outgoing.iter().skip(sig_cap) {
            let name = self.sym_name(r);
            match rest.iter_mut().find(|g| g.0 == r.file) {
                Some(g) if g.1.contains(&name) => {}
                Some(g) => g.1.push(name),
                None => rest.push((r.file, vec![name])),
            }
        }
        for (file, names) in rest.iter().take(MAX_LISTED_EDGES) {
            let _ = writeln!(s, "      <callee path=\"{}\" symbols=\"{}\"/>", esc(&self.files[*file as usize].path), esc(&names.join(",")));
        }
        if rest.len() > MAX_LISTED_EDGES {
            let _ = writeln!(s, "      <more files=\"{}\"/>", rest.len() - MAX_LISTED_EDGES);
        }
        s.push_str("    </outgoing_calls>\n  </symbol_graph_1hop>\n");
        s
    }
    /* ---------- tools ---------- */

    /// `read_file`: skeleton at `level` under `budget` tokens, with directives and 1-hop graph.
    pub fn read_file(&mut self, path: &str, level: u8, budget: usize) -> String {
        let rel = match self.resolve_rel(path) {
            Ok(r) => r,
            Err(r) => return self.refusal(path, r),
        };
        let fi = match self.ensure_fresh(&rel) {
            Ok(fi) => fi,
            Err(r) => return self.refusal(&rel, r),
        };
        let abs = self.root.join(&rel);
        let Some(fi) = fi else {
            /* unsupported language: plain text, budgeted */
            let bytes = match index::with_bytes(&abs, |b| b.to_vec()) {
                Ok(b) => b,
                Err(_) => return self.refusal(&rel, Refusal::NotFound),
            };
            if let Some(r) = guard::check_content(&bytes) {
                return self.refusal(&rel, r);
            }
            let ex = parse::Extract::default();
            let fitted = skeleton::fit(Lang::Rust, &bytes, &ex, 0, budget.saturating_sub(80));
            let txt = if fitted.level == 0 { fitted.text } else { String::from_utf8_lossy(&bytes).chars().take(budget * 3).collect() };
            let open = format!("<code_intelligence_context path=\"{}\" language=\"text\" level=\"0\">", esc(&rel));
            let body = format!("  <structure>{}</structure>\n", cdata(&txt));
            return self.finish(&open, "</code_intelligence_context>", &body, budget, "");
        };

        /* indexed + fresh: content guards already passed when it was parsed */
        let summary = match self.summary(fi) {
            Some(s) => s,
            None => return self.refusal(&rel, Refusal::NotFound),
        };
        let raw = summary.raw_tokens as usize;
        let (incoming, outgoing) = self.graph.file_neighbors(fi, &self.files);
        let incoming: Vec<(Ref, String)> =
            incoming.into_iter().map(|(r, s)| (r, self.files[fi as usize].ex.symbols[s as usize].name.clone())).collect();
        let mut deps: Vec<u32> = self.graph.imports[fi as usize].clone();
        deps.extend(outgoing.iter().map(|r| r.0.file));
        deps.sort_unstable();
        deps.dedup();
        deps.retain(|&d| d != fi);

        /* extras (directives + graph + receipt) are paid from the same budget; shrink them until they fit their share */
        let items = self.directive_items(fi, &deps, &outgoing);
        let (top, sigs) = if level >= 2 { (TOP_N_API, 0) } else { (TOP_N, MAX_SIGNATURES) };
        let extras_cap = budget * EXTRAS_SHARE_PCT / 100;
        let steps = [(top, sigs, MAX_DIRECTIVES), (top.min(3), 0, 6), (1, 0, 3), (0, 0, 1), (0, 0, 0)];
        let (mut dir_xml, mut graph_xml) = (String::new(), String::new());
        let mut overhead = 0;
        for (n, sc, md) in steps {
            dir_xml = Self::directives_xml(&items, md);
            graph_xml = self.file_graph_xml(&incoming, &outgoing, n, sc);
            overhead = count(&dir_xml) + count(&graph_xml) + WRAPPER_TOKENS;
            if overhead <= extras_cap {
                break;
            }
        }
        /* pick the requested level, or the next smaller one that fits; cached text and counts, no parsing */
        let room = budget.saturating_sub(overhead).max(1);
        let mut chosen: Option<(u8, String, usize)> = None;
        for lvl in level.min(2)..=2 {
            let (text, tokens) = match lvl {
                0 => match index::with_bytes(&abs, |b| String::from_utf8_lossy(b).into_owned()) {
                    Ok(t) => (t, raw),
                    Err(_) => return self.refusal(&rel, Refusal::NotFound),
                },
                l => {
                    let (t, n) = summary.level(l).expect("levels 1-2 are cached");
                    (t.to_string(), n)
                }
            };
            if tokens <= room {
                chosen = Some((lvl, text, tokens));
                break;
            }
        }
        let (lvl, text, tokens, fit) = match chosen {
            Some((l, t, n)) => (l, t, n, if l == level.min(2) { "exact" } else { "degraded" }),
            None => {
                let f = &self.files[fi as usize];
                let src = index::with_bytes(&abs, |b| b.to_vec()).unwrap_or_default();
                let fitted = skeleton::fit(f.lang, &src, &f.ex, 2, room);
                (2, fitted.text, fitted.tokens, "truncated")
            }
        };
        let f = &self.files[fi as usize];
        let requested = if lvl != level { format!(" requested_level=\"{level}\"") } else { String::new() };
        let open = format!("<code_intelligence_context path=\"{}\" level=\"{lvl}\"{requested} lines=\"{}\">", esc(&rel), f.ex.lines);
        let body = format!("{dir_xml}{graph_xml}");
        let el = format!("<structure>{}</structure>\n", cdata(&text));
        let el_tokens = tokens + element_overhead(&text);
        let extra = if fit == "exact" { format!(" raw_file_tokens=\"{raw}\"") } else { format!(" raw_file_tokens=\"{raw}\" fit=\"{fit}\"") };
        self.finish_with_code(&open, "</code_intelligence_context>", &body, Some((&el, el_tokens)), budget, &extra)
    }

    /// Cached summary for an indexed, fresh file; computed (and cached) on a miss.
    pub(crate) fn summary(&mut self, fi: u32) -> Option<Arc<summary::Summary>> {
        let f = &self.files[fi as usize];
        if let Some(s) = summary::get(&self.cache, f) {
            return Some(s);
        }
        let s = index::with_bytes(&self.root.join(&f.path), |b| summary::compute(f, b)).ok()??;
        Some(summary::put(&self.cache, &f.path, s))
    }

    pub(crate) fn find_symbols(&self, name: &str, path: Option<&str>) -> Vec<(u32, u32)> {
        let rel = path.and_then(|p| self.resolve_rel(p).ok());
        let mut v: Vec<(u32, u32)> = self.graph.by_name.get(name).cloned().unwrap_or_default();
        if let Some(rel) = rel {
            v.retain(|(f, _)| self.files[*f as usize].path == rel);
        }
        v.sort_by_key(|&(f, s)| (!self.files[f as usize].ex.symbols[s as usize].exported, f, s));
        v
    }

    /// `read_symbol`: one symbol's source (level 0), outline (1) or signature (2), plus its 1-hop graph.
    pub fn read_symbol(&mut self, name: &str, path: Option<&str>, level: u8, budget: usize) -> String {
        if let Some(p) = path {
            if let Ok(rel) = self.resolve_rel(p) {
                if let Err(r) = self.ensure_fresh(&rel) {
                    return self.refusal(&rel, r);
                }
            }
        }
        let found = self.find_symbols(name, path);
        let Some(&(fi, _)) = found.first() else {
            let open = format!("<symbol_context name=\"{}\" found=\"false\">", esc(name));
            return self.finish(&open, "</symbol_context>", "", budget, "");
        };
        let rel = self.files[fi as usize].path.clone();
        if let Err(r) = self.ensure_fresh(&rel) {
            return self.refusal(&rel, r);
        }
        let Ok(bytes) = index::with_bytes(&self.root.join(&rel), |b| b.to_vec()) else {
            return self.refusal(&rel, Refusal::NotFound);
        };
        if let Some(r) = guard::check_content(&bytes) {
            return self.refusal(&rel, r);
        }
        /* the refresh may have moved symbols; look the name up again */
        let found = self.find_symbols(name, Some(&rel));
        let Some(&(fi, si)) = found.first() else {
            let open = format!("<symbol_context name=\"{}\" found=\"false\">", esc(name));
            return self.finish(&open, "</symbol_context>", "", budget, "");
        };
        let f = &self.files[fi as usize];
        let s = &f.ex.symbols[si as usize];
        let src = String::from_utf8_lossy(&bytes).into_owned();
        let lines: Vec<&str> = src.lines().collect();
        let a = (s.start_line as usize).saturating_sub(1).min(lines.len());
        let b = (s.end_line as usize).min(lines.len()).max(a);
        let text = match level {
            0 => lines[a..b].join("\n"),
            1 => {
                let mut t = s.sig.clone();
                for c in f.ex.symbols.iter().filter(|c| c.parent == Some(si)) {
                    let _ = write!(t, "\n    {}", c.sig);
                }
                t
            }
            _ => s.sig.clone(),
        };
        let callers: Vec<(Ref, Option<String>)> = self.graph.callers(fi, si).into_iter().map(|r| (r, None)).collect();
        let callees = self.graph.callees(fi, Some(si));
        let graph_xml = self.graph_xml(&callers, &callees, if level >= 2 { 0 } else { MAX_SIGNATURES });
        let budget_left = budget.saturating_sub(count(&graph_xml) + 90).max(1);
        let tokens = count(&text);
        let (text, fit) = if tokens <= budget_left {
            (text, "exact")
        } else {
            let keep: String = text.lines().scan(0usize, |acc, l| {
                *acc += count(l) + 1;
                (*acc <= budget_left.saturating_sub(16)).then(|| format!("{l}\n"))
            }).collect();
            (format!("{keep}/* ... truncated over budget ... */"), "truncated")
        };
        let others = found.len() - 1;
        let open = format!(
            "<symbol_context name=\"{}\" path=\"{}\" kind=\"{}\" line=\"{}\" end_line=\"{}\" level=\"{level}\" other_matches=\"{others}\">",
            esc(name),
            esc(&f.path),
            s.kind.as_str(),
            s.start_line,
            s.end_line
        );
        let body = format!("{graph_xml}  <source>{}</source>\n", cdata(&text));
        self.finish(&open, "</symbol_context>", &body, budget, &format!(" fit=\"{fit}\""))
    }

    /// `callers`: 1-hop graph for every symbol with this name (pure graph query, no file IO).
    pub fn callers(&mut self, name: &str, path: Option<&str>) -> String {
        let found = self.find_symbols(name, path);
        let mut body = String::new();
        for &(fi, si) in found.iter().take(5) {
            let f = &self.files[fi as usize];
            let s = &f.ex.symbols[si as usize];
            let _ = writeln!(body, "  <symbol path=\"{}\" line=\"{}\" signature=\"{}\">", esc(&f.path), s.start_line, esc(&s.sig));
            let callers: Vec<(Ref, Option<String>)> = self.graph.callers(fi, si).into_iter().map(|r| (r, None)).collect();
            let callees = self.graph.callees(fi, Some(si));
            body.push_str(&self.graph_xml(&callers, &callees, MAX_SIGNATURES).replace("\n  ", "\n    ").replacen("  <symbol_graph_1hop>", "    <symbol_graph_1hop>", 1));
            body.push_str("  </symbol>\n");
        }
        let open = format!("<call_graph name=\"{}\" matches=\"{}\">", esc(name), found.len());
        self.finish(&open, "</call_graph>", &body, 0, "")
    }

    /// `find_symbol`: case-insensitive substring search over symbol names.
    pub fn find(&mut self, query: &str, limit: usize) -> String {
        let q = query.to_lowercase();
        /* lowercase names are built once per graph change, not per query */
        let names = self.names_lc.get_or_insert_with(|| {
            Arc::new(self.graph.by_name.keys().map(|k| (k.to_lowercase(), k.clone())).collect())
        }).clone();
        let finder = memchr::memmem::Finder::new(q.as_bytes());
        let mut hits: Vec<(u8, u32, u32)> = Vec::new();
        for (ln, name) in names.iter() {
            let rank = if *ln == q { 0 } else if ln.starts_with(&q) { 1 } else if finder.find(ln.as_bytes()).is_some() { 2 } else { continue };
            for &(f, s) in self.graph.by_name.get(name).into_iter().flatten() {
                hits.push((rank, f, s));
            }
        }
        hits.sort_by_key(|&(r, f, s)| (r, !self.files[f as usize].ex.symbols[s as usize].exported, f, s));
        let mut body = String::new();
        for &(_, f, s) in hits.iter().take(limit) {
            let fr = &self.files[f as usize];
            let sym = &fr.ex.symbols[s as usize];
            let _ = writeln!(
                body,
                "  <match path=\"{}\" line=\"{}\" kind=\"{}\" exported=\"{}\" signature=\"{}\"/>",
                esc(&fr.path),
                sym.start_line,
                sym.kind.as_str(),
                sym.exported,
                esc(&sym.sig)
            );
        }
        let open = format!("<symbol_search query=\"{}\" total=\"{}\" shown=\"{}\">", esc(query), hits.len(), hits.len().min(limit));
        self.finish(&open, "</symbol_search>", &body, 0, "")
    }

    /// `search_text`: literal search over indexed files using a SIMD memmem prefilter.
    pub fn grep(&mut self, pattern: &str, limit: usize) -> String {
        let finder = memchr::memmem::Finder::new(pattern.as_bytes());
        let root = &self.root;
        let mut hits: Vec<(String, usize, String)> = self
            .files
            .par_iter()
            .flat_map_iter(|f| {
                let abs = root.join(&f.path);
                index::with_bytes(&abs, |b| {
                    let mut out = Vec::new();
                    if guard::check_content(b).is_some() {
                        return out;
                    }
                    let mut last_line_start = 0usize;
                    let mut line_no = 1usize;
                    let mut scanned = 0usize;
                    for pos in finder.find_iter(b) {
                        line_no += memchr::memchr_iter(b'\n', &b[scanned..pos]).count();
                        scanned = pos;
                        last_line_start = memchr::memrchr(b'\n', &b[..pos]).map(|i| i + 1).unwrap_or(0).max(last_line_start);
                        let end = memchr::memchr(b'\n', &b[pos..]).map(|i| pos + i).unwrap_or(b.len());
                        let line = String::from_utf8_lossy(&b[last_line_start..end]).trim().chars().take(200).collect();
                        if out.last().map(|(_, l, _): &(String, usize, String)| *l != line_no).unwrap_or(true) {
                            out.push((f.path.clone(), line_no, line));
                        }
                    }
                    out
                })
                .unwrap_or_default()
            })
            .collect();
        hits.sort();
        let mut body = String::new();
        for (p, l, t) in hits.iter().take(limit) {
            let _ = writeln!(body, "  <hit path=\"{}\" line=\"{l}\">{}</hit>", esc(p), esc(t));
        }
        let open = format!("<text_search pattern=\"{}\" total=\"{}\" shown=\"{}\">", esc(pattern), hits.len(), hits.len().min(limit));
        self.finish(&open, "</text_search>", &body, 0, "")
    }

    pub fn status_xml(&mut self) -> String {
        let syms: usize = self.files.iter().map(|f| f.ex.symbols.len()).sum();
        let body = format!(
            "  <index root=\"{}\" files=\"{}\" symbols=\"{syms}\" graph_nodes=\"{}\" graph_edges=\"{}\" warm=\"{}\" last_refresh_ms=\"{:.3}\" session_status=\"{}\"/>\n",
            esc(&self.root.to_string_lossy()),
            self.files.len(),
            self.graph.g.node_count(),
            self.graph.g.edge_count(),
            self.is_warm(),
            self.last_refresh_ms,
            self.status()
        );
        self.finish("<index_status>", "</index_status>", &body, 0, "")
    }

    /// All callable symbols as (file, sym) pairs; used by the benchmark harness.
    pub fn all_symbols(&self) -> Vec<(u32, u32)> {
        let mut v = Vec::new();
        for (fi, f) in self.files.iter().enumerate() {
            for (si, s) in f.ex.symbols.iter().enumerate() {
                if matches!(s.kind, SymKind::Function | SymKind::Method) {
                    v.push((fi as u32, si as u32));
                }
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, Engine) {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("src/db")).unwrap();
        fs::create_dir_all(r.join("src/services")).unwrap();
        fs::create_dir_all(r.join("src/controllers")).unwrap();
        fs::write(r.join("src/lib.rs"), "mod db;\nmod services;\nmod controllers;\n").unwrap();
        fs::write(
            r.join("src/db/repo.rs"),
            "pub struct Repo;\nimpl Repo {\n    pub async fn get_by_id(&self, id: u64) -> Result<u64, ()> {\n        Ok(id)\n    }\n}\n",
        )
        .unwrap();
        let body: String = (0..40).map(|i| format!("        let v{i} = {i};\n")).collect();
        fs::write(
            r.join("src/services/user.rs"),
            format!(
                "use crate::db::repo::Repo;\n\n// @agent-rule: Do not modify this signature without updating schema.proto\npub struct UserService {{ repo: Repo }}\n\nimpl UserService {{\n    /// Fetch active user profile and update last login timestamp\n    pub async fn get_profile(&self, user_id: u64) -> Result<u64, ()> {{\n{body}        self.repo.get_by_id(user_id).await\n    }}\n    fn raw(&self) {{\n        unsafe {{ core::hint::unreachable_unchecked() }}\n    }}\n}}\n"
            ),
        )
        .unwrap();
        fs::write(
            r.join("src/controllers/auth.rs"),
            "use crate::services::user::UserService;\npub async fn login_handler(s: &UserService) {\n    s.get_profile(1).await;\n}\n",
        )
        .unwrap();
        fs::write(r.join("Cargo.lock"), "# lock").unwrap();
        fs::write(r.join(".env"), "SECRET=1").unwrap();
        let db = r.join("idx.redb");
        let (e, _, _) = Engine::open(r, Some(&db)).unwrap();
        (d, e)
    }

    #[test]
    fn read_file_emits_section3_payload() {
        let (_d, mut e) = fixture();
        let out = e.read_file("src/services/user.rs", 1, 4000);
        assert!(out.starts_with("<code_intelligence_context path=\"src/services/user.rs\" level=\"1\""), "{out}");
        assert!(out.contains("<budget_receipt tokens_used_this_call=\""));
        assert!(out.contains("kind=\"rule\" line=\"3\">Do not modify this signature without updating schema.proto</directive>"));
        assert!(out.contains("<agent_hint lines=\"52\">unsafe block"));
        assert!(out.contains("<symbol_graph_1hop incoming_calls=\"1\" outgoing_calls=\"1\">"), "{out}");
        assert!(out.contains("<caller path=\"src/controllers/auth.rs\" uses=\"get_profile\"/>"), "{out}");
        assert!(out.contains("<callee path=\"src/db/repo.rs\" symbols=\"get_by_id\"/>\n<signature>pub async fn get_by_id(&amp;self, id: u64) -&gt; Result&lt;u64, ()&gt;</signature>"), "{out}");
        assert!(out.contains("/* 41 lines hidden */"));
        assert!(out.ends_with("</code_intelligence_context>"));
    }

    #[test]
    fn one_hop_list_is_ranked_and_capped() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::write(r.join("lib.py"), "def target():\n    pass\n").unwrap();
        for i in 0..7 {
            let calls: String = (0..=i).map(|_| "    target()\n").collect();
            fs::write(r.join(format!("c{i}.py")), format!("from lib import target\ndef f{i}():\n{calls}")).unwrap();
        }
        let (mut e, _, _) = Engine::open(r, Some(&r.join("i.redb"))).unwrap();
        let out = e.read_file("lib.py", 2, 8000);
        assert!(out.contains("incoming_calls=\"28\""), "{out}");
        assert_eq!(common_dir(&["a/b/c.ts", "a/b/d/e.ts"]), "a/b/");
        assert_eq!(common_dir(&["a/b.ts", "c/d.ts"]), "");
        assert_eq!(common_dir(&["ab/x.ts", "a/y.ts"]), "");
        assert!(out.contains("<caller path=\"c6.py\" calls=\"7\" uses=\"target\"/>"));
        assert!(!out.contains("c0.py"));
        assert!(out.contains("<more_callers files=\"4\" calls=\"10\"/>"), "{out}");
    }

    #[test]
    fn git_directives_only_for_names_this_file_uses() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git").args(args).current_dir(r)
                .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
                .status().unwrap().success();
            assert!(ok);
        };
        git(&["init", "-q"]);
        fs::write(r.join("repo.py"), "def load(a):\n    pass\n\ndef save(a):\n    pass\n").unwrap();
        fs::write(r.join("uses_load.py"), "from repo import load\ndef run():\n    load(1)\n").unwrap();
        fs::write(r.join("uses_save.py"), "from repo import save\ndef run():\n    save(1)\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        fs::write(r.join("repo.py"), "def load(a, b):\n    pass\n\ndef save(a):\n    pass\n").unwrap();
        git(&["commit", "-qam", "change load"]);
        let (mut e, _, _) = Engine::open(r, Some(&r.join("i.redb"))).unwrap();
        let out = e.read_file("uses_load.py", 1, 8000);
        assert!(out.contains("kind=\"dependency_changed\" commit=\"") && out.contains("repo.py: `load` signature changed"), "{out}");
        assert!(!e.read_file("uses_save.py", 1, 8000).contains("dependency_changed"));
        assert!(e.read_file("repo.py", 1, 8000).contains("kind=\"self_changed\""));
    }

    #[test]
    fn extras_shrink_to_fit_a_small_budget() {
        let (_d, mut e) = fixture();
        let out = e.read_file("src/services/user.rs", 2, 300);
        assert!(out.contains("<symbol_graph_1hop incoming_calls=\"1\" outgoing_calls=\"1\"/>"), "{out}");
        let used: usize = out.split("tokens_used_this_call=\"").nth(1).unwrap().split('"').next().unwrap().parse().unwrap();
        assert!(used <= 300, "{used}");
    }

    #[test]
    fn receipt_counts_whole_response_and_accumulates() {
        let (_d, mut e) = fixture();
        let out = e.read_file("src/services/user.rs", 0, 8000);
        let used: usize = out.split("tokens_used_this_call=\"").nth(1).unwrap().split('"').next().unwrap().parse().unwrap();
        let actual = count(&out);
        assert!(used.abs_diff(actual) <= 3, "receipt {used} vs actual {actual}");
        let first = e.session_tokens;
        e.read_file("src/db/repo.rs", 0, 8000);
        assert!(e.session_tokens > first);
    }

    #[test]
    fn budget_forces_degradation() {
        let (_d, mut e) = fixture();
        let out = e.read_file("src/services/user.rs", 0, 450);
        assert!(out.contains("requested_level=\"0\""));
        assert!(!out.contains("level=\"0\" requested_level"), "{out}");
        assert!(out.contains("fit=\"degraded\"") || out.contains("fit=\"truncated\""));
        let used: usize = out.split("tokens_used_this_call=\"").nth(1).unwrap().split('"').next().unwrap().parse().unwrap();
        assert!(used <= 450, "{used}");
    }

    #[test]
    pub(crate) fn refusals_are_machine_readable() {
        let (_d, mut e) = fixture();
        assert!(e.read_file("Cargo.lock", 0, 1000).contains("<refusal code=\"REFUSED_LOCKFILE\""));
        assert!(e.read_file(".env", 0, 1000).contains("<refusal code=\"REFUSED_SECRET\""));
        assert!(e.read_file("nope.rs", 0, 1000).contains("<refusal code=\"NOT_FOUND\""));
        assert!(e.read_file("../../etc/hosts", 0, 1000).contains("refused=\"true\""));
        assert!(e.read_file("Cargo.lock", 0, 1000).contains("<budget_receipt"));
        fs::create_dir_all(_d.path().join(".git")).unwrap();
        fs::write(_d.path().join(".git/config"), "[remote]").unwrap();
        assert!(e.read_file(".git/config", 0, 1000).contains("<refusal code=\"REFUSED_HIDDEN\""));
        fs::write(_d.path().join(".envrc"), "export K=1").unwrap();
        assert!(e.read_file(".envrc", 0, 1000).contains("REFUSED_SECRET"));
        fs::create_dir_all(_d.path().join(".env-secrets")).unwrap();
        fs::write(_d.path().join(".env-secrets/k.ts"), "export const k = 1;").unwrap();
        assert!(e.read_file(".env-secrets/k.ts", 0, 1000).contains("REFUSED_HIDDEN"));
        fs::write(_d.path().join(".env.example"), "K=").unwrap();
        assert!(!e.read_file(".env.example", 0, 1000).contains("refused"));
        fs::write(_d.path().join("src/db/repo.rs"), "pub fn k() { let a = \"AKIAABCDEFGHIJKLMNOP\"; }\n").unwrap();
        assert!(e.read_symbol("get_by_id", Some("src/db/repo.rs"), 0, 1000).contains("REFUSED_SECRET"));
        assert!(e.grep("AKIA", 10).contains("total=\"0\""));
    }

    #[test]
    fn symbol_callers_find_and_grep() {
        let (_d, mut e) = fixture();
        let s = e.read_symbol("get_profile", None, 0, 4000);
        assert!(s.contains("kind=\"method\"") && s.contains("let v39 = 39;"));
        let c = e.callers("get_by_id", None);
        assert!(c.contains("at=\"get_profile:"), "{c}");
        let f = e.find("profile", 10);
        assert!(f.contains("signature=\"pub async fn get_profile"));
        let g = e.grep("get_by_id", 10);
        assert!(g.contains("total=\"2\""), "{g}");
    }

    #[test]
    fn refresh_patches_deletes_in_place_and_ignores_symlinks() {
        let (d, mut e) = fixture();
        let repo = e.graph.path_idx["src/db/repo.rs"];
        assert_eq!(e.graph.callers(repo, 2).len(), 1);
        let nodes = e.graph.g.node_count();
        fs::remove_file(d.path().join("src/db/repo.rs")).unwrap();
        assert!(e.refresh("src/db/repo.rs"));
        assert!(e.graph.g.node_count() < nodes);
        assert!(!e.graph.by_name.contains_key("get_by_id"));
        assert!(e.callers("get_profile", None).contains("get_by_id") == false);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.path().join("src/lib.rs"), d.path().join("src/link.rs")).unwrap();
            assert!(!e.refresh("src/link.rs"));
            assert!(!e.graph.path_idx.contains_key("src/link.rs"));
        }
    }

    #[test]
    fn implausible_index_paths_force_a_rebuild() {
        assert!(plausible_path("src/a.rs"));
        for p in ["/etc/passwd.rs", "../x.rs", "src/../../x.rs", ".git/x.rs", "node_modules/a.js", ".env", "a.png"] {
            assert!(!plausible_path(p), "{p}");
        }
    }

    #[test]
    fn picks_up_edits_without_reindex() {
        let (d, mut e) = fixture();
        fs::write(d.path().join("src/db/repo.rs"), "pub fn brand_new() {}\n").unwrap();
        let out = e.read_file("src/db/repo.rs", 0, 4000);
        assert!(out.contains("brand_new"));
        assert!(e.graph.by_name.contains_key("brand_new"));
    }
}
