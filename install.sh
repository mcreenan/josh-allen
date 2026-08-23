#!/usr/bin/env bash
# Install prebuilt JOSH/ALLEN binaries and connect them to installed agent hosts.
set -euo pipefail

release_base_url="${JOSH_ALLEN_RELEASE_BASE_URL:-https://github.com/mcreenan/josh-allen/releases/latest/download}"
marketplace_source="mcreenan/josh-allen"
marketplace_name="josh-allen"
plugin_id="josh-allen@josh-allen"
configured_hosts=0

say() {
  printf '%s\n' "$*"
}

fail() {
  printf 'JOSH/ALLEN installer: %s\n' "$*" >&2
  exit 1
}

has_json_value() {
  local field="$1"
  local value="$2"
  grep -Eq "\"${field}\"[[:space:]]*:[[:space:]]*\"${value}\""
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"
command -v python3 >/dev/null 2>&1 || fail "python3 is required by the MCP bridge"

case "$(uname -s)" in
  Darwin)
    case "$(uname -m)" in
      arm64 | aarch64) platform="macos-aarch64" ;;
      *) fail "prebuilt macOS binaries require Apple silicon; install from source on this Mac" ;;
    esac
    ;;
  Linux)
    case "$(uname -m)" in
      x86_64 | amd64) platform="linux-x86_64" ;;
      *) fail "prebuilt Linux binaries require x86_64; install from source on this machine" ;;
    esac
    ;;
  *)
    fail "prebuilt binaries support macOS on Apple silicon and Linux on x86_64"
    ;;
esac

if [[ -n "${JOSH_ALLEN_INSTALL_DIR:-}" ]]; then
  install_dir="$JOSH_ALLEN_INSTALL_DIR"
elif [[ -n "${HOME:-}" ]]; then
  install_dir="$HOME/.local/bin"
else
  fail "HOME is not set; set JOSH_ALLEN_INSTALL_DIR to an absolute directory"
fi

archive="josh-allen-${platform}.tar.gz"
temporary_dir="$(mktemp -d "${TMPDIR:-/tmp}/josh-allen-install.XXXXXX")"

cleanup() {
  rm -rf -- "$temporary_dir"
}
trap cleanup EXIT

say "Downloading the latest JOSH/ALLEN release for ${platform}..."
curl --proto '=https' --tlsv1.2 -fsSL "$release_base_url/$archive" -o "$temporary_dir/$archive"
curl --proto '=https' --tlsv1.2 -fsSL "$release_base_url/SHA256SUMS" -o "$temporary_dir/SHA256SUMS"

expected_checksum="$(awk -v archive="$archive" '$2 == archive { print $1 }' "$temporary_dir/SHA256SUMS")"
[[ "$expected_checksum" =~ ^[0-9a-fA-F]{64}$ ]] || fail "release checksum is missing for $archive"

if command -v sha256sum >/dev/null 2>&1; then
  actual_checksum="$(sha256sum "$temporary_dir/$archive" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
  actual_checksum="$(shasum -a 256 "$temporary_dir/$archive" | awk '{ print $1 }')"
else
  fail "sha256sum or shasum is required to verify the download"
fi

[[ "$actual_checksum" == "$expected_checksum" ]] || fail "checksum verification failed for $archive"

mkdir -p "$temporary_dir/unpacked" "$install_dir"
tar -xzf "$temporary_dir/$archive" -C "$temporary_dir/unpacked"

for binary in allen josh; do
  [[ -f "$temporary_dir/unpacked/$binary" ]] || fail "$archive does not contain $binary"
  install -m 755 "$temporary_dir/unpacked/$binary" "$install_dir/$binary"
done

if command -v codex >/dev/null 2>&1; then
  say "Configuring the JOSH/ALLEN marketplace and plugin for Codex..."
  if ! codex plugin marketplace list --json </dev/null 2>/dev/null | has_json_value name "$marketplace_name"; then
    codex plugin marketplace add "$marketplace_source" </dev/null
  fi
  if ! codex plugin list --json </dev/null 2>/dev/null | has_json_value pluginId "$plugin_id"; then
    codex plugin add "$plugin_id" </dev/null
  fi
  configured_hosts=$((configured_hosts + 1))
fi

if command -v claude >/dev/null 2>&1; then
  say "Configuring the JOSH/ALLEN marketplace and plugin for Claude Code..."
  if ! claude plugin marketplace list --json </dev/null 2>/dev/null | has_json_value name "$marketplace_name"; then
    claude plugin marketplace add "$marketplace_source" </dev/null
  fi
  if ! claude plugin list --json </dev/null 2>/dev/null | has_json_value id "$plugin_id"; then
    claude plugin install "$plugin_id" </dev/null
  fi
  configured_hosts=$((configured_hosts + 1))
fi

say ""
say "Installed allen and josh in $install_dir"

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) say "Add $install_dir to PATH to run allen and josh from a new shell." ;;
esac

if [[ "$configured_hosts" -eq 0 ]]; then
  say "No Codex or Claude Code CLI was found. Run this command again after installing one."
elif [[ "$configured_hosts" -eq 1 ]]; then
  say "Installed the JOSH/ALLEN marketplace, Agent Skill, and MCP server for one agent host."
  say "Restart that agent host before using JOSH/ALLEN."
else
  say "Installed the JOSH/ALLEN marketplace, Agent Skill, and MCP server for Codex and Claude Code."
  say "Restart both agent hosts before using JOSH/ALLEN."
fi
