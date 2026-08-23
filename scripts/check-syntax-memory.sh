#!/usr/bin/env bash
# Enforce the Phase 22 syntax peak-RSS allowance on representative sparse and
# dense concrete trees. A fresh Python child reports Darwin's byte-valued
# RUSAGE_CHILDREN peak RSS without relying on sandbox-blocked sysctl queries.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
benchmark="$repo_root/target/release/examples/syntax_bench"
rss_runner="$repo_root/scripts/measure-peak-rss.py"
evidence_dir="${SYNTAX_MEMORY_EVIDENCE_DIR:-${TMPDIR:-/tmp}/allen-syntax-memory}"

# Dense measurements establish a 72 RSS-byte baseline per concrete node/token
# after subtracting twice the source and the empty process. The one-third safety
# margin applies both to the cap-derived tree allowance and to the empirical
# dense-fixture canary. The benchmark reports the source-of-truth
# SyntaxLimits::DEFAULT token and node caps. This gate verifies that the frozen
# values have not drifted before deriving the allowance.
expected_token_cap=1048576
expected_node_cap=2097152
measured_bytes_per_element_baseline=72
margin_numerator=1
margin_denominator=3
margin_adjusted_bytes_per_element_ceiling=$((
  measured_bytes_per_element_baseline
    + measured_bytes_per_element_baseline * margin_numerator / margin_denominator
))

if [[ "$(uname -s)" != "Darwin" ]]; then
  printf 'syntax memory RSS gate requires Darwin byte-valued ru_maxrss\n' >&2
  exit 1
fi
mkdir -p "$evidence_dir"
cd "$repo_root"
cargo build -p allen-syntax --release --example syntax_bench

measure() {
  local mode="$1"
  local output="$evidence_dir/$mode.out"
  local timing="$evidence_dir/$mode.time"
  python3 "$rss_runner" "$benchmark" "$mode" > "$output" 2> "$timing"
  cat "$output"
  cat "$timing"
  measured_source_bytes="$(sed -n 's/.*source_bytes=\([0-9][0-9]*\).*/\1/p' "$output")"
  measured_nodes="$(sed -n 's/.*nodes=\([0-9][0-9]*\).*/\1/p' "$output")"
  measured_tokens="$(sed -n 's/.*tokens=\([0-9][0-9]*\).*/\1/p' "$output")"
  measured_token_cap="$(sed -n 's/.*token_cap=\([0-9][0-9]*\).*/\1/p' "$output")"
  measured_node_cap="$(sed -n 's/.*node_cap=\([0-9][0-9]*\).*/\1/p' "$output")"
  measured_rss="$(awk '/maximum resident set size/ { print $1 }' "$timing")"
  if [[ -z "$measured_source_bytes" || -z "$measured_nodes" || -z "$measured_tokens" || -z "$measured_token_cap" || -z "$measured_node_cap" || -z "$measured_rss" ]]; then
    printf 'incomplete memory evidence for %s\n' "$mode" >&2
    exit 1
  fi
}

measure memory-empty
empty_rss="$measured_rss"
token_cap="$measured_token_cap"
node_cap="$measured_node_cap"
if (( token_cap != expected_token_cap || node_cap != expected_node_cap )); then
  printf 'frozen syntax limits drifted: token_cap=%d expected_token_cap=%d node_cap=%d expected_node_cap=%d\n' \
    "$token_cap" "$expected_token_cap" "$node_cap" "$expected_node_cap" >&2
  exit 1
fi
derived_dense_allowance=$(((token_cap + node_cap) * measured_bytes_per_element_baseline))
fixed_tree_allowance=$((
  derived_dense_allowance + derived_dense_allowance * margin_numerator / margin_denominator
))

for mode in memory-max memory-dense-newlines memory-dense-tree; do
  measure "$mode"
  if (( measured_token_cap != token_cap || measured_node_cap != node_cap )); then
    printf 'syntax limit evidence changed between fixtures: mode=%s token_cap=%d node_cap=%d\n' \
      "$mode" "$measured_token_cap" "$measured_node_cap" >&2
    exit 1
  fi
  if (( measured_rss < empty_rss )); then
    printf 'memory RSS for %s is below the empty process\n' "$mode" >&2
    exit 1
  fi
  rss_delta=$((measured_rss - empty_rss))
  allowance=$((2 * measured_source_bytes + fixed_tree_allowance))
  tree_overhead=$((rss_delta - 2 * measured_source_bytes))
  syntax_elements=$((measured_nodes + measured_tokens))
  observed_bytes_per_element=$(((tree_overhead + syntax_elements - 1) / syntax_elements))
  if (( rss_delta > allowance )); then
    printf 'syntax memory bound exceeded for %s: delta=%d allowance=%d\n' "$mode" "$rss_delta" "$allowance" >&2
    exit 1
  fi
  printf 'syntax-memory mode=%s source_bytes=%d nodes=%d tokens=%d token_cap=%d node_cap=%d rss=%d empty_rss=%d delta=%d tree_overhead=%d observed_bytes_per_element=%d allowance=%d\n' \
    "$mode" "$measured_source_bytes" "$measured_nodes" "$measured_tokens" \
    "$token_cap" "$node_cap" "$measured_rss" "$empty_rss" "$rss_delta" "$tree_overhead" \
    "$observed_bytes_per_element" "$allowance"

  if [[ "$mode" == "memory-dense-newlines" ]] && (( measured_tokens < 900000 )); then
    printf 'dense-newline fixture no longer exercises the token cap\n' >&2
    exit 1
  fi
  if [[ "$mode" == memory-dense-* ]] && (( observed_bytes_per_element > margin_adjusted_bytes_per_element_ceiling )); then
    printf 'dense fixture exceeds margin-adjusted bytes-per-element ceiling: mode=%s observed=%d baseline=%d ceiling=%d\n' \
      "$mode" "$observed_bytes_per_element" "$measured_bytes_per_element_baseline" \
      "$margin_adjusted_bytes_per_element_ceiling" >&2
    exit 1
  fi
  if [[ "$mode" == "memory-dense-tree" ]] && (( measured_nodes < 600000 || measured_tokens < 600000 )); then
    printf 'dense-tree fixture no longer exercises high node/token density\n' >&2
    exit 1
  fi
done

printf 'syntax memory gate passed: token_cap=%d node_cap=%d bytes_per_element_baseline=%d margin_adjusted_bytes_per_element_ceiling=%d derived_dense_allowance=%d margin_numerator=%d margin_denominator=%d fixed_tree_allowance=%d\n' \
  "$token_cap" "$node_cap" "$measured_bytes_per_element_baseline" \
  "$margin_adjusted_bytes_per_element_ceiling" "$derived_dense_allowance" \
  "$margin_numerator" "$margin_denominator" "$fixed_tree_allowance"
