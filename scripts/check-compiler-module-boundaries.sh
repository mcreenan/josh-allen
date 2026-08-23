#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

fixture_check_dir=$(mktemp -d "${TMPDIR:-/tmp}/allen-fixture-check.XXXXXX")
fixture_check_bin="$fixture_check_dir/check-frontend-fixtures"
cleanup_fixture_check() {
  rm -f "$fixture_check_bin"
  rmdir "$fixture_check_dir" 2>/dev/null || true
}
trap cleanup_fixture_check EXIT

files=(
  "crates/allen-compiler/src/frontend.rs"
  "crates/allen-compiler/src/frontend/syntax_lowering.rs"
  "crates/allen-compiler/src/frontend/resolution.rs"
  "crates/allen-compiler/src/frontend/checking.rs"
  "crates/allen-compiler/src/frontend/hir.rs"
  "crates/allen-compiler/src/frontend/mir.rs"
  "crates/allen-compiler/src/frontend/bytecode_lowering.rs"
  "crates/allen-compiler/src/frontend/ir.rs"
  "crates/allen-compiler/src/frontend/tool.rs"
)
budgets=(1100 1350 3800 450 220 600 8600 100 350)

for index in "${!files[@]}"; do
  file=${files[$index]}
  budget=${budgets[$index]}
  first_line=$(head -n 1 "$file")
  if [[ "$first_line" != "//!"* ]]; then
    echo "$file must begin with a module-responsibility doc comment" >&2
    exit 1
  fi
  lines=$(wc -l < "$file" | tr -d ' ')
  if (( lines > budget )); then
    echo "$file has $lines handwritten lines; budget is $budget" >&2
    exit 1
  fi
  printf '%-68s %5d / %d\n' "$file" "$lines" "$budget"
done

too_many_lines=$(rg -c 'clippy::too_many_lines' \
  crates/allen-compiler/src/frontend.rs \
  crates/allen-compiler/src/frontend/*.rs \
  | awk -F: '{ total += $2 } END { print total + 0 }')
if (( too_many_lines > 38 )); then
  echo "semantic split introduced a new clippy::too_many_lines suppression" >&2
  exit 1
fi

if rg -n 'allen-(compiler|bytecode|runtime|vm)|josh' crates/allen-syntax/Cargo.toml; then
  echo "allen-syntax must remain compiler and semantic-layer independent" >&2
  exit 1
fi

if ! rg -q '^allen-syntax = ' crates/allen-compiler/Cargo.toml; then
  echo "allen-compiler must retain its one-way dependency on allen-syntax" >&2
  exit 1
fi

if rg -n 'resolution::' crates/allen-compiler/src/frontend/checking.rs; then
  echo "checking must not depend back on resolution" >&2
  exit 1
fi

if rg -n 'bytecode_lowering::' \
  crates/allen-compiler/src/frontend/checking.rs \
  crates/allen-compiler/src/frontend/resolution.rs \
  crates/allen-compiler/src/frontend/hir.rs \
  crates/allen-compiler/src/frontend/mir.rs; then
  echo "resolution, checking, HIR, and MIR must not depend on bytecode lowering" >&2
  exit 1
fi

if ! rg -U -q '#\[cfg\(test\)\]\n#\[rustfmt::skip\]\nmod tests;' \
  crates/allen-compiler/src/frontend.rs; then
  echo "extracted frontend tests must preserve embedded source fixture bytes" >&2
  exit 1
fi

rustfmt --check --edition 2024 scripts/check-frontend-fixture-preservation.rs
echo "frontend fixture-preservation checker formatting passes"

rustc --edition=2024 -D warnings \
  scripts/check-frontend-fixture-preservation.rs \
  -o "$fixture_check_bin"
"$fixture_check_bin" "$repo_root"

echo "compiler module responsibilities, handwritten budgets, and syntax boundary pass"
