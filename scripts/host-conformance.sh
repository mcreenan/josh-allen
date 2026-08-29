#!/usr/bin/env bash
# Run the current host-0.1 checks and retain only safe, deterministic evidence.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
evidence_dir="${HOST_CONFORMANCE_EVIDENCE_DIR:-${TMPDIR:-/tmp}/allen-host-conformance}"
fuzz_runs="${HOST_CONFORMANCE_FUZZ_RUNS:-256}"
fuzz_toolchain="${HOST_CONFORMANCE_FUZZ_TOOLCHAIN:-}"
fuzz_rustup_home="${HOST_CONFORMANCE_FUZZ_RUSTUP_HOME:-}"
fuzz_cargo_home="${HOST_CONFORMANCE_FUZZ_CARGO_HOME:-}"
report_path="$repo_root/docs/conformance/host-0.1.json"
error_registry_path="$repo_root/docs/conformance/errors-0.1.json"
summary_path="$evidence_dir/runner-evidence.json"
generated_output_paths="${HOST_CONFORMANCE_GENERATED_OUTPUT_PATHS:-}"
fuzz_target_build_status="not_run"
fuzz_live_status="not_run"
fuzz_live_reason="runner_not_reached"
fuzz_manifests=(
  "fuzz/Cargo.toml"
  "crates/allen-syntax/fuzz/Cargo.toml"
)
fuzz_packages=()
fuzz_target_packages=()
fuzz_target_manifests=()
fuzz_targets=()

