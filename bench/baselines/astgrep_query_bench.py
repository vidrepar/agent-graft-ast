#!/usr/bin/env python3
"""ast-grep per-query latency: each query is a fresh `ast-grep run -p 'SYM($$$)'` process.

Usage: astgrep_query_bench.py <repo-name> <repo-dir> <lang> [n=30]
Uses the first n symbols of symbols_<repo>.txt. Prints one JSON object.
maxrss comes from os.wait4 (per-child rusage; bytes on macOS).
"""
import json
import os
import subprocess
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
name, root, lang = sys.argv[1:4]
n = int(sys.argv[4]) if len(sys.argv) > 4 else 30
syms = [s for s in open(f"{here}/symbols_{name}.txt").read().split("\n") if s][:n]


def pct(xs, p):
    s = sorted(xs)
    return s[min(len(s) - 1, -(-p * len(s) // 100) - 1)]


ms, rss, hits = [], [], 0
for sym in syms:
    t = time.perf_counter()
    p = subprocess.Popen(["ast-grep", "run", "-p", f"{sym}($$$)", "-l", lang, "--json=stream", "."],
                         cwd=root, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    out = p.stdout.read()
    _, _, ru = os.wait4(p.pid, 0)
    ms.append((time.perf_counter() - t) * 1000)
    rss.append(ru.ru_maxrss)
    hits += out.count(b"\n")

print(json.dumps({
    "repo": name, "lang": lang, "n": len(ms),
    "p50_ms": round(pct(ms, 50), 2), "p99_ms": round(pct(ms, 99), 2), "max_ms": round(max(ms), 2),
    "mean_ms": round(sum(ms) / len(ms), 2), "maxrss_bytes_max": max(rss), "total_matches": hits,
    "cmd": f"ast-grep run -p '<SYM>($$$)' -l {lang} --json=stream .",
}))
