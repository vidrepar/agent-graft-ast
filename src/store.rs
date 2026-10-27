//! On-disk index in redb: one row per file (MessagePack-encoded FileRecord).

use crate::index::FileRecord;
use crate::summary::Summary;
use anyhow::{Context, Result};
use rayon::prelude::*;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::path::Path;

const FILES: TableDefinition<&str, &[u8]> = TableDefinition::new("files");
const META: TableDefinition<&str, &str> = TableDefinition::new("meta");
/* v4: summaries carry a compact identifier -> lines index */
const SUMMARIES: TableDefinition<&str, &[u8]> = TableDefinition::new("summaries4");
const GRAPH: TableDefinition<&str, &[u8]> = TableDefinition::new("graph");
const PAGE_CACHE_BYTES: usize = 8 << 20;
const SCHEMA: &str = "agent-graft-ast/4";

pub enum StoreMsg {
    Put(String, Vec<u8>),
    Done(Vec<String>),
    /// resolved graph for the record set with this signature (see `index::files_sig`)
    Graph(u64, Vec<u8>),
}

pub fn encode(r: &FileRecord) -> Vec<u8> {
    rmp_serde::to_vec(r).expect("encode record")
}

/// redb's file backend, except that `sync_data` is fsync(2) instead of macOS's F_FULLFSYNC (~15 ms per commit).
/// The index is a rebuildable cache: a power cut can at worst lose the last commit, which redb's checksums
/// detect on open (the engine then re-indexes). Ordinary crashes lose nothing: fsync still reaches the OS.
#[derive(Debug)]
struct CacheBackend {
    inner: redb::backends::FileBackend,
    #[cfg(unix)]
    fd: i32,
}

impl CacheBackend {
    fn new(file: std::fs::File) -> Result<CacheBackend> {
        #[cfg(unix)]
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        Ok(CacheBackend {
            inner: redb::backends::FileBackend::new(file)?,
            #[cfg(unix)]
            fd,
        })
    }
}

impl redb::StorageBackend for CacheBackend {
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        self.inner.read(offset, out)
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            /* SAFETY: fd belongs to the file `inner` owns and keeps open for our lifetime */
            if unsafe { libc::fsync(self.fd) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        self.inner.sync_data()
    }
    fn write(&self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        self.inner.write(offset, data)
    }
    fn close(&self) -> std::io::Result<()> {
        self.inner.close()
    }
    fn try_lock_range(&self, start: Range0, end: Range0) -> std::result::Result<bool, redb::BackendError> {
        self.inner.try_lock_range(start, end)
    }
    fn try_lock_shared_range(&self, start: Range0, end: Range0) -> std::result::Result<bool, redb::BackendError> {
        self.inner.try_lock_shared_range(start, end)
    }
    fn lock_range(&self, start: Range0, end: Range0) -> std::result::Result<(), redb::BackendError> {
        self.inner.lock_range(start, end)
    }
    fn lock_shared_range(&self, start: Range0, end: Range0) -> std::result::Result<(), redb::BackendError> {
        self.inner.lock_shared_range(start, end)
    }
    fn unlock_range(&self, start: Range0, end: Range0) -> std::result::Result<(), redb::BackendError> {
        self.inner.unlock_range(start, end)
    }
    fn query_lock_range(&self, start: Range0, end: Range0) -> std::result::Result<bool, redb::BackendError> {
        self.inner.query_lock_range(start, end)
    }
}

type Range0 = std::ops::Bound<u64>;

pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        /* the tables are read once at startup: a big page cache would only pin them in memory */
        let db = Database::builder()
            .set_cache_size(PAGE_CACHE_BYTES)
            .create_with_backend(CacheBackend::new(file)?)
            .with_context(|| format!("opening index {}", path.display()))?;
        Ok(Store { db })
    }

    /// All records; empty when the schema tag differs (forces a full rebuild).
    pub fn load(&self) -> Result<HashMap<String, FileRecord>> {
        let txn = self.db.begin_read()?;
        let schema_ok = match txn.open_table(META) {
            Ok(t) => t.get("schema")?.map(|v| v.value() == SCHEMA).unwrap_or(false),
            Err(_) => false,
        };
        if !schema_ok {
            return Ok(HashMap::new());
        }
        let table = txn.open_table(FILES)?;
        let mut raw = Vec::new();
        for row in table.iter()? {
            let (_, v) = row?;
            raw.push(v.value().to_vec());
        }
        let recs: Vec<FileRecord> = raw.par_iter().filter_map(|b| rmp_serde::from_slice(b).ok()).collect();
        Ok(recs.into_iter().map(|r| (r.path.clone(), r)).collect())
    }

    /// The persisted resolved graph, if it was built from records with this signature.
    pub fn load_graph(&self, sig: u64) -> Result<Option<crate::graph::Resolved>> {
        Ok(self.load_graph_signed()?.and_then(|(s, r)| (s == sig).then_some(r)))
    }

    /// The persisted resolved graph with the signature it was built for; the caller checks it, so the decode
    /// can run before the records are loaded.
    pub fn load_graph_signed(&self) -> Result<Option<(u64, crate::graph::Resolved)>> {
        let txn = self.db.begin_read()?;
        let Ok(t) = txn.open_table(GRAPH) else { return Ok(None) };
        let Some(sig) = t.get("sig")?.and_then(|v| <[u8; 8]>::try_from(v.value()).ok()) else { return Ok(None) };
        Ok(t.get("resolved")?.and_then(|v| rmp_serde::from_slice(v.value()).ok()).map(|r| (u64::from_le_bytes(sig), r)))
    }

    pub fn load_summaries(&self) -> Result<HashMap<String, Summary>> {
        let txn = self.db.begin_read()?;
        let Ok(table) = txn.open_table(SUMMARIES) else { return Ok(HashMap::new()) };
        /* decode row by row: never hold every row's raw bytes and its decoded copy at once */
        let mut out = HashMap::new();
        for row in table.iter()? {
            let (k, v) = row?;
            if let Ok(s) = rmp_serde::from_slice::<Summary>(v.value()) {
                out.insert(k.value().to_string(), s);
            }
        }
        Ok(out)
    }

    /// Persisted summaries, decoded on all cores: for a cold process that needs them at once.
    pub fn load_summaries_par(&self) -> Result<Vec<(String, Summary)>> {
        let txn = self.db.begin_read()?;
        let Ok(table) = txn.open_table(SUMMARIES) else { return Ok(Vec::new()) };
        let mut raw = Vec::new();
        for row in table.iter()? {
            let (k, v) = row?;
            raw.push((k.value().to_string(), v.value().to_vec()));
        }
        Ok(raw.into_par_iter().filter_map(|(k, v)| rmp_serde::from_slice::<Summary>(&v).ok().map(|s| (k, s))).collect())
    }

    pub fn save_summaries(&self, items: &[(String, std::sync::Arc<Summary>)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let encoded: Vec<(&str, Vec<u8>)> =
            items.par_iter().map(|(p, s)| (p.as_str(), rmp_serde::to_vec(&**s).expect("encode summary"))).collect();
        let txn = self.db.begin_write()?;
        {
            let mut t = txn.open_table(SUMMARIES)?;
            for (k, v) in &encoded {
                t.insert(*k, v.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Consume encoded records while the parse is still running; `Done` carries the live paths for deletions.
    pub fn write_stream(&self, rx: std::sync::mpsc::Receiver<StoreMsg>, prev: &HashMap<String, (u64, u64)>) -> Result<usize> {
        /* 1. records: one transaction, committed as soon as `Done` arrives (while the graph is still building) */
        let txn = self.db.begin_write()?;
        let (mut n, mut removed) = (0, 0);
        let mut graph: Option<(u64, Vec<u8>)> = None;
        {
            let mut meta = txn.open_table(META)?;
            meta.insert("schema", SCHEMA)?;
            let mut t = txn.open_table(FILES)?;
            for msg in rx.iter() {
                match msg {
                    StoreMsg::Put(path, bytes) => {
                        t.insert(path.as_str(), bytes.as_slice())?;
                        n += 1;
                    }
                    StoreMsg::Graph(sig, bytes) => graph = Some((sig, bytes)),
                    StoreMsg::Done(live) => {
                        let live: std::collections::HashSet<&str> = live.iter().map(|s| s.as_str()).collect();
                        for gone in prev.keys().filter(|k| !live.contains(k.as_str())) {
                            t.remove(gone.as_str())?;
                            removed += 1;
                        }
                        break;
                    }
                }
            }
        }
        /* nothing changed on a warm start: skip the commit (and its fsync) */
        if n == 0 && removed == 0 && !prev.is_empty() {
            txn.abort()?;
        } else {
            txn.commit()?;
        }
        /* 2. the resolved graph: a small second transaction once the build finishes */
        for msg in rx.iter() {
            if let StoreMsg::Graph(sig, bytes) = msg {
                graph = Some((sig, bytes));
            }
        }
        if let Some((sig, bytes)) = graph {
            let txn = self.db.begin_write()?;
            let changed = {
                let mut g = txn.open_table(GRAPH)?;
                let same = g.get("sig")?.map(|v| v.value() == sig.to_le_bytes()).unwrap_or(false);
                if !same {
                    g.insert("sig", sig.to_le_bytes().as_slice())?;
                    g.insert("resolved", bytes.as_slice())?;
                }
                !same
            };
            if changed {
                txn.commit()?;
                n += 1;
            } else {
                txn.abort()?;
            }
        }
        Ok(n)
    }

    /// Write changed records and delete paths that no longer exist, in one transaction.
    pub fn save(&self, recs: &[FileRecord], prev: &HashMap<String, (u64, u64)>) -> Result<usize> {
        let encoded: Vec<(&str, Vec<u8>)> = recs
            .par_iter()
            .filter(|r| prev.get(&r.path) != Some(&(r.mtime_ns, r.size)))
            .map(|r| (r.path.as_str(), rmp_serde::to_vec(r).expect("encode record")))
            .collect();
        let live: std::collections::HashSet<&str> = recs.iter().map(|r| r.path.as_str()).collect();
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            meta.insert("schema", SCHEMA)?;
            let mut t = txn.open_table(FILES)?;
            for (k, v) in &encoded {
                t.insert(*k, v.as_slice())?;
            }
            for gone in prev.keys().filter(|k| !live.contains(k.as_str())) {
                t.remove(gone.as_str())?;
            }
        }
        txn.commit()?;
        Ok(encoded.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Lang;
    use crate::parse::extract;

    #[test]
    fn roundtrip_and_incremental_delete() {
        let dir = tempfile::tempdir().unwrap();
        let st = Store::open(&dir.path().join("i.redb")).unwrap();
        let rec = |p: &str| FileRecord {
            path: p.into(),
            lang: Lang::Rust,
            mtime_ns: 1,
            size: 2,
            hash: 0,
            ex: extract(Lang::Rust, b"pub fn a() {}"),
        };
        assert_eq!(st.save(&[rec("a.rs"), rec("b.rs")], &HashMap::new()).unwrap(), 2);
        let loaded = st.load().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["a.rs"].ex.symbols[0].name, "a");
        let prev: HashMap<_, _> = loaded.values().map(|r| (r.path.clone(), (r.mtime_ns, r.size))).collect();
        assert_eq!(st.save(&[rec("a.rs")], &prev).unwrap(), 0);
        assert_eq!(st.load().unwrap().len(), 1);
    }
}
