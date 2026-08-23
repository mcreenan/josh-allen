#!/usr/bin/env bash
# Run the mandatory Codex-to-JOSH Chunk -1 boundary gate.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
evidence_dir="${CODEX_CAPABILITY_EVIDENCE_DIR:-${TMPDIR:-/tmp}/allen-codex-capability-spike}"
codex_binary="${CODEX_CAPABILITY_CODEX_BIN:-$(command -v codex || true)}"
trace_path="$evidence_dir/trace.json"

if [[ -z "$codex_binary" ]]; then
  printf 'codex binary was not found\n' >&2
  exit 2
fi

mkdir -p "$evidence_dir"

set +e
python3 "$repo_root/tools/codex-capability-spike/capability_spike.py" \
  --codex "$codex_binary" \
  --output "$trace_path"
probe_status=$?
set -e

if [[ -f "$trace_path" ]]; then
  cat "$trace_path"
fi

case "$probe_status" in
  0)
    printf 'Chunk -1 gate passed with live adversarial evidence.\n'
    ;;
  3)
    printf 'Chunk -1 is blocked; do not change ALLEN or JOSH integration contracts.\n' >&2
    ;;
  *)
    printf 'Chunk -1 probe could not inspect the host.\n' >&2
    ;;
esac

exit "$probe_status"
