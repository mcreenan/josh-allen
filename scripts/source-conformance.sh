#!/usr/bin/env bash
# Run the current source-language acceptance checks without writing tracked files.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ $# -gt 1 ]]; then
  printf 'usage: %s [evidence-directory]\n' "$0" >&2
  exit 2
fi
evidence_dir="${1:-${SOURCE_CONFORMANCE_EVIDENCE_DIR:-${TMPDIR:-/tmp}/allen-source-conformance}}"
logs_dir="$evidence_dir/logs"
artifacts_dir="$evidence_dir/artifacts"
docs_dir="$evidence_dir/docs"
summary_path="$evidence_dir/summary.json"
max_log_bytes="${SOURCE_CONFORMANCE_MAX_LOG_BYTES:-65536}"
max_doc_example_bytes="${SOURCE_CONFORMANCE_MAX_DOC_EXAMPLE_BYTES:-262144}"
allen_bin="${SOURCE_CONFORMANCE_ALLEN_BIN:-$repo_root/target/debug/allen}"
current_bytecode_version=13

mkdir -p "$logs_dir" "$artifacts_dir" "$docs_dir"

if ! [[ "$max_log_bytes" =~ ^[1-9][0-9]*$ ]]; then
  printf 'SOURCE_CONFORMANCE_MAX_LOG_BYTES must be a positive decimal integer\n' >&2
  exit 2
fi
if ! [[ "$max_doc_example_bytes" =~ ^[1-9][0-9]*$ ]]; then
  printf 'SOURCE_CONFORMANCE_MAX_DOC_EXAMPLE_BYTES must be a positive decimal integer\n' >&2
  exit 2
fi

write_summary() {
  local status="$1"
  printf '{"profile":"source-conformance","status":"%s","example_allen_files":79,"rosetta_programs":40,"comment_modes":4,"control_modes":4,"loop_modes":4,"operator_modes":4,"string_modes":4,"closed_error_model":true}\n' "$status" > "$summary_path"
}

on_exit() {
  local status=$?
  if [[ $status -eq 0 ]]; then
    write_summary "passed"
  else
    write_summary "failed"
  fi
}
trap on_exit EXIT

bounded_log() {
  local path="$1"
  local temporary="$path.tmp"
  if [[ -f "$temporary" ]]; then
    head -c "$max_log_bytes" "$temporary" > "$path"
    rm -f "$temporary"
  else
    : > "$path"
  fi
}

run_check() {
  local name="$1"
  shift
  local log="$logs_dir/$name.log"
  if ! "$@" > "$log.tmp" 2>&1; then
    bounded_log "$log"
    cat "$log"
    return 1
  fi
  bounded_log "$log"
}

run_capture() {
  local path="$1"
  shift
  local stdout="$path.stdout"
  local stderr="$path.stderr"
  local status="$path.status"
  set +e
  "$@" > "$stdout.tmp" 2> "$stderr.tmp"
  local command_status=$?
  set -e
  printf '%s\n' "$command_status" > "$status"
  for stream in stdout stderr; do
    local temporary="$path.$stream.tmp"
    local retained="$path.$stream"
    local bytes
    bytes="$(wc -c < "$temporary")"
    if (( bytes > max_log_bytes )); then
      head -c "$max_log_bytes" "$temporary" > "$retained"
      rm -f "$temporary"
      printf '%s output exceeds the %s-byte evidence bound: %s\n' \
        "$stream" "$max_log_bytes" "$path" >&2
      return 1
    fi
    mv "$temporary" "$retained"
  done
}

assert_artifact_bound() {
  local artifact="$1"
  local bytes
  bytes="$(wc -c < "$artifact")"
  if (( bytes > 16 * 1024 * 1024 )); then
    printf 'artifact exceeds the 16 MiB worker bound: %s\n' "$artifact" >&2
    return 1
  fi
}

sha256_file() {
  local path="$1"
  if command -v shasum > /dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
  elif command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  else
    printf 'no SHA-256 command is available (need shasum or sha256sum)\n' >&2
    return 127
  fi
}

