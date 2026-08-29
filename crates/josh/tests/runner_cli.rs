use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use allen_compiler::assemble_inline_source;
use allen_schema::{CatalogLimits, FrozenCatalog};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("allen-josh-{name}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn josh(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_josh"))
        .args(arguments)
        .output()
        .unwrap()
}

fn assert_completed(output: &Output, value: &str) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{{\"outcome\":\"completed\",\"output\":{value}}}\n")
    );
}

fn write_template_package(directory: &TestDirectory) -> PathBuf {
    let package = directory.path().join("package");
    fs::create_dir_all(package.join("src")).unwrap();
    fs::create_dir_all(package.join("templates")).unwrap();
    fs::write(
        package.join("allen.toml"),
        include_bytes!("../../../examples/template-package/allen.toml"),
    )
    .unwrap();
    fs::write(
        package.join("allen.lock"),
        include_bytes!("../../../examples/template-package/allen.lock"),
    )
    .unwrap();
    fs::write(
        package.join("src/main.allen"),
        include_bytes!("../../../examples/template-package/src/main.allen"),
    )
    .unwrap();
    fs::write(
        package.join("templates/notice.txt"),
        include_bytes!("../../../examples/template-package/templates/notice.txt"),
    )
    .unwrap();
    package
}

fn write_filesystem_package(directory: &TestDirectory) -> PathBuf {
    let package = directory.path().join("package");
    for relative in ["src", "packages/text-utils/src"] {
        fs::create_dir_all(package.join(relative)).unwrap();
    }
    for relative in [
        "allen.toml",
        "allen.lock",
        "src/main.allen",
        "src/support.allen",
        "packages/text-utils/allen.toml",
        "packages/text-utils/src/text.allen",
    ] {
        fs::write(
            package.join(relative),
            fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join("examples/filesystem-package")
                    .join(relative),
            )
            .unwrap(),
        )
        .unwrap();
    }
    package
}

fn write_exec_package(directory: &TestDirectory, command: &str) -> PathBuf {
    let package = directory.path().join("package");
    fs::create_dir_all(package.join("src")).unwrap();
    fs::write(
        package.join("allen.toml"),
        format!(
            r#"[package]
name = "exec-runner-test"
version = "0.1.0"
language = "^0.1"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Int"

[capabilities]
required = ["exec.run"]

[exec]
commands = ["{command}"]
environment = []
"#,
        ),
    )
    .unwrap();
    fs::write(
        package.join("src/main.allen"),
        format!(
            r#"export async fn main() returns Int effects [exec.run] {{
  let result = await exec.run(["{command}"], Some(b""));
  match (result) {{
    Ok(response) => response.status
    Err(_) => -1
  }}
}}
"#,
        ),
    )
    .unwrap();
    package
}

#[test]
fn bare_josh_requires_an_explicit_command() {
    let output = josh(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("a command is required"));
}

