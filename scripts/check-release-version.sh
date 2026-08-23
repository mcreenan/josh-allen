#!/usr/bin/env bash
# Keep release tags and shipped package versions on the same 0.1.x version.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

version="$(sed -n 's/^version = "\(0\.1\.[0-9][0-9]*\)"$/\1/p' Cargo.toml | head -n 1)"
[[ "$version" =~ ^0\.1\.[0-9]+$ ]] || {
  printf 'workspace version must match 0.1.x\n' >&2
  exit 1
}

for manifest in \
  crates/allen-testkit/Cargo.toml \
  plugins/josh-allen/.codex-plugin/plugin.json \
  plugins/josh-allen/.claude-plugin/plugin.json \
  .claude-plugin/marketplace.json; do
  grep -Fq "version = \"$version\"" "$manifest" || \
    grep -Fq "\"version\": \"$version\"" "$manifest" || {
      printf '%s does not use version %s\n' "$manifest" "$version" >&2
      exit 1
    }
done

server_version_count="$(grep -Fc "\"version\": \"$version\"" plugins/josh-allen/mcp/server.py)"
[[ "$server_version_count" -eq 2 ]] || {
  printf 'plugins/josh-allen/mcp/server.py does not report version %s consistently\n' \
    "$version" >&2
  exit 1
}

cmp -s \
  docs/agents/reference/allen-language.md \
  plugins/josh-allen/skills/josh-allen/references/allen-language.md || {
    printf 'packaged ALLEN language reference is out of sync\n' >&2
    exit 1
  }

if [[ -n "${1:-}" && "$1" != "v$version" ]]; then
  printf 'release tag %s does not match workspace version %s\n' "$1" "$version" >&2
  exit 1
fi

printf '%s\n' "$version"
