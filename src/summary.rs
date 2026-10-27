//! Per-file read cache: folded text and token counts, keyed by content hash, so a read never re-parses.

use crate::index::{self, FileRecord};
use crate::skeleton;
use crate::store::Store;
use crate::tokens::count;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    /// content hash of the source this summary was built from
    pub hash: u64,
    pub raw_tokens: u32,
    pub l1: String,
    pub l1_tokens: u32,
    pub l2: String,
    pub l2_tokens: u32,
    /// Lines of every identifier, so `context` finds every line that names a symbol without reading a file
    #[serde(default)]
    pub occ: Occ,
}

impl Summary {
    pub fn level(&self, level: u8) -> Option<(&str, usize)> {
        match level {
            1 => Some((&self.l1, self.l1_tokens as usize)),
            2 => Some((&self.l2, self.l2_tokens as usize)),
            _ => None,
        }
    }
}

/// path -> summary; shared between the request path and the background warmer.
pub type Cache = Arc<RwLock<HashMap<String, Arc<Summary>>>>;

pub fn new_cache() -> Cache {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Summary of `src`; None when the bytes no longer match the record (changed mid-read) or fail the content guard.
pub fn compute(rec: &FileRecord, src: &[u8]) -> Option<Summary> {
    if xxhash_rust::xxh3::xxh3_64(src) != rec.hash || crate::guard::check_content(src).is_some() {
        return None;
    }
    let l1 = skeleton::render(rec.lang, src, &rec.ex, 1);
    let l2 = skeleton::render(rec.lang, src, &rec.ex, 2);
    Some(Summary {
        hash: rec.hash,
        raw_tokens: count(&String::from_utf8_lossy(src)) as u32,
        l1_tokens: count(&l1) as u32,
        l2_tokens: count(&l2) as u32,
        l1,
        l2,
        occ: Occ::from_pairs(occurrences(src)),
    })
}

/// Hash of one identifier as stored in `Summary::idents`.
pub fn ident_hash(name: &[u8]) -> u32 {
    xxhash_rust::xxh3::xxh3_64(name) as u32
}

/// (identifier hash, line) of every identifier in a source file, sorted, one entry per line.
fn occurrences(src: &[u8]) -> Vec<(u32, u32)> {
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut v = Vec::new();
    let (mut i, mut line) = (0, 1u32);
    while i < src.len() {
        let b = src[i];
        if b == b'\n' {
            line += 1;
            i += 1;
        } else if is_ident(b) {
            let start = i;
            while i < src.len() && is_ident(src[i]) {
                i += 1;
            }
            if !src[start].is_ascii_digit() {
                v.push((ident_hash(&src[start..i]), line));
            }
        } else {
            i += 1;
        }
    }
    v.sort_unstable();
    v.dedup();
    v
}

/// Compact identifier → lines index: one hash per distinct identifier, its lines as varint deltas
/// (~2.7 bytes per occurrence instead of 8).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Occ {
    /// sorted distinct identifier hashes
    hashes: Vec<u32>,
    /// start of each hash's line deltas in `deltas` (one more entry than `hashes`)
    starts: Vec<u32>,
    /// ascending lines per hash, LEB128 varint deltas from 0
    deltas: Vec<u8>,
}

impl Occ {
    /// From (hash, line) pairs sorted by hash then line, deduped.
    pub fn from_pairs(pairs: Vec<(u32, u32)>) -> Occ {
        let mut o = Occ::default();
        let mut prev_line = 0u32;
        for (h, line) in pairs {
            if o.hashes.last() != Some(&h) {
                o.hashes.push(h);
                o.starts.push(o.deltas.len() as u32);
                prev_line = 0;
            }
            let mut d = line - prev_line;
            prev_line = line;
            loop {
                let byte = (d & 0x7f) as u8;
                d >>= 7;
                if d == 0 {
                    o.deltas.push(byte);
                    break;
                }
                o.deltas.push(byte | 0x80);
            }
        }
        o.starts.push(o.deltas.len() as u32);
        o
    }

