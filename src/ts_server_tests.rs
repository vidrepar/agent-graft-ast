use super::*;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn exe(p: &Path) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, "#!/bin/sh\n").unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

/// (repo, a directory outside it with a real-looking `node` and `tsserver.js`)
fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let repo = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    exe(&tools.path().join("bin/node"));
    exe(&tools.path().join("ts/lib/tsserver.js"));
    (repo, tools)
}

fn env(vars: &[(&str, String)]) -> impl Fn(&str) -> Option<OsString> + use<> {
    let m: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    move |k| m.get(k).map(OsString::from)
}

#[test]
fn relative_path_entries_never_supply_node() {
    let (repo, tools) = setup();
    exe(&repo.path().join("node_modules/.bin/node"));
    let ts = tools.path().join("ts").display().to_string();
    let bin = tools.path().join("bin").display().to_string();
    /* `node_modules/.bin` would resolve inside the repo: skipped, the absolute entry wins */
    let (node, _) = locate_with(repo.path(), env(&[("PATH", format!("node_modules/.bin:.:{bin}")), ("AGENT_GRAFT_TSSERVER", ts.clone())])).unwrap();
    assert_eq!(node, tools.path().join("bin/node").canonicalize().unwrap());
    /* with only relative entries there is no node at all */
    let err = locate_with(repo.path(), env(&[("PATH", "node_modules/.bin:.".into()), ("AGENT_GRAFT_TSSERVER", ts)])).unwrap_err();
    assert!(err.contains("not on PATH"), "{err}");
}

#[test]
fn node_inside_the_repo_is_refused() {
    let (repo, tools) = setup();
    let planted = repo.path().join("bin/node");
    exe(&planted);
    let ts = tools.path().join("ts").display().to_string();
    let err = locate_with(repo.path(), env(&[("PATH", repo.path().join("bin").display().to_string()), ("AGENT_GRAFT_TSSERVER", ts.clone())])).unwrap_err();
    assert!(err.contains("inside the repo"), "{err}");
    /* even named explicitly, and even for a trusted repo */
    let err = locate_with(
        repo.path(),
        env(&[("AGENT_GRAFT_NODE", planted.display().to_string()), ("AGENT_GRAFT_TSSERVER", ts), ("AGENT_GRAFT_ENABLE_CHECK", "1".into())]),
    )
    .unwrap_err();
    assert!(err.contains("inside the repo"), "{err}");
}

#[test]
fn relative_env_paths_and_untrusted_repo_tsserver_are_refused() {
    let (repo, tools) = setup();
    let node = tools.path().join("bin/node").display().to_string();
    let err = locate_with(repo.path(), env(&[("AGENT_GRAFT_NODE", "bin/node".into())])).unwrap_err();
    assert!(err.contains("relative"), "{err}");
    let err = locate_with(repo.path(), env(&[("AGENT_GRAFT_NODE", node.clone()), ("AGENT_GRAFT_TSSERVER", "ts/lib/tsserver.js".into())])).unwrap_err();
    assert!(err.contains("relative"), "{err}");
    /* a tsserver.js inside the repo runs repo code: only with AGENT_GRAFT_ENABLE_CHECK=1 */
    let local = repo.path().join("node_modules/typescript/lib/tsserver.js");
    exe(&local);
    let named = env(&[("AGENT_GRAFT_NODE", node.clone()), ("AGENT_GRAFT_TSSERVER", local.display().to_string())]);
    assert!(locate_with(repo.path(), named).unwrap_err().contains("inside the repo"));
    let trusted = env(&[("AGENT_GRAFT_NODE", node), ("AGENT_GRAFT_TSSERVER", local.display().to_string()), ("AGENT_GRAFT_ENABLE_CHECK", "1".into())]);
    assert_eq!(locate_with(repo.path(), trusted).unwrap().1, local.canonicalize().unwrap());
}
