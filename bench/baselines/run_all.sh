#!/usr/bin/env bash
# Full baseline run. Serial on purpose (no cross-benchmark CPU contention).
# Usage: run_all.sh [runs=3]   -> out/*.jsonl, then aggregate.py -> results.json
set -euo pipefail
export PATH="$HOME/Library/pnpm/bin:$PATH"   # graft installed via `pnpm add -g`
RUNS=${1:-3}
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/out"; mkdir -p "$OUT"
cd "$HERE"

uptime > "$OUT/load.txt"
bash bench_index.sh "$RUNS"             # leaves a fresh graft/ index in each repo
python3 select_inputs.py                # deterministic; regenerates the same lists

: > "$OUT/mcp.jsonl"; : > "$OUT/mcp_norefresh.jsonl"; : > "$OUT/astgrep_query.jsonl"; : > "$OUT/tokens.jsonl"
# repo dirs come from REPO_TS_MONOREPO / REPO_TS_SERVICE / REPO_RUST_ANALYZER; unset = skipped
for spec in "ts-monorepo:REPO_TS_MONOREPO:ts" "ts-service:REPO_TS_SERVICE:ts" "rust-analyzer:REPO_RUST_ANALYZER:rs"; do
  IFS=: read -r name var lang <<<"$spec"
  dir="${!var:-}"
  if [ -z "$dir" ]; then echo "skip $name: \$$var not set"; continue; fi
  for i in $(seq 1 "$RUNS"); do
    node mcp_bench.mjs "$name" "$dir" >> "$OUT/mcp.jsonl"
    GRAFT_NO_REFRESH=1 node mcp_bench.mjs "$name" "$dir" >> "$OUT/mcp_norefresh.jsonl"
    python3 astgrep_query_bench.py "$name" "$dir" "$lang" 30 >> "$OUT/astgrep_query.jsonl"
  done
  python3 tokens_bench.py "$name" "$dir" >> "$OUT/tokens.jsonl"
  echo "done $name"
done
uptime >> "$OUT/load.txt"
python3 aggregate.py
