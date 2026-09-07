#!/usr/bin/env bash
# Compare allocation churn using identical real captured markdown.
# First build zeron-ui to populate deps; no extra crates are needed.
# Usage: profile-markdown.sh TARGET/debug/deps TEXT_FILE [GIT_REF]
# Omit GIT_REF for working-tree code. Output is JSON; bytes are allocator
# requests/live heap, not RSS. The parser is compiled optimized in both runs.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
deps=$(realpath "$1")
source_text=$(realpath "$2")
bench_dir=$(mktemp -d)
trap 'rm -rf "$bench_dir"' EXIT
for module in parser mend; do
  if [[ -n "${3:-}" ]]; then
    git -C "$root" show "$3:crates/ui/src/markdown/$module.rs" > "$bench_dir/$module.rs"
  else
    cp "$root/crates/ui/src/markdown/$module.rs" "$bench_dir/$module.rs"
  fi
done
cat > "$bench_dir/main.rs" <<EOF
mod markdown {
    #[path = "$bench_dir/mend.rs"] pub mod mend;
    #[path = "$bench_dir/parser.rs"] pub mod parser;
}
include!("$root/scripts/markdown-profile.rs");
EOF
cmark=$(ls -t "$deps"/libpulldown_cmark-*.rlib | head -1)
rustc --edition=2024 -O -Awarnings -L "dependency=$deps" \
  --extern "pulldown_cmark=$cmark" "$bench_dir/main.rs" -o "$bench_dir/profile"
"$bench_dir/profile" "$source_text"
