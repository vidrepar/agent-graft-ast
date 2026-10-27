use super::*;
use std::fs;

/// A small repo with an index at the default location.
fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    fs::create_dir_all(r.join("src")).unwrap();
    fs::write(r.join("src/repo.rs"), "pub struct UserRepo;\nimpl UserRepo {\n    pub fn get_by_id(&self, id: u64) -> u64 { id }\n}\npub fn tokens() {}\n").unwrap();
    fs::write(r.join("src/user.rs"), "use crate::repo::UserRepo;\npub fn get_profile(r: &UserRepo) -> u64 { r.get_by_id(1) }\n").unwrap();
    fs::write(r.join("README.md"), "UserRepo docs\n").unwrap();
    Engine::open(r, None).unwrap();
    d
}

fn call(tool: &str, input: Value, cwd: &Path) -> Option<String> {
    pre_tool_hook(&json!({"hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": input, "cwd": cwd}).to_string(), None)
}

fn reason(out: &str) -> String {
    let v: Value = serde_json::from_str(out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().to_string()
}

fn t(name: &str, definition: bool) -> Option<Target> {
    Some(Target { name: name.into(), definition, file_search: false })
}

#[test]
fn regex_patterns_that_name_one_identifier() {
    assert_eq!(regex_target("get_by_id"), t("get_by_id", false));
    assert_eq!(regex_target("\\bget_by_id\\b"), t("get_by_id", false));
    assert_eq!(regex_target("get_by_id\\("), t("get_by_id", false));
    assert_eq!(regex_target("fn get_by_id"), t("get_by_id", true));
    assert_eq!(regex_target("pub fn\\s+get_by_id"), t("get_by_id", true));
    assert_eq!(regex_target("^\\s*class UserRepo"), t("UserRepo", true));
    for text in ["get_by_id|get_profile", "get.*id", "TODO: fix", "fn [a-z]+", "foo bar", "\"quoted\"", "id"] {
        assert_eq!(regex_target(text), None, "{text}");
    }
}

#[test]
fn bash_searches_are_parsed_conservatively() {
    assert_eq!(bash_target("rg -l get_by_id src/"), t("get_by_id", false));
    /* a bare name without -l asks for the matching lines, which the context does not print */
    assert_eq!(bash_target("rg -n get_by_id src/"), None);
    assert_eq!(bash_target("grep -rn -A 3 'fn get_by_id' ."), t("get_by_id", true));
    assert_eq!(bash_target("git grep -c -e UserRepo"), t("UserRepo", false));
    assert_eq!(bash_target("find . -name 'UserRepo*.rs'"), Some(Target { name: "UserRepo".into(), definition: false, file_search: true }));
    for cmd in ["rg get_by_id | head", "grep get_by_id src > out", "rg $(cat x)", "cat src/repo.rs", "rg -e a -e get_by_id", "rg -f pats.txt"] {
        assert_eq!(bash_target(cmd), None, "{cmd}");
    }
}

#[test]
fn symbol_search_is_denied_with_context() {
    let d = repo();
    let out = call("Grep", json!({"pattern": "fn get_by_id", "path": "src"}), d.path()).expect("a symbol search is answered");
    let text = reason(&out);
    assert!(text.starts_with("Not an error: agent-graft-ast answered this search from its index"), "{text}");
    assert!(text.contains("path=\"src/repo.rs\"") && text.contains("src/user.rs [Read"), "{text}");
    assert!(!text.contains("budget_receipt"), "{text}");
    /* a type name, by Grep, Bash and a file Glob */
    assert!(call("Grep", json!({"pattern": "UserRepo"}), d.path()).is_some());
    assert!(call("Bash", json!({"command": "rg -l get_profile"}), d.path()).is_some());
    assert!(call("Grep", json!({"pattern": "fn get_profile", "output_mode": "content"}), d.path()).is_some());
    assert!(call("Glob", json!({"pattern": "**/UserRepo*"}), d.path()).is_some());
}

#[test]
fn other_searches_pass_through() {
    let d = repo();
    let r = d.path();
    for (tool, input) in [
        ("Grep", json!({"pattern": "get_by_id|get_profile"})),
        ("Grep", json!({"pattern": "no_such_symbol"})),
        ("Grep", json!({"pattern": "tokens"})),
        ("Grep", json!({"pattern": "UserRepo", "glob": "*.md"})),
        ("Grep", json!({"pattern": "UserRepo", "type": "md"})),
        ("Grep", json!({"pattern": "get_by_id", "multiline": true})),
        ("Grep", json!({"pattern": "get_by_id\\(", "output_mode": "content"})),
        ("Glob", json!({"pattern": "**/get_profile*"})),
        ("Glob", json!({"pattern": "src/**/*.rs"})),
        ("Bash", json!({"command": "cargo test get_by_id"})),
        ("Read", json!({"file_path": r.join("src/repo.rs")})),
        ("Edit", json!({"file_path": r.join("src/repo.rs"), "old_string": "get_by_id", "new_string": "x"})),
    ] {
        assert!(call(tool, input.clone(), r).is_none(), "{tool} {input}");
    }
    /* `fn tokens` names a definition, so a plain word is answered there */
    assert!(call("Grep", json!({"pattern": "fn tokens"}), r).is_some());
}

#[test]
fn malformed_input_or_no_index_passes_through() {
    assert!(pre_tool_hook("not json", None).is_none());
    assert!(pre_tool_hook("{}", None).is_none());
    assert!(pre_tool_hook(&json!({"tool_name": "Grep", "tool_input": {"pattern": 7}}).to_string(), None).is_none());
    let empty = tempfile::tempdir().unwrap();
    assert!(call("Grep", json!({"pattern": "get_by_id"}), empty.path()).is_none());
}
