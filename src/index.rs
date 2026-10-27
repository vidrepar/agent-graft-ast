//! Parallel discovery + parse: ignore-walker -> rayon -> memmap2 -> tree-sitter.

use crate::guard::{self, Refusal};
use crate::lang::Lang;
use crate::parse::{self, Extract};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String,
    pub lang: Lang,
    pub mtime_ns: u64,
    pub size: u64,
    /// xxh3 of the content: keys the read cache and lets a touched-but-identical file skip re-parsing
    pub hash: u64,
    pub ex: Extract,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub rel: String,
    pub lang: Lang,
    pub mtime_ns: u64,
    pub size: u64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct IndexStats {
    pub discovered: usize,
    pub parsed: usize,
    pub reused: usize,
    pub refused: HashMap<String, usize>,
    pub symbols: usize,
    pub calls: usize,
    pub bytes: u64,
}

/// Files at least this large are memory-mapped; smaller ones are read.
const MMAP_MIN_BYTES: u64 = 256 * 1024;

pub const SKIP_DIRS: &[&str] = &[
    "node_modules", "target", "dist", "build", ".git", ".agent-graft", "graft", "vendor", "__pycache__", ".venv",
    "venv", ".next", ".turbo", ".wrangler", "coverage", ".astro", ".svelte-kit", ".cache", ".herdr",
];

/// Identity of a record set (paths + content hashes, in path order): keys the persisted graph.
pub fn files_sig(files: &[FileRecord]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    for f in files {
        h.update(f.path.as_bytes());
        h.update(&f.hash.to_le_bytes());
    }
    h.digest()
}

pub fn mtime_ns(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

pub fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// Walk the tree in parallel, honoring .gitignore, keeping only supported source files.
pub fn discover(root: &Path, stats: &mut IndexStats) -> Vec<Candidate> {
    let out = Mutex::new(Vec::new());
    let refused = Mutex::new(HashMap::<String, usize>::new());
    walk(root, &refused, &|c| out.lock().unwrap().push(c));
    let mut v = out.into_inner().unwrap();
    v.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.rel.cmp(&b.rel)));
    stats.discovered = v.len();
    for (k, n) in refused.into_inner().unwrap() {
        *stats.refused.entry(k).or_default() += n;
    }
    v
}

/// Parallel walk honoring .gitignore; `found` is called (from walker threads) for every supported source file.
fn walk(root: &Path, refused: &Mutex<HashMap<String, usize>>, found: &(dyn Fn(Candidate) + Sync)) {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .ignore(false)
        .require_git(false)
        .filter_entry(|e| {
            !(e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && SKIP_DIRS.contains(&e.file_name().to_str().unwrap_or("")))
        })
        .threads(rayon::current_num_threads())
        .build_parallel();
    walker.run(|| {
        Box::new(move |entry| {
            let Ok(entry) = entry else { return ignore::WalkState::Continue };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return ignore::WalkState::Continue;
            }
            let path = entry.path();
            let Some(lang) = Lang::from_path(path) else { return ignore::WalkState::Continue };
            let rel = rel_path(root, path);
            let refuse = |code: &str| *refused.lock().unwrap().entry(code.into()).or_default() += 1;
            if rel.ends_with(".min.js") {
                refuse("MINIFIED");
                return ignore::WalkState::Continue;
            }
            if let Some(r) = guard::check_path(&rel) {
                refuse(r.code());
                return ignore::WalkState::Continue;
            }
            let Ok(meta) = entry.metadata() else { return ignore::WalkState::Continue };
            if meta.len() > guard::MAX_FILE_BYTES {
                refuse(Refusal::TooLarge(0).code());
                return ignore::WalkState::Continue;
            }
            found(Candidate { rel, lang, mtime_ns: mtime_ns(&meta), size: meta.len() });
            ignore::WalkState::Continue
        })
    });
}

/// Read a file through a memory map (zero-copy); empty files skip the map.
pub fn with_bytes<T>(path: &Path, f: impl FnOnce(&[u8]) -> T) -> std::io::Result<T> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(f(&[]));
    }
    /* small files: one read() beats mmap+munmap, whose page-table work serializes across threads */
    if len < MMAP_MIN_BYTES {
        let mut buf = Vec::with_capacity(len as usize);
        std::io::Read::read_to_end(&mut file, &mut buf)?;
        return Ok(f(&buf));
    }
    /* SAFETY: read-only map; a concurrent writer can only produce garbage text, never UB in safe consumers of &[u8] beyond torn reads */
    let map = unsafe { memmap2::Mmap::map(&file)? };
    Ok(f(&map))
}

