//! Exact TypeScript/JavaScript name resolution: a long-lived `tsserver` (the TypeScript language service
//! that editors use) answers rename locations, references and diagnostics, so TS/JS edits match the
//! compiler one to one instead of syntax and heuristics.
//!
//! One server per repo root, started on the first TS/JS request (or pre-warmed by the MCP server), and
//! restarted when it dies. It speaks the tsserver protocol over stdio: one JSON request per line in,
//! `Content-Length`-framed JSON out. Files the engine writes or the watcher sees change are re-sent as
//! open files before the next request, so tsserver never answers from stale text.
//!
//! Which tsserver runs: a repo's own `node_modules/typescript` is code from the repo, so it is used only
//! when the user trusts the repo (`AGENT_GRAFT_ENABLE_CHECK=1`, the same gate as `check`). Otherwise
//! `AGENT_GRAFT_TSSERVER` (a path to `tsserver.js`), or the pinned copy that `agent-graft-ast init
//! --typescript` installs under `~/.cache/agent-graft-ast/typescript/<version>`. Plugins and automatic
//! typings acquisition are off.

use crate::ts_server::Server;
pub use crate::ts_server::{install_command, locate, pinned_dir, ENABLE_HINT, PINNED};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 1-based line and UTF-16 offset of byte `pos` in `text`, as tsserver counts them.
pub fn to_loc(text: &str, pos: usize) -> (u32, u32) {
    let pos = pos.min(text.len());
    let line_start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = text[..line_start].bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    let col: usize = text[line_start..pos].chars().map(char::len_utf16).sum();
    (line, col as u32 + 1)
}

/// Byte position of tsserver's 1-based (line, UTF-16 offset) in `text`.
pub fn to_byte(text: &str, line: u32, offset: u32) -> Option<usize> {
    let mut start = 0;
    for _ in 1..line {
        start += text[start..].find('\n')? + 1;
    }
    let mut units = 0usize;
    for (i, c) in text[start..].char_indices() {
        if units + 1 >= offset as usize {
            return Some(start + i);
        }
        if c == '\n' {
            return None;
        }
        units += c.len_utf16();
    }
    (units + 1 == offset as usize).then_some(text.len())
}

/// One location tsserver returned: absolute path, 1-based (line, offset) start and end.
#[derive(Clone, Debug, PartialEq)]
pub struct Loc {
    pub file: PathBuf,
    pub start: (u32, u32),
    pub end: (u32, u32),
    /// rename only: text to put before / after the new name (`x: ` for a shorthand property)
    pub prefix: String,
    pub suffix: String,
    /// references only: this location declares the symbol
    pub is_def: bool,
}

/// A compiler error tsserver reports for a file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Diag {
    pub file: PathBuf,
    pub line: u32,
    pub code: i64,
    pub text: String,
}

fn pos(v: &Value) -> (u32, u32) {
    (v["line"].as_u64().unwrap_or(0) as u32, v["offset"].as_u64().unwrap_or(0) as u32)
}


enum State {
    /// not started yet (or stopped; the next request starts it)
    Idle,
    Live(Server),
    /// cannot run: the reason, shown in every affected reply
    Down(String),
}

/// The repo's tsserver, shared by the engine and the MCP warm-up thread.
pub struct TsService {
    root: PathBuf,
    /// off: no tsserver is ever started and replies carry no exact/fallback note (unit tests)
    quiet_off: bool,
    st: Mutex<State>,
    /// files changed on disk since tsserver last saw them
    dirty: Mutex<HashSet<PathBuf>>,
    /// one file per TS/JS project, kept open so every project is loaded: tsserver only searches loaded
    /// projects for references
    projects: Mutex<Option<Vec<PathBuf>>>,
}

/// Why a request was answered by the syntax engine instead.
#[derive(Clone, Debug)]
pub struct Fallback(pub String);

impl TsService {
    /// The service for `root`: lazily started. `AGENT_GRAFT_TS_EXACT=0` turns it off (replies still say so).
    pub fn new(root: &Path) -> TsService {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let st = if std::env::var_os("AGENT_GRAFT_TS_EXACT").is_some_and(|v| v == "0") {
            State::Down("turned off by AGENT_GRAFT_TS_EXACT=0".into())
        } else {
            State::Idle
        };
        /* unit tests stay on the syntax engine unless a test turns tsserver on */
        TsService { root, quiet_off: cfg!(test), st: Mutex::new(st), dirty: Mutex::new(HashSet::new()), projects: Mutex::new(None) }
    }