for manifest in "${fuzz_manifests[@]}"; do
  manifest_path="$repo_root/$manifest"
  package="$(awk '
    /^\[package\]$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^name = / {
      value = $0
      sub(/^name = "/, "", value)
      sub(/"$/, "", value)
      print value
      exit
    }
  ' "$manifest_path")"
  [[ -n "$package" ]] || {
    printf 'fuzz manifest has no package name: %s\n' "$manifest" >&2
    exit 2
  }
  fuzz_packages+=("$package")

  package_target_count=0
  while IFS= read -r fuzz_target; do
    fuzz_target_packages+=("$package")
    fuzz_target_manifests+=("$manifest")
    fuzz_targets+=("$fuzz_target")
    package_target_count=$((package_target_count + 1))
  done < <(
    awk '
      /^\[\[bin\]\]$/ { in_bin = 1; next }
      in_bin && /^name = / {
        value = $0
        sub(/^name = "/, "", value)
        sub(/"$/, "", value)
        print value
        in_bin = 0
      }
    ' "$manifest_path"
  )
  if (( package_target_count == 0 )); then
    printf 'fuzz manifest has no binary targets: %s\n' "$manifest" >&2
    exit 2
  fi
done

fuzz_inventory_json="$({
  for index in "${!fuzz_targets[@]}"; do
    printf '%s\t%s\t%s\n' \
      "${fuzz_target_packages[$index]}" \
      "${fuzz_target_manifests[$index]}" \
      "${fuzz_targets[$index]}"
  done
} | jq -Rn '
  [inputs | split("\t") | {package: .[0], manifest: .[1], target: .[2]}]
  | group_by([.package, .manifest])
  | map({
      package: .[0].package,
      manifest: .[0].manifest,
      targets: map(.target)
    })
')"

mkdir -p "$evidence_dir"

write_summary() {
  local status="$1"
  jq -n \
    --arg runner_status "$status" \
    --arg build_status "$fuzz_target_build_status" \
    --arg live_status "$fuzz_live_status" \
    --arg live_reason "$fuzz_live_reason" \
    --arg runs_per_target "$fuzz_runs" \
    --argjson fuzz_packages "$fuzz_inventory_json" \
    '{
      profile: "host-0.1",
      runner_status: $runner_status,
      report: "docs/conformance/host-0.1.json",
      fuzz_target_build: {
        status: $build_status,
        method: "ordinary_cargo",
        packages: $fuzz_packages
      },
      fuzz_live_execution: {
        status: $live_status,
        reason: $live_reason,
        runs_per_target: $runs_per_target,
        packages: $fuzz_packages,
        targets: [
          $fuzz_packages[]
          | .package as $package
          | .targets[]
          | "\($package):\(.)"
        ]
      }
    }' > "$summary_path"
}

on_exit() {
  local status=$?
  if [[ $status -eq 0 ]]; then
    write_summary "passed"
  else
    if [[ "$fuzz_target_build_status" == "running" ]]; then
      fuzz_target_build_status="failed"
    fi
    if [[ "$fuzz_live_status" == "running" ]]; then
      fuzz_live_status="failed"
      fuzz_live_reason="live_fuzz_command_failed"
    fi
    write_summary "failed"
  fi
}
trap on_exit EXIT

run_check() {
  local name="$1"
  shift
  local log="$evidence_dir/$name.log"
  if ! "$@" > "$log" 2>&1; then
    cat "$log"
    return 1
  fi
  cat "$log"
}

require_absent() {
  local name="$1"
  local pattern="$2"
  shift 2
  local log="$evidence_dir/$name.log"
  if rg -n --glob '*.rs' -- "$pattern" "$@" > "$log" 2>&1; then
    cat "$log"
    printf 'Host conformance scan %s found a prohibited match.\n' "$name" >&2
    return 1
  else
    local rg_status=$?
    if [[ $rg_status -ne 1 ]]; then
      cat "$log"
      return "$rg_status"
    fi
  fi
  : > "$log"
}

require_absent_except_file() {
  local name="$1"
  local pattern="$2"
  local excluded_file="$3"
  shift 3
  local log="$evidence_dir/$name.log"
  local matches="$log.matches"
  local rg_status
  set +e
  rg -n --glob '*.rs' -- "$pattern" "$@" > "$matches" 2>&1
  rg_status=$?
  set -e
  if [[ $rg_status -gt 1 ]]; then
    cat "$matches"
    rm -f "$matches"
    return "$rg_status"
  fi
  awk -F: -v excluded="$excluded_file" '$1 != excluded' "$matches" > "$log"
  rm -f "$matches"
  if [[ -s "$log" ]]; then
    cat "$log"
    printf 'Host conformance scan %s found a prohibited match.\n' "$name" >&2
    return 1
  fi
  : > "$log"
}

verify_cutover_process_exception() {
  local parent="crates/allen-compiler/src/frontend/syntax_lowering.rs"
  local exception="crates/allen-compiler/src/frontend/syntax_lowering/cutover.rs"
  local declaration_pattern='^#\[cfg\(test\)\]\nmod cutover;$'
  local process_pattern='Command::new|std::process::Command|Child::spawn'
  local declaration_count
  local all_declaration_count

  declaration_count="$(rg -U -c "$declaration_pattern" "$parent" || true)"
  all_declaration_count="$(rg -c '^mod cutover;$' "$parent" || true)"
  declaration_count="${declaration_count:-0}"
  all_declaration_count="${all_declaration_count:-0}"
  [[ "$declaration_count" == 1 && "$all_declaration_count" == 1 ]] || {
    printf 'expected one exact #[cfg(test)] mod cutover declaration in %s\n' "$parent" >&2
    return 1
  }
  [[ -f "$exception" ]] || {
    printf 'cutover process-scan exception is missing: %s\n' "$exception" >&2
    return 1
  }
  rg -n -- "$process_pattern" "$exception"
  printf 'verified test-only process-scan exception: %s\n' "$exception"
}

verify_production_compiler_artifact() {
  local messages="$evidence_dir/production-compiler-build.jsonl"
  local symbols="$evidence_dir/production-compiler-symbols.log"
  local artifact

  cargo build -p allen-compiler --lib --message-format=json-render-diagnostics > "$messages"
  artifact="$(jq -r '
    select(
      .reason == "compiler-artifact"
      and .target.name == "allen_compiler"
      and (.target.kind | index("lib"))
    )
    | .filenames[]
    | select(endswith(".rlib"))
  ' "$messages" | tail -n 1)"
  [[ -n "$artifact" && -f "$artifact" ]] || {
    printf 'normal compiler build did not report an rlib artifact\n' >&2
    return 1
  }
  nm -g "$artifact" > "$symbols"
  if rg -i -n 'cutover|std.*process.*command|process.*command' "$symbols"; then
    printf 'test-only cutover/process symbols reached the normal compiler rlib\n' >&2
    return 1
  fi
  rg -q 'allen_syntax' "$symbols" || {
    printf 'normal compiler rlib has no link edge to allen_syntax\n' >&2
    return 1
  }
  printf 'normal compiler rlib excludes cutover/process symbols and links allen_syntax: %s\n' \
    "$artifact"
}

run_cargo_fuzz() {
  local log_name="$1"
  local fuzz_dir="$2"
  local subcommand="$3"
  shift 3
  if [[ -n "$fuzz_rustup_home" ]]; then
    run_check "$log_name" env \
      "RUSTUP_HOME=$fuzz_rustup_home" \
      "CARGO_HOME=$fuzz_cargo_home" \
      "RUSTUP_TOOLCHAIN=${fuzz_toolchain:-nightly}" \
      cargo fuzz "$subcommand" --fuzz-dir "$fuzz_dir" "$@"
  elif [[ -n "$fuzz_toolchain" ]]; then
    run_check "$log_name" cargo "+$fuzz_toolchain" fuzz \
      "$subcommand" --fuzz-dir "$fuzz_dir" "$@"
  else
    run_check "$log_name" cargo fuzz "$subcommand" --fuzz-dir "$fuzz_dir" "$@"
  fi
}

print_fuzz_inventory() {
  for index in "${!fuzz_targets[@]}"; do
    printf 'package=%s manifest=%s target=%s\n' \
      "${fuzz_target_packages[$index]}" \
      "${fuzz_target_manifests[$index]}" \
      "${fuzz_targets[$index]}"
  done
}

scan_canaries() {
  local log="$evidence_dir/canary-scan.log"
  local -a scan_paths=("$evidence_dir" "$report_path")
  local -a extra_paths=()
  : > "$log"
  # Replay fixtures can append their bounded output paths via a colon-separated list.
  if [[ -n "$generated_output_paths" ]]; then
    local IFS=:
    read -r -a extra_paths <<< "$generated_output_paths"
    scan_paths+=("${extra_paths[@]}")
  fi
  for scan_path in "${scan_paths[@]}"; do
    if [[ ! -e "$scan_path" ]]; then
      printf 'canary scan path does not exist\n' >> "$log"
      return 1
    fi
  done
  while IFS= read -r canary; do
    [[ -z "$canary" ]] && continue
    if rg -F -q -- "$canary" "${scan_paths[@]}"; then
      printf 'canary scan found a protected value in captured or generated output\n' >> "$log"
      return 1
    fi
  done < <(
    rg --no-filename -o --glob '*.rs' '"[[:alnum:]_-]*[Cc][Aa][Nn][Aa][Rr][Yy][[:alnum:]_-]*"' "$repo_root/crates" |
      tr -d '"' |
      awk 'length >= 12' |
      sort -u
  )
}

cd "$repo_root"

run_check format cargo fmt --all --check
run_check clippy cargo clippy --workspace --all-targets --all-features -- -D warnings
run_check workspace-tests cargo test --workspace --all-targets --all-features
run_check workspace-doctests cargo test --workspace --doc --all-features
run_check current-bytecode-tests cargo test -p allen-bytecode --all-targets --all-features
run_check protocol-tests cargo test -p josh-protocol --all-targets --all-features
run_check host-protocol-tests cargo test -p josh-host --all-targets --all-features
run_check stdio-protocol-tests cargo test -p josh --all-targets --all-features
run_check cargo-metadata cargo metadata --format-version 1 --no-deps
run_check duplicate-dependencies cargo tree --workspace --duplicates
run_check conformance-json jq -e . "$report_path"
run_check fuzz-report-claims jq -e '
  (.required.fuzz_target_build == "conforming_stable_build") and
  (.required.fuzz_live_execution == "unverified_optional_cargo_fuzz_unavailable") and
  (.required | has("fuzz_targets") | not)
' "$report_path"
run_check error-registry-json jq -e '
  (.channels == ["diagnostic", "failed", "result", "trap", "stopped"]) and
  (([.registry[] | .code] | length) == ([.registry[] | .code] | unique | length)) and
  ([.registry[] | select(.code == "stopped")] | length == 0) and
  (.outcomes == [{"outcome":"failed","channel":"failed"},{"outcome":"stopped","channel":"stopped"}]) and
  ([.registry[].code] == ([.registry[].code] | sort)) and
  ([.registry[] | select(.code as $code | ["runtime.entry_not_found", "runtime.manifest_invalid", "runtime.capability_denied", "tool.catalog_mismatch", "runtime.invalid_input", "resource.input_bytes", "runtime.workspace_unavailable", "replay.diverged"] | index($code))] | length == 8) and
  ([.registry[] | select(.code as $code | ["runtime.entry_not_found", "runtime.manifest_invalid", "runtime.capability_denied", "tool.catalog_mismatch", "runtime.invalid_input", "resource.input_bytes", "runtime.workspace_unavailable", "replay.diverged"] | index($code)) | select(.channel != "diagnostic" or has("versions"))] | length == 0) and
  ([.operations[] | select((.operation | type) != "string" or (.codes | type) != "array") ] | length == 0) and
  (([.operations[] | .operation] | length) == ([.operations[] | .operation] | unique | length)) and
  ([.operations[] | .operation] == [
    "decode", "fs.read_text", "fs.read_bytes", "fs.write_text", "fs.write_bytes", "fs.list", "fs.search",
    "http.get", "exec.run", "agent.message", "agent.ask", "agent.transcript", "model.request",
    "user.ask", "sub_agent.create", "sub_agent.run", "sub_agent.message", "sub_agent.ask",
    "permission.request_file", "permission.request_directory", "generated_tool.call"
  ]) and
  ([.operations[] | .codes == (.codes | sort)] | all) and
  ([.operations[].codes[] as $code | select([.registry[].code] | index($code) | not)] | length == 0)
' "$error_registry_path"
cp "$report_path" "$evidence_dir/host-0.1.json"
cp "$error_registry_path" "$evidence_dir/errors-0.1.json"

# The language core has no ambient process, socket, or unsafe escape hatch.
core_crates=(
  crates/allen-bytecode
  crates/allen-cli
  crates/allen-compiler
  crates/allen-package
  crates/allen-runtime
  crates/allen-sandbox-fs
  crates/allen-schema
  crates/allen-syntax
  crates/allen-vm
)
language_core_crates=(
  crates/allen-bytecode
  crates/allen-compiler
  crates/allen-package
  crates/allen-runtime
  crates/allen-schema
  crates/allen-syntax
  crates/allen-vm
)
require_absent unsafe-scan '(^|[[:space:]])unsafe([[:space:]]+(fn|impl|trait)|[[:space:]]*\{)' "${core_crates[@]}"
run_check cutover-test-only-process-exception verify_cutover_process_exception
# The exact exception above is safe only while both the declaration gate and
# the normal-build symbol/link inspection remain green.
require_absent_except_file process-scan \
  'Command::new|std::process::Command|Child::spawn' \
  'crates/allen-compiler/src/frontend/syntax_lowering/cutover.rs' \
  "${language_core_crates[@]}"
run_check production-compiler-artifact verify_production_compiler_artifact
require_absent socket-scan 'TcpStream|TcpListener|UdpSocket|UnixStream|UnixListener' "${language_core_crates[@]}"
require_absent deferred-feature-scan 'TODO|FIXME|unimplemented!' "${core_crates[@]}"
run_check worker-process-audit rg -n 'ProcessCommand::new' crates/allen-cli/src/app.rs

fuzz_target_build_status="running"
run_check fuzz-target-inventory print_fuzz_inventory
for index in "${!fuzz_manifests[@]}"; do
  run_check "fuzz-build-${fuzz_packages[$index]}" cargo check \
    --manifest-path "${fuzz_manifests[$index]}" --bins
done
fuzz_target_build_status="conforming_stable_build"

if command -v cargo-fuzz > /dev/null 2>&1; then
  fuzz_live_status="running"
  fuzz_live_reason="live_fuzz_in_progress"
  for index in "${!fuzz_manifests[@]}"; do
    run_cargo_fuzz \
      "cargo-fuzz-build-${fuzz_packages[$index]}" \
      "$(dirname "${fuzz_manifests[$index]}")" \
      build
  done
  for index in "${!fuzz_targets[@]}"; do
    run_cargo_fuzz \
      "cargo-fuzz-${fuzz_target_packages[$index]}-${fuzz_targets[$index]}" \
      "$(dirname "${fuzz_target_manifests[$index]}")" \
      run "${fuzz_targets[$index]}" -- -runs="$fuzz_runs"
  done
  fuzz_live_status="verified_configured_runs_each"
  fuzz_live_reason="completed"
else
  fuzz_live_status="unverified_optional_cargo_fuzz_unavailable"
  fuzz_live_reason="cargo_fuzz_not_installed"
  {
    printf 'skipped: cargo-fuzz is not installed\n'
    printf 'ordinary Cargo built every enumerated package target:\n'
    print_fuzz_inventory
  } > "$evidence_dir/fuzz-smoke.log"
fi

scan_canaries
run_check diff-check git diff --check
