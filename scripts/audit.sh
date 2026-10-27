#!/usr/bin/env bash
# Gate: zero known advisories in Rust and JS dependencies. Run before every commit.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
cargo audit --deny warnings
find . -name package.json -not -path '*/node_modules/*' -not -path './target/*' -not -path './.herdr/*' -print0 |
  while IFS= read -r -d '' pkg; do
    (cd "$(dirname "$pkg")" && pnpm audit)
  done
