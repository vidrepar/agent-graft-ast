//! The tsserver child process: which one runs, how it starts, and the protocol (one JSON request per
//! line on stdin, `Content-Length`-framed JSON responses on stdout).

use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

pub const PINNED: &str = "6.0.3";

/// One line telling the agent how to turn exact mode on.
pub const ENABLE_HINT: &str = "Exact TypeScript mode needs Node.js and tsserver: run `agent-graft-ast init --typescript` (installs TypeScript 6.0.3 with pnpm into ~/.cache/agent-graft-ast/typescript) or set AGENT_GRAFT_TSSERVER=/path/to/typescript/lib/tsserver.js, then restart the server.";

/// Default seconds one tsserver request may take (a cold project load included).
const DEFAULT_TIMEOUT_S: u64 = 120;

/// Where `init --typescript` installs the pinned TypeScript.
pub fn pinned_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cache/agent-graft-ast/typescript").join(PINNED))
}

/// The shell command that installs the pinned TypeScript (shown by `init`, run by `init --typescript`).
pub fn install_command(dir: &Path) -> String {
    format!(
        "mkdir -p '{d}' && cd '{d}' && printf '{{\"private\":true}}' > package.json && pnpm add --ignore-scripts --config.node-linker=hoisted typescript@{PINNED}",
        d = dir.display()
    )
}

fn timeout() -> Duration {
    let s = std::env::var("AGENT_GRAFT_TS_TIMEOUT_S").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(DEFAULT_TIMEOUT_S);
    Duration::from_secs(s.max(1))
}

/// `exe` in an absolute PATH entry outside the repo. A relative entry (`.`, `node_modules/.bin`) would
/// resolve inside the repo, where the child runs, so the repo could plant the binary.
fn on_path(exe: &str, path: Option<OsString>, root: &Path) -> Result<PathBuf, String> {
    let mut planted = None;
    for dir in std::env::split_paths(&path.ok_or("PATH is not set")?).filter(|d| d.is_absolute()) {
        let Ok(p) = dir.join(exe).canonicalize() else { continue };
        if !p.is_file() {
            continue;
        }
        if p.starts_with(root) {
            planted = Some(p);
            continue;
        }
        return Ok(p);
    }
    Err(match planted {
        Some(p) => format!("the only {exe} on PATH is inside the repo ({}); refused", p.display()),
        None => format!("{exe} is not on PATH"),
    })
}

/// An absolute path from an environment variable, canonicalized; a relative one is refused (it would
/// resolve against wherever the server was started).
fn env_path(name: &str, v: OsString) -> Result<PathBuf, String> {
    let p = PathBuf::from(&v);
    if !p.is_absolute() {
        return Err(format!("{name}={} is relative; give an absolute path", p.display()));
    }
    p.canonicalize().map_err(|e| format!("{name}={}: {e}", p.display()))
}

/// `node` and `tsserver.js` to run for `root`, or why there are none.
pub fn locate(root: &Path) -> Result<(PathBuf, PathBuf), String> {
    locate_with(root, |k| std::env::var_os(k))
}

/// `locate`, reading the environment through `env`. Nothing inside the repo runs unless the user trusts
/// it (AGENT_GRAFT_ENABLE_CHECK=1), and `node` never comes from the repo.
fn locate_with(root: &Path, env: impl Fn(&str) -> Option<OsString>) -> Result<(PathBuf, PathBuf), String> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let trusted = env("AGENT_GRAFT_ENABLE_CHECK").is_some_and(|v| v == "1");
    let node = match env("AGENT_GRAFT_NODE") {
        Some(n) => env_path("AGENT_GRAFT_NODE", n)?,
        None => on_path("node", env("PATH"), &root)?,
    };
    if node.starts_with(&root) {
        return Err(format!("node at {} is inside the repo; refused", node.display()));
    }
    if let Some(p) = env("AGENT_GRAFT_TSSERVER") {
        let p = env_path("AGENT_GRAFT_TSSERVER", p)?;
        let p = if p.is_dir() { p.join("lib/tsserver.js") } else { p };
        if !p.is_file() {
            return Err(format!("AGENT_GRAFT_TSSERVER={} is not a tsserver.js", p.display()));
        }
        if p.starts_with(&root) && !trusted {
            return Err(format!("tsserver at {} is inside the repo; set AGENT_GRAFT_ENABLE_CHECK=1 to trust it", p.display()));
        }
        return Ok((node, p));
    }
    /* the repo's own TypeScript runs repo code: only for repos the user trusts */
    if trusted {
        let local = root.join("node_modules/typescript/lib/tsserver.js");
        if local.is_file() {
            return Ok((node, local));
        }
    }
    let pinned = pinned_dir().map(|d| d.join("node_modules/typescript/lib/tsserver.js"));
    match pinned {
        Some(p) if p.is_file() => Ok((node, p)),
        _ => Err("no tsserver found".into()),
    }
}