write_non_log_inventory() {
  local inventory="$evidence_dir/non-log-artifacts.sha256"
  local entries="$inventory.entries.tmp"
  local temporary="$inventory.tmp"
  local digest
  local count

  # The labels are always relative to the caller-selected evidence root, so an
  # identical run in a different temporary directory has identical evidence.
  {
    while IFS= read -r -d '' relative_path; do
      printf '%s  %s\n' "$(sha256_file "$evidence_dir/$relative_path")" "$relative_path"
    done < <(
      cd "$evidence_dir"
      find artifacts docs -type f -print0 | LC_ALL=C sort -z
    )
  } > "$entries"
  digest="$(sha256_file "$entries")"
  count="$(wc -l < "$entries" | tr -d '[:space:]')"
  {
    printf 'algorithm: sha256\n'
    printf 'scope: artifacts/** and docs/**; logs/** and runner metadata excluded\n'
    printf 'inventory_sha256: %s\n' "$digest"
    printf 'entries: %s\n' "$count"
    cat "$entries"
  } > "$temporary"
  mv "$temporary" "$inventory"
  rm -f "$entries"
  printf 'Source conformance non-log inventory: %s entries, sha256:%s\n' "$count" "$digest"
}

check_and_compare_source_artifact() {
  local name="$1"
  local source="$2"
  shift 2
  local -a run_options=("$@")
  local first="$artifacts_dir/$name.first.allenb"
  local second="$artifacts_dir/$name.second.allenb"

  run_check "$name.check" "$allen_bin" check "$source"
  run_check "$name.build-first" "$allen_bin" build "$source" -o "$first"
  run_check "$name.build-second" "$allen_bin" build "$source" -o "$second"
  assert_artifact_bound "$first"
  assert_artifact_bound "$second"
  cmp -s "$first" "$second" || {
    printf 'repeated build differs for %s\n' "$source" >&2
    return 1
  }
  run_check "$name.inspect" "$allen_bin" inspect "$first"
  rg -q "^bytecode_version: $current_bytecode_version$" "$logs_dir/$name.inspect.log" || {
    printf 'expected current version %s artifact for %s\n' "$current_bytecode_version" "$source" >&2
    return 1
  }

  run_capture "$logs_dir/$name.source-run" "$allen_bin" run "${run_options[@]}" "$source"
  run_capture "$logs_dir/$name.artifact-run" "$allen_bin" run "${run_options[@]}" "$first"
  cmp -s "$logs_dir/$name.source-run.status" "$logs_dir/$name.artifact-run.status" || {
    printf 'source/artifact exit status differs for %s\n' "$source" >&2
    return 1
  }
  cmp -s "$logs_dir/$name.source-run.stdout" "$logs_dir/$name.artifact-run.stdout" || {
    printf 'source/artifact stdout differs for %s\n' "$source" >&2
    return 1
  }
  local runtime_status
  runtime_status="$(< "$logs_dir/$name.source-run.status")"
  if [[ "$runtime_status" == 0 ]]; then
    cmp -s "$logs_dir/$name.source-run.stderr" "$logs_dir/$name.artifact-run.stderr" || {
      printf 'source/artifact stderr differs for %s\n' "$source" >&2
      return 1
    }
  else
    local source_error artifact_error
    source_error="$(sed -n '1p' "$logs_dir/$name.source-run.stderr")"
    artifact_error="$(sed -n '1p' "$logs_dir/$name.artifact-run.stderr")"
    [[ -n "$source_error" && "$source_error" == "$artifact_error" ]] || {
      printf 'source/artifact runtime failure code or message differs for %s\n' "$source" >&2
      return 1
    }
  fi
}

check_comment_conformance() {
  local fixture_root="$repo_root/crates/allen-cli/tests/fixtures/comments/parity"
  local seed_root="$repo_root/fuzz/seeds/parser"
  local package_workspace="$artifacts_dir/comments-package-workspace"
  mkdir -p "$package_workspace"
  cp -R "$fixture_root/package-commented/." "$package_workspace"

  # The focused CLI test compares comment-free and comment-bearing decoded artifacts
  # after excluding debug spans; the checks below retain source/artifact execution
  # parity for every CLI source mode in caller-selected bounded evidence.
  run_check comments-cli-tests cargo test -p allen-cli --test cli comments_ -- --nocapture
  check_and_compare_source_artifact \
    comments-loose-comments "$fixture_root/loose-commented.allen"
  check_and_compare_source_artifact \
    comments-modules-comments "$fixture_root/modules-commented/main.allen"
  check_and_compare_source_artifact \
    comments-inline-comments "$fixture_root/inline-commented.allen"
  run_check comments-package-lock "$allen_bin" lock "$package_workspace"
  check_and_compare_source_artifact comments-package-comments "$package_workspace"

  {
    for seed in \
      comment-delimiter-interleavings \
      invalid-utf8 \
      comment-literals-adjacent \
      comment-max-nesting \
      comment-over-nesting; do
      local path="$seed_root/$seed"
      [[ -s "$path" ]] || {
        printf 'missing required comment parser seed: %s\n' "$path" >&2
        return 1
      }
      printf '%s %s\n' "$seed" "$(wc -c < "$path")"
    done
  } > "$logs_dir/comments-fuzz-seeds.log.tmp"
  bounded_log "$logs_dir/comments-fuzz-seeds.log"
}