    /// Structurally sound (it is read from disk): one start per hash plus an end, in order, inside `deltas`.
    pub fn is_valid(&self) -> bool {
        self.starts.len() == self.hashes.len() + 1
            && self.starts.windows(2).all(|w| w[0] <= w[1])
            && self.starts.last().map(|&e| e as usize <= self.deltas.len()).unwrap_or(false)
            && self.hashes.windows(2).all(|w| w[0] < w[1])
    }

    /// Lines (ascending) where the identifier with hash `h` occurs; empty on any malformed data.
    pub fn lines_of(&self, h: u32) -> Vec<u32> {
        let Ok(i) = self.hashes.binary_search(&h) else { return Vec::new() };
        let (Some(&a), Some(&b)) = (self.starts.get(i), self.starts.get(i + 1)) else { return Vec::new() };
        let Some(bytes) = self.deltas.get(a as usize..b as usize) else { return Vec::new() };
        let (mut out, mut line, mut acc, mut shift) = (Vec::new(), 0u32, 0u32, 0u32);
        for &byte in bytes {
            if shift >= 32 {
                return Vec::new();
            }
            acc |= ((byte & 0x7f) as u32) << shift;
            if byte & 0x80 == 0 {
                let Some(next) = line.checked_add(acc) else { return Vec::new() };
                line = next;
                out.push(line);
                acc = 0;
                shift = 0;
            } else {
                shift += 7;
            }
        }
        out
    }
}

impl Summary {
    /// Lines (ascending) where the identifier with hash `h` occurs.
    pub fn lines_of(&self, h: u32) -> Vec<u32> {
        self.occ.lines_of(h)
    }
}

/// Cached summary for `rec` if its hash still matches.
pub fn get(cache: &Cache, rec: &FileRecord) -> Option<Arc<Summary>> {
    let c = cache.read().unwrap_or_else(|p| p.into_inner());
    c.get(&rec.path).filter(|s| s.hash == rec.hash).cloned()
}

pub fn put(cache: &Cache, path: &str, s: Summary) -> Arc<Summary> {
    let s = Arc::new(s);
    cache.write().unwrap_or_else(|p| p.into_inner()).insert(path.to_string(), s.clone());
    s
}

/// Persisted summaries whose hash still matches, into an empty cache. A cold process (a hook, the CLI) gets the
/// identifier index this way in a few ms instead of scanning every file; a warm cache is left alone.
pub fn load_persisted(files: &[FileRecord], store: &Store, cache: &Cache) {
    if !cache.read().unwrap_or_else(|p| p.into_inner()).is_empty() {
        return;
    }
    let Ok(rows) = store.load_summaries_par() else { return };
    if rows.is_empty() {
        return;
    }
    let hashes: HashMap<&str, u64> = files.iter().map(|f| (f.path.as_str(), f.hash)).collect();
    let mut c = cache.write().unwrap_or_else(|p| p.into_inner());
    for (path, s) in rows {
        if hashes.get(path.as_str()) == Some(&s.hash) && s.occ.is_valid() {
            c.entry(path).or_insert_with(|| Arc::new(s));
        }
    }
}

/// Fill the cache for every file: persisted summaries first, then compute the rest in parallel and persist them.
pub fn warm_all(root: &Path, files: &[FileRecord], store: &Store, cache: &Cache) -> usize {
    let mut persisted = store.load_summaries().unwrap_or_default();
    let fresh = warm_batch(root, files, &mut persisted, cache);
    let n = fresh.len();
    /* persist so the next server start skips the work */
    let _ = store.save_summaries(&fresh);
    n
}