pub(crate) struct Server {
    pub(crate) child: Child,
    pub(crate) stdin: ChildStdin,
    pub(crate) rx: Receiver<Value>,
    pub(crate) seq: u64,
    /// files sent as open, with the hash of the text sent
    pub(crate) open: HashMap<PathBuf, u64>,
    pub(crate) started: Instant,
}

impl Server {
    pub(crate) fn start(root: &Path) -> Result<Server, String> {
        let (node, tsserver) = locate(root)?;
        let mut child = Command::new(&node)
            .arg(&tsserver)
            .args(["--disableAutomaticTypingAcquisition", "--suppressDiagnosticEvents", "--noGetErrOnBackgroundUpdate", "--locale", "en"])
            .args(["--useInferredProjectPerProjectRoot"])
            .current_dir(root)
            .env_remove("TSS_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", node.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                /* `Content-Length: N`, a blank line, then N bytes of JSON */
                let mut len = None;
                loop {
                    let mut line = String::new();
                    match r.read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let l = line.trim();
                    if l.is_empty() {
                        if len.is_some() {
                            break;
                        }
                        continue;
                    }
                    if let Some(n) = l.strip_prefix("Content-Length:") {
                        len = n.trim().parse::<usize>().ok();
                    }
                }
                let mut buf = vec![0; len.unwrap_or(0)];
                if r.read_exact(&mut buf).is_err() {
                    return;
                }
                let Ok(v) = serde_json::from_slice::<Value>(&buf) else { continue };
                if v["type"] == "response" && tx.send(v).is_err() {
                    return;
                }
            }
        });
        let mut s = Server { child, stdin, rx, seq: 0, open: HashMap::new(), started: Instant::now() };
        s.request(
            "configure",
            json!({"hostInfo": "agent-graft-ast", "preferences": {"providePrefixAndSuffixTextForRename": true, "allowRenameOfImportPath": false}}),
        )?;
        Ok(s)
    }

    pub(crate) fn send(&mut self, command: &str, arguments: Value) -> Result<u64, String> {
        self.seq += 1;
        let mut line = serde_json::to_string(&json!({"seq": self.seq, "type": "request", "command": command, "arguments": arguments})).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).and_then(|_| self.stdin.flush()).map_err(|e| format!("tsserver stopped: {e}"))?;
        Ok(self.seq)
    }

    /// Send a request and wait for its response body. Err(text) on failure; "tsserver stopped" when it died.
    pub(crate) fn request(&mut self, command: &str, arguments: Value) -> Result<Value, String> {
        let seq = self.send(command, arguments)?;
        let deadline = Instant::now() + timeout();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(v) if v["request_seq"].as_u64() == Some(seq) => {
                    return if v["success"].as_bool() == Some(true) {
                        Ok(v.get("body").cloned().unwrap_or(Value::Null))
                    } else {
                        Err(v["message"].as_str().unwrap_or("request failed").to_string())
                    };
                }
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    /* a hung server is useless: kill it, the next request starts a fresh one */
                    let _ = self.child.kill();
                    return Err(format!("tsserver stopped: no answer to {command} in {}s", timeout().as_secs()));
                }
                Err(RecvTimeoutError::Disconnected) => return Err("tsserver stopped".into()),
            }
        }
    }

    /// Send these files' current disk text as open files (one request), skipping ones tsserver already has.
    pub(crate) fn open_files(&mut self, root: &Path, files: &[PathBuf]) -> Result<(), String> {
        let mut open = Vec::new();
        let mut closed = Vec::new();
        for f in files {
            match std::fs::read_to_string(f) {
                Ok(text) => {
                    let h = xxhash_rust::xxh3::xxh3_64(text.as_bytes());
                    if self.open.get(f) != Some(&h) {
                        self.open.insert(f.clone(), h);
                        open.push(json!({"file": f, "fileContent": text, "projectRootPath": root}));
                    }
                }
                /* deleted: let tsserver read the disk again */
                Err(_) => {
                    if self.open.remove(f).is_some() {
                        closed.push(json!(f));
                    }
                }
            }
        }
        if open.is_empty() && closed.is_empty() {
            return Ok(());
        }
        self.request("updateOpen", json!({"openFiles": open, "closedFiles": closed})).map(|_| ())
    }

    pub(crate) fn rss_kb(&self) -> Option<u64> {
        let out = Command::new("ps").args(["-o", "rss=", "-p", &self.child.id().to_string()]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.send("exit", json!({}));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
#[path = "ts_server_tests.rs"]
mod tests;
