#!/usr/bin/env bash
# Benchmarks agent-graft-ast on the three repos. Output: bench/engine/out/<repo>.{index,bench,mcp}.*.json
# Needs bench/baselines/{files,symbols,apifiles}_<repo>.txt: run bench/baselines/select_inputs.py first for the TS repos.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$HERE/../../target/release/agent-graft-ast"
OUT="$HERE/out"; mkdir -p "$OUT"
DB="$(mktemp -d)"   # fresh index dir per run; nothing outside it is ever deleted
trap 'rm -rf "${DB:?}"' EXIT
# Repo dirs come from env vars; unset = skipped. The edit probe appends to one file and
# restores it afterwards, so point these at throwaway copies, not your working repos.
var_of() {
  case "$1" in
    ts-monorepo) echo REPO_TS_MONOREPO ;;
    ts-service) echo REPO_TS_SERVICE ;;
    rust-analyzer) echo REPO_RUST_ANALYZER ;;
  esac
}
# edit-probe file: rust-analyzer's is fixed; for the TS repos, line 5 (ts-monorepo) or
# line 3 (ts-service) of baselines/files_<repo>.txt, the same picks the recorded results used
edit_args() {
  local f ext=ts
  case "$1" in
    rust-analyzer) f=crates/ide/src/expand_macro.rs; ext=rs ;;
    ts-monorepo) f="$(sed -n 5p "$HERE/../baselines/files_$1.txt")" ;;
    ts-service) f="$(sed -n 3p "$HERE/../baselines/files_$1.txt")" ;;
  esac
  echo "$2 $DB/edit-$1.redb $f $ext"
}
for name in ts-monorepo ts-service rust-analyzer; do
  var="$(var_of "$name")"; root="${!var:-}"; db="$DB/$name.redb"
  if [ -z "$root" ]; then echo "skip $name: \$$var not set"; continue; fi
  # cold index: no index on disk, 5 runs
  for i in 1 2 3 4 5; do
    rm -f "${db:?}"
    /usr/bin/time -l "$BIN" --root "$root" --db "$db" index --fresh > "$OUT/$name.index.$i.json" 2> "$OUT/$name.index.$i.time"
  done
  /usr/bin/time -l "$BIN" --root "$root" --db "$db" bench --iters 5000 \
    --files "$HERE/../baselines/files_$name.txt" > "$OUT/$name.bench.json" 2> "$OUT/$name.bench.time"
  # stdio: startup, warm queries, edit->visible; 5 runs (edit probe on the first only)
  for i in 1 2 3 4 5; do
    extra=""; [ "$i" = 1 ] && extra="$(edit_args "$name" "$root")"
    # shellcheck disable=SC2086
    python3 -I "$HERE/mcp_bench.py" "$BIN" "$root" "$db" "$HERE/../baselines/symbols_$name.txt" \
      "$HERE/../baselines/apifiles_$name.txt" "$OUT/$name.mcp.$i.json" $extra > /dev/null
  done
done
