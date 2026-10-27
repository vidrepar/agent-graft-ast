"""Rewrite flows through the MCP server, as an agent calls them (1 s between calls).

usage: python3 -I bench/rewrite/flows.py <agent-graft-ast binary> <rust-analyzer checkout>
The checkout is reset with `git checkout .` after every run.
"""
import json, subprocess, sys, time

B, R = sys.argv[1], sys.argv[2]
PAT = {"pattern": "$X.unwrap()", "lang": "rust"}
REP = {**PAT, "replacement": "$X.expect(\"checked\")"}


def server():
    p = subprocess.Popen([B, "--root", R, "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
    n = [0]

    def call(name, args):
        time.sleep(1)
        n[0] += 1
        msg = {"jsonrpc": "2.0", "id": n[0], "method": "tools/call", "params": {"name": name, "arguments": args}}
        if name is None:
            msg = {"jsonrpc": "2.0", "id": n[0], "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}
        p.stdin.write(json.dumps(msg) + "\n")
        p.stdin.flush()
        t = time.perf_counter()
        r = json.loads(p.stdout.readline())
        return (time.perf_counter() - t) * 1000, r

    call(None, None)
    call("find_symbol", {"query": "x"})
    time.sleep(12)  # index load, warm-up, idle tree parsing
    call("rewrite", {"pattern": "$X.clone()", "lang": "rust"})
    call("rewrite", {"pattern": "$X.to_string()", "lang": "rust"})
    return p, call


for flow in ["rewrite only", "search -> rewrite", "dry run -> rewrite"]:
    rows = []
    for _ in range(3):
        p, call = server()
        first = 0
        if flow == "search -> rewrite":
            first, _ = call("rewrite", PAT)
        elif flow == "dry run -> rewrite":
            first, _ = call("rewrite", {**REP, "dry_run": True})
        second, r = call("rewrite", REP)
        assert 'applied="true"' in r["result"]["content"][0]["text"]
        p.stdin.close()
        p.wait()
        subprocess.run(["git", "-C", R, "checkout", "-q", "."])
        rows.append((first, second))
    print(f"{flow:20s}", " | ".join(f"{a:.0f}+{b:.0f}={a + b:.0f}" for a, b in rows), "ms")
