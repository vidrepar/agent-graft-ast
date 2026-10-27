"""Medians from bench/engine/out -> bench/engine/results.json"""
import json, os, re, statistics as st
HERE = os.path.dirname(os.path.abspath(__file__)); OUT = os.path.join(HERE, "out")
TOOLS = ["callers", "find_symbol", "context", "read_file_l1", "read_file_l2"]
res = {}
for n in ["ts-monorepo", "ts-service", "rust-analyzer"]:
    if not os.path.exists(f"{OUT}/{n}.bench.json"):
        print(f"skip {n}: no results in out/")
        continue
    idx = [json.load(open(f"{OUT}/{n}.index.{i}.json")) for i in range(1, 6)]
    rss = [int(re.search(r"(\d+)\s+maximum resident", open(f"{OUT}/{n}.index.{i}.time").read()).group(1)) for i in range(1, 6)]
    mcp = [json.load(open(f"{OUT}/{n}.mcp.{i}.json")) for i in range(1, 6)]
    b = json.load(open(f"{OUT}/{n}.bench.json"))
    files = b["files"]
    tot = lambda k: sum(f[k] for f in files)
    r = {
        "files": idx[0]["files"], "symbols": idx[0]["stats"]["symbols"], "graph_edges": idx[0]["graph_edges"],
        "cold_index_ms_median": st.median(x["total_ms"] for x in idx), "cold_index_ms_all": [x["total_ms"] for x in idx],
        "cold_index_peak_rss_mb_median": st.median(rss) / 2**20,
        "startup_ms_median": st.median(x["startup_ms"] for x in mcp), "startup_ms_all": [x["startup_ms"] for x in mcp],
        "first_answer_ms_median": st.median(x["first_answer_ms"] for x in mcp),
        "warm_ms_median": st.median(x["warm_ms"] for x in mcp),
        "server_peak_rss_mb_median": st.median(x["server_peak_rss_bytes"] for x in mcp) / 2**20,
        "edit_visible": mcp[0]["edit_visible"], "edit_reindex_engine": mcp[0]["edit_reindex_engine"],
        "inproc_graph_query": b["graph_query"],
        "tokens": {k: tot(k) for k in ["raw_tokens", "level1_tokens", "level2_tokens", "read_file_l1_response_tokens", "read_file_l2_response_tokens"]},
        "tokens_per_file": files,
    }
    for t in TOOLS:
        r[f"stdio_{t}_p50_ms"] = st.median(x[t]["p50_ms"] for x in mcp)
        r[f"stdio_{t}_p99_ms"] = st.median(x[t]["p99_ms"] for x in mcp)
        r[f"stdio_{t}_p99_ms_worst_run"] = max(x[t]["p99_ms"] for x in mcp)
    res[n] = r
json.dump(res, open(os.path.join(HERE, "results.json"), "w"), indent=2)
for n, r in res.items():
    print(n, {k: (round(v, 3) if isinstance(v, float) else v) for k, v in r.items() if not isinstance(v, (list, dict))})
