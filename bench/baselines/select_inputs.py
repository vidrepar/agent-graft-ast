#!/usr/bin/env python3
"""Pick deterministic benchmark inputs per repo (seed 42).

files_<repo>.txt    5 representative mid-size source files (token comparison; reused by our engine)
symbols_<repo>.txt  100 function/method names that have >=1 incoming call edge in graft's graph
apifiles_<repo>.txt 100 source files for graft_file_api (sampled with replacement if fewer exist)
"""
import json
import os
import random

HERE = os.path.dirname(os.path.abspath(__file__))
# repo dirs come from env vars; a repo whose var is unset is skipped
REPOS = {
    "ts-monorepo": ("REPO_TS_MONOREPO", (".ts", ".tsx")),
    "ts-service": ("REPO_TS_SERVICE", (".ts",)),
    "rust-analyzer": ("REPO_RUST_ANALYZER", (".rs",)),
}
SKIP = ("test", "fixture", "generated", ".d.ts", "/tests/", "e2e", "spec", "bench")


def lines(path):
    with open(path, "rb") as f:
        return f.read().count(b"\n")


for name, (var, exts) in REPOS.items():
    root = os.environ.get(var)
    if not root:
        print(f"skip {name}: ${var} not set")
        continue
    rnd = random.Random(42)
    g = json.load(open(f"{root}/graft/.graph/wiring.json"))
    files = sorted(
        n["path"] for n in g["nodes"]
        if n["kind"] == "file" and n["path"].endswith(exts) and not any(s in n["path"].lower() for s in SKIP)
    )

    # 5 representative files: evenly spaced across the 60th-90th percentile by line count
    by_size = sorted(files, key=lambda p: (lines(f"{root}/{p}"), p))
    lo, hi = int(len(by_size) * 0.6), max(int(len(by_size) * 0.9) - 1, int(len(by_size) * 0.6))
    picks = [by_size[lo + round(i * (hi - lo) / 4)] for i in range(5)]
    if len(set(picks)) < 5:  # tiny repo: take the 5 largest non-test files instead
        picks = by_size[-5:]
    open(f"{HERE}/files_{name}.txt", "w").write("\n".join(picks) + "\n")

    called = {e["target"] for e in g["edges"] if e["relation"] == "calls"}
    syms = sorted({
        n["name"] for n in g["nodes"]
        if n["kind"] in ("function", "method") and n["id"] in called and n["path"].endswith(exts)
        and n["name"].isidentifier()
    })
    sample = rnd.sample(syms, 100) if len(syms) >= 100 else [rnd.choice(syms) for _ in range(100)]
    open(f"{HERE}/symbols_{name}.txt", "w").write("\n".join(sample) + "\n")

    api = rnd.sample(files, 100) if len(files) >= 100 else [rnd.choice(files) for _ in range(100)]
    open(f"{HERE}/apifiles_{name}.txt", "w").write("\n".join(api) + "\n")
    print(name, "files:", len(files), "called syms:", len(syms), "picks:", [(p, lines(f'{root}/{p}')) for p in picks])
