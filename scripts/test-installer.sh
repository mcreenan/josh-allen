#!/usr/bin/env bash
# Exercise install.sh without changing real binaries or agent configuration.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/josh-allen-installer.XXXXXX")"
fake_bin="$test_root/bin"
release_dir="$test_root/release"
payload_dir="$test_root/payload"
install_dir="$test_root/installed"
test_log="$test_root/install.log"
test_state="$test_root/state"
test_version="$($repo_root/scripts/check-release-version.sh)"

cleanup() {
  rm -rf -- "$test_root"
}
trap cleanup EXIT

mkdir -p "$fake_bin" "$release_dir" "$payload_dir" "$test_state"

cat >"$payload_dir/allen" <<'EOF'
#!/usr/bin/env bash
printf 'fake allen\n'
EOF

cat >"$payload_dir/josh" <<'EOF'
#!/usr/bin/env bash
printf 'fake josh\n'
EOF

printf '%s\n' "$test_version" >"$payload_dir/VERSION"
chmod +x "$payload_dir/allen" "$payload_dir/josh"
tar -czf "$release_dir/josh-allen-macos-aarch64.tar.gz" -C "$payload_dir" allen josh VERSION
if command -v sha256sum >/dev/null 2>&1; then
  archive_checksum="$(sha256sum "$release_dir/josh-allen-macos-aarch64.tar.gz" | awk '{ print $1 }')"
else
  archive_checksum="$(shasum -a 256 "$release_dir/josh-allen-macos-aarch64.tar.gz" | awk '{ print $1 }')"
fi
printf '%s  %s\n' "$archive_checksum" "josh-allen-macos-aarch64.tar.gz" >"$release_dir/SHA256SUMS"

cat >"$fake_bin/curl" <<'EOF'
#!/usr/bin/env bash
output=""
url=""
while [[ "$#" -gt 0 ]]; do
  case "$1" in
    -o)
      output="$2"
      shift 2
      ;;
    http://* | https://*)
      url="$1"
      shift
      ;;
    *)
      shift
      ;;
  esac
done
[[ -n "$output" && -n "$url" ]]
cp "$INSTALL_TEST_RELEASE_DIR/${url##*/}" "$output"
EOF

cat >"$fake_bin/uname" <<'EOF'
#!/usr/bin/env bash
case "${1:-}" in
  -s) printf 'Darwin\n' ;;
  -m) printf 'arm64\n' ;;
  *) exit 1 ;;
esac
EOF

cat >"$fake_bin/python3" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF

cat >"$fake_bin/codex" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  "plugin marketplace list --json")
    if [[ -f "$INSTALL_TEST_STATE/codex-marketplace" ]]; then
      printf '{"marketplaces":[{"name":"josh-allen"}]}\n'
    else
      printf '{"marketplaces":[]}\n'
    fi
    ;;
  "plugin marketplace add mcreenan/allen")
    touch "$INSTALL_TEST_STATE/codex-marketplace"
    printf 'codex add-marketplace\n' >>"$INSTALL_TEST_LOG"
    ;;
  "plugin list --json")
    if [[ -f "$INSTALL_TEST_STATE/codex-plugin" ]]; then
      printf '{"installed":[{"pluginId":"josh-allen@josh-allen"}]}\n'
    else
      printf '{"installed":[]}\n'
    fi
    ;;
  "plugin add josh-allen@josh-allen")
    touch "$INSTALL_TEST_STATE/codex-plugin"
    printf 'codex add-plugin\n' >>"$INSTALL_TEST_LOG"
    ;;
  *)
    printf 'unexpected codex arguments: %s\n' "$*" >&2
    exit 1
    ;;
esac
EOF

cat >"$fake_bin/claude" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  "plugin marketplace list --json")
    if [[ -f "$INSTALL_TEST_STATE/claude-marketplace" ]]; then
      printf '[{"name":"josh-allen"}]\n'
    else
      printf '[]\n'
    fi
    ;;
  "plugin marketplace add mcreenan/allen")
    touch "$INSTALL_TEST_STATE/claude-marketplace"
    printf 'claude add-marketplace\n' >>"$INSTALL_TEST_LOG"
    ;;
  "plugin list --json")
    if [[ -f "$INSTALL_TEST_STATE/claude-plugin" ]]; then
      printf '[{"id":"josh-allen@josh-allen"}]\n'
    else
      printf '[]\n'
    fi
    ;;
  "plugin install josh-allen@josh-allen")
    touch "$INSTALL_TEST_STATE/claude-plugin"
    printf 'claude add-plugin\n' >>"$INSTALL_TEST_LOG"
    ;;
  *)
    printf 'unexpected claude arguments: %s\n' "$*" >&2
    exit 1
    ;;
esac
EOF

chmod +x "$fake_bin/curl" "$fake_bin/uname" "$fake_bin/python3" "$fake_bin/codex" "$fake_bin/claude"

run_installer() {
  PATH="$fake_bin:/usr/bin:/bin" \
    JOSH_ALLEN_INSTALL_DIR="$install_dir" \
    JOSH_ALLEN_RELEASE_BASE_URL="https://releases.example.test" \
    INSTALL_TEST_RELEASE_DIR="$release_dir" \
    INSTALL_TEST_LOG="$test_log" \
    INSTALL_TEST_STATE="$test_state" \
    bash "$repo_root/install.sh"
}

run_piped_installer() {
  PATH="$fake_bin:/usr/bin:/bin" \
    JOSH_ALLEN_INSTALL_DIR="$install_dir" \
    JOSH_ALLEN_RELEASE_BASE_URL="https://releases.example.test" \
    INSTALL_TEST_RELEASE_DIR="$release_dir" \
    INSTALL_TEST_LOG="$test_log" \
    INSTALL_TEST_STATE="$test_state" \
    bash <"$repo_root/install.sh"
}

first_output="$(run_piped_installer)"
second_output="$(run_installer)"

grep -Fq "Installed allen and josh in $install_dir" <<<"$first_output"
grep -Fq "for Codex and Claude Code" <<<"$first_output"
grep -Fq "Installed allen and josh in $install_dir" <<<"$second_output"
[[ "$($install_dir/allen)" == "fake allen" ]]
[[ "$($install_dir/josh)" == "fake josh" ]]

[[ "$(grep -c '^codex add-marketplace$' "$test_log")" -eq 1 ]]
[[ "$(grep -c '^codex add-plugin$' "$test_log")" -eq 1 ]]
[[ "$(grep -c '^claude add-marketplace$' "$test_log")" -eq 1 ]]
[[ "$(grep -c '^claude add-plugin$' "$test_log")" -eq 1 ]]

printf 'installer smoke test passed\n'
