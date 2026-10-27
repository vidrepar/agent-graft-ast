//! Temporal awareness: detect recent function signature changes from git history (libgit2, no shell).

use git2::{DiffFormat, DiffOptions, Repository, Sort};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct SigChange {
    pub commit: String,
    pub path: String,
    pub summary: String,
    /// symbol names involved (old and new for renames)
    pub names: Vec<String>,
    pub time: i64,
}

static SIG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:export\s+)?(?:default\s+)?(?:async\s+)?(?:unsafe\s+)?(?:fn|function|def)\s+([A-Za-z_$][\w$]*)",
        r"|^\s*(?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:function\b|\([^()]*\)\s*(?::[^=]+)?=>|[A-Za-z_$][\w$]*\s*=>)",
    ))
    .unwrap()
});

/// Only changes from the last 30 days count as "recent".
const RECENT_SECS: i64 = 30 * 24 * 3600;

const SOURCE_EXTS: &[&str] = &[".rs", ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".py", ".pyi"];

fn sig_name(line: &str) -> Option<String> {
    let c = SIG_RE.captures(line)?;
    c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_string())
}

fn norm(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches('{').trim_end_matches(':').trim().to_string()
}

#[derive(Default)]
struct FileDiff {
    removed: BTreeMap<String, String>,
    added: BTreeMap<String, String>,
}

/// Scan the last `max_commits` first-parent commits; returns root-relative path -> changes (newest first).
pub fn scan(root: &Path, max_commits: usize) -> HashMap<String, Vec<SigChange>> {
    let mut out: HashMap<String, Vec<SigChange>> = HashMap::new();
    let Ok(repo) = Repository::discover(root) else { return out };
    let Some(workdir) = repo.workdir().map(|w| w.to_path_buf()) else { return out };
    let root_abs = root.canonicalize().unwrap_or(root.to_path_buf());
    let workdir = workdir.canonicalize().unwrap_or(workdir);
    let prefix = root_abs.strip_prefix(&workdir).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
    let prefix = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };

    let Ok(mut walk) = repo.revwalk() else { return out };
    if walk.push_head().is_err() {
        return out;
    }
    let _ = walk.set_sorting(Sort::TIME);
    walk.simplify_first_parent().ok();

    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 - RECENT_SECS)
        .unwrap_or(0);
    for oid in walk.take(max_commits).flatten() {
        let Ok(commit) = repo.find_commit(oid) else { continue };
        if commit.time().seconds() < cutoff {
            break;
        }
        /* shallow clones: a missing parent would turn the diff into "everything added" */
        let Ok(parent) = commit.parent(0) else { continue };
        let (Ok(new_tree), Ok(old_tree)) = (commit.tree(), parent.tree()) else { continue };
        let mut opts = DiffOptions::new();
        opts.context_lines(0);
        if !prefix.is_empty() {
            opts.pathspec(&prefix);
        }
        let Ok(diff) = repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), Some(&mut opts)) else { continue };
        let mut per_file: BTreeMap<String, FileDiff> = BTreeMap::new();
        let _ = diff.print(DiffFormat::Patch, |delta, _hunk, line| {
            let Some(p) = delta.new_file().path().or(delta.old_file().path()) else { return true };
            let p = p.to_string_lossy();
            if !SOURCE_EXTS.iter().any(|e| p.ends_with(e)) || !p.starts_with(prefix.as_str()) {
                return true;
            }
            let origin = line.origin();
            if origin != '+' && origin != '-' {
                return true;
            }
            let text = String::from_utf8_lossy(line.content());
            if let Some(name) = sig_name(&text) {
                let fd = per_file.entry(p[prefix.len()..].to_string()).or_default();
                let slot = if origin == '-' { &mut fd.removed } else { &mut fd.added };
                slot.insert(name, norm(&text));
            }
            true
        });
        let short = oid.to_string()[..7].to_string();
        let time = commit.time().seconds();
        for (path, fd) in per_file {
            let mut changes = Vec::new();
            for (name, old) in &fd.removed {
                if let Some(new) = fd.added.get(name) {
                    if new != old {
                        changes.push((format!("`{name}` signature changed"), vec![name.clone()]));
                    }
                }
            }
            let gone: Vec<&String> = fd.removed.keys().filter(|k| !fd.added.contains_key(*k)).collect();
            let new: Vec<&String> = fd.added.keys().filter(|k| !fd.removed.contains_key(*k)).collect();
            if gone.len() == 1 && new.len() == 1 {
                changes.push((format!("`{}` renamed to `{}`", gone[0], new[0]), vec![gone[0].clone(), new[0].clone()]));
            } else {
                for g in gone {
                    changes.push((format!("`{g}` removed"), vec![g.clone()]));
                }
            }
            let entry = out.entry(path.clone()).or_default();
            for (summary, names) in changes {
                if entry.len() < 5 {
                    entry.push(SigChange { commit: short.clone(), path: path.clone(), summary, names, time });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn detects_rename_and_signature_change() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        git(r, &["init", "-q"]);
        fs::create_dir_all(r.join("src/db")).unwrap();
        fs::write(r.join("src/db/repo.rs"), "pub fn find_by_id(id: u32) -> u32 { id }\npub fn save(x: u32) {}\n").unwrap();
        git(r, &["add", "."]);
        git(r, &["commit", "-qm", "init"]);
        fs::write(r.join("src/db/repo.rs"), "pub fn get_by_id(id: u32) -> u32 { id }\npub fn save(x: u64) {}\n").unwrap();
        git(r, &["commit", "-qam", "rename"]);
        let res = scan(r, 10);
        let ch = &res["src/db/repo.rs"];
        let s: Vec<&str> = ch.iter().map(|c| c.summary.as_str()).collect();
        assert!(s.contains(&"`find_by_id` renamed to `get_by_id`"), "{s:?}");
        assert!(s.contains(&"`save` signature changed"), "{s:?}");
        assert_eq!(ch[0].commit.len(), 7);
        assert!(ch.iter().any(|c| c.names == vec!["find_by_id".to_string(), "get_by_id".to_string()]));
    }

    #[test]
    fn non_repo_is_empty() {
        let d = tempfile::tempdir().unwrap();
        assert!(scan(d.path(), 10).is_empty());
    }

    #[test]
    fn recognizes_signatures() {
        assert_eq!(sig_name("    pub(crate) async fn go(x: u8)").as_deref(), Some("go"));
        assert_eq!(sig_name("export const h = async (r) => {").as_deref(), Some("h"));
        assert_eq!(sig_name("def run(self):").as_deref(), Some("run"));
        assert_eq!(sig_name("let x = 1;"), None);
        assert_eq!(sig_name("const id = (await f(x)).slice(0, 32);"), None);
        assert_eq!(sig_name("const f = (a: number): string => a"), Some("f".into()));
    }
}
