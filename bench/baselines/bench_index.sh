#!/usr/bin/env bash
# Cold index / full-scan timing for graft and ast-grep.
# Usage: bench_index.sh [runs]   (default 3)
# Writes raw lines to out/index_raw.tsv: tool  repo  run  wall_s  maxrss_bytes
set -euo pipefail
export PATH="$HOME/Library/pnpm/bin:$PATH"   # graft installed via `pnpm add -g`
RUNS=${1:-3}
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/out"; mkdir -p "$OUT"
RAW="$OUT/index_raw.tsv"; : > "$RAW"; : > "$OUT/astgrep_match_counts.txt"
export DO_NOT_TRACK=1 GRAFT_NO_GITIGNORE=1 GRAFT_NO_IGNORE=1 GRAFT_NO_BROWSER=1

# repo-name  dir  ast-grep-langs. Each dir comes from an env var; unset = skipped.
# Point the TS ones at throwaway copies: graft writes a graft/ dir into each repo.
REPOS=()
add_repo() { if [ -n "${2:-}" ]; then REPOS+=("$1:$2:$3"); else echo "skip $1: \$$4 not set"; fi; }
add_repo ts-monorepo "${REPO_TS_MONOREPO:-}" "ts tsx" REPO_TS_MONOREPO
add_repo ts-service "${REPO_TS_SERVICE:-}" "ts tsx" REPO_TS_SERVICE
add_repo rust-analyzer "${REPO_RUST_ANALYZER:-}" "rs" REPO_RUST_ANALYZER

# /usr/bin/time -l output -> "wall maxrss"
parse_time() { awk '/ real /{w=$1} /maximum resident set size/{r=$1} END{print w"\t"r}' "$1"; }

ts_rule() { cat <<EOF
id: fn-ts
language: TypeScript
rule: { any: [ { kind: function_declaration }, { kind: method_definition }, { kind: arrow_function }, { kind: function_expression } ] }
---
id: fn-tsx
language: Tsx
rule: { any: [ { kind: function_declaration }, { kind: method_definition }, { kind: arrow_function }, { kind: function_expression } ] }
EOF
}
rs_rule() { cat <<EOF
id: fn-rs
language: Rust
rule: { kind: function_item }
EOF
}

for spec in ${REPOS[@]+"${REPOS[@]}"}; do
  IFS=: read -r name dir langs <<<"$spec"
  for i in $(seq 1 "$RUNS"); do
    # graft: cold build, index dir + extraction cache wiped, --no-reuse
    rm -rf "${dir:?}/graft"
    ( cd "$dir" && /usr/bin/time -l graft build --no-reuse . >/dev/null 2>"$OUT/t.txt" ) || { echo "graft build failed on $name"; cat "$OUT/t.txt" | tail -5; }
    printf "graft_build\t%s\t%s\t%s\n" "$name" "$i" "$(parse_time "$OUT/t.txt")" >> "$RAW"

    # ast-grep: kind-based "all function definitions" scan (primary)
    if [[ $langs == rs ]]; then rule=$(rs_rule); else rule=$(ts_rule); fi
    ( cd "$dir" && /usr/bin/time -l ast-grep scan --inline-rules "$rule" --json=stream . > "$OUT/sg_$name.jsonl" 2>"$OUT/t.txt" ) || true
    printf "astgrep_scan_kind\t%s\t%s\t%s\n" "$name" "$i" "$(parse_time "$OUT/t.txt")" >> "$RAW"

    # ast-grep: pattern-based scan (secondary, the literal pattern from the brief)
    if [[ $langs == rs ]]; then pat='fn $F($$$) $$$'; lang=rs; else pat='function $F($$$) { $$$ }'; lang=ts; fi
    ( cd "$dir" && /usr/bin/time -l ast-grep run -p "$pat" -l "$lang" --json=stream . > "$OUT/sgp_$name.jsonl" 2>"$OUT/t.txt" ) || true
    printf "astgrep_run_pattern\t%s\t%s\t%s\n" "$name" "$i" "$(parse_time "$OUT/t.txt")" >> "$RAW"
  done
  echo "$name: kind-scan matches=$(wc -l < "$OUT/sg_$name.jsonl") pattern matches=$(wc -l < "$OUT/sgp_$name.jsonl")" | tee -a "$OUT/astgrep_match_counts.txt"
  rm -f "${OUT:?}/sg_$name.jsonl" "${OUT:?}/sgp_$name.jsonl"  # match dumps are large (~80 MB)
done
# leave a fresh index in place for the query benchmark
cat "$RAW"
