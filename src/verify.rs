//! Compile check after an edit: the agent learns whether the code still builds in the same reply.
//!
//! - Each edited file maps to its nearest project inside the repo root: `Cargo.toml` (cargo check),
//!   `tsconfig.json` (the project's own `node_modules/.bin/tsc`), or a `.py` file (py_compile).
//! - Every distinct project runs once, without a shell, under a hard timeout
//!   (`AGENT_GRAFT_CHECK_TIMEOUT_S`, default 120 s); a check that runs over is killed and reported.
//! - Output is reduced to error lines with repo-relative paths, at most `MAX_ERRORS`.

use crate::engine::{esc, Engine};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Error lines kept per reply.
const MAX_ERRORS: usize = 20;
/// Default hard timeout per project, in seconds.
const DEFAULT_TIMEOUT_S: u64 = 120;

/// Which checker a project needs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Cargo,
    Tsc,
    Python,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Cargo => "cargo",
            Kind::Tsc => "tsc",
            Kind::Python => "python",
        }
    }
}

/// What all checks found.
struct Report {
    projects: usize,
    errors: usize,
    lines: Vec<String>,
    skipped: Vec<(String, String)>,
    timed_out: Vec<String>,
    seconds: f64,
}

/// One finished process: exit success, merged stdout+stderr, or None on timeout.
fn run(mut cmd: Command, timeout: Duration) -> Result<Option<(bool, String)>, std::io::Error> {
    /* 1. capture to temp files, not pipes: a chatty compiler can never block on a full pipe */
    let out = tempfile::tempfile()?;
    let err = tempfile::tempfile()?;
    cmd.stdin(Stdio::null()).stdout(out.try_clone()?).stderr(err.try_clone()?);
    let mut child = cmd.spawn()?;
    /* 2. poll until exit or deadline; kill on timeout */
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut text = String::new();
    for mut f in [out, err] {
        use std::io::{Read, Seek};
        f.seek(std::io::SeekFrom::Start(0))?;
        let mut b = Vec::new();
        f.read_to_end(&mut b)?;
        text.push_str(&String::from_utf8_lossy(&b));
    }
    Ok(Some((status.success(), text)))
}

/// Compile checks execute repo content (build scripts, proc macros, repo-local tsc), so the user opts in
/// when starting the server: AGENT_GRAFT_ENABLE_CHECK=1. Tests always run them.
pub(crate) fn check_enabled() -> bool {
    cfg!(test) || std::env::var_os("AGENT_GRAFT_ENABLE_CHECK").is_some_and(|v| v == "1")
}

fn timeout() -> Duration {
    let s = std::env::var("AGENT_GRAFT_CHECK_TIMEOUT_S").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(DEFAULT_TIMEOUT_S);
    Duration::from_secs(s.max(1))
}

/// Nearest directory at or above `start` (and inside `root`) holding `marker`.
fn nearest(root: &Path, start: &Path, marker: &str) -> Option<PathBuf> {
    let mut d = start;
    loop {
        if !d.starts_with(root) {
            return None;
        }
        if d.join(marker).is_file() {
            return Some(d.to_path_buf());
        }
        if d == root {
            return None;
        }
        d = d.parent()?;
    }
}

/// `dir` relative to `root`, "." for the root itself.
fn rel_dir(root: &Path, dir: &Path) -> String {
    let r = dir.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    if r.is_empty() {
        ".".into()
    } else {
        r
    }
}

/// Error lines from cargo (`--message-format short`) or tsc, made repo-relative.
fn compiler_errors(text: &str, root: &Path, dir: &str) -> Vec<String> {
    let root_s = format!("{}/", root.display());
    let prefix = if dir == "." { String::new() } else { format!("{dir}/") };
    text.lines()
        .map(|l| l.trim_end().replace(&root_s, ""))
        .filter(|l| {
            let t = l.trim_start();
            let summary = t.starts_with("error: could not compile") || t.starts_with("error: aborting");
            !summary && (t.starts_with("error") || t.contains(": error") || t.contains("): error"))
        })
        .map(|l| if l.starts_with("error") || l.starts_with(&prefix) || prefix.is_empty() { l } else { format!("{prefix}{l}") })
        .collect()
}

/// `path:line: XError: message` from py_compile tracebacks.
fn python_errors(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at: Option<String> = None;
    for l in text.lines() {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("File \"") {
            if let Some((file, tail)) = rest.split_once('"') {
                let line = tail.trim_start_matches(", line ").split(|c: char| !c.is_ascii_digit()).next().unwrap_or("");
                at = Some(format!("{file}:{line}"));
            }
        } else if let Some((kind, _)) = t.split_once(':') {
            let is_err = (kind.ends_with("Error") || kind.ends_with("Exception")) && !kind.contains(' ') && !kind.is_empty();
            if is_err {
                match at.take() {
                    Some(a) => out.push(format!("{a}: {t}")),
                    None => out.push(t.to_string()),
                }
            }
        }
    }
    out
}

