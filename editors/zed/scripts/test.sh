#!/usr/bin/env bash
set -euo pipefail

zed_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
grammar_dir="$zed_dir/tree-sitter-allen"
fixture_dir="$zed_dir/../vscode/fixtures"

(
  cd "$grammar_dir"
  tree-sitter generate
  tree-sitter build
)

valid_fixtures=(
  comments.allen
  control-flow-and-errors.allen
  current.allen
  l1-language.allen
  l2-language.allen
  l3-language.allen
  no-exception-keywords.allen
  operators.allen
  spec-preview.allen
  template-resources.allen
  template-strings.allen
)

for fixture in "${valid_fixtures[@]}"; do
  tree-sitter parse --quiet \
    --lib-path "$grammar_dir/allen.so" \
    --lang-name allen \
    "$fixture_dir/$fixture"
done

for query in highlights indents brackets outline overrides; do
  tree-sitter query "$zed_dir/languages/allen/$query.scm" \
    "$fixture_dir/current.allen" \
    --lib-path "$grammar_dir/allen.so" \
    --lang-name allen \
    >/dev/null
done
