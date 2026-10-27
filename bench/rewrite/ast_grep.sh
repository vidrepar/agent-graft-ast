#!/usr/bin/env bash
# ast-grep on the same flows, 1 s idle before each command. usage: bench/rewrite/ast_grep.sh <rust-analyzer checkout>
set -euo pipefail
cd "${1:?rust-analyzer checkout}"
ms() { sleep 1; local s; s=$(python3 -c 'import time;print(time.time())'); "$@" >/dev/null 2>&1; python3 -c "import time;print(round((time.time()-$s)*1000))"; }
for i in 1 2 3; do
  a=$(ms sg run -p '$X.unwrap()' -l rust .)
  b=$(ms sg run -p '$X.unwrap()' -r '$X.expect("checked")' -l rust -U .)
  git checkout -q .
  echo "search $a ms, rewrite $b ms"
done
