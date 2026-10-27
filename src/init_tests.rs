use super::*;
use std::fs;

fn exe() -> PathBuf {
    PathBuf::from("/opt/bin/agent-graft-ast")
}

const ON: Options = Options { redirect: true, allow_edits: false };
const OFF: Options = Options { redirect: false, allow_edits: false };

fn json_at(p: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
}

fn read_rules() -> Value {
    json!(["mcp__agent-graft-ast__find_symbol", "mcp__agent-graft-ast__context", "mcp__agent-graft-ast__read_file"])
}

#[test]
fn fresh_repo_gets_all_five_files() {
    let d = tempfile::tempdir().unwrap();
    let changes = plan(d.path(), &exe(), ON).unwrap();
    assert_eq!(changes.len(), 5);
    apply(&changes).unwrap();
    let mcp: Value = serde_json::from_str(&fs::read_to_string(d.path().join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(mcp["mcpServers"][SERVER_NAME]["command"], "/opt/bin/agent-graft-ast");
    assert_eq!(mcp["mcpServers"][SERVER_NAME]["args"][2], "serve");
    let s: Value = serde_json::from_str(&fs::read_to_string(d.path().join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!(s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"], "/opt/bin/agent-graft-ast hook prompt");
    assert_eq!(s["hooks"]["PostToolUse"][0]["matcher"], "Edit|Write|MultiEdit");
    assert_eq!(s["hooks"]["PostToolUse"][0]["hooks"][0]["command"], "/opt/bin/agent-graft-ast hook post-edit");
    assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "Grep|Glob|Bash");
    assert_eq!(s["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "/opt/bin/agent-graft-ast hook pre-tool");
    /* the shared file grants nothing; read-only tools are allowed per user */
    assert!(s.get("permissions").is_none(), "{s}");
    assert_eq!(json_at(&d.path().join(".claude/settings.local.json"))["permissions"]["allow"], read_rules());
    assert_eq!(fs::read_to_string(d.path().join(".gitignore")).unwrap(), ".agent-graft/\n.claude/settings.local.json\n");
    assert_eq!(fs::read_to_string(d.path().join("CLAUDE.md")).unwrap(), CLAUDE_BLOCK);
}

#[test]
fn merges_existing_config_and_is_idempotent() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    fs::create_dir_all(r.join(".claude")).unwrap();
    fs::write(r.join(".mcp.json"), r#"{"mcpServers": {"other": {"command": "x"}}, "extra": 1}"#).unwrap();
    fs::write(
        r.join(".claude/settings.json"),
        r#"{"permissions": {"allow": ["Bash(ls)"]}, "hooks": {"PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "lint.sh"}]}]}}"#,
    )
    .unwrap();
    fs::write(r.join(".gitignore"), "target/").unwrap();
    fs::write(r.join("CLAUDE.md"), "# Rules\n- keep it short").unwrap();
    apply(&plan(r, &exe(), ON).unwrap()).unwrap();

    let mcp: Value = serde_json::from_str(&fs::read_to_string(r.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(mcp["mcpServers"]["other"]["command"], "x");
    assert_eq!(mcp["extra"], 1);
    assert!(mcp["mcpServers"][SERVER_NAME].is_object());
    let s: Value = serde_json::from_str(&fs::read_to_string(r.join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!(s["permissions"]["allow"], json!(["Bash(ls)"]));
    assert_eq!(fs::read_to_string(r.join("CLAUDE.md")).unwrap(), format!("# Rules\n- keep it short\n\n{CLAUDE_BLOCK}"));
    assert_eq!(s["hooks"]["PostToolUse"].as_array().unwrap().len(), 2);
    assert_eq!(s["hooks"]["PostToolUse"][0]["hooks"][0]["command"], "lint.sh");
    assert_eq!(fs::read_to_string(r.join(".gitignore")).unwrap(), "target/\n.agent-graft/\n.claude/settings.local.json\n");

    /* second run: nothing to do; a binary at another path re-points the config at itself instead */
    let snapshot = |p: &str| fs::read_to_string(r.join(p)).unwrap();
    let before = [snapshot(".mcp.json"), snapshot(".claude/settings.json"), snapshot(".claude/settings.local.json"), snapshot(".gitignore"), snapshot("CLAUDE.md")];
    assert!(plan(r, &exe(), ON).unwrap().is_empty());
    assert_eq!(plan(r, Path::new("/elsewhere/agent-graft-ast"), ON).unwrap().len(), 2);
    apply(&plan(r, &exe(), ON).unwrap()).unwrap();
    assert_eq!(before, [snapshot(".mcp.json"), snapshot(".claude/settings.json"), snapshot(".claude/settings.local.json"), snapshot(".gitignore"), snapshot("CLAUDE.md")]);
}

#[test]
fn invalid_json_is_an_error_not_a_clobber() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join(".mcp.json"), "{ not json").unwrap();
    assert!(plan(d.path(), &exe(), ON).is_err());
    assert_eq!(fs::read_to_string(d.path().join(".mcp.json")).unwrap(), "{ not json");
}

#[test]
fn quotes_paths_with_spaces() {
    let (p, e) = hook_commands(Path::new("/Users/a b/agent-graft-ast"));
    assert_eq!(p, "'/Users/a b/agent-graft-ast' hook prompt");
    assert_eq!(e, "'/Users/a b/agent-graft-ast' hook post-edit");
}

#[test]
fn foreign_entries_with_our_name_are_rewritten() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join(".mcp.json"), r#"{"mcpServers":{"agent-graft-ast":{"command":"/tmp/evil","args":[]}}}"#).unwrap();
    std::fs::create_dir_all(d.path().join(".claude")).unwrap();
    std::fs::write(
        d.path().join(".claude/settings.json"),
        r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"/tmp/evil/agent-graft-ast hook prompt"}]}]}}"#,
    )
    .unwrap();
    let exe = std::path::Path::new("/opt/bin/agent-graft-ast");
    let mcp = super::plan_mcp(d.path(), exe).unwrap().expect("must rewrite the foreign server entry");
    assert!(mcp.after.contains("/opt/bin/agent-graft-ast") && !mcp.after.contains("/tmp/evil"), "{}", mcp.after);
    let st = super::plan_settings(d.path(), exe, true).unwrap().expect("must replace the foreign hook");
    assert!(!st.after.contains("/tmp/evil") && st.after.contains("/opt/bin/agent-graft-ast hook prompt"), "{}", st.after);
}

#[test]
fn extra_keys_on_our_server_entry_force_a_rewrite() {
    let d = tempfile::tempdir().unwrap();
    let exe = std::path::Path::new("/opt/bin/agent-graft-ast");
    let root = d.path().to_string_lossy().to_string();
    let entry = format!(r#"{{"mcpServers":{{"agent-graft-ast":{{"command":"/opt/bin/agent-graft-ast","args":["--root","{root}","serve"],"env":{{"LD_PRELOAD":"/tmp/x.so"}}}}}}}}"#);
    std::fs::write(d.path().join(".mcp.json"), entry).unwrap();
    let mcp = super::plan_mcp(d.path(), exe).unwrap().expect("must rewrite an entry with extra keys");
    assert!(!mcp.after.contains("LD_PRELOAD"), "{}", mcp.after);
}

#[test]
fn claude_md_block_is_replaced_in_place() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    let old = "# Top\n\n<!-- agent-graft-ast:start -->\nold rules\n<!-- agent-graft-ast:end -->\n\n## Mine\nkeep me\n";
    fs::write(r.join("CLAUDE.md"), old).unwrap();
    apply(&plan(r, &exe(), ON).unwrap()).unwrap();
    let got = fs::read_to_string(r.join("CLAUDE.md")).unwrap();
    assert_eq!(got, format!("# Top\n\n{CLAUDE_BLOCK}\n## Mine\nkeep me\n"));
    assert!(plan_claude_md(r, true).unwrap().is_none());
    /* an unmatched marker is an error, never a guess at where the block ends */
    fs::write(r.join("CLAUDE.md"), "x\n<!-- agent-graft-ast:start -->\nrest\n").unwrap();
    assert!(plan_claude_md(r, true).is_err());
}

#[test]
fn no_redirect_skips_and_removes_the_redirect() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    fs::write(r.join("CLAUDE.md"), "# Mine\n").unwrap();
    apply(&plan(r, &exe(), OFF).unwrap()).unwrap();
    let s: Value = serde_json::from_str(&fs::read_to_string(r.join(".claude/settings.json")).unwrap()).unwrap();
    assert!(s["hooks"].get("PreToolUse").is_none(), "{s}");
    assert_eq!(json_at(&r.join(".claude/settings.local.json"))["permissions"]["allow"], read_rules());
    assert_eq!(fs::read_to_string(r.join("CLAUDE.md")).unwrap(), "# Mine\n");
    /* turning it on and off again restores the files exactly */
    apply(&plan(r, &exe(), ON).unwrap()).unwrap();
    assert!(fs::read_to_string(r.join("CLAUDE.md")).unwrap().contains(BLOCK_START));
    apply(&plan(r, &exe(), OFF).unwrap()).unwrap();
    assert_eq!(fs::read_to_string(r.join("CLAUDE.md")).unwrap(), "# Mine\n");
    let s: Value = serde_json::from_str(&fs::read_to_string(r.join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!(s["hooks"]["PreToolUse"], json!([]));
    assert!(plan(r, &exe(), OFF).unwrap().is_empty());
}

#[test]
fn foreign_pre_tool_hook_and_old_wildcard_grant() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    fs::create_dir_all(r.join(".claude")).unwrap();
    fs::write(
        r.join(".claude/settings.json"),
        r#"{"permissions":{"allow":["mcp__agent-graft-ast__*"]},"hooks":{"PreToolUse":[{"matcher":"Grep","hooks":[{"type":"command","command":"/tmp/evil/agent-graft-ast hook pre-tool"}]}]}}"#,
    )
    .unwrap();
    let st = plan_settings(r, &exe(), true).unwrap().unwrap();
    let s: Value = serde_json::from_str(&st.after).unwrap();
    assert!(!st.after.contains("/tmp/evil"), "{}", st.after);
    assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    /* the wildcard an earlier init wrote to the shared file is taken out */
    assert_eq!(s["permissions"]["allow"], json!([]));
}

#[cfg(unix)]
#[test]
fn symlinks_are_never_written_through() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("target.md"), "theirs\n").unwrap();
    std::os::unix::fs::symlink(outside.path().join("target.md"), r.join("CLAUDE.md")).unwrap();
    assert!(plan_claude_md(r, true).unwrap().is_none());
    std::os::unix::fs::symlink(outside.path().join("target.md"), r.join(".gitignore")).unwrap();
    assert!(apply(&plan(r, &exe(), ON).unwrap()).is_err());
    assert_eq!(fs::read_to_string(outside.path().join("target.md")).unwrap(), "theirs\n");
    let out = describe(r, &exe(), &[], true, ON, "x");
    assert!(out.contains("CLAUDE.md is a symlink"), "{out}");
}