check_control_flow_conformance() {
  local fixture_root="$repo_root/crates/allen-cli/tests/fixtures/control-flow"
  local seed_root="$repo_root/fuzz/seeds/parser"
  local package_workspace="$artifacts_dir/control-flow-package-workspace"
  mkdir -p "$package_workspace"
  cp -R "$fixture_root/parity/package/." "$package_workspace"

  run_check control-flow-cli-tests cargo test -p allen-cli --test cli control_flow_ -- --nocapture
  check_and_compare_source_artifact \
    control-flow-loose-control "$fixture_root/parity/loose.allen"
  check_and_compare_source_artifact \
    control-flow-modules-control "$fixture_root/parity/modules/main.allen"
  check_and_compare_source_artifact \
    control-flow-inline-control "$fixture_root/parity/inline.allen"
  run_check control-flow-package-lock "$allen_bin" lock "$package_workspace"
  check_and_compare_source_artifact control-flow-package-control "$package_workspace"

  {
    for seed in \
      else-if-comments \
      nested-control \
      return-and-truncated; do
      local path="$seed_root/$seed"
      [[ -s "$path" ]] || {
        printf 'missing required control-flow parser seed: %s\n' "$path" >&2
        return 1
      }
      printf '%s %s\n' "$seed" "$(wc -c < "$path")"
    done
  } > "$logs_dir/control-flow-fuzz-seeds.log.tmp"
  bounded_log "$logs_dir/control-flow-fuzz-seeds.log"
}

check_loop_conformance() {
  local fixture_root="$repo_root/crates/allen-cli/tests/fixtures/loops"
  local seed_root="$repo_root/fuzz/seeds/parser"
  local package_workspace="$artifacts_dir/loops-package-workspace"
  mkdir -p "$package_workspace"
  cp -R "$fixture_root/parity/package/." "$package_workspace"

  run_check loops-cli-tests cargo test -p allen-cli --test cli loops_ -- --nocapture
  check_and_compare_source_artifact \
    loops-loose-loops "$fixture_root/parity/loose.allen"
  check_and_compare_source_artifact \
    loops-modules-loops "$fixture_root/parity/modules/main.allen"
  check_and_compare_source_artifact \
    loops-inline-loops "$fixture_root/parity/inline.allen"
  run_check loops-package-lock "$allen_bin" lock "$package_workspace"
  check_and_compare_source_artifact loops-package-loops "$package_workspace"
  check_and_compare_source_artifact \
    loops-loop-behavior "$fixture_root/loop-behavior.allen"

  run_capture "$logs_dir/loops-trace-first" \
    "$allen_bin" run --trace-tasks "$fixture_root/loop-behavior.allen"
  run_capture "$logs_dir/loops-trace-second" \
    "$allen_bin" run --trace-tasks "$fixture_root/loop-behavior.allen"
  for suffix in status stdout stderr; do
    cmp -s \
      "$logs_dir/loops-trace-first.$suffix" \
      "$logs_dir/loops-trace-second.$suffix" || {
      printf 'repeated task trace differs in %s\n' "$suffix" >&2
      return 1
    }
  done

  {
    for seed in \
      loops-and-bindings \
      range-comments \
      truncated-loop-control; do
      local path="$seed_root/$seed"
      [[ -s "$path" ]] || {
        printf 'missing required loop parser seed: %s\n' "$path" >&2
        return 1
      }
      printf '%s %s\n' "$seed" "$(wc -c < "$path")"
    done
  } > "$logs_dir/loops-fuzz-seeds.log.tmp"
  bounded_log "$logs_dir/loops-fuzz-seeds.log"
}