#[test]
fn tool_grants_require_the_headless_executor_provider() {
    let directory = TestDirectory::new("grant-tool-provider");
    let source = directory.path().join("answer.allen");
    fs::write(&source, "export fn main() returns Int { 42 }\n").unwrap();

    let output = josh(&[
        "run",
        "--grant-tool",
        "example_echo",
        source.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--grant-tool requires --executor"));
}

#[test]
fn run_help_lists_the_headless_executor_options() {
    let output = josh(&["run", "--help"]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--executor"));
    assert!(stderr.contains("--grant-tool <name>"));
}

#[test]
fn run_executes_one_loose_source_without_a_package_manifest() {
    let directory = TestDirectory::new("loose");
    let source = directory.path().join("answer.allen");
    fs::write(&source, "export fn main() returns Int { 40 + 2 }\n").unwrap();

    let output = josh(&["run", source.to_str().unwrap()]);
    assert_completed(&output, "42");
}

#[test]
fn run_projects_newtype_entries_as_bare_json() {
    let directory = TestDirectory::new("newtype-entry");
    let source = directory.path().join("epoch.allen");
    fs::write(
        &source,
        r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
}
newtype EpochSeconds = Int
export fn main(input: EpochSeconds) returns EpochSeconds {
  EpochSeconds(input.value + 1)
}
"#,
    )
    .unwrap();

    let output = josh(&["run", "--input", "41", source.to_str().unwrap()]);
    assert_completed(&output, "42");
}

#[test]
fn run_reports_source_fail_as_a_nonretryable_failed_outcome() {
    let directory = TestDirectory::new("program-fail");
    for (name, reason, message) in [
        ("empty", "", "program failed"),
        ("reason", "bad input", "bad input"),
    ] {
        let source = directory.path().join(format!("{name}.allen"));
        fs::write(
            &source,
            format!(
                "export fn main() returns Int {{ if (true) {{ fail(\"{reason}\") }} else {{ 0 }} }}\n"
            ),
        )
        .unwrap();
        let output = josh(&["run", source.to_str().unwrap()]);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            output.stderr.is_empty(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["outcome"], "failed");
        assert_eq!(result["error"]["code"], "program.failed");
        assert_eq!(result["error"]["message"], message);
        assert_eq!(result["error"]["retryable"], false);
    }

    let source = directory.path().join("host-message-bound.allen");
    let reason = "x".repeat(1_500);
    fs::write(
        &source,
        format!(
            "export fn main() returns Int {{ if (true) {{ fail(\"{reason}\") }} else {{ 0 }} }}\n"
        ),
    )
    .unwrap();
    let output = josh(&["run", source.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"]["code"], "program.failed");
    assert_eq!(result["error"]["message"], "program failed");

    let source = directory.path().join("control-text.allen");
    fs::write(
        &source,
        "export fn main() returns Int { if (true) { fail(\"bad\\nforged:\\r\\t\\0\\b\\f\") } else { 0 } }\n",
    )
    .unwrap();
    let output = josh(&["run", source.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::str::from_utf8(&output.stdout)
            .unwrap()
            .matches('\n')
            .count(),
        1
    );
    assert!(output.stderr.is_empty());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"]["code"], "program.failed");
    assert_eq!(result["error"]["message"], "bad\nforged:\r\t\0\u{8}\u{c}");
}

#[test]
fn run_infers_the_input_boundary_of_a_loose_main_entry() {
    let directory = TestDirectory::new("input");
    let source = directory.path().join("increment.allen");
    fs::write(
        &source,
        r"record Input { value: Int }
export fn main(input: Input) returns Int { input.value + 1 }
",
    )
    .unwrap();

    let output = josh(&[
        "run",
        "--input",
        r#"{"value":41}"#,
        source.to_str().unwrap(),
    ]);
    assert_completed(&output, "42");
}

#[test]
fn run_rejects_entry_input_before_json_can_collapse_wire_errors() {
    let directory = TestDirectory::new("strict-input");
    let source = directory.path().join("strict-input.allen");
    fs::write(
        &source,
        r"record Input { value: Int }
export fn main(input: Input) returns Int { input.value }
",
    )
    .unwrap();

    for (name, bytes, error_fragment) in [
        (
            "duplicate",
            br#"{"value":1,"value":2}"#.as_slice(),
            "duplicate JSON key 'value'",
        ),
        ("invalid-utf8", &[0xff], "protocol body is not UTF-8"),
        (
            "bom",
            b"\xef\xbb\xbf{\"value\":1}",
            "protocol body is not strict JSON",
        ),
        (
            "trailing",
            br#"{"value":1} null"#,
            "protocol body is not strict JSON",
        ),
    ] {
        let input = directory.path().join(format!("{name}.json"));
        fs::write(&input, bytes).unwrap();
        let input_argument = format!("@{}", input.display());
        let output = josh(&["run", "--input", &input_argument, source.to_str().unwrap()]);
        assert_eq!(output.status.code(), Some(1), "case {name}");
        assert!(output.stdout.is_empty(), "case {name}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(error_fragment), "case {name}: {stderr}");
    }

    for input in [r"{}", r#"{"value":1,"extra":2}"#, r#"{"value":"1"}"#] {
        let output = josh(&["run", "--input", input, source.to_str().unwrap()]);
        assert_eq!(output.status.code(), Some(1), "input {input}");
        assert!(output.stdout.is_empty(), "input {input}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("execution preflight failed"),
            "input {input}"
        );
    }

    let map_source = directory.path().join("map-input.allen");
    fs::write(
        &map_source,
        "export fn main(input: Map<String, Int>) returns Map<String, Int> { input }\n",
    )
    .unwrap();
    let output = josh(&[
        "run",
        "--input",
        r#"[["b",1],["a",2]]"#,
        map_source.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("execution preflight failed")
    );
}

#[test]
fn run_accepts_package_directories_and_artifacts() {
    let directory = TestDirectory::new("forms");
    let package = directory.path().join("package");
    fs::create_dir_all(package.join("src")).unwrap();
    fs::write(
        package.join("allen.toml"),
        r#"[package]
name = "runner-test"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Int"

[capabilities]
required = []
optional = []
"#,
    )
    .unwrap();
    let source = "export fn main() returns Int { 42 }\n";
    fs::write(package.join("src/main.allen"), source).unwrap();

    let package_output = josh(&["run", package.to_str().unwrap()]);
    assert_completed(&package_output, "42");

    let catalog = FrozenCatalog::freeze(Vec::new(), &CatalogLimits::default()).unwrap();
    let compiled = assemble_inline_source(source, &catalog).unwrap();
    let artifact = directory.path().join("answer.allenb");
    fs::write(
        &artifact,
        allen_bytecode::encode(&compiled.artifact).unwrap(),
    )
    .unwrap();
    let artifact_output = josh(&["run", artifact.to_str().unwrap()]);
    assert_completed(&artifact_output, "42");
}

#[test]
fn run_accepts_byte_exact_package_template_resources() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/template-package");
    let output = josh(&["run", package.to_str().unwrap()]);
    assert_completed(
        &output,
        r#""Hello, Ada. Count: 7. Enabled: true. Hello again, Ada.\n""#,
    );
}

#[test]
fn run_executes_the_verified_filesystem_dependency_graph() {
    let directory = TestDirectory::new("filesystem-package-graph");
    let package = write_filesystem_package(&directory);
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let output = josh(&[
        "run",
        "--workdir",
        workspace.to_str().unwrap(),
        "--grant",
        "fs.read",
        "--grant",
        "fs.write",
        "--input",
        r#""hello through JOSH""#,
        package.to_str().unwrap(),
    ]);
    assert_completed(&output, r#"{"tag":"Ok","value":"hello through JOSH"}"#);
    assert_eq!(
        fs::read_to_string(workspace.join("message.txt")).unwrap(),
        "hello through JOSH"
    );
}

#[test]
fn run_rejects_missing_and_tampered_dependency_graph_files() {
    for case in ["missing", "extra", "tampered", "escape"] {
        let directory = TestDirectory::new(case);
        let package = write_filesystem_package(&directory);
        let dependency = package.join("packages/text-utils/src/text.allen");
        match case {
            "missing" => fs::remove_file(dependency).unwrap(),
            "extra" => {
                fs::write(
                    package.join("packages/text-utils/src/extra.allen"),
                    "export fn extra() returns Int { 1 }\n",
                )
                .unwrap();
            }
            "tampered" => {
                fs::write(
                    dependency,
                    "export fn target_path() returns String { \"changed.txt\" }\n",
                )
                .unwrap();
            }
            "escape" => {
                let manifest = fs::read_to_string(package.join("allen.toml"))
                    .unwrap()
                    .replace("packages/text-utils", "../text-utils");
                fs::write(package.join("allen.toml"), manifest).unwrap();
            }
            _ => unreachable!(),
        }
        let output = josh(&["run", package.to_str().unwrap()]);
        assert!(!output.status.success(), "case {case}");
        assert!(output.stdout.is_empty(), "case {case}");
        assert!(!output.stderr.is_empty(), "case {case}");
    }
}

#[test]
fn required_exec_uses_grant_exec_without_a_generic_capability_grant() {
    let directory = TestDirectory::new("required-exec");
    let package = write_exec_package(&directory, "env");

    let denied = josh(&["run", package.to_str().unwrap()]);
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("execution preflight failed"),
        "stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let generic_only = josh(&["run", "--grant", "exec.run", package.to_str().unwrap()]);
    assert!(!generic_only.status.success());

    let no_pattern_directory = TestDirectory::new("required-exec-no-pattern");
    let no_pattern_package = write_exec_package(&no_pattern_directory, "env");
    let manifest = fs::read_to_string(no_pattern_package.join("allen.toml"))
        .unwrap()
        .replace("commands = [\"env\"]", "commands = []");
    fs::write(no_pattern_package.join("allen.toml"), manifest).unwrap();
    let no_pattern = josh(&[
        "run",
        "--grant-exec",
        "env",
        no_pattern_package.to_str().unwrap(),
    ]);
    assert!(!no_pattern.status.success());

    let granted = josh(&["run", "--grant-exec", "env", package.to_str().unwrap()]);
    #[cfg(target_os = "linux")]
    assert_completed(&granted, "0");
    #[cfg(target_os = "macos")]
    {
        assert!(!granted.status.success());
        assert!(String::from_utf8_lossy(&granted.stderr).contains("execution preflight failed"));
    }

    let unavailable_directory = TestDirectory::new("unavailable-exec");
    let unavailable_name = "allen-executable-that-must-not-exist";
    let unavailable_package = write_exec_package(&unavailable_directory, unavailable_name);
    let unavailable = josh(&[
        "run",
        "--grant-exec",
        unavailable_name,
        unavailable_package.to_str().unwrap(),
    ]);
    assert!(!unavailable.status.success());
    assert!(String::from_utf8_lossy(&unavailable.stderr).contains("execution preflight failed"));
}

#[test]
fn run_rejects_missing_tampered_escaping_and_oversized_template_resources() {
    for case in [
        "missing",
        "tampered",
        "escape",
        "oversized",
        "manifest-collision",
        "lock-collision",
        "source-collision",
    ] {
        let directory = TestDirectory::new(case);
        let package = write_template_package(&directory);
        match case {
            "missing" => fs::remove_file(package.join("templates/notice.txt")).unwrap(),
            "tampered" => {
                fs::write(package.join("templates/notice.txt"), "tampered {{name}}").unwrap();
            }
            "escape" => {
                let manifest = fs::read_to_string(package.join("allen.toml"))
                    .unwrap()
                    .replace("templates/notice.txt", "../notice.txt");
                fs::write(package.join("allen.toml"), manifest).unwrap();
            }
            "oversized" => {
                fs::write(package.join("templates/notice.txt"), vec![b'x'; 1_048_577]).unwrap();
            }
            "manifest-collision" | "lock-collision" | "source-collision" => {
                let collision = match case {
                    "manifest-collision" => "allen.toml",
                    "lock-collision" => "allen.lock",
                    "source-collision" => "src/main.allen",
                    _ => unreachable!(),
                };
                let manifest = fs::read_to_string(package.join("allen.toml"))
                    .unwrap()
                    .replace("templates/notice.txt", collision);
                fs::write(package.join("allen.toml"), manifest).unwrap();
            }
            _ => unreachable!(),
        }
        let output = josh(&["run", package.to_str().unwrap()]);
        assert!(!output.status.success(), "case {case}");
        assert!(output.stdout.is_empty(), "case {case}");
        assert!(!output.stderr.is_empty(), "case {case}");
    }
}

#[test]
fn run_skips_host_effects_and_honors_bare_return_in_conditionals() {
    let directory = TestDirectory::new("control-flow");
    let package = directory.path().join("package");
    fs::create_dir_all(package.join("src")).unwrap();
    fs::write(
        package.join("allen.toml"),
        r#"[package]
name = "control-flow"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Void"

[capabilities]
required = ["fs.write"]
optional = []
"#,
    )
    .unwrap();
    let skipped_output = directory.path().join("must-not-exist.txt");
    fs::write(
        package.join("src/main.allen"),
        r#"export async fn main() returns Void effects [fs.write] {
  let workspace = fs.workspace();
  if (false) {
    let ignored = await fs.write_text(workspace, "must-not-exist.txt", "wrong branch");
  }
  if (true) { return; }
  let ignored = await fs.write_text(workspace, "must-not-exist.txt", "after return");
  return;
}
"#,
    )
    .unwrap();

    let output = josh(&[
        "run",
        "--workdir",
        directory.path().to_str().unwrap(),
        "--grant",
        "fs.write",
        package.to_str().unwrap(),
    ]);
    assert_completed(&output, "null");
    assert!(
        !skipped_output.exists(),
        "neither an unselected branch nor code after return may perform a host write"
    );
}

#[test]
fn run_executes_loops_from_loose_source() {
    let directory = TestDirectory::new("loops");
    let source = directory.path().join("loops.allen");
    fs::write(
        &source,
        r#"export fn main() returns Int {
  mut total = 0;
  for value in [1, 2, 3] { total = total + value; }
  for (_, value) in map { "z": 3, "a": 4 } { total = total + value; }
  for index in 0..4 {
    if (index == 1) { continue; }
    if (index == 3) { break; }
    total = total + index;
  }
  while (total < 16) { total = total + 1; }
  loop { break; }
  total
}
"#,
    )
    .unwrap();

    let output = josh(&["run", source.to_str().unwrap()]);
    assert_completed(&output, "16");
}

#[test]
fn run_executes_operators_and_skips_unselected_boolean_operands() {
    let directory = TestDirectory::new("operators");
    let source = directory.path().join("operators.allen");
    fs::write(
        &source,
        r"export fn main() returns Int {
  mut value = -13;
  value %= 5;
  value += 10;
  value -= 2;
  value *= 3;
  value /= 2;
  mut decimal: Float = 2.0;
  decimal += 3.0;
  decimal -= 1.5;
  decimal *= 2.0;
  decimal /= 2.0;
  let skipped_and = false && ((1 % 0) == 0);
  let skipped_or = true || ((1 % 0) == 0);
  let evaluated_and = true && (value % 2 == 1);
  let evaluated_or = false || (decimal == 3.5);
  if (skipped_and) { value += 1000; }
  if (skipped_or) { value += 1; }
  if (evaluated_and) { value += 10; }
  if (evaluated_or) { value += 100; }
  value
}
",
    )
    .unwrap();

    let output = josh(&["run", source.to_str().unwrap()]);
    assert_completed(&output, "118");
}

#[test]
fn run_exposes_only_the_effective_manifest_grants_to_capability_inspection() {
    let directory = TestDirectory::new("capability-inspection");
    let package = directory.path().join("package");
    fs::create_dir_all(package.join("src")).unwrap();
    fs::write(
        package.join("allen.toml"),
        r#"[package]
name = "capability-inspection"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "List<String>"

[capabilities]
required = []
optional = ["fs.read"]
"#,
    )
    .unwrap();
    fs::write(
        package.join("src/main.allen"),
        r"export fn main() returns List<String> effects [capability.inspect] {
  capability.granted()
}
",
    )
    .unwrap();

    let granted = josh(&[
        "run",
        "--workdir",
        directory.path().to_str().unwrap(),
        "--grant",
        "fs.read",
        package.to_str().unwrap(),
    ]);
    assert_completed(&granted, r#"["fs.read"]"#);

    let denied = josh(&["run", package.to_str().unwrap()]);
    assert_completed(&denied, "[]");
}

#[test]
fn run_searches_workspace_text_files_recursively() {
    let directory = TestDirectory::new("search");
    let workspace = directory.path().join("workspace");
    fs::create_dir_all(workspace.join("nested")).unwrap();
    fs::write(workspace.join("alpha.txt"), "first\nfind me here\n").unwrap();
    fs::write(workspace.join("nested/beta.txt"), "find me too\n").unwrap();
    fs::write(workspace.join("binary"), [0xff, 0x00]).unwrap();
    let source = directory.path().join("search.allen");
    fs::write(
        &source,
        r#"manifest {
  language: "0.1"
  entry: main
  capabilities: [fs.read(workdir)]
}

export async fn main(query: String)
  returns Result<List<SearchMatch>, FileError> effects [fs.read] {
  await fs.search(fs.workspace(), ".", query)
}
"#,
    )
    .unwrap();

    let output = josh(&[
        "run",
        "--grant",
        "fs.read",
        "--workdir",
        workspace.to_str().unwrap(),
        "--input",
        r#""find me""#,
        source.to_str().unwrap(),
    ]);
    assert_completed(
        &output,
        r#"{"tag":"Ok","value":[{"column":1,"line":2,"path":"alpha.txt","text":"find me here"},{"column":1,"line":1,"path":"nested/beta.txt","text":"find me too"}]}"#,
    );
}