impl Engine {
    /// Group repo-relative paths by project and run each project's checker once.
    fn run_checks(&self, rels: &[String]) -> Report {
        let t0 = Instant::now();
        let root = self.root.clone();
        let limit = timeout();
        let mut rep = Report { projects: 0, errors: 0, lines: Vec::new(), skipped: Vec::new(), timed_out: Vec::new(), seconds: 0.0 };

        /* 1. map every file to its project; python files batch per repo */
        let mut projects: BTreeMap<(Kind, PathBuf), Vec<String>> = BTreeMap::new();
        for rel in rels {
            let abs = root.join(rel);
            let start = if abs.is_dir() { abs.clone() } else { abs.parent().map(Path::to_path_buf).unwrap_or_else(|| root.clone()) };
            let ext = abs.extension().and_then(|e| e.to_str()).unwrap_or("");
            let found = match ext {
                "rs" => nearest(&root, &start, "Cargo.toml").map(|d| (Kind::Cargo, d)),
                "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => nearest(&root, &start, "tsconfig.json").map(|d| (Kind::Tsc, d)),
                "py" => Some((Kind::Python, root.clone())),
                _ if abs.is_dir() => nearest(&root, &start, "Cargo.toml")
                    .map(|d| (Kind::Cargo, d))
                    .or_else(|| nearest(&root, &start, "tsconfig.json").map(|d| (Kind::Tsc, d))),
                _ => None,
            };
            match found {
                Some(k) => projects.entry(k).or_default().push(rel.clone()),
                None => rep.skipped.push((rel.clone(), "no Cargo.toml, tsconfig.json or .py checker for this file".into())),
            }
        }

        /* 2. one process per project, never through a shell */
        for ((kind, dir), files) in projects {
            let rd = rel_dir(&root, &dir);
            let mut cmd = match kind {
                Kind::Cargo => {
                    let mut c = Command::new("cargo");
                    c.args(["check", "--message-format", "short", "--quiet"]);
                    c
                }
                Kind::Tsc => {
                    let Some(bin) = nearest(&root, &dir, "node_modules/.bin/tsc").map(|d| d.join("node_modules/.bin/tsc")) else {
                        rep.skipped.push((rd, "tsc not installed".into()));
                        continue;
                    };
                    let mut c = Command::new(bin);
                    c.args(["--noEmit", "--pretty", "false", "-p"]).arg(&dir);
                    c
                }
                Kind::Python => {
                    let mut c = Command::new("python3");
                    c.args(["-I", "-m", "py_compile"]).args(&files);
                    c
                }
            };
            cmd.current_dir(&dir);
            rep.projects += 1;
            let found = match run(cmd, limit) {
                Err(e) => {
                    rep.projects -= 1;
                    rep.skipped.push((rd, format!("{} could not start: {e}", kind.name())));
                    continue;
                }
                Ok(None) => {
                    rep.timed_out.push(format!("{} {rd}", kind.name()));
                    continue;
                }
                Ok(Some((ok, text))) => {
                    let mut errs = if kind == Kind::Python { python_errors(&text) } else { compiler_errors(&text, &root, &rd) };
                    /* a failed run with nothing parseable still counts: show its first line */
                    if !ok && errs.is_empty() {
                        let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed with no output");
                        errs.push(format!("{} {rd}: {first}", kind.name()));
                    }
                    errs
                }
            };
            rep.errors += found.len();
            rep.lines.extend(found);
        }
        rep.lines.truncate(MAX_ERRORS);
        rep.seconds = t0.elapsed().as_secs_f64();
        rep
    }

    /// Render a report as `<tag projects errors seconds ok>` with error lines, skips and a `<next>` line.
    fn check_element(&self, rels: &[String], tag: &str) -> String {
        if !check_enabled() {
            return format!(
                "<{tag} ok=\"disabled\">\nCompile checks run the repo's build scripts and toolchain, so they are off unless the user starts the server with AGENT_GRAFT_ENABLE_CHECK=1.\n</{tag}>\n"
            );
        }
        let r = self.run_checks(rels);
        let ok = if !r.timed_out.is_empty() || r.errors > 0 {
            "false"
        } else if r.projects == 0 {
            "unknown"
        } else {
            "true"
        };
        let mut s = format!("<{tag} projects=\"{}\" errors=\"{}\" seconds=\"{:.1}\" ok=\"{ok}\">\n", r.projects, r.errors, r.seconds);
        for l in &r.lines {
            s.push_str(&esc(l));
            s.push('\n');
        }
        if r.errors > r.lines.len() {
            s.push_str(&format!("({} more errors not shown)\n", r.errors - r.lines.len()));
        }
        for p in &r.timed_out {
            s.push_str(&format!("<timeout project=\"{}\" after_s=\"{}\"/>\n", esc(p), timeout().as_secs()));
        }
        for (p, why) in &r.skipped {
            s.push_str(&format!("<skipped path=\"{}\" reason=\"{}\"/>\n", esc(p), esc(why)));
        }
        s.push_str(match ok {
            "true" => "<next>Compiles: no errors in the checked projects.</next>\n",
            "false" if r.errors > 0 => "<next>Fix these errors (Read the listed lines), then call `check` again.</next>\n",
            "false" => "<next>The check timed out: raise AGENT_GRAFT_CHECK_TIMEOUT_S or run the compiler yourself.</next>\n",
            _ => "<next>No checker ran for these files; verify by hand.</next>\n",
        });
        s.push_str(&format!("</{tag}>\n"));
        s
    }