check_operator_conformance() {
  local fixture_root="$repo_root/crates/allen-cli/tests/fixtures/operators/parity"
  local seed_root="$repo_root/fuzz/seeds/parser"
  local package_workspace="$artifacts_dir/operators-package-workspace"
  mkdir -p "$package_workspace"
  cp -R "$fixture_root/package/." "$package_workspace"

  run_check operators-cli-tests cargo test -p allen-cli --test cli operators_ -- --nocapture
  check_and_compare_source_artifact \
    operators-loose-operators "$fixture_root/loose.allen"
  check_and_compare_source_artifact \
    operators-modules-operators "$fixture_root/modules/main.allen"
  check_and_compare_source_artifact \
    operators-inline-operators "$fixture_root/inline.allen"
  run_check operators-package-lock "$allen_bin" lock "$package_workspace"
  check_and_compare_source_artifact operators-package-operators "$package_workspace"

  {
    for seed in \
      operators-and-precedence \
      malformed-operator-truncated; do
      local path="$seed_root/$seed"
      [[ -s "$path" ]] || {
        printf 'missing required operator parser seed: %s\n' "$path" >&2
        return 1
      }
      printf '%s %s\n' "$seed" "$(wc -c < "$path")"
    done
  } > "$logs_dir/operators-fuzz-seeds.log.tmp"
  bounded_log "$logs_dir/operators-fuzz-seeds.log"
}

check_string_conformance() {
  local fixture_root="$repo_root/crates/allen-cli/tests/fixtures/strings/parity"
  local seed_root="$repo_root/fuzz/seeds/parser"
  local package_workspace="$artifacts_dir/strings-package-workspace"
  mkdir -p "$package_workspace"
  cp -R "$fixture_root/package/." "$package_workspace"

  run_check strings-cli-tests cargo test -p allen-cli --test cli strings_ -- --nocapture
  check_and_compare_source_artifact \
    strings-loose-strings "$fixture_root/loose.allen"
  check_and_compare_source_artifact \
    strings-modules-strings "$fixture_root/modules/main.allen"
  check_and_compare_source_artifact \
    strings-inline-strings "$fixture_root/inline.allen"
  run_check strings-package-lock "$allen_bin" lock "$package_workspace"
  check_and_compare_source_artifact strings-package-strings "$package_workspace"

  {
    for seed in \
      templates-and-unicode \
      template-truncated; do
      local path="$seed_root/$seed"
      [[ -s "$path" ]] || {
        printf 'missing required String parser seed: %s\n' "$path" >&2
        return 1
      }
      printf '%s %s\n' "$seed" "$(wc -c < "$path")"
    done
  } > "$logs_dir/strings-fuzz-seeds.log.tmp"
  bounded_log "$logs_dir/strings-fuzz-seeds.log"
}

