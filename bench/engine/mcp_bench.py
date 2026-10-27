"""Stdio latency, startup and edit->visible time of `agent-graft-ast serve`; same protocol as bench/baselines/mcp_bench.mjs.

usage: python3 -I mcp_bench.py <bin> <root> <db> <symbols.txt> <apifiles.txt> <out.json> [edit_root edit_db edit_file ext]
The edit probe appends a function to edit_file in a throwaway copy, times until find_symbol sees it, then restores the file.
"""
import json, os, re, subprocess, sys, time

binary, root, db, symfile, apifile, out = sys.argv[1:7]
edit = sys.argv[7:11]
symbols = [l.strip() for l in open(symfile) if l.strip()]
files = [l.strip() for l in open(apifile) if l.strip()]


class Server:
    def __init__(self, root, db):
        self.t0 = time.perf_counter()
        self.p = subprocess.Popen(["/usr/bin/time", "-l", binary, "--root", root, "--db", db, "serve"],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        self.seq = 0

    def rpc(self, method, params=None):
        self.seq += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.seq, "method": method, "params": params or {}}) + "\n")
        self.p.stdin.flush()
        return json.loads(self.p.stdout.readline())

    def call(self, tool, args):
        r = self.rpc("tools/call", {"name": tool, "arguments": args})
        return r["result"]["content"][0]["text"], r["result"].get("isError", False)

    def init(self):
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "bench", "version": "0"}})
        t = (time.perf_counter() - self.t0) * 1000
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        return t

    def wait_warm(self):
        while 'warm="true"' not in self.call("index_status", {})[0]:
            time.sleep(0.005)
        return (time.perf_counter() - self.t0) * 1000

    def close(self):
        self.p.stdin.close()
        self.p.wait()
        m = re.search(r"(\d+)\s+maximum resident set size", self.p.stderr.read())
        return int(m.group(1)) if m else None


def stats(lat):
    s = sorted(lat)
    pick = lambda p: s[min(len(s) - 1, round((len(s) - 1) * p))]
    return {"n": len(s), "p50_ms": pick(0.5), "p99_ms": pick(0.99), "max_ms": s[-1]}


def bench(srv, tool, args_list):
    for a in args_list[:4]:  # 1 first call + 3 warmup, not counted
        srv.call(tool, a)
    lat, errors, sizes = [], 0, []
    for a in args_list:
        t = time.perf_counter()
        text, err = srv.call(tool, a)
        lat.append((time.perf_counter() - t) * 1000)
        errors += bool(err)
        sizes.append(len(text))
    r = stats(lat)
    r.update({"errors": errors, "mean_bytes": sum(sizes) / len(sizes)})
    return r


srv = Server(root, db)
res = {"startup_ms": srv.init()}
# first real answer: a graph query (needs the index and the graph loaded)
first_text, _ = srv.call("callers", {"name": symbols[0]})
res["first_answer_ms"] = (time.perf_counter() - srv.t0) * 1000
res["first_answer_estimated"] = 'estimated="true"' in first_text
res["warm_ms"] = srv.wait_warm()
res["callers"] = bench(srv, "callers", [{"name": n} for n in symbols])
res["find_symbol"] = bench(srv, "find_symbol", [{"query": n} for n in symbols])
res["context"] = bench(srv, "context", [{"target": n} for n in symbols])
res["read_file_l1"] = bench(srv, "read_file", [{"path": f, "level": 1} for f in files])
res["read_file_l2"] = bench(srv, "read_file", [{"path": f, "level": 2} for f in files])
res["server_peak_rss_bytes"] = srv.close()

if edit:
    eroot, edb, efile, ext = edit
    path = os.path.join(eroot, efile)
    original = open(path).read()
    srv = Server(eroot, edb)
    srv.init()
    srv.wait_warm()
    visible, engine = [], []
    try:
        for i in range(20):
            name = f"agbProbe{i}"
            body = f"\npub fn {name}() {{}}\n" if ext == "rs" else f"\nexport function {name}() {{}}\n"
            t = time.perf_counter()
            with open(path, "w") as fh:
                fh.write(original + body)
            while 'total="0"' in srv.call("find_symbol", {"query": name})[0]:
                pass
            visible.append((time.perf_counter() - t) * 1000)
            st = srv.call("index_status", {})[0]
            engine.append(float(re.search(r'last_refresh_ms="([0-9.]+)"', st).group(1)))
            time.sleep(0.05)
    finally:
        with open(path, "w") as fh:
            fh.write(original)
    srv.close()
    res["edit_visible"] = stats(visible)
    res["edit_reindex_engine"] = stats(engine)

json.dump(res, open(out, "w"), indent=2)
print(json.dumps(res))
