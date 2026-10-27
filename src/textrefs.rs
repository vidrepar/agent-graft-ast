//! Leftover mentions of a renamed symbol in non-code text (docs, config): the code-only rename leaves
//! these alone, so the agent would otherwise Grep and Edit them by hand.
//!
//! Same visibility as the indexer: .gitignore honored, hidden and `SKIP_DIRS` directories skipped,
//! and anything the guard refuses (lockfiles, secrets, binaries) is never read or written.

use crate::context::is_ident;
use crate::guard;
use crate::index::{self, SKIP_DIRS};
use std::path::Path;

/// Text files scanned for leftover mentions.
pub const TEXT_EXTS: &[&str] = &["md", "mdx", "txt", "rst", "yaml", "yml", "toml", "json"];

/// Larger text files are skipped: generated data, not docs.
pub const MAX_TEXT_BYTES: u64 = 1_000_000;

/// Whole-word occurrences of a name in one text file.
pub struct Mention {
    pub path: String,
    pub src: String,
    /// byte ranges of each occurrence
    pub spans: Vec<(usize, usize)>,
}

impl Mention {
    pub fn lines(&self) -> Vec<u32> {
        let mut ls: Vec<u32> = self.spans.iter().map(|s| self.src[..s.0].matches('\n').count() as u32 + 1).collect();
        ls.dedup();
        ls
    }

    /// The file's text with every occurrence replaced.
    pub fn replaced(&self, new: &str) -> String {
        let mut s = self.src.clone();
        for &(a, b) in self.spans.iter().rev() {
            s.replace_range(a..b, new);
        }
        s
    }
}

/// Byte ranges where `word` occurs as a whole identifier.
pub fn whole_word_spans(src: &str, word: &str) -> Vec<(usize, usize)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    if word.is_empty() {
        return out;
    }
    let mut i = 0;
    while let Some(off) = memchr::memmem::find(&b[i..], word.as_bytes()) {
        let pos = i + off;
        let end = pos + word.len();
        if (pos == 0 || !is_ident(b[pos - 1])) && (end >= b.len() || !is_ident(b[end])) {
            out.push((pos, end));
        }
        i = end;
    }
    out
}

fn is_text(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).map(|e| TEXT_EXTS.contains(&e.to_ascii_lowercase().as_str())).unwrap_or(false)
}

/// Every non-code text file under `root` that mentions `word` as a whole word, sorted by path.
pub fn scan(root: &Path, word: &str) -> Vec<Mention> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .ignore(false)
        .require_git(false)
        .filter_entry(|e| {
            !(e.file_type().map(|t| t.is_dir()).unwrap_or(false) && SKIP_DIRS.contains(&e.file_name().to_str().unwrap_or("")))
        })
        .build();
    let mut out = Vec::new();
    for entry in walker.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) || !is_text(path) {
            continue;
        }
        let rel = index::rel_path(root, path);
        if guard::check_path(&rel).is_some() {
            continue;
        }
        if entry.metadata().map(|m| m.len() > MAX_TEXT_BYTES).unwrap_or(true) {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else { continue };
        if memchr::memmem::find(&bytes, word.as_bytes()).is_none() || guard::check_content(&bytes).is_some() {
            continue;
        }
        let Ok(src) = String::from_utf8(bytes) else { continue };
        let spans = whole_word_spans(&src, word);
        if !spans.is_empty() {
            out.push(Mention { path: rel, src, spans });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scans_text_files_whole_word_and_skips_ignored_and_secrets() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for (p, s) in [
            ("README.md", "Call `fetchUser` here.\nNot fetchUsers or my_fetchUser.\nfetchUser again\n"),
            ("docs/api.yaml", "op: fetchUser\n"),
            ("gen/out.md", "fetchUser\n"),
            (".gitignore", "gen/\n"),
            (".env", "FN=fetchUser\n"),
            ("config/secrets.json", "{\"fetchUser\": 1}\n"),
            ("pnpm-lock.yaml", "fetchUser: 1\n"),
            ("src/a.ts", "fetchUser();\n"),
            ("node_modules/x/r.md", "fetchUser\n"),
        ] {
            fs::create_dir_all(r.join(p).parent().unwrap()).unwrap();
            fs::write(r.join(p), s).unwrap();
        }
        let found = scan(r, "fetchUser");
        let paths: Vec<&str> = found.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, vec!["README.md", "docs/api.yaml"]);
        assert_eq!(found[0].lines(), vec![1, 3]);
        assert_eq!(found[0].replaced("loadUser"), "Call `loadUser` here.\nNot fetchUsers or my_fetchUser.\nloadUser again\n");
    }
}