pub enum ParseOutcome {
    Ok(FileRecord),
    Refused(&'static str),
}

/// The record for `c` from its bytes and an extract the caller already made from them.
pub fn record_from(c: &Candidate, bytes: &[u8], ex: parse::Extract) -> ParseOutcome {
    if let Some(r) = guard::check_content(bytes) {
        return ParseOutcome::Refused(r.code());
    }
    ParseOutcome::Ok(FileRecord { path: c.rel.clone(), lang: c.lang, mtime_ns: c.mtime_ns, size: c.size, hash: xxhash_rust::xxh3::xxh3_64(bytes), ex })
}

pub fn parse_one(root: &Path, c: &Candidate) -> ParseOutcome {
    let abs: PathBuf = root.join(&c.rel);
    let res = with_bytes(&abs, |bytes| {
        if let Some(r) = guard::check_content(bytes) {
            return Err(r.code());
        }
        Ok((parse::extract(c.lang, bytes), xxhash_rust::xxh3::xxh3_64(bytes)))
    });
    match res {
        Ok(Ok((ex, hash))) => ParseOutcome::Ok(FileRecord { path: c.rel.clone(), lang: c.lang, mtime_ns: c.mtime_ns, size: c.size, hash, ex }),
        Ok(Err(code)) => ParseOutcome::Refused(code),
        Err(_) => ParseOutcome::Refused("IO_ERROR"),
    }
}

/// Build records for every candidate, reusing unchanged ones from `prev`.
pub fn build(root: &Path, prev: HashMap<String, FileRecord>, stats: &mut IndexStats) -> Vec<FileRecord> {
    build_with(root, prev, stats, &|_| {})
}

/// Like `build`, calling `on_new` (from worker threads) for every freshly parsed record.
pub fn build_with(
    root: &Path,
    prev: HashMap<String, FileRecord>,
    stats: &mut IndexStats,
    on_new: &(dyn Fn(&FileRecord) + Sync),
) -> Vec<FileRecord> {
    /* parse each file as soon as the walk finds it: one rayon task per file, no wait for the full list */
    let results: Mutex<Vec<(Option<FileRecord>, Option<&'static str>, bool)>> = Mutex::new(Vec::new());
    let refused = Mutex::new(HashMap::<String, usize>::new());
    let prev = &prev;
    rayon::in_place_scope(|s| {
        walk(root, &refused, &|c: Candidate| {
            let results = &results;
            s.spawn(move |_| {
                let r = match prev.get(&c.rel) {
                    Some(p) if p.mtime_ns == c.mtime_ns && p.size == c.size => (Some(p.clone()), None, true),
                    _ => match parse_one(root, &c) {
                        ParseOutcome::Ok(r) => {
                            on_new(&r);
                            (Some(r), None, false)
                        }
                        ParseOutcome::Refused(code) => (None, Some(code), false),
                    },
                };
                results.lock().unwrap().push(r);
            });
        });
    });
    let results = results.into_inner().unwrap();
    for (k, n) in refused.into_inner().unwrap() {
        *stats.refused.entry(k).or_default() += n;
    }
    stats.discovered = results.len();
    let mut recs = Vec::with_capacity(results.len());
    for (r, refused, reused) in results {
        if let Some(code) = refused {
            *stats.refused.entry(code.into()).or_default() += 1;
        }
        if let Some(r) = r {
            if reused {
                stats.reused += 1;
            } else {
                stats.parsed += 1;
            }
            stats.symbols += r.ex.symbols.len();
            stats.calls += r.ex.calls.len();
            stats.bytes += r.size;
            recs.push(r);
        }
    }
    recs.sort_by(|a, b| a.path.cmp(&b.path));
    recs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn indexes_tree_skips_ignored_and_refused() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        fs::create_dir_all(r.join("src")).unwrap();
        fs::create_dir_all(r.join("node_modules/x")).unwrap();
        fs::write(r.join("src/a.rs"), "pub fn a() { b(); }\nfn b() {}").unwrap();
        fs::write(r.join("src/c.ts"), "export function c() {}").unwrap();
        fs::write(r.join("src/key.py"), "K = 'AKIAABCDEFGHIJKLMNOP'").unwrap();
        fs::write(r.join("node_modules/x/i.js"), "function x(){}").unwrap();
        fs::write(r.join("Cargo.lock"), "x").unwrap();
        fs::write(r.join("ignored.js"), "function y(){}").unwrap();
        fs::write(r.join(".gitignore"), "ignored.js\n").unwrap();
        let mut st = IndexStats::default();
        let recs = build(r, HashMap::new(), &mut st);
        let paths: Vec<_> = recs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["src/a.rs", "src/c.ts"]);
        assert_eq!(st.refused.get("REFUSED_SECRET"), Some(&1));
        assert_eq!(st.symbols, 3);

        let prev: HashMap<_, _> = recs.into_iter().map(|r| (r.path.clone(), r)).collect();
        let mut st2 = IndexStats::default();
        build(r, prev, &mut st2);
        assert_eq!((st2.reused, st2.parsed), (2, 0));
    }
}