validate_agent_docs() {
  local links_log="$logs_dir/agent-links.log"
  local fences_log="$logs_dir/agent-fences.log"
  local generated="$docs_dir/fenced-main"
  mkdir -p "$generated"
  find "$generated" -mindepth 1 -maxdepth 1 -type f -name '*.allen' -delete

  if ! perl -MFile::Basename=dirname -MFile::Spec -e '
    my $failed = 0;
    for my $file (@ARGV) {
      open my $input, "<", $file or die "$file: $!\n";
      while (my $line = <$input>) {
        while ($line =~ /\[[^\]]+\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g) {
          my $target = $1;
          next if $target =~ m{^(?:https?:|mailto:|data:|#)};
          $target =~ s/#.*$//;
          next if $target eq q{};
          my $path = File::Spec->catfile(dirname($file), $target);
          if (!-e $path) {
            print "$file: missing relative link $target\n";
            $failed = 1;
          }
        }
      }
    }
    exit $failed;
  ' $(find "$repo_root/docs/agents" -type f -name '*.md' | sort) > "$links_log.tmp" 2>&1; then
    bounded_log "$links_log"
    cat "$links_log"
    return 1
  fi
  bounded_log "$links_log"

  : > "$fences_log"
  while IFS= read -r document; do
    local count
    count="$(grep -c '^```' "$document" || true)"
    if (( count % 2 != 0 )); then
      printf '%s: unbalanced fenced code block\n' "$document" >> "$fences_log"
    fi
  done < <(find "$repo_root/docs/agents" -type f -name '*.md' | sort)
  if [[ -s "$fences_log" ]]; then
    cat "$fences_log"
    return 1
  fi

  awk -v output="$generated" '
    function flush() {
      if (active && code ~ /(^|\n)(export )?(async )?fn main\(/) {
        count += 1;
        path = sprintf("%s/%03d.allen", output, count);
        print code > path;
        close(path);
      }
      code = "";
    }
    /^```allen[[:space:]]*$/ { flush(); active = 1; next }
    /^```[[:space:]]*$/ { flush(); active = 0; next }
    active { code = code $0 "\n" }
    END { flush() }
  ' $(find "$repo_root/docs/agents" -type f -name '*.md' | sort)

  while IFS= read -r example; do
    if (( $(wc -c < "$example") > max_doc_example_bytes )); then
      printf 'fenced ALLEN example exceeds the bounded evidence limit: %s\n' "$example" >&2
      return 1
    fi
    run_check "agent-fence-$(basename "$example" .allen)" "$allen_bin" check "$example"
  done < <(find "$generated" -type f -name '*.allen' | sort)
}

cd "$repo_root"
run_check build-cli cargo build -p allen-cli --bin allen
[[ -x "$allen_bin" ]] || {
  printf 'ALLEN CLI binary is unavailable: %s\n' "$allen_bin" >&2
  exit 1
}

# This behavior check is intentionally separate from a source scan: token text must not route
# a program through a different grammar or artifact layout.
run_check frontend-cli-tests cargo test -p allen-cli --test cli frontend_ -- --nocapture
check_comment_conformance
check_control_flow_conformance
check_loop_conformance
check_operator_conformance
check_string_conformance

# The tutorial deliberately declares the required `demo.echo` tool. This
# catalog-backed compiler integration test freezes that contract and executes
# its pure entry without weakening required-tool preflight.
run_check closed-errors-learnxinyminutes-catalog \
  cargo test -p allen-compiler closed_errors_learnxinyminutes_catalog_compiles_and_runs -- --nocapture
run_check josh-allen-mcp-examples \
  python3 -m unittest plugins/josh-allen/tests/test_server.py

loose_examples=(
  examples/answer.allen
  examples/core-values.allen
  examples/data-types.allen
  examples/dynamic-collections.allen
  examples/functions-and-effects/main.allen
  examples/josh-answer.allen
  examples/min-int.allen
  examples/operations.allen
  examples/overflow.allen
  examples/result-err.allen
  examples/result-ok.allen
  examples/stop.allen
  examples/structured-concurrency.allen
  examples/task-debug.allen
)
for source in "${loose_examples[@]}"; do
  name="example-$(basename "$source" .allen)"
  check_and_compare_source_artifact "$name" "$repo_root/$source"
done

rosetta_examples=()
while IFS= read -r source; do
  rosetta_examples+=("$source")
done < <(find "$repo_root/examples/rosetta-code" -maxdepth 1 -type f -name '*.allen' | sort)
[[ ${#rosetta_examples[@]} -eq 40 ]] || {
  printf 'expected exactly 40 Rosetta programs, found %s\n' "${#rosetta_examples[@]}" >&2
  exit 1
}
for source in "${rosetta_examples[@]}"; do
  check_and_compare_source_artifact "rosetta-$(basename "$source" .allen)" "$source"
done

inline_workspace="$evidence_dir/inline-workspace"
mkdir -p "$inline_workspace"
printf 'inline\n' > "$inline_workspace/message.txt"
check_and_compare_source_artifact \
  example-filesystem-inline "$repo_root/examples/filesystem-inline.allen" --workdir "$inline_workspace"

package_workspace="$evidence_dir/package-workspace"
mkdir -p "$package_workspace"
check_and_compare_source_artifact \
  example-filesystem-package "$repo_root/examples/filesystem-package" \
  --entry main --input "$repo_root/examples/filesystem-package/input.json" --workdir "$package_workspace"

# Record how each physical example source is covered. The roots above compile imported and
# package modules transitively, but this audit makes the complete example surface explicit.
# The tutorial's catalog-backed test above covers its required tool contract;
# this audit records that physical source exactly once with the other examples.
coverage="$logs_dir/example-coverage.log"
expected="$logs_dir/example-files.expected"
{
  for source in "${loose_examples[@]}"; do
    printf '%s\n' "$source"
  done
  printf '%s\n' examples/functions-and-effects/support.allen
  printf '%s\n' examples/filesystem-inline.allen
  printf '%s\n' examples/learnxinyminutes.allen
  find examples/josh-allen -type f -name '*.allen' -print | sort
  printf '%s\n' examples/codex-agent-mvp.allen
  find examples/filesystem-package -type f -name '*.allen' -print | sort
  for source in "${rosetta_examples[@]}"; do
    printf '%s\n' "${source#"$repo_root/"}"
  done
} | sort -u > "$coverage"
find examples -type f -name '*.allen' -print | sed "s#^$repo_root/##" | sort > "$expected"
[[ $(wc -l < "$expected") -eq 79 ]] || {
  printf 'expected exactly 79 ALLEN example files\n' >&2
  exit 1
}
cmp -s "$coverage" "$expected" || {
  printf 'example coverage audit does not account for every ALLEN source\n' >&2
  diff -u "$expected" "$coverage" >&2 || true
  exit 1
}

validate_agent_docs
write_summary "passed"
write_non_log_inventory
