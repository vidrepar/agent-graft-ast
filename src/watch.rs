//! File watching. macOS FSEvents delivers ~10 ms after a write even with zero latency, so every
//! indexed file is also watched directly with kqueue (`EVFILT_VNODE`), which fires as the write lands.
//! FSEvents stays on for files created later; they join the kqueue set once indexed.

use crate::lang::Lang;
use crate::mcp::Slot;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Leave this many descriptors for everything else the process opens.
const FD_HEADROOM: u64 = 512;

fn rel_of(root: &Path, p: &Path) -> Option<String> {
    let r = p.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
    let hidden = r.split('/').any(|c| c.starts_with('.') || crate::index::SKIP_DIRS.contains(&c));
    (Lang::from_path(p).is_some() && !hidden).then_some(r)
}

/// Apply a batch of changed paths under the engine lock (one patch per file).
fn apply(slot: &Slot, rels: &[String]) -> Vec<String> {
    slot.with(|e| rels.iter().filter(|r| e.refresh(r)).cloned().collect())
}

/// Watch `root` for the life of the server. `indexed` = repo-relative paths to watch with kqueue.
pub fn run(root: PathBuf, slot: Arc<Slot>, indexed: Vec<String>) {
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    /* FSEvents for creations anywhere under root */
    let fs_tx = tx.clone();
    let mut fsevents = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            for p in ev.paths {
                let _ = fs_tx.send(p);
            }
        }
    })
    .ok();
    if let Some(w) = fsevents.as_mut() {
        use notify::Watcher;
        let _ = w.watch(&root, notify::RecursiveMode::Recursive);
    }
    #[cfg(target_os = "macos")]
    {
        let kq = kqueue::Kq::new(&root, tx);
        if let Some(kq) = kq {
            let kq = Arc::new(kq);
            for r in &indexed {
                kq.add(r);
            }
            let k2 = kq.clone();
            std::thread::spawn(move || k2.run());
            drain(&root, &slot, &rx, Some(&kq));
            return;
        }
    }
    let _ = indexed;
    drain(&root, &slot, &rx, None);
    drop(fsevents);
}

#[cfg(target_os = "macos")]
type KqRef<'a> = Option<&'a kqueue::Kq>;
#[cfg(not(target_os = "macos"))]
type KqRef<'a> = Option<&'a ()>;

/// A save often truncates first and writes second: an empty file is held this long for the write to land.
const EMPTY_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

fn drain(root: &Path, slot: &Slot, rx: &std::sync::mpsc::Receiver<PathBuf>, kq: KqRef) {
    let mut held: HashMap<String, std::time::Instant> = HashMap::new();
    loop {
        /* 1. wait for events; wake for a held empty file when its grace runs out */
        let first = match held.values().min() {
            Some(&due) => match rx.recv_timeout(due.saturating_duration_since(std::time::Instant::now())) {
                Ok(p) => Some(p),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(p) => Some(p),
                Err(_) => return,
            },
        };
        /* 2. coalesce whatever is already queued */
        let mut batch: Vec<PathBuf> = first.into_iter().collect();
        batch.extend(rx.try_iter());
        let mut rels: Vec<String> = batch.iter().filter_map(|p| rel_of(root, p)).collect();
        let now = std::time::Instant::now();
        rels.extend(held.iter().filter(|(_, due)| **due <= now).map(|(r, _)| r.clone()));
        rels.sort();
        rels.dedup();
        /* 3. hold files that are empty right now (mid-save); apply everything else at once */
        let mut ready = Vec::new();
        for r in rels {
            let empty = std::fs::metadata(root.join(&r)).map(|m| m.len() == 0).unwrap_or(false);
            match held.get(&r) {
                Some(due) if empty && *due > now => {}
                None if empty => {
                    held.insert(r, now + EMPTY_GRACE);
                }
                _ => {
                    held.remove(&r);
                    ready.push(r);
                }
            }
        }
        if ready.is_empty() {
            continue;
        }
        let changed = apply(slot, &ready);
        #[cfg(target_os = "macos")]
        if let Some(kq) = kq {
            /* new files and atomic saves (new inode) get a fresh kqueue registration */
            for r in &changed {
                kq.add(r);
            }
        }
        let _ = (changed, &kq);
    }
}

#[cfg(target_os = "macos")]
mod kqueue {
    use super::*;
    use std::ffi::CString;
    use std::sync::Mutex;

    pub struct Kq {
        fd: i32,
        root: PathBuf,
        /// ident (file descriptor) -> repo-relative path
        files: Mutex<HashMap<usize, String>>,
        /// path -> ident, so a re-registration closes the old descriptor
        by_path: Mutex<HashMap<String, usize>>,
        tx: std::sync::mpsc::Sender<PathBuf>,
        limit: u64,
    }

    impl Kq {
        pub fn new(root: &Path, tx: std::sync::mpsc::Sender<PathBuf>) -> Option<Kq> {
            /* SAFETY: plain syscalls; rlimit struct is fully initialized before use */
            let (fd, limit) = unsafe {
                let fd = libc::kqueue();
                let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl);
                let want = rl.rlim_max.min(24_576);
                if rl.rlim_cur < want {
                    rl.rlim_cur = want;
                    libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
                    libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl);
                }
                (fd, rl.rlim_cur)
            };
            if fd < 0 {
                return None;
            }
            Some(Kq {
                fd,
                root: root.to_path_buf(),
                files: Mutex::new(HashMap::new()),
                by_path: Mutex::new(HashMap::new()),
                tx,
                limit: limit.saturating_sub(FD_HEADROOM),
            })
        }

        /// Watch one file for writes, renames and deletes (re-registers if already watched).
        pub fn add(&self, rel: &str) {
            let mut by_path = self.by_path.lock().unwrap_or_else(|p| p.into_inner());
            let mut files = self.files.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(old) = by_path.remove(rel) {
                files.remove(&old);
                /* SAFETY: closing a descriptor we opened; kqueue drops its registration with it */
                unsafe { libc::close(old as i32) };
            }
            if files.len() as u64 >= self.limit {
                return;
            }
            let Ok(c) = CString::new(self.root.join(rel).to_string_lossy().as_bytes()) else { return };
            /* SAFETY: valid NUL-terminated path; O_EVTONLY opens for event notification only */
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY) };
            if fd < 0 {
                return;
            }
            let ev = libc::kevent {
                ident: fd as usize,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_ATTRIB,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            /* SAFETY: one fully initialized change entry, no event list */
            let rc = unsafe { libc::kevent(self.fd, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            if rc < 0 {
                /* SAFETY: closing the descriptor we just opened */
                unsafe { libc::close(fd) };
                return;
            }
            files.insert(fd as usize, rel.to_string());
            by_path.insert(rel.to_string(), fd as usize);
        }

        /// Block on kqueue and forward every touched file's path to the patch loop.
        pub fn run(&self) {
            let mut evs: Vec<libc::kevent> = (0..64)
                .map(|_| libc::kevent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: std::ptr::null_mut() })
                .collect();
            loop {
                /* SAFETY: evs has room for 64 events; null timeout blocks until one arrives */
                let n = unsafe { libc::kevent(self.fd, std::ptr::null(), 0, evs.as_mut_ptr(), 64, std::ptr::null()) };
                if n < 0 {
                    return;
                }
                let files = self.files.lock().unwrap_or_else(|p| p.into_inner());
                for ev in &evs[..n as usize] {
                    let ident = ev.ident; /* copy out of the packed struct */
                    if let Some(rel) = files.get(&ident) {
                        if self.tx.send(self.root.join(rel)).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}
