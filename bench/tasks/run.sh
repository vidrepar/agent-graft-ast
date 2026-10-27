#!/usr/bin/env bash
# HopCount end to end: bench/tasks/run.sh [runs_per_arm] [task_id ...]
# Env: REPO_<NAME> (task repo checkouts), HOPCOUNT_WORK, MODEL (default claude-sonnet-5-5), RUN_TIMEOUT_S, MAX_BUDGET_USD, GRAFT_BIN, ARMS (default "baseline treatment").
set -euo pipefail
cd "$(dirname "$0")"
RUNS="${1:-3}"
shift || true
TASKS="${*:-$(python3 -I bench.py tasks)}"
BIN="${GRAFT_BIN:-../../target/release/agent-graft-ast}"
[ -x "$BIN" ] || { echo "missing $BIN: run cargo build --release first" >&2; exit 1; }

python3 -I bench.py prep
for n in $(seq 1 "$RUNS"); do
  for t in $TASKS; do
    # Alternate arm order per run so no arm always goes first.
    arms="${ARMS:-baseline treatment}"
    if [ $((n % 2)) -eq 0 ]; then arms="$(echo $arms | tr ' ' '\n' | tail -r | tr '\n' ' ')"; fi
    for arm in $arms; do
      python3 -I bench.py run "$t" "$arm" "$n" || echo "run $t $arm $n errored" >&2
    done
  done
done
python3 -I bench.py aggregate