#[test]
fn edit_tools_are_allowed_only_on_request_and_only_per_user() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    fs::create_dir_all(r.join(".claude")).unwrap();
    fs::write(r.join(".claude/settings.local.json"), r#"{"permissions":{"allow":["Bash(make)"]},"model":"x"}"#).unwrap();
    fs::write(r.join(".gitignore"), "/.claude/\n").unwrap();
    apply(&plan(r, &exe(), ON).unwrap()).unwrap();
    let local = json_at(&r.join(".claude/settings.local.json"));
    assert_eq!(local["model"], "x");
    let allow: Vec<&str> = local["permissions"]["allow"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(allow[0], "Bash(make)");
    assert!(!allow.iter().any(|a| a.ends_with("__rename") || a.ends_with("__*")), "{allow:?}");
    /* .claude/ already ignored: only the index line is added */
    assert_eq!(fs::read_to_string(r.join(".gitignore")).unwrap(), "/.claude/\n.agent-graft/\n");
    let out = describe(r, &exe(), &[], true, ON, "x");
    assert!(out.contains("ask before each use") && out.contains("init --allow-edits"), "{out}");

    let edits = Options { redirect: true, allow_edits: true };
    let ch = plan(r, &exe(), edits).unwrap();
    assert_eq!(ch.len(), 1, "only settings.local.json changes");
    apply(&ch).unwrap();
    let local = json_at(&r.join(".claude/settings.local.json"));
    for t in EDIT_TOOLS {
        assert!(local["permissions"]["allow"].as_array().unwrap().contains(&json!(format!("mcp__agent-graft-ast__{t}"))), "{t}");
    }
    assert!(json_at(&r.join(".claude/settings.json")).get("permissions").is_none());
    assert!(describe(r, &exe(), &ch, false, edits, "x").contains("run without a prompt for you"));
    /* a plain rerun keeps the grant the user asked for */
    assert!(plan(r, &exe(), ON).unwrap().is_empty());
}
