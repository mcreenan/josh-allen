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

#[test]
fn bare_josh_requires_an_explicit_command() {
    let output = josh(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("a command is required"));
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