    /// The compile result for files an edit tool just wrote, to append to its reply.
    pub(crate) fn check_written(&self, rels: &[String]) -> String {
        self.check_element(rels, "compile")
    }

    /// `check`: compile the projects that own `paths` and return their errors.
    pub fn check(&mut self, paths: &[String]) -> String {
        let budget = 4000;
        /* 1. only paths inside the repo, under the same visibility rules as reads */
        let mut rels = Vec::new();
        let mut refused = String::new();
        for p in paths {
            match self.resolve_rel(p) {
                Ok(r) => rels.push(r),
                Err(r) => refused.push_str(&format!("<refused path=\"{}\" code=\"{}\"/>\n", esc(p), r.code())),
            }
        }
        /* 2. run and split the element into the reply's open tag and body */
        let el = self.check_element(&rels, "check");
        let (open, rest) = el.split_once('\n').unwrap_or((&el, ""));
        let body = format!("{refused}{}", rest.strip_suffix("</check>\n").unwrap_or(rest));
        let open = open.to_string();
        self.finish(&open, "</check>", &body, budget, "")
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::Engine;
    use std::fs;

    fn engine(files: &[(&str, &str)]) -> (tempfile::TempDir, Engine) {
        let d = tempfile::tempdir().unwrap();
        for (p, s) in files {
            let path = d.path().join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, s).unwrap();
        }
        let (e, _, _) = Engine::open(d.path(), Some(&d.path().join("i.redb"))).unwrap();
        (d, e)
    }

    const MANIFEST: &str = "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";

    #[test]
    fn cargo_type_error_then_fixed() {
        let (d, mut e) = engine(&[("Cargo.toml", MANIFEST), ("src/lib.rs", "pub fn f() -> u32 {\n    \"no\"\n}\n")]);
        let out = e.check(&["src/lib.rs".into()]);
        assert!(out.contains("projects=\"1\"") && out.contains("ok=\"false\"") && !out.contains("errors=\"0\""), "{out}");
        assert!(out.contains("src/lib.rs:2:"), "{out}");
        fs::write(d.path().join("src/lib.rs"), "pub fn f() -> u32 {\n    1\n}\n").unwrap();
        let out = e.check(&["src/lib.rs".into()]);
        assert!(out.contains("errors=\"0\"") && out.contains("ok=\"true\""), "{out}");
    }

    #[test]
    fn python_syntax_error_is_reported() {
        let (_d, mut e) = engine(&[("pkg/m.py", "def f(:\n    pass\n"), ("ok.py", "x = 1\n")]);
        let out = e.check(&["pkg/m.py".into(), "ok.py".into()]);
        assert!(out.contains("projects=\"1\"") && out.contains("errors=\"1\"") && out.contains("ok=\"false\""), "{out}");
        assert!(out.contains("pkg/m.py:1: SyntaxError"), "{out}");
    }

    #[test]
    fn tsc_absent_is_skipped_with_reason() {
        let (_d, mut e) = engine(&[("web/tsconfig.json", "{}\n"), ("web/a.ts", "export const a: number = 1;\n")]);
        let out = e.check(&["web/a.ts".into()]);
        assert!(out.contains("projects=\"0\"") && out.contains("ok=\"unknown\""), "{out}");
        assert!(out.contains("<skipped path=\"web\" reason=\"tsc not installed\"/>"), "{out}");
    }

    #[test]
    fn paths_outside_the_root_are_refused() {
        let (_d, mut e) = engine(&[("ok.py", "x = 1\n")]);
        let out = e.check(&["/etc/hosts".into()]);
        assert!(out.contains("<refused path=\"/etc/hosts\"") && out.contains("projects=\"0\""), "{out}");
    }

    #[test]
    fn rename_with_check_appends_the_compile_result() {
        let (_d, mut e) = engine(&[("Cargo.toml", MANIFEST), ("src/lib.rs", "pub fn old() -> u32 {\n    1\n}\npub fn g() -> u32 {\n    old()\n}\n")]);
        let out = e.rename_and_check("old", "new_one", false, false, true);
        assert!(out.contains("applied=\"true\"") && out.contains("<compile projects=\"1\" errors=\"0\""), "{out}");
    }
}