    /// A service that always runs (tests that exercise tsserver).
    pub fn enabled(root: &Path) -> TsService {
        let mut s = TsService::new(root);
        s.quiet_off = false;
        s
    }

    /// A service that never runs, for this reason (replies fall back and say why).
    pub fn unavailable(root: &Path, why: &str) -> TsService {
        let s = TsService::enabled(root);
        *s.st.lock().unwrap_or_else(|p| p.into_inner()) = State::Down(why.to_string());
        s
    }

    /// Off without a note: callers use the syntax engine and say nothing about exactness.
    pub fn is_quiet_off(&self) -> bool {
        self.quiet_off
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A file changed on disk (an engine write or the watcher): tsserver gets its text before the next request.
    pub fn touch(&self, abs: PathBuf) {
        if self.quiet_off {
            return;
        }
        self.dirty.lock().unwrap_or_else(|p| p.into_inner()).insert(abs);
    }

    /// Run `f` on a live server (started if needed, restarted once if it died), with every changed file synced.
    fn with<R>(&self, f: impl Fn(&mut Server) -> Result<R, String>) -> Result<R, Fallback> {
        if self.quiet_off {
            return Err(Fallback("off".into()));
        }
        let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
        for attempt in 0..2 {
            if let State::Idle = *st {
                *st = match Server::start(&self.root) {
                    Ok(s) => State::Live(s),
                    Err(e) => State::Down(e),
                };
            }
            let server = match &mut *st {
                State::Live(s) => s,
                State::Down(why) => return Err(Fallback(why.clone())),
                State::Idle => unreachable!(),
            };
            let dirty: Vec<PathBuf> = {
                let mut d = self.dirty.lock().unwrap_or_else(|p| p.into_inner());
                /* open files changed by hand since they were sent are re-sent as well */
                d.extend(server.open.keys().cloned());
                d.extend(self.projects.lock().unwrap_or_else(|p| p.into_inner()).iter().flatten().cloned());
                d.drain().collect()
            };
            let r = server.open_files(&self.root, &dirty).and_then(|_| f(server));
            match r {
                Err(e) if e.starts_with("tsserver stopped") => {
                    *st = State::Idle;
                    /* the restarted server reads every file from disk again */
                    if attempt == 1 {
                        return Err(Fallback(e));
                    }
                }
                Err(e) => return Err(Fallback(format!("tsserver: {e}"))),
                Ok(v) => return Ok(v),
            }
        }
        unreachable!()
    }

    /// Whether the projects are loaded (see `load_projects`).
    pub fn has_projects(&self) -> bool {
        self.projects.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }

    /// Load every project and keep one file of each open from then on, so every project is searched.
    /// Per project, the first candidate tsserver places in that project's config is kept.
    pub fn load_projects(&self, groups: &[(Option<PathBuf>, Vec<PathBuf>)]) -> Result<Vec<PathBuf>, Fallback> {
        let picked = self.with(|s| {
            let mut picked = Vec::new();
            for (cfg, cands) in groups {
                let mut pick = None;
                for c in cands {
                    s.open_files(&self.root, std::slice::from_ref(c))?;
                    let info = s.request("projectInfo", json!({"file": c, "needFileNameList": false}))?;
                    let in_cfg = info["configFileName"].as_str().map(PathBuf::from);
                    if cfg.is_none() || in_cfg.as_ref() == cfg.as_ref() {
                        pick = Some(c.clone());
                        break;
                    }
                }
                picked.extend(pick.or_else(|| cands.first().cloned()));
            }
            Ok(picked)
        })?;
        *self.projects.lock().unwrap_or_else(|p| p.into_inner()) = Some(picked.clone());
        Ok(picked)
    }

    /// Load every project before the first edit and build each one's type checker by checking one file.
    pub fn prewarm(&self, groups: &[(Option<PathBuf>, Vec<PathBuf>)]) -> Result<(), Fallback> {
        let files = self.load_projects(groups)?;
        self.with(|s| {
            for f in &files {
                s.request("semanticDiagnosticsSync", json!({"file": f}))?;
            }
            Ok(())
        })
    }

    /// Rename locations of the symbol at (line, offset) in `file`, comments too when `comments`.
    /// Err(Fallback) when tsserver cannot run; Ok(Err(reason)) when it refuses this rename.
    pub fn rename(&self, file: &Path, at: (u32, u32), comments: bool) -> Result<Result<Vec<Loc>, String>, Fallback> {
        self.with(|s| {
            s.open_files(&self.root, &[file.to_path_buf()])?;
            let body = s.request(
                "rename",
                json!({"file": file, "line": at.0, "offset": at.1, "findInStrings": false, "findInComments": comments}),
            )?;
            if body["info"]["canRename"].as_bool() != Some(true) {
                return Ok(Err(body["info"]["localizedErrorMessage"].as_str().unwrap_or("tsserver cannot rename this").to_string()));
            }
            let mut out = Vec::new();
            for group in body["locs"].as_array().into_iter().flatten() {
                let file = PathBuf::from(group["file"].as_str().unwrap_or(""));
                for l in group["locs"].as_array().into_iter().flatten() {
                    out.push(Loc {
                        file: file.clone(),
                        start: pos(&l["start"]),
                        end: pos(&l["end"]),
                        prefix: l["prefixText"].as_str().unwrap_or("").to_string(),
                        suffix: l["suffixText"].as_str().unwrap_or("").to_string(),
                        is_def: false,
                    });
                }
            }
            Ok(Ok(out))
        })
    }

    /// Every reference to the symbol at (line, offset) in `file`, declarations included.
    pub fn references(&self, file: &Path, at: (u32, u32)) -> Result<Vec<Loc>, Fallback> {
        self.with(|s| {
            s.open_files(&self.root, &[file.to_path_buf()])?;
            let body = s.request("references", json!({"file": file, "line": at.0, "offset": at.1}))?;
            Ok(body["refs"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| Loc {
                    file: PathBuf::from(r["file"].as_str().unwrap_or("")),
                    start: pos(&r["start"]),
                    end: pos(&r["end"]),
                    prefix: String::new(),
                    suffix: String::new(),
                    is_def: r["isDefinition"].as_bool().unwrap_or(false),
                })
                .collect())
        })
    }

    /// Syntax and type errors in these files, as tsserver sees them now, checked in order until
    /// `deadline`; the files left unchecked come back second.
    pub fn errors(&self, files: &[PathBuf], deadline: std::time::Instant) -> Result<(Vec<Diag>, Vec<PathBuf>), Fallback> {
        self.with(|s| {
            s.open_files(&self.root, files)?;
            let mut out = Vec::new();
            for (i, f) in files.iter().enumerate() {
                if std::time::Instant::now() >= deadline {
                    return Ok((out, files[i..].to_vec()));
                }
                for cmd in ["syntacticDiagnosticsSync", "semanticDiagnosticsSync"] {
                    let body = s.request(cmd, json!({"file": f}))?;
                    for d in body.as_array().into_iter().flatten().filter(|d| d["category"] == "error") {
                        out.push(Diag {
                            file: f.clone(),
                            line: pos(&d["start"]).0,
                            code: d["code"].as_i64().unwrap_or(0),
                            text: d["text"].as_str().unwrap_or("").to_string(),
                        });
                    }
                }
            }
            Ok((out, Vec::new()))
        })
    }

    /// Resident memory of the tsserver process in KiB, when it runs.
    pub fn rss_kb(&self) -> Option<u64> {
        match &*self.st.lock().unwrap_or_else(|p| p.into_inner()) {
            State::Live(s) => s.rss_kb(),
            _ => None,
        }
    }

    /// Seconds since the running server started (None when not running).
    pub fn uptime(&self) -> Option<f64> {
        match &*self.st.lock().unwrap_or_else(|p| p.into_inner()) {
            State::Live(s) => Some(s.started.elapsed().as_secs_f64()),
            _ => None,
        }
    }

    /// Stop the server (it restarts on the next request).
    pub fn stop(&self) {
        let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
        if let State::Live(_) = *st {
            *st = State::Idle;
        }
    }
}

#[cfg(test)]
#[path = "ts_exact_tests.rs"]
pub(crate) mod tests;
