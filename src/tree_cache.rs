//! Parsed syntax trees kept in memory for structural search, bounded by an estimate of their size.
//! Walking a kept tree costs a fifth of parsing the file again. Filled by full parses that searches
//! already did, and by the MCP server while it is idle (`preparse`); least recently used trees go first.

use crate::lang::Lang;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tree_sitter::Tree;

/// Measured on rust-analyzer: 1498 trees take ~360 MB for 17 MB of source.
const TREE_BYTES_PER_SOURCE_BYTE: usize = 21;

/// Default budget; `AGENT_GRAFT_TREE_CACHE_MB` overrides it, and 0 turns the cache off.
const DEFAULT_MB: usize = 256;

struct Entry {
    hash: u64,
    tree: Tree,
    cost: usize,
    used: u64,
}

#[derive(Default)]
struct Inner {
    map: HashMap<String, Entry>,
    bytes: usize,
    tick: u64,
}

pub struct TreeCache {
    inner: Mutex<Inner>,
    budget: usize,
    /// set while a tool call runs: the idle parser waits instead of competing for CPU
    pub busy: AtomicBool,
}

impl Default for TreeCache {
    fn default() -> Self {
        let mb = std::env::var("AGENT_GRAFT_TREE_CACHE_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MB);
        TreeCache::with_budget(mb * 1024 * 1024)
    }
}

impl TreeCache {
    pub fn with_budget(budget: usize) -> TreeCache {
        TreeCache { inner: Mutex::new(Inner::default()), budget, busy: AtomicBool::new(false) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The tree of `path` if it was parsed from text hashing to `hash`.
    pub fn get(&self, path: &str, hash: u64) -> Option<Tree> {
        let mut g = self.lock();
        g.tick += 1;
        let tick = g.tick;
        let e = g.map.get_mut(path).filter(|e| e.hash == hash)?;
        e.used = tick;
        Some(e.tree.clone())
    }

    pub fn contains(&self, path: &str, hash: u64) -> bool {
        self.lock().map.get(path).is_some_and(|e| e.hash == hash)
    }

    /// True when a tree for a source of `src_len` bytes fits without evicting anything.
    pub fn has_room(&self, src_len: usize) -> bool {
        self.lock().bytes + src_len * TREE_BYTES_PER_SOURCE_BYTE <= self.budget
    }

    /// Keep `tree` (the whole file's) for `path`, evicting the least recently used trees to fit.
    pub fn put(&self, path: &str, hash: u64, tree: Tree, src_len: usize) {
        let cost = src_len * TREE_BYTES_PER_SOURCE_BYTE;
        if cost > self.budget / 2 {
            return;
        }
        let mut evicted: Vec<Tree> = Vec::new();
        {
            let mut g = self.lock();
            g.tick += 1;
            let used = g.tick;
            if let Some(old) = g.map.insert(path.to_string(), Entry { hash, tree, cost, used }) {
                g.bytes -= old.cost;
                evicted.push(old.tree);
            }
            g.bytes += cost;
            while g.bytes > self.budget {
                let Some(victim) = g.map.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone()) else { break };
                if let Some(e) = g.map.remove(&victim) {
                    g.bytes -= e.cost;
                    evicted.push(e.tree);
                }
            }
        }
        /* freeing trees is slow: never on the caller's path */
        if !evicted.is_empty() {
            rayon::spawn(move || drop(evicted));
        }
    }

    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Parse `files` (path, language, indexed hash) into the cache in order until the budget is full,
    /// one at a time and only while no tool call runs. `root` is the repo root.
    pub fn preparse(&self, root: &std::path::Path, files: &[(String, Lang, u64)]) {
        if self.budget == 0 {
            return;
        }
        for (path, lang, hash) in files {
            while self.busy.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if self.contains(path, *hash) {
                continue;
            }
            let Ok(Some((tree, len))) = crate::index::with_bytes(&root.join(path), |b| {
                (xxhash_rust::xxh3::xxh3_64(b) == *hash && self.has_room(b.len())).then(|| crate::parse::parse_tree(*lang, b).map(|t| (t, b.len()))).flatten()
            }) else {
                continue;
            };
            if !self.has_room(len) {
                drop(tree);
                break;
            }
            self.put(path, *hash, tree, len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(src: &str) -> Tree {
        crate::parse::parse_tree(Lang::Rust, src.as_bytes()).unwrap()
    }

    #[test]
    fn hit_needs_same_hash_and_lru_evicts_oldest() {
        let c = TreeCache::with_budget(100 * TREE_BYTES_PER_SOURCE_BYTE);
        c.put("a.rs", 1, tree("fn a() {}"), 30);
        c.put("b.rs", 2, tree("fn b() {}"), 30);
        assert!(c.get("a.rs", 1).is_some() && c.get("a.rs", 9).is_none());
        /* a was used last, so b goes when c needs room */
        c.put("c.rs", 3, tree("fn c() {}"), 50);
        assert!(c.contains("a.rs", 1) && !c.contains("b.rs", 2) && c.contains("c.rs", 3));
        assert!(!c.has_room(30));
    }

    #[test]
    fn preparse_fills_until_the_budget_is_full() {
        let d = tempfile::tempdir().unwrap();
        let src = "fn f() { let x = 1; }\n";
        let mut files = Vec::new();
        for i in 0..5 {
            std::fs::write(d.path().join(format!("{i}.rs")), src).unwrap();
            files.push((format!("{i}.rs"), Lang::Rust, xxhash_rust::xxh3::xxh3_64(src.as_bytes())));
        }
        let c = TreeCache::with_budget(3 * src.len() * TREE_BYTES_PER_SOURCE_BYTE);
        c.preparse(d.path(), &files);
        assert_eq!(c.len(), 3);
    }
}