/// One batch of `warm_all`: cached or persisted summaries are reused; the rest are computed and returned.
/// Persisted summaries are moved (not copied) into the cache; the new ones are returned for saving.
pub fn warm_batch(root: &Path, files: &[FileRecord], persisted: &mut HashMap<String, Summary>, cache: &Cache) -> Vec<(String, Arc<Summary>)> {
    /* 1. persisted summaries whose hash still matches */
    let mut missing = Vec::new();
    {
        let mut c = cache.write().unwrap_or_else(|p| p.into_inner());
        for f in files {
            match persisted.remove(&f.path) {
                Some(s) if s.hash == f.hash && s.occ.is_valid() => {
                    c.entry(f.path.clone()).or_insert_with(|| Arc::new(s));
                }
                _ if c.get(&f.path).map(|s| s.hash == f.hash).unwrap_or(false) => {}
                _ => missing.push(f),
            }
        }
    }
    /* 2. compute the rest on all cores */
    let fresh: Vec<(String, Arc<Summary>)> = missing
        .par_iter()
        .filter_map(|f| index::with_bytes(&root.join(&f.path), |b| compute(f, b).map(|s| (f.path.clone(), Arc::new(s)))).ok().flatten())
        .collect();
    {
        let mut c = cache.write().unwrap_or_else(|p| p.into_inner());
        for (p, s) in &fresh {
            c.insert(p.clone(), s.clone());
        }
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Lang;
    use crate::parse::extract;

    #[test]
    fn occ_roundtrips_lines() {
        let pairs = vec![(1, 1), (1, 2), (1, 300), (1, 70_000), (7, 5)];
        let o = Occ::from_pairs(pairs);
        assert_eq!(o.lines_of(1), vec![1, 2, 300, 70_000]);
        assert_eq!(o.lines_of(7), vec![5]);
        assert!(o.lines_of(3).is_empty());
        assert!(o.is_valid());
        let mut bad = o.clone();
        bad.starts[1] = 9_999;
        assert!(!bad.is_valid());
        assert!(bad.lines_of(1).is_empty());
        let s = compute_for_test(b"fn a() {}\nlet a = b;\n// a\n");
        assert_eq!(s.lines_of(ident_hash(b"a")), vec![1, 2, 3]);
        assert_eq!(s.lines_of(ident_hash(b"b")), vec![2]);
    }

    fn compute_for_test(src: &[u8]) -> Summary {
        let rec = FileRecord { path: "x.rs".into(), lang: Lang::Rust, mtime_ns: 0, size: src.len() as u64, hash: xxhash_rust::xxh3::xxh3_64(src), ex: extract(Lang::Rust, src) };
        compute(&rec, src).unwrap()
    }

    #[test]
    fn summary_matches_live_render_and_is_persisted() {
        let d = tempfile::tempdir().unwrap();
        let src = b"/// doc\npub fn a() {\n    let x = 1;\n    let y = 2;\n    x + y;\n}\n";
        std::fs::write(d.path().join("a.rs"), src).unwrap();
        let rec = FileRecord {
            path: "a.rs".into(),
            lang: Lang::Rust,
            mtime_ns: 1,
            size: src.len() as u64,
            hash: xxhash_rust::xxh3::xxh3_64(src),
            ex: extract(Lang::Rust, src),
        };
        let store = Store::open(&d.path().join("i.redb")).unwrap();
        let cache = new_cache();
        assert_eq!(warm_all(d.path(), std::slice::from_ref(&rec), &store, &cache), 1);
        let s = get(&cache, &rec).unwrap();
        assert_eq!(s.l1, skeleton::render(Lang::Rust, src, &rec.ex, 1));
        assert_eq!(s.l2_tokens as usize, count(&s.l2));
        /* a second warm finds it on disk and computes nothing */
        let cache2 = new_cache();
        assert_eq!(warm_all(d.path(), std::slice::from_ref(&rec), &store, &cache2), 0);
        assert!(get(&cache2, &rec).is_some());
        /* bytes that no longer match the record are never summarized */
        assert!(compute(&rec, b"pub fn other() {}").is_none());
        /* a changed hash invalidates it */
        let mut changed = rec.clone();
        changed.hash ^= 1;
        assert!(get(&cache2, &changed).is_none());
    }
}
