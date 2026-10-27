#!/usr/bin/env python3
"""Tokens per read: raw file vs graft_file_api output, for files_<repo>.txt.

Usage: tokens_bench.py <repo-name> <repo-dir>
Tokenizer: tiktoken o200k_base (same as count_tokens.py). Saves each file_api response
under out/fileapi/<repo>/ and prints one JSON object.
"""
import json
import os
import subprocess
import sys

import tiktoken

here = os.path.dirname(os.path.abspath(__file__))
name, root = sys.argv[1:3]
enc = tiktoken.get_encoding("o200k_base")
count = lambda s: len(enc.encode(s, disallowed_special=()))
files = [f for f in open(f"{here}/files_{name}.txt").read().split("\n") if f]

env = {**os.environ, "DO_NOT_TRACK": "1", "GRAFT_NO_GITIGNORE": "1", "GRAFT_NO_IGNORE": "1"}
p = subprocess.Popen(["graft", "mcp", root], cwd=root, env=env, text=True,
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
nid = 0


def rpc(method, params):
    global nid
    nid += 1
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": nid, "method": method, "params": params}) + "\n")
    p.stdin.flush()
    while True:
        msg = json.loads(p.stdout.readline())
        if msg.get("id") == nid:
            return msg


rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "bench", "version": "0"}})
p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
outdir = f"{here}/out/fileapi/{name}"
os.makedirs(outdir, exist_ok=True)
rows = []
for f in files:
    raw = open(f"{root}/{f}", encoding="utf-8", errors="replace").read()
    r = rpc("tools/call", {"name": "graft_file_api", "arguments": {"file": f}})
    api = "".join(c.get("text", "") for c in r["result"]["content"])
    open(f"{outdir}/{f.replace('/', '__')}.txt", "w").write(api)
    rows.append({"file": f, "lines": raw.count("\n"), "raw_tokens": count(raw), "file_api_tokens": count(api),
                 # same output minus graft's "[graft] tokens saved ..." banner line
                 "file_api_tokens_no_banner": count("\n".join(l for l in api.split("\n") if not l.startswith("[graft] tokens saved"))),
                 "is_error": bool(r["result"].get("isError"))})
p.stdin.close()
p.wait()
tr = sum(r["raw_tokens"] for r in rows)
ta = sum(r["file_api_tokens"] for r in rows)
tb = sum(r["file_api_tokens_no_banner"] for r in rows)
print(json.dumps({"repo": name, "tokenizer": "tiktoken o200k_base", "files": rows,
                  "total_raw": tr, "total_file_api": ta, "total_file_api_no_banner": tb, "ratio_raw_over_api": round(tr / ta, 2)}))
