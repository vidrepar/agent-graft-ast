#!/usr/bin/env python3
"""Fold out/*.tsv|jsonl into results.json (medians across runs)."""
import json
import os
import statistics as st
import subprocess
from collections import defaultdict

here = os.path.dirname(os.path.abspath(__file__))
out = f"{here}/out"
med = lambda xs: round(st.median(xs), 3)
jl = lambda f: [json.loads(l) for l in open(f"{out}/{f}") if l.strip()]
ver = lambda cmd: subprocess.run(cmd, shell=True, capture_output=True, text=True).stdout.strip().splitlines()[-1]

res = {
    "versions": {
        "ast-grep": ver("ast-grep --version"),
        "graft": ver("graft --version").split()[-1] + " (pnpm add -g @nanonets/graft@0.25.0)",
        "node": ver("node --version"),
        "tiktoken": ver("python3 -c 'import tiktoken; print(tiktoken.__version__)'"),
        "os": ver("sw_vers -productVersion"),
        "cpu": ver("sysctl -n machdep.cpu.brand_string") + f" ({os.cpu_count()} cores)",
    },
    "tokenizer": "tiktoken o200k_base",
    "load_avg_start_end": open(f"{out}/load.txt").read().strip().splitlines(),
    "index": {}, "query": {}, "tokens": {},
}

idx = defaultdict(lambda: {"wall_s": [], "maxrss_bytes": []})
for line in open(f"{out}/index_raw.tsv"):
    tool, repo, _, wall, rss = line.split("\t")
    idx[(tool, repo)]["wall_s"].append(float(wall))
    idx[(tool, repo)]["maxrss_bytes"].append(int(rss))
for (tool, repo), v in idx.items():
    res["index"].setdefault(tool, {})[repo] = {
        "runs": len(v["wall_s"]), "wall_s_median": med(v["wall_s"]), "wall_s_all": v["wall_s"],
        "maxrss_bytes_median": int(st.median(v["maxrss_bytes"])), "maxrss_bytes_max": max(v["maxrss_bytes"]),
    }

for key, f in (("graft_mcp", "mcp.jsonl"), ("graft_mcp_no_refresh", "mcp_norefresh.jsonl")):
    by = defaultdict(list)
    for r in jl(f):
        by[r["repo"]].append(r)
    for repo, rs in by.items():
        d = {"runs": len(rs), "init_ms_median": med([r["init_ms"] for r in rs]),
             "server_maxrss_bytes_median": int(st.median([r["server_maxrss_bytes"] for r in rs])),
             "server_tree_peak_rss_bytes_sampled_max": max(r["server_tree_peak_rss_bytes_sampled"] for r in rs)}
        for tool in ("graft_trace_calls", "graft_file_api"):
            d[tool] = {"calls_per_run": rs[0][tool]["n"],
                       "p50_ms_median": med([r[tool]["p50_ms"] for r in rs]),
                       "p99_ms_median": med([r[tool]["p99_ms"] for r in rs]),
                       "errors_total": sum(r[tool]["errors"] for r in rs),
                       "mean_resp_bytes": rs[0][tool]["mean_resp_bytes"]}
        res["query"].setdefault(key, {})[repo] = d

by = defaultdict(list)
for r in jl("astgrep_query.jsonl"):
    by[r["repo"]].append(r)
for repo, rs in by.items():
    res["query"].setdefault("astgrep_run", {})[repo] = {
        "runs": len(rs), "queries_per_run": rs[0]["n"], "cmd": rs[0]["cmd"],
        "p50_ms_median": med([r["p50_ms"] for r in rs]), "p99_ms_median": med([r["p99_ms"] for r in rs]),
        "maxrss_bytes_max": max(r["maxrss_bytes_max"] for r in rs)}

for r in jl("tokens.jsonl"):
    res["tokens"][r["repo"]] = {k: r[k] for k in ("files", "total_raw", "total_file_api", "total_file_api_no_banner", "ratio_raw_over_api")}

json.dump(res, open(f"{here}/results.json", "w"), indent=2)
print(json.dumps(res, indent=1)[:200], "... -> results.json")
