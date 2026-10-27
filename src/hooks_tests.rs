use super::*;
use std::fs;

/// A small repo with an index at the default location.
fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    fs::create_dir_all(r.join("src/db")).unwrap();
    fs::create_dir_all(r.join("src/services")).unwrap();
    fs::create_dir_all(r.join("src/controllers")).unwrap();
    fs::write(r.join("src/db/repo.rs"), "pub struct Repo;\nimpl Repo {\n    pub fn get_by_id(&self, id: u64) -> u64 { id }\n}\n").unwrap();
    fs::write(r.join("src/services/user.rs"), "use crate::db::repo::Repo;\npub fn get_profile(r: &Repo) -> u64 { r.get_by_id(1) }\n").unwrap();
    fs::write(
        r.join("src/controllers/auth.rs"),
        "use crate::services::user::get_profile;\nuse crate::db::repo::Repo;\npub fn login_handler() { get_profile(&Repo); }\n",
    )
    .unwrap();
    Engine::open(r, None).unwrap();
    d
}

fn hook_json(prompt: &str, cwd: &Path) -> String {
    json!({"hook_event_name": "UserPromptSubmit", "prompt": prompt, "cwd": cwd}).to_string()
}

fn context_of(out: &str) -> String {
    let v: Value = serde_json::from_str(out).unwrap();
    v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_string()
}

#[test]
fn tokens_keep_identifiers_in_order() {
    assert_eq!(tokens("fix getProfile, then get_by_id; 2x Repo::new"), vec!["fix", "getProfile", "then", "get_by_id", "Repo", "new"]);
}

#[test]
fn prompt_hook_points_at_named_symbols() {
    let d = repo();
    let cwd = d.path().join("src");
    let out = prompt_hook(&hook_json("The login_handler breaks when get_by_id returns 0; also look at Repo and the profile", &cwd), None).unwrap();
    let text = context_of(&out);
    let lines: Vec<&str> = text.lines().collect();
    /* code-shaped names first, in prompt order; then the capitalised word */
    assert!(lines[1].starts_with("login_handler function src/controllers/auth.rs:3 (0 callers)"), "{text}");
    assert!(lines[2].starts_with("get_by_id method src/db/repo.rs:3 (1 callers)"), "{text}");
    assert!(lines[3].starts_with("Repo struct src/db/repo.rs:1"), "{text}");
    /* "profile" is a plain word with no symbol of that exact name */
    assert_eq!(lines.len(), 4, "{text}");
    assert!(!text.contains("Renaming"));
}

#[test]
fn prompt_hook_adds_tool_tip_and_case_insensitive_match() {
    let d = repo();
    let out = prompt_hook(&hook_json("rename getProfile to fetchProfile", d.path()), None).unwrap();
    let text = context_of(&out);
    assert!(text.starts_with("agent-graft-ast: symbols named in this prompt"), "{text}");
    assert!(text.contains("get_profile function src/services/user.rs:2"), "{text}");
    assert!(text.ends_with("Renaming? Use the rename tool: it edits every reference in one call."), "{text}");
}

#[test]
fn prompt_hook_silent_without_match_or_index() {
    let d = repo();
    assert!(prompt_hook(&hook_json("make the README friendlier please", d.path()), None).is_none());
    assert!(prompt_hook("not json", None).is_none());
    let empty = tempfile::tempdir().unwrap();
    assert!(prompt_hook(&hook_json("fix get_by_id", empty.path()), None).is_none());
}

#[test]
fn post_edit_lists_dependents_after_refresh() {
    let d = repo();
    let r = d.path();
    let edited = r.join("src/db/repo.rs");
    /* the edit adds a symbol the index has not seen yet; the hook refreshes the file first */
    fs::write(&edited, "pub struct Repo;\nimpl Repo {\n    pub fn get_by_id(&self, id: u64) -> u64 { id }\n    pub fn fresh(&self) {}\n}\n").unwrap();
    fs::write(r.join("src/services/user.rs"), "use crate::db::repo::Repo;\npub fn get_profile(r: &Repo) -> u64 { r.get_by_id(1) }\n").unwrap();
    let input = json!({"hook_event_name": "PostToolUse", "tool_name": "Edit", "tool_input": {"file_path": edited}, "cwd": r}).to_string();
    let text = context_of(&post_edit_hook(&input, None).unwrap());
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Files that depend on src/db/repo.rs; check them if you changed a signature or behaviour:");
    assert!(lines.contains(&"src/services/user.rs (uses get_by_id)"), "{text}");
    assert!(lines.contains(&"src/controllers/auth.rs (imports it)"), "{text}");
}

#[test]
fn post_edit_silent_without_dependents_or_index() {
    let d = repo();
    let leaf = d.path().join("src/controllers/auth.rs");
    let input = json!({"tool_name": "Write", "tool_input": {"file_path": leaf}}).to_string();
    assert!(post_edit_hook(&input, None).is_none());
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("a.rs"), "fn a() {}").unwrap();
    let input = json!({"tool_name": "Write", "tool_input": {"file_path": outside.path().join("a.rs")}}).to_string();
    assert!(post_edit_hook(&input, None).is_none());
}

#[test]
fn hooks_work_while_a_server_holds_the_index() {
    let d = repo();
    /* the MCP server keeps the redb file open (and locked) for its whole life */
    let (_server, _, _) = Engine::open(d.path(), None).unwrap();
    let out = prompt_hook(&hook_json("why is get_by_id slow", d.path()), None).unwrap();
    assert!(context_of(&out).contains("get_by_id method src/db/repo.rs:3"));
    /* the repaired copy is kept for the next run and reused while the index file is unchanged */
    let dir = d.path().join(".agent-graft");
    assert!(dir.join("hook-snapshot.redb").exists() && dir.join("hook-snapshot.stamp").exists());
    let out = prompt_hook(&hook_json("why is get_by_id slow", d.path()), None).unwrap();
    assert!(context_of(&out).contains("get_by_id method src/db/repo.rs:3"));
    let leftovers = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).count();
    assert_eq!(leftovers, 0);
}
