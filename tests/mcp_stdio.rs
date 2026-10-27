use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn serves_mcp_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("svc.ts"), "export function a() { b(); }\nexport function b() {}\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-graft-ast"))
        .args(["--root", dir.path().to_str().unwrap(), "--db", dir.path().join("i.redb").to_str().unwrap(), "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut rpc = |msg: &str| {
        writeln!(stdin, "{msg}").unwrap();
        if !msg.contains("\"id\"") {
            return serde_json::Value::Null;
        }
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    let init = rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#);
    assert_eq!(init["result"]["serverInfo"]["name"], "agent-graft-ast");
    rpc(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let r = rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"svc.ts","level":2,"token_budget":500}}}"#);
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("export function a();"), "{text}");
    assert!(text.contains("<budget_receipt"));
    drop(rpc);
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
