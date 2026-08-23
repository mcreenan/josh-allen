#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn example() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/answer.allen")
}

fn named_example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../examples/{name}"))
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}"))
}

fn temporary_directory() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "allen-cli-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).expect("temporary test directory must be created");
    path
}

fn copy_directory(source: &std::path::Path, target: &std::path::Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
}

fn hard_coded_public_cli_error_codes() -> Vec<(&'static str, &'static str)> {
    let mut source = include_str!("../src/app.rs");
    let mut codes = Vec::new();
    while let Some((before, after_marker)) = source.split_once("error[") {
        let (code, after_code) = after_marker
            .split_once(']')
            .expect("a public CLI error code must have a closing bracket");
        source = after_code;
        if code.contains('{') || code.contains('}') {
            continue;
        }
        let prefix = before.rsplit_once('\n').map_or(before, |(_, line)| line);
        let channel = if prefix.ends_with("runtime ") {
            "trap"
        } else if prefix.ends_with("artifact ") {
            "diagnostic"
        } else {
            panic!("hard-coded public CLI error code {code} has an unknown channel")
        };
        codes.push((code, channel));
    }
    codes
}

#[test]
fn every_hard_coded_public_cli_error_code_is_registered_once() {
    let registry: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/conformance/errors-0.1.json")).unwrap();
    let rows = registry["registry"].as_array().unwrap();
    let codes = hard_coded_public_cli_error_codes();
    assert!(
        codes.contains(&("runtime.panic", "trap")),
        "worker supervisor failures must use the bounded terminal host code"
    );

    for (code, channel) in codes {
        let matching = rows
            .iter()
            .filter(|row| row["code"].as_str() == Some(code))
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            1,
            "registry must contain hard-coded CLI code {code} exactly once"
        );
        assert_eq!(matching[0]["channel"].as_str(), Some(channel), "{code}");
    }
}

const CONTRACT_TRAP_BODY: &str = r"async fn answer() returns Int {
  let a = 1;
  let b = 2;
  let c = 3;
  let d = 4;
  let e = 5;
  42
}

async fn blocked() returns Int effects [task.spawn] {
  await {
    let inner = spawn answer();
    await inner
  }
}

export async fn main() returns Int effects [task.spawn] {
  await {
    let task = spawn blocked();
    let a = 1;
    let b = 2;
    let c = 3;
    let d = 4;
    let failed = 1 / 0;
    failed + await task
  }
}
";

const CONTRACT_TRAP_TRACE: [&str; 7] = [
    "task_event sequence=1 task_id=0 owner_id=0 kind=spawned",
    "task_event sequence=2 task_id=1 owner_id=0 kind=spawned",
    "task_event sequence=3 task_id=2 owner_id=1 kind=spawned",
    "task_event sequence=4 task_id=1 owner_id=0 kind=waiting",
    "task_event sequence=5 task_id=0 owner_id=0 kind=failed",
    "task_event sequence=6 task_id=1 owner_id=0 kind=cancelled",
    "task_event sequence=7 task_id=2 owner_id=1 kind=cancelled",
];

#[test]
fn help_prints_usage_and_succeeds() {
    for flag in ["-h", "--help"] {
        let output = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg(flag)
            .output()
            .expect("CLI must start");

        assert!(output.status.success(), "{flag}");
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .starts_with("usage:")
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn run_prints_the_program_result() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(example())
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "42\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn internal_worker_child_rejects_a_malformed_bounded_request() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("--internal-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("internal worker must start");
    child
        .stdin
        .as_mut()
        .expect("worker stdin")
        .write_all(&[0, 0, 0, 3, b'{', b'}', b'!'])
        .expect("malformed request must be sent");
    let output = child.wait_with_output().expect("worker must exit");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .starts_with("internal worker error: worker message JSON is invalid\n")
    );
}

#[test]
fn check_accepts_the_program_without_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(example())
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn run_prints_core_values_in_deterministic_order() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(named_example("core-values.allen"))
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "[42.0,\"core\",{\"$bytes\":\"AEE=\"},[42,7],[[\"a\",1],[\"b\",2]],[true,null]]\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn run_reports_the_stable_overflow_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(named_example("overflow.allen"))
        .output()
        .expect("CLI must start");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "runtime error[arithmetic.overflow]: arithmetic overflow\n"
    );
}

#[test]
fn run_executes_core_operators_indexes_and_conversions() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(named_example("operations.allen"))
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "[20,65,1,\"x\",{\"$bytes\":\"b2s=\"},\"42\",1.5,true,5,-5]\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn dynamic_collections_check_build_and_run_identically() {
    let directory = temporary_directory();
    let artifact = directory.join("dynamic-collections.allenb");
    let source = named_example("dynamic-collections.allen");

    let checked = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(&source)
        .output()
        .expect("CLI must start");
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(checked.stdout.is_empty());
    assert!(checked.stderr.is_empty());

    let source_run = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(&source)
        .output()
        .expect("CLI must start");
    assert!(
        source_run.status.success(),
        "{}",
        String::from_utf8_lossy(&source_run.stderr)
    );
    assert_eq!(
        String::from_utf8(source_run.stdout.clone()).unwrap(),
        "[3,2,[1,2],[1,2,3],[1,9,3],13]\n"
    );
    assert!(source_run.stderr.is_empty());

    let built = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&artifact)
        .output()
        .expect("CLI must start");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(built.stdout.is_empty());
    assert!(built.stderr.is_empty());
    assert_eq!(
        &std::fs::read(&artifact).unwrap()[10..12],
        &13_u16.to_le_bytes()
    );

    let artifact_run = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(&artifact)
        .output()
        .expect("CLI must start");
    assert!(artifact_run.status.success());
    assert_eq!(artifact_run.stdout, source_run.stdout);
    assert!(artifact_run.stderr.is_empty());

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn run_prints_the_minimum_int() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(named_example("min-int.allen"))
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "-9223372036854775808\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn check_accepts_data_types_without_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(named_example("data-types.allen"))
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn run_prints_values_in_canonical_order() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(named_example("data-types.allen"))
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "[{\"x\":1,\"y\":2},{\"tag\":\"Named\",\"value\":{\"label\":\"cpu\",\"value\":7}},3,7,\"yes\",{\"tag\":\"Some\",\"value\":7},{\"tag\":\"Ok\",\"value\":8},8,{\"tag\":\"Some\",\"value\":{\"x\":1,\"y\":2}},{\"tag\":\"None\"},true]\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn result_question_mark_unwraps_ok_and_preserves_err() {
    let ok = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(named_example("result-ok.allen"))
        .output()
        .expect("CLI must start");
    let error = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(named_example("result-err.allen"))
        .output()
        .expect("CLI must start");

    assert!(ok.status.success());
    assert_eq!(
        String::from_utf8(ok.stdout).unwrap(),
        "{\"tag\":\"Ok\",\"value\":42}\n"
    );
    assert!(ok.stderr.is_empty());
    assert!(error.status.success());
    assert_eq!(
        String::from_utf8(error.stdout).unwrap(),
        "{\"tag\":\"Err\",\"value\":{\"code\":7,\"message\":\"failed\"}}\n"
    );
    assert!(error.stderr.is_empty());
}

#[test]
fn check_reports_match_and_unknown_diagnostics() {
    for (name, code) in [
        ("non-exhaustive.allen", "E2015"),
        ("unreachable-match.allen", "E2016"),
        ("unknown-use.allen", "E2018"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("check")
            .arg(fixture(name))
            .output()
            .expect("CLI must start");

        assert!(!output.status.success(), "{name} must fail");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr.lines().count(), 1, "{name}: {stderr}");
        assert!(
            stderr.contains(&format!("error[{code}]")),
            "{name}: {stderr}"
        );
    }
}

#[test]
fn functions_modules_generics_and_closures_execute() {
    let source = named_example("functions-and-effects/main.allen");
    let checked = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(&source)
        .output()
        .expect("CLI must start");
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(checked.stdout.is_empty());
    assert!(checked.stderr.is_empty());

    let executed = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(source)
        .output()
        .expect("CLI must start");
    assert!(
        executed.status.success(),
        "{}",
        String::from_utf8_lossy(&executed.stderr)
    );
    assert_eq!(String::from_utf8(executed.stdout).unwrap(), "[42,true,7]\n");
    assert!(executed.stderr.is_empty());
}

#[test]
fn compiler_negative_fixtures_have_stable_diagnostics() {
    for (name, code) in [
        ("import-cycle-a.allen", "E3002"),
        ("missing-import.allen", "E3003"),
        ("missing-effect.allen", "E2403"),
        ("generic-constraint.allen", "E3008"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("check")
            .arg(fixture(name))
            .output()
            .expect("CLI must start");

        assert!(!output.status.success(), "{name} must fail");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr.lines().count(), 1, "{name}: {stderr}");
        assert!(
            stderr.contains(&format!("error[{code}]")),
            "{name}: {stderr}"
        );
    }
}

#[test]
fn effect_report_is_canonical() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg("--show-effects")
        .arg(named_example("functions-and-effects/main.allen"))
        .output()
        .expect("CLI must start");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let report = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        report,
        "support.allen::callback_effect effects [fs.read]\n\
support.allen::closure_effect effects [fs.read]\n\
support.allen::declared_effect effects [fs.read]\n\
support.allen::pure_identity\n\
support.allen::transitive_effect effects [fs.read]\n"
    );
}

#[test]
fn current_artifact_build_is_canonical() {
    let directory = temporary_directory();
    let first = directory.join("first.allenb");
    let second = directory.join("second.allenb");
    let source = named_example("functions-and-effects/main.allen");

    for output in [&first, &second] {
        let built = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("build")
            .arg(&source)
            .arg("-o")
            .arg(output)
            .output()
            .expect("CLI must start");
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        assert!(built.stdout.is_empty());
        assert!(built.stderr.is_empty());
    }
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap()
    );
    assert_eq!(
        &std::fs::read(&first).unwrap()[10..12],
        &13_u16.to_le_bytes()
    );
    let bytes = std::fs::read(&first).unwrap();
    let decoded =
        allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
            .expect("loose multi-file source must build a verified artifact");
    let manifest = decoded
        .manifest()
        .expect("loose source artifacts must contain a synthesized manifest");
    assert_eq!(manifest.package, "inline");
    assert_eq!(manifest.version, "0.1.0");
    assert_eq!(manifest.language_requirement, "0.1");
    assert!(manifest.required_capabilities.is_empty());
    assert!(manifest.optional_capabilities.is_empty());
    assert_eq!(decoded.entries().len(), 1);
    assert_eq!(decoded.entries()[0].name, "main");

    let executed = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(&first)
        .output()
        .expect("CLI must start");
    assert!(executed.status.success());
    assert_eq!(String::from_utf8(executed.stdout).unwrap(), "[42,true,7]\n");
    assert!(executed.stderr.is_empty());

    let inspected = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("inspect")
        .arg(&first)
        .output()
        .expect("CLI must start");
    assert!(inspected.status.success());
    assert!(inspected.stderr.is_empty());
    let inspected_again = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("inspect")
        .arg(&second)
        .output()
        .expect("CLI must start");
    assert!(inspected_again.status.success());
    assert_eq!(inspected.stdout, inspected_again.stdout);
    let report = String::from_utf8(inspected.stdout).unwrap();
    assert!(report.starts_with("bytecode_version: 13\n"));
    assert!(report.contains("language_version: 0.1.0\n"));
    assert!(report.contains("target_profile: portable\n"));
    assert!(report.contains("section.manifest_contracts: 1\n"));
    assert!(report.contains("manifest.package: inline@0.1.0\n"));
    assert!(report.contains("contract.entry.main: function=0 input_schema=0 output_schema=1\n"));
    assert!(
        report.contains("entry: pkg/x696e6c696e65/x302e312e30/x737263/x6d61696e.allen::main\n")
    );
    assert!(report.ends_with("debug: present\n"));

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn current_source_and_artifact_match() {
    let directory = temporary_directory();
    let artifact = directory.join("structured-concurrency.allenb");
    let source = named_example("structured-concurrency.allen");

    let source_run = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(&source)
        .output()
        .expect("CLI must start");
    assert!(source_run.status.success());
    assert_eq!(
        String::from_utf8(source_run.stdout.clone()).unwrap(),
        "42\n"
    );
    assert!(source_run.stderr.is_empty());

    let built = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&artifact)
        .output()
        .expect("CLI must start");
    assert!(built.status.success());
    assert!(built.stdout.is_empty());
    assert!(built.stderr.is_empty());
    assert_eq!(
        &std::fs::read(&artifact).unwrap()[10..12],
        &13_u16.to_le_bytes()
    );

    let artifact_run = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(&artifact)
        .output()
        .expect("CLI must start");
    assert!(artifact_run.status.success());
    assert_eq!(source_run.stdout, artifact_run.stdout);
    assert!(artifact_run.stderr.is_empty());

    let inspected = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("inspect")
        .arg(&artifact)
        .output()
        .expect("CLI must start");
    assert!(inspected.status.success());
    assert!(inspected.stderr.is_empty());
    let report = String::from_utf8(inspected.stdout).unwrap();
    assert!(report.starts_with("bytecode_version: 13\n"));

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn structured_concurrency_task_snapshot_and_trace_are_deterministic() {
    let directory = temporary_directory();
    let artifact = directory.join("task-debug.allenb");
    let source = named_example("task-debug.allen");
    let expected_trace = "task_event sequence=1 task_id=0 owner_id=0 kind=spawned\n\
task_event sequence=2 task_id=1 owner_id=0 kind=spawned\n\
task_event sequence=3 task_id=1 owner_id=0 kind=completed\n\
task_event sequence=4 task_id=0 owner_id=0 kind=waiting\n\
task_event sequence=5 task_id=0 owner_id=0 kind=ready\n\
task_event sequence=6 task_id=0 owner_id=0 kind=waiting\n\
task_event sequence=7 task_id=0 owner_id=0 kind=ready\n\
task_event sequence=8 task_id=0 owner_id=0 kind=completed\n";

    let source_run = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg("--trace-tasks")
        .arg(&source)
        .output()
        .expect("CLI must start");
    assert!(source_run.status.success());
    assert_eq!(
        String::from_utf8(source_run.stdout.clone()).unwrap(),
        "[\"pkg/x696e6c696e65/x302e312e30/x737263/x7461736b2d6465627567.allen::answer\",1,{\"tag\":\"Some\",\"value\":\"pkg://inline@0.1.0/src/task-debug.allen:30..38\"},0,\"ready\",42]\n"
    );
    assert_eq!(
        String::from_utf8(source_run.stderr.clone()).unwrap(),
        expected_trace
    );

    let built = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&artifact)
        .output()
        .expect("CLI must start");
    assert!(built.status.success());
    assert!(built.stdout.is_empty());
    assert!(built.stderr.is_empty());

    let artifact_run = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg("--trace-tasks")
        .arg(&artifact)
        .output()
        .expect("CLI must start");
    assert!(artifact_run.status.success());
    assert_eq!(artifact_run.stdout, source_run.stdout);
    assert_eq!(artifact_run.stderr, source_run.stderr);

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn structured_concurrency_task_snapshot_observes_scheduled_terminal_and_waiting_states() {
    for (fixture_name, expected) in [
        ("task-snapshot-completed.allen", "stopped: \"completed\"\n"),
        ("task-snapshot-failed.allen", "stopped: \"failed\"\n"),
        ("task-snapshot-waiting.allen", "stopped: \"waiting\"\n"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("run")
            .arg(fixture(fixture_name))
            .output()
            .expect("CLI must start");
        assert!(output.status.success(), "{fixture_name}");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        assert!(output.stderr.is_empty(), "{fixture_name}");
    }

    let cancelled = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg("--trace-tasks")
        .arg(fixture("task-snapshot-waiting.allen"))
        .output()
        .expect("CLI must start");
    assert!(cancelled.status.success());
    assert_eq!(
        String::from_utf8(cancelled.stderr).unwrap(),
        "task_event sequence=1 task_id=0 owner_id=0 kind=spawned\n\
task_event sequence=2 task_id=1 owner_id=0 kind=spawned\n\
task_event sequence=3 task_id=2 owner_id=1 kind=spawned\n\
task_event sequence=4 task_id=1 owner_id=0 kind=waiting\n\
task_event sequence=5 task_id=1 owner_id=0 kind=cancelled\n\
task_event sequence=6 task_id=2 owner_id=1 kind=cancelled\n\
task_event sequence=7 task_id=0 owner_id=0 kind=stopped\n"
    );
}

#[test]
fn structured_concurrency_stop_is_a_successful_terminal_outcome() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(named_example("stop.allen"))
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "stopped: \"done\"\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn closed_errors_contract_stop_stays_stopped_across_source_package_and_artifact_modes() {
    let directory = temporary_directory();
    let source = directory.join("stop-contract.allen");
    let artifact = directory.join("stop-contract.allenb");
    std::fs::write(
        &source,
        r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
}
async fn answer() returns Int {
  let a = 1;
  let b = 2;
  let c = 3;
  let d = 4;
  let e = 5;
  42
}

async fn blocked() returns Int effects [task.spawn] {
  await {
    let inner = spawn answer();
    await inner
  }
}

export async fn main() returns Void effects [debug.inspect, task.spawn] {
  await {
    let task = spawn blocked();
    let a = 1;
    let b = 2;
    let c = 3;
    let d = 4;
    let snapshot = allen.internal.task_snapshot(task);
    stop(snapshot.state)
  }
}
"#,
    )
    .unwrap();
    frontend_build(&source, &artifact);
    for input in [&source, &artifact] {
        let output = frontend_run_with_trace(input);
        assert!(output.status.success(), "{}", input.display());
        assert_eq!(output.stdout, b"stopped: \"waiting\"\n");
        assert_eq!(
            output.stderr,
            b"task_event sequence=1 task_id=0 owner_id=0 kind=spawned\n\
task_event sequence=2 task_id=1 owner_id=0 kind=spawned\n\
task_event sequence=3 task_id=2 owner_id=1 kind=spawned\n\
task_event sequence=4 task_id=1 owner_id=0 kind=waiting\n\
task_event sequence=5 task_id=1 owner_id=0 kind=cancelled\n\
task_event sequence=6 task_id=2 owner_id=1 kind=cancelled\n\
task_event sequence=7 task_id=0 owner_id=0 kind=stopped\n"
        );
    }

    let package = directory.join("package");
    let package_artifact = directory.join("stop-package.allenb");
    std::fs::create_dir_all(package.join("src")).unwrap();
    std::fs::write(
        package.join("allen.toml"),
        r#"[package]
name = "stop-package"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Void"

[capabilities]
required = []
optional = []
"#,
    )
    .unwrap();
    std::fs::write(
        package.join("src/main.allen"),
        "export fn main() returns Void { stop(\"package done\") }\n",
    )
    .unwrap();
    frontend_lock(&package);
    frontend_build(&package, &package_artifact);
    for input in [&package, &package_artifact] {
        let output = frontend_run_with_trace(input);
        assert!(output.status.success(), "{}", input.display());
        assert_eq!(output.stdout, b"stopped: \"package done\"\n");
        assert_eq!(
            output.stderr,
            b"task_event sequence=1 task_id=0 owner_id=0 kind=spawned\n\
task_event sequence=2 task_id=0 owner_id=0 kind=stopped\n"
        );
    }

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn closed_errors_contract_completion_trace_matches_for_inline_source_and_artifact() {
    let directory = temporary_directory();
    let source = directory.join("complete-contract.allen");
    let artifact = directory.join("complete-contract.allenb");
    std::fs::write(
        &source,
        r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
}
export fn main() returns Int { 42 }
"#,
    )
    .unwrap();
    frontend_build(&source, &artifact);
    for input in [&source, &artifact] {
        let output = frontend_run_with_trace(input);
        assert!(output.status.success(), "{}", input.display());
        assert_eq!(output.stdout, b"42\n");
        assert_eq!(
            output.stderr,
            b"task_event sequence=1 task_id=0 owner_id=0 kind=spawned\n\
task_event sequence=2 task_id=0 owner_id=0 kind=completed\n"
        );
    }
    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn closed_errors_contract_trap_trace_preserves_owned_task_cleanup_for_source_and_artifact() {
    let directory = temporary_directory();
    let source = directory.join("trap-contract.allen");
    let artifact = directory.join("trap-contract.allenb");
    std::fs::write(
        &source,
        format!(
            r#"manifest {{
  language: "0.1"
  entry: main
  capabilities: []
}}
{CONTRACT_TRAP_BODY}"#
        ),
    )
    .unwrap();
    frontend_build(&source, &artifact);
    for input in [&source, &artifact] {
        let output = frontend_run_with_trace(input);
        assert!(!output.status.success(), "{}", input.display());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        let actual_trace = stderr
            .lines()
            .filter(|line| line.starts_with("task_event "))
            .collect::<Vec<_>>();
        assert_eq!(actual_trace, CONTRACT_TRAP_TRACE, "{}", input.display());
        assert!(
            stderr.contains("runtime error[arithmetic.division_by_zero]"),
            "{stderr}"
        );
    }
    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn closed_errors_package_trap_trace_matches_its_built_artifact() {
    let directory = temporary_directory();
    let package = directory.join("trap-package");
    let artifact = directory.join("trap-package.allenb");
    std::fs::create_dir_all(package.join("src")).unwrap();
    std::fs::write(
        package.join("allen.toml"),
        r#"[package]
name = "trap-package"
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
    std::fs::write(package.join("src/main.allen"), CONTRACT_TRAP_BODY).unwrap();
    frontend_lock(&package);
    frontend_build(&package, &artifact);
    for input in [&package, &artifact] {
        let output = frontend_run_with_trace(input);
        assert!(!output.status.success(), "{}", input.display());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        let actual_trace = stderr
            .lines()
            .filter(|line| line.starts_with("task_event "))
            .collect::<Vec<_>>();
        assert_eq!(actual_trace, CONTRACT_TRAP_TRACE, "{}", input.display());
        assert!(
            stderr.contains("runtime error[arithmetic.division_by_zero]"),
            "{stderr}"
        );
    }
    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn closed_errors_oversized_stop_reason_is_omitted_without_changing_the_terminal_channel() {
    let directory = temporary_directory();
    let source = directory.join("stop-oversized.allen");
    let artifact = directory.join("stop-oversized.allenb");
    let reason = "é".repeat(513);
    std::fs::write(
        &source,
        format!(
            "manifest {{\n  language: \"0.1\"\n  entry: main\n  capabilities: []\n}}\nexport fn main() returns Void {{ stop(\"{reason}\") }}\n"
        ),
    )
    .unwrap();
    frontend_build(&source, &artifact);
    for input in [&source, &artifact] {
        let output = frontend_run(input);
        assert!(output.status.success(), "{}", input.display());
        assert_eq!(output.stdout, b"stopped\n");
        assert!(output.stderr.is_empty());
    }
    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn structured_concurrency_stop_trace_is_redacted_and_not_a_failure() {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg("--trace-tasks")
        .arg(named_example("stop.allen"))
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "stopped: \"done\"\n"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "task_event sequence=1 task_id=0 owner_id=0 kind=spawned\n\
task_event sequence=2 task_id=0 owner_id=0 kind=stopped\n"
    );
}

#[test]
fn structured_concurrency_stop_escapes_untrusted_terminal_text() {
    let directory = temporary_directory();
    let source = directory.join("stop-control.allen");
    std::fs::write(
        &source,
        "export fn main() returns Void { stop(\"done\\nforged:\\tsuccess\") }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(&source)
        .output()
        .expect("CLI must start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "stopped: \"done\\nforged:\\tsuccess\"\n"
    );
    assert!(output.stderr.is_empty());
    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn loaded_artifacts_cover_existing_control_and_errors() {
    let directory = temporary_directory();
    for (source_name, expected_stdout, expected_error) in [
        (
            "data-types.allen",
            "[{\"x\":1,\"y\":2},{\"tag\":\"Named\",\"value\":{\"label\":\"cpu\",\"value\":7}},3,7,\"yes\",{\"tag\":\"Some\",\"value\":7},{\"tag\":\"Ok\",\"value\":8},8,{\"tag\":\"Some\",\"value\":{\"x\":1,\"y\":2}},{\"tag\":\"None\"},true]\n",
            None,
        ),
        ("result-ok.allen", "{\"tag\":\"Ok\",\"value\":42}\n", None),
        (
            "result-err.allen",
            "{\"tag\":\"Err\",\"value\":{\"code\":7,\"message\":\"failed\"}}\n",
            None,
        ),
        (
            "overflow.allen",
            "",
            Some("runtime error[arithmetic.overflow]: arithmetic overflow\n"),
        ),
    ] {
        let artifact = directory.join(format!("{source_name}.allenb"));
        let built = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("build")
            .arg(named_example(source_name))
            .arg("-o")
            .arg(&artifact)
            .output()
            .expect("CLI must start");
        assert!(built.status.success(), "{source_name}");

        let executed = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("run")
            .arg(&artifact)
            .output()
            .expect("CLI must start");
        assert_eq!(
            String::from_utf8(executed.stdout).unwrap(),
            expected_stdout,
            "{source_name}"
        );
        let stderr = String::from_utf8(executed.stderr).unwrap();
        if let Some(expected_error) = expected_error {
            assert!(!executed.status.success(), "{source_name}");
            assert_eq!(stderr, expected_error, "{source_name}");
        } else {
            assert!(executed.status.success(), "{source_name}: {stderr}");
            assert!(stderr.is_empty(), "{source_name}: {stderr}");
        }
    }
    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
#[allow(clippy::too_many_lines)]
fn package_lock_source_artifact_and_inspect_are_deterministic() {
    let directory = temporary_directory();
    let package = directory.join("package");
    copy_directory(&named_example("filesystem-package"), &package);
    let workspace = directory.join("work");
    std::fs::create_dir(&workspace).unwrap();
    let input = package.join("input.json");

    let first_lock = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("lock")
        .arg(&package)
        .output()
        .unwrap();
    assert!(
        first_lock.status.success(),
        "{}",
        String::from_utf8_lossy(&first_lock.stderr)
    );
    let lock = std::fs::read(package.join("allen.lock")).unwrap();
    let second_lock = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("lock")
        .arg(&package)
        .output()
        .unwrap();
    assert!(second_lock.status.success());
    assert_eq!(std::fs::read(package.join("allen.lock")).unwrap(), lock);

    let source_run = Command::new(env!("CARGO_BIN_EXE_allen"))
        .args(["run", "--entry", "main", "--input"])
        .arg(&input)
        .arg("--workdir")
        .arg(&workspace)
        .arg(&package)
        .output()
        .unwrap();
    assert!(
        source_run.status.success(),
        "{}",
        String::from_utf8_lossy(&source_run.stderr)
    );
    assert_eq!(
        source_run.stdout,
        b"{\"tag\":\"Ok\",\"value\":\"hello from ALLEN\"}\n"
    );

    let pure_entry = Command::new(env!("CARGO_BIN_EXE_allen"))
        .args(["run", "--entry", "status"])
        .arg("--workdir")
        .arg(&workspace)
        .arg(&package)
        .output()
        .unwrap();
    assert!(
        pure_entry.status.success(),
        "{}",
        String::from_utf8_lossy(&pure_entry.stderr)
    );
    assert_eq!(pure_entry.stdout, b"\"ready\"\n");

    let first = directory.join("first.allenb");
    let second = directory.join("second.allenb");
    for artifact in [&first, &second] {
        let built = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("build")
            .arg(&package)
            .arg("-o")
            .arg(artifact)
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
    }
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap()
    );
    assert_eq!(
        &std::fs::read(&first).unwrap()[10..12],
        &13_u16.to_le_bytes()
    );

    let artifact_run = Command::new(env!("CARGO_BIN_EXE_allen"))
        .args(["run", "--entry", "main", "--input"])
        .arg(&input)
        .arg("--workdir")
        .arg(&workspace)
        .arg(&first)
        .output()
        .unwrap();
    assert!(artifact_run.status.success());
    assert_eq!(artifact_run.stdout, source_run.stdout);

    let inspected = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("inspect")
        .arg(&first)
        .output()
        .unwrap();
    assert!(inspected.status.success());
    let report = String::from_utf8(inspected.stdout).unwrap();
    assert!(report.starts_with("bytecode_version: 13\n"));
    assert!(report.contains("section.tools: 0\n"));
    assert!(report.contains("manifest.package: filesystem-example@0.1.0\n"));
    assert!(report.contains("contract.entry.main: function=0 input_schema=0 output_schema=1\n"));
    assert!(report.contains(
        "contract.import.filesystem-example@0.1.0:text_utils: text-utils@1.0.0 src/text.allen "
    ));

    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn http_source_and_artifact_apply_the_same_origin_and_address_policy() {
    let directory = temporary_directory();
    let source = directory.join("http.allen");
    let artifact = directory.join("http.allenb");
    std::fs::write(
        &source,
        r#"manifest {
  language: "0.1"
  entry: main
  capabilities: [net.http_get]
  http_origins: ["https://192.0.2.1"]
}
export async fn main() returns Result<HttpResponse, NetworkError>
  effects [net.http_get] {
  await http.get("https://192.0.2.1/data")
}
"#,
    )
    .unwrap();

    let built = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&artifact)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let inspected = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("inspect")
        .arg(&artifact)
        .output()
        .unwrap();
    assert!(inspected.status.success());
    let report = String::from_utf8(inspected.stdout).unwrap();
    assert!(report.starts_with("bytecode_version: 13\n"));
    assert!(report.contains("manifest.https_origins: [https://192.0.2.1]\n"));

    let mut outputs = Vec::new();
    for input in [&source, &artifact] {
        let denied = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("run")
            .arg(input)
            .output()
            .unwrap();
        assert!(!denied.status.success());
        assert_eq!(
            String::from_utf8(denied.stderr).unwrap(),
            "runtime error[runtime.capability_denied]: required HTTP capability has no effective origin\n"
        );

        let executed = Command::new(env!("CARGO_BIN_EXE_allen"))
            .args(["run", "--allow-net-origin", "https://192.0.2.1"])
            .arg(input)
            .output()
            .unwrap();
        assert!(
            executed.status.success(),
            "{}",
            String::from_utf8_lossy(&executed.stderr)
        );
        assert!(executed.stderr.is_empty());
        outputs.push(executed.stdout);
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(
        outputs[0],
        b"{\"tag\":\"Err\",\"value\":{\"code\":\"net.destination_denied\",\"message\":\"the destination address is denied\"}}\n"
    );

    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn standalone_grant_request_uses_the_current_result_contract() {
    let directory = temporary_directory();
    let external = directory.join("external");
    let workspace = directory.join("work");
    let source = directory.join("grant.allen");
    let artifact = directory.join("grant.allenb");
    std::fs::create_dir(&external).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let external = std::fs::canonicalize(external).unwrap();
    let program = r#"manifest {
  language: "0.1"
  entry: main
  capabilities: [fs.read(workdir), permission.request_external_fs]
}
export async fn main() returns Result<List<String>, FileError>
  effects [fs.read, permission.request_external_fs] {
  let grant = (await permission.request_directory({
    access: ExternalFsAccess.Read,
    path: "EXTERNAL_PATH",
    reason: "Read the selected directory.",
    recursive: false
  }))?;
  let names = (await fs.list(grant, "."))?;
  Ok(names)
}
"#
    .replace("EXTERNAL_PATH", external.to_str().unwrap());
    std::fs::write(&source, program).unwrap();

    let built = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&artifact)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );

    let mut outputs = Vec::new();
    for input in [&source, &artifact] {
        let executed = Command::new(env!("CARGO_BIN_EXE_allen"))
            .args(["run", "--workdir"])
            .arg(&workspace)
            .arg(input)
            .output()
            .unwrap();
        assert!(
            executed.status.success(),
            "{} failed: {}",
            input.display(),
            String::from_utf8_lossy(&executed.stderr)
        );
        assert!(executed.stderr.is_empty());
        outputs.push(executed.stdout);
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(
        outputs[0],
        b"{\"tag\":\"Err\",\"value\":{\"code\":\"permission.unavailable\",\"message\":\"the external filesystem grant provider is unavailable\"}}\n"
    );

    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn package_preflight_rejects_invalid_input_missing_workdir_and_stale_lock() {
    let directory = temporary_directory();
    let package = directory.join("package");
    copy_directory(&named_example("filesystem-package"), &package);
    let workspace = directory.join("work");
    std::fs::create_dir(&workspace).unwrap();
    let invalid = directory.join("invalid.json");
    std::fs::write(&invalid, "7\n").unwrap();

    let no_workspace = Command::new(env!("CARGO_BIN_EXE_allen"))
        .args(["run", "--entry", "main", "--input"])
        .arg(package.join("input.json"))
        .arg(&package)
        .output()
        .unwrap();
    assert!(!no_workspace.status.success());
    assert!(
        String::from_utf8(no_workspace.stderr)
            .unwrap()
            .contains("require --workdir")
    );

    let bad_input = Command::new(env!("CARGO_BIN_EXE_allen"))
        .args(["run", "--entry", "main", "--input"])
        .arg(&invalid)
        .arg("--workdir")
        .arg(&workspace)
        .arg(&package)
        .output()
        .unwrap();
    assert!(!bad_input.status.success());
    assert!(
        String::from_utf8(bad_input.stderr)
            .unwrap()
            .contains("runtime.invalid_input")
    );
    assert!(!workspace.join("message.txt").exists());

    let source = package.join("src/support.allen");
    std::fs::write(
        &source,
        "export fn prepare(input: String) returns String { \"changed\" }\n",
    )
    .unwrap();
    let stale = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(&package)
        .output()
        .unwrap();
    assert!(!stale.status.success());
    assert!(
        String::from_utf8(stale.stderr)
            .unwrap()
            .contains("lock.mismatch")
    );

    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn package_inline_manifest_executes_with_an_explicit_workspace() {
    let directory = temporary_directory();
    std::fs::write(directory.join("message.txt"), "inline\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg("--workdir")
        .arg(&directory)
        .arg(named_example("filesystem-inline.allen"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"{\"tag\":\"Ok\",\"value\":\"inline\\n\"}\n");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn package_rejects_conflicting_inline_and_file_manifests() {
    let directory = temporary_directory();
    let source = directory.join("main.allen");
    std::fs::copy(named_example("filesystem-inline.allen"), &source).unwrap();
    std::fs::write(
        directory.join("allen.toml"),
        "[package]\nname = \"conflict\"\nversion = \"0.1.0\"\nlanguage = \"^0.1\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(&source)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "{}: error: inline manifest conflicts with allen.toml\n",
            source.display()
        )
    );
    std::fs::remove_dir_all(directory).unwrap();
}

fn frontend_fixture(name: &str) -> PathBuf {
    fixture(&format!("frontend/{name}"))
}

fn comments_fixture(name: &str) -> PathBuf {
    fixture(&format!("comments/{name}"))
}

fn control_flow_fixture(name: &str) -> PathBuf {
    fixture(&format!("control-flow/{name}"))
}

fn loops_fixture(name: &str) -> PathBuf {
    fixture(&format!("loops/{name}"))
}

fn operators_fixture(name: &str) -> PathBuf {
    fixture(&format!("operators/{name}"))
}

fn strings_fixture(name: &str) -> PathBuf {
    fixture(&format!("strings/{name}"))
}

fn frontend_check(path: &std::path::Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(path)
        .output()
        .expect("CLI must start");
    assert!(
        output.status.success(),
        "{}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

fn frontend_run(path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg(path)
        .output()
        .expect("CLI must start")
}

fn frontend_run_with_trace(path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg("--trace-tasks")
        .arg(path)
        .output()
        .expect("CLI must start")
}

fn frontend_build(path: &std::path::Path, output: &std::path::Path) {
    let built = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("build")
        .arg(path)
        .arg("-o")
        .arg(output)
        .output()
        .expect("CLI must start");
    assert!(
        built.status.success(),
        "{}: {}",
        path.display(),
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(built.stdout.is_empty());
    assert!(built.stderr.is_empty());
}

fn frontend_lock(package: &std::path::Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("lock")
        .arg(package)
        .output()
        .expect("CLI must start");
    assert!(
        output.status.success(),
        "{}: {}",
        package.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

fn frontend_assert_diagnostic(
    path: &std::path::Path,
    output: std::process::Output,
    line: usize,
    column: usize,
    category: &str,
) {
    assert!(!output.status.success(), "{} must fail", path.display());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("diagnostic must be UTF-8");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.contains(&format!(":{line}:{column}: error[")),
        "diagnostic must retain its source location: {stderr}"
    );
    assert!(
        stderr.contains(category),
        "diagnostic must retain the {category:?} category: {stderr}"
    );
}

#[test]
fn frontend_cross_feature_fixture_checks_builds_and_runs_with_current_source_artifact_parity() {
    let source = frontend_fixture("cross-feature/main.allen");
    let directory = temporary_directory();
    let first = directory.join("first.allenb");
    let second = directory.join("second.allenb");

    frontend_check(&source);
    let source_run = frontend_run(&source);
    assert!(
        source_run.status.success(),
        "{}",
        String::from_utf8_lossy(&source_run.stderr)
    );
    assert_eq!(
        source_run.stdout,
        b"[-18.5,{\"$bytes\":\"MjI=\"},65,\"pair\",2,7,true]\n"
    );
    assert!(source_run.stderr.is_empty());

    frontend_build(&source, &first);
    frontend_build(&source, &second);
    let first_bytes = std::fs::read(&first).expect("first artifact must exist");
    assert_eq!(first_bytes, std::fs::read(&second).unwrap());
    assert_eq!(&first_bytes[10..12], &13_u16.to_le_bytes());

    let artifact_run = frontend_run(&first);
    assert!(
        artifact_run.status.success(),
        "{}",
        String::from_utf8_lossy(&artifact_run.stderr)
    );
    assert_eq!(artifact_run.stdout, source_run.stdout);
    assert_eq!(artifact_run.stderr, source_run.stderr);

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn frontend_cross_mode_sources_have_the_same_value_and_deterministic_artifacts() {
    let directory = temporary_directory();
    let package = directory.join("package");
    copy_directory(&frontend_fixture("parity/package"), &package);
    frontend_lock(&package);
    let modes = [
        frontend_fixture("parity/loose.allen"),
        frontend_fixture("parity/modules/main.allen"),
        frontend_fixture("parity/inline.allen"),
        package.clone(),
    ];
    let mut outputs = Vec::new();

    for (index, source) in modes.iter().enumerate() {
        frontend_check(source);
        let source_run = frontend_run(source);
        assert!(
            source_run.status.success(),
            "{}: {}",
            source.display(),
            String::from_utf8_lossy(&source_run.stderr)
        );
        assert_eq!(source_run.stdout, b"8\n", "{}", source.display());
        assert!(source_run.stderr.is_empty());

        let first = directory.join(format!("mode-{index}-first.allenb"));
        let second = directory.join(format!("mode-{index}-second.allenb"));
        frontend_build(source, &first);
        frontend_build(source, &second);
        let first_bytes = std::fs::read(&first).unwrap();
        assert_eq!(first_bytes, std::fs::read(&second).unwrap());
        assert_eq!(&first_bytes[10..12], &13_u16.to_le_bytes());

        let artifact_run = frontend_run(&first);
        assert!(
            artifact_run.status.success(),
            "{}: {}",
            first.display(),
            String::from_utf8_lossy(&artifact_run.stderr)
        );
        assert_eq!(artifact_run.stdout, source_run.stdout);
        assert_eq!(artifact_run.stderr, source_run.stderr);
        outputs.push(source_run.stdout);
    }
    assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn frontend_exported_omitted_effects_is_pure_in_every_mode() {
    let directory = temporary_directory();
    let package = directory.join("package");
    copy_directory(
        &frontend_fixture("parity/missing-effects-package"),
        &package,
    );
    frontend_lock(&package);
    for (source, line) in [
        (frontend_fixture("parity/missing-effects-loose.allen"), 1),
        (
            frontend_fixture("parity/missing-effects-modules/main.allen"),
            3,
        ),
        (frontend_fixture("parity/missing-effects-inline.allen"), 7),
        (package.clone(), 1),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("check")
            .arg(&source)
            .output()
            .expect("CLI must start");
        assert!(
            output.status.success(),
            "{}:{}: {}",
            source.display(),
            line,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn frontend_imported_module_diagnostic_uses_the_imported_path_and_source() {
    let directory = temporary_directory();
    let main = directory.join("main.allen");
    let support = directory.join("support.allen");
    std::fs::write(
        &main,
        "import { answer } from \"./support.allen\";\n\nexport fn main() returns Int { answer() }\n",
    )
    .unwrap();
    std::fs::write(
        &support,
        "export fn answer() returns Int {\n  \"wrong\"\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(&main)
        .output()
        .expect("CLI must start");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with(&format!("{}:2:3: error[", support.display())),
        "{stderr}"
    );
    assert!(stderr.contains("expected Int, found String"), "{stderr}");

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn frontend_mut_succeeds_in_capability_free_ordinary_module_and_async_functions() {
    for source in [
        frontend_fixture("mut/ordinary.allen"),
        frontend_fixture("mut/modules/main.allen"),
        frontend_fixture("mut/async.allen"),
    ] {
        frontend_check(&source);
        let output = frontend_run(&source);
        assert!(
            output.status.success(),
            "{}: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"2\n", "{}", source.display());
        assert!(output.stderr.is_empty());
    }

    let effectful = frontend_fixture("mut/effectful.allen");
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(&effectful)
        .output()
        .expect("CLI must start");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "{}: error: inline manifest does not declare entry effect 'fs.read'\n",
            effectful.display()
        )
    );
}

#[test]
fn frontend_mut_rejections_are_source_located_and_categorized() {
    for (name, line, column, category) in [
        ("mut/let-mut.allen", 2, 7, "mut"),
        ("mut/immutable-assignment.allen", 3, 3, "immutable"),
        (
            "mut/different-type-assignment.allen",
            3,
            11,
            "expected Int, found String",
        ),
        ("mut/use-before-declare.allen", 2, 3, "unknown local value"),
        ("mut/duplicate.allen", 3, 7, "duplicate local binding"),
    ] {
        let source = frontend_fixture(name);
        let output = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("check")
            .arg(&source)
            .output()
            .expect("CLI must start");
        frontend_assert_diagnostic(&source, output, line, column, category);
    }
}

#[test]
fn frontend_dispatch_words_in_strings_and_identifiers_do_not_change_frontend_behavior() {
    let directory = temporary_directory();
    let baseline = directory.join("baseline.allen");
    let canary = directory.join("canary.allen");
    std::fs::write(
        &baseline,
        "export fn main() returns String { \"stable\" }\n",
    )
    .unwrap();
    std::fs::write(
        &canary,
        "export fn main() returns String {\n  let marker = \"import effects async await spawn stop\";\n  \"stable\"\n}\n",
    )
    .unwrap();

    for source in [&baseline, &canary] {
        frontend_check(source);
        let output = frontend_run(source);
        assert!(
            output.status.success(),
            "{}: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"\"stable\"\n");
        assert!(output.stderr.is_empty());
    }

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

fn comments_executable_contract(bytes: &[u8]) -> allen_bytecode::Artifact {
    let mut artifact = allen_bytecode::decode(bytes, &allen_bytecode::DecodeLimits::default())
        .expect("artifact must decode")
        .into_artifact();
    artifact.debug = None;
    artifact
}

#[test]
fn comments_preserve_cli_and_executable_contracts_in_every_source_mode() {
    let directory = temporary_directory();
    let clean_loose = directory.join("loose-clean/main.allen");
    let commented_loose = directory.join("loose-commented/main.allen");
    let clean_inline = directory.join("inline-clean/main.allen");
    let commented_inline = directory.join("inline-commented/main.allen");
    for (source, target) in [
        (comments_fixture("parity/loose-clean.allen"), &clean_loose),
        (
            comments_fixture("parity/loose-commented.allen"),
            &commented_loose,
        ),
        (comments_fixture("parity/inline-clean.allen"), &clean_inline),
        (
            comments_fixture("parity/inline-commented.allen"),
            &commented_inline,
        ),
    ] {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(source, target).unwrap();
    }
    let clean_package = directory.join("package-clean");
    let commented_package = directory.join("package-commented");
    copy_directory(&comments_fixture("parity/package-clean"), &clean_package);
    copy_directory(
        &comments_fixture("parity/package-commented"),
        &commented_package,
    );
    frontend_lock(&clean_package);
    frontend_lock(&commented_package);
    let modes = [
        (clean_loose, commented_loose),
        (
            comments_fixture("parity/modules-clean/main.allen"),
            comments_fixture("parity/modules-commented/main.allen"),
        ),
        (clean_inline, commented_inline),
        (clean_package, commented_package),
    ];

    for (index, (clean, commented)) in modes.iter().enumerate() {
        frontend_check(clean);
        frontend_check(commented);
        let clean_run = frontend_run(clean);
        let commented_run = frontend_run(commented);
        for (path, output) in [(clean, &clean_run), (commented, &commented_run)] {
            assert!(
                output.status.success(),
                "{}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"8\n", "{}", path.display());
            assert!(output.stderr.is_empty(), "{}", path.display());
        }
        assert_eq!(commented_run.stdout, clean_run.stdout);

        let clean_artifact = directory.join(format!("mode-{index}-clean.allenb"));
        let commented_first = directory.join(format!("mode-{index}-commented-first.allenb"));
        let commented_second = directory.join(format!("mode-{index}-commented-second.allenb"));
        frontend_build(clean, &clean_artifact);
        frontend_build(commented, &commented_first);
        frontend_build(commented, &commented_second);
        let clean_bytes = std::fs::read(&clean_artifact).expect("clean artifact must exist");
        let commented_bytes =
            std::fs::read(&commented_first).expect("commented artifact must exist");
        assert_eq!(
            commented_bytes,
            std::fs::read(&commented_second).unwrap(),
            "commented artifact must remain deterministic: {}",
            commented.display()
        );
        assert_eq!(
            comments_executable_contract(&commented_bytes),
            comments_executable_contract(&clean_bytes),
            "comments may change debug offsets but not executable contracts: {}",
            commented.display()
        );

        let artifact_run = frontend_run(&commented_first);
        assert!(
            artifact_run.status.success(),
            "{}: {}",
            commented_first.display(),
            String::from_utf8_lossy(&artifact_run.stderr)
        );
        assert_eq!(artifact_run.stdout, commented_run.stdout);
        assert_eq!(artifact_run.stderr, commented_run.stderr);
    }

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn comments_cli_reports_exact_comment_diagnostics_and_rejects_invalid_utf8_before_parsing() {
    let unterminated = comments_fixture("diagnostics/unterminated-multibyte.allen");
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(&unterminated)
        .output()
        .expect("CLI must start");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "{}:2:1: error[E0005]: unterminated block comment\n",
            unterminated.display()
        )
    );

    let directory = temporary_directory();
    for (name, terminator) in [("lf", "\n"), ("crlf", "\r\n"), ("cr", "\r")] {
        let source = directory.join(format!("unterminated-{name}.allen"));
        std::fs::write(&source, format!("// first line{terminator}/*"))
            .expect("line-ending fixture must be written");
        let output = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("check")
            .arg(&source)
            .output()
            .expect("CLI must start");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!(
                "{}:2:1: error[E0005]: unterminated block comment\n",
                source.display()
            )
        );
    }

    let nesting = directory.join("over-nested.allen");
    std::fs::write(&nesting, "/*".repeat(129)).expect("fixture must be written");
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(&nesting)
        .output()
        .expect("CLI must start");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "{}:1:257: error[E0005]: block comment nesting exceeds the limit of 128\n",
            nesting.display()
        )
    );

    let eof_line_comment = directory.join("line-comment-at-eof.allen");
    std::fs::write(
        &eof_line_comment,
        "export fn main() returns Int { 8 } // no trailing newline",
    )
    .expect("end-of-file comment fixture must be written");
    frontend_check(&eof_line_comment);
    let output = frontend_run(&eof_line_comment);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"8\n");
    assert!(output.stderr.is_empty());

    let invalid_utf8 = directory.join("invalid-utf8.allen");
    std::fs::write(&invalid_utf8, b"// comment before invalid UTF-8: \xff\n")
        .expect("invalid UTF-8 fixture must be written");
    let output = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg(&invalid_utf8)
        .output()
        .expect("CLI must start");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "{}: error: cannot read source file: stream did not contain valid UTF-8\n",
            invalid_utf8.display()
        )
    );

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn control_flow_has_source_artifact_parity_in_every_source_mode() {
    let directory = temporary_directory();
    let package = directory.join("package");
    copy_directory(&control_flow_fixture("parity/package"), &package);
    frontend_lock(&package);
    let modes = [
        control_flow_fixture("parity/loose.allen"),
        control_flow_fixture("parity/modules/main.allen"),
        control_flow_fixture("parity/inline.allen"),
        package,
    ];

    for (index, source) in modes.iter().enumerate() {
        frontend_check(source);
        let source_run = frontend_run(source);
        assert!(
            source_run.status.success(),
            "{}: {}",
            source.display(),
            String::from_utf8_lossy(&source_run.stderr)
        );
        assert_eq!(source_run.stdout, b"30\n", "{}", source.display());
        assert!(source_run.stderr.is_empty(), "{}", source.display());

        let first = directory.join(format!("control-flow-mode-{index}-first.allenb"));
        let second = directory.join(format!("control-flow-mode-{index}-second.allenb"));
        frontend_build(source, &first);
        frontend_build(source, &second);
        let first_bytes = std::fs::read(&first).expect("first artifact must exist");
        assert_eq!(first_bytes, std::fs::read(&second).unwrap());
        assert_eq!(&first_bytes[10..12], &13_u16.to_le_bytes());

        let artifact_run = frontend_run(&first);
        assert!(
            artifact_run.status.success(),
            "{}: {}",
            first.display(),
            String::from_utf8_lossy(&artifact_run.stderr)
        );
        assert_eq!(artifact_run.stdout, source_run.stdout);
        assert_eq!(artifact_run.stderr, source_run.stderr);
    }

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn control_flow_async_returns_never_branches_and_task_cleanup_execute() {
    for (name, expected) in [
        ("bare-return-void.allen", b"null\n".as_slice()),
        ("async-control.allen", b"7\n".as_slice()),
        ("ownership-both-branches.allen", b"42\n".as_slice()),
        ("ownership-moved-in-condition.allen", b"1\n".as_slice()),
        ("ownership-never-branch.allen", b"42\n".as_slice()),
        ("await-scope-return.allen", b"7\n".as_slice()),
    ] {
        let source = control_flow_fixture(name);
        frontend_check(&source);
        let output = frontend_run(&source);
        assert!(
            output.status.success(),
            "{}: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, expected, "{}", source.display());
        assert!(output.stderr.is_empty(), "{}", source.display());
    }
}

#[test]
fn control_flow_skipped_branch_does_not_trap_or_stop() {
    let directory = temporary_directory();
    let source = directory.join("skipped.allen");
    std::fs::write(
        &source,
        "export fn main() returns Int {\n  if (false) { 1 / 0 } else { 42 }\n}\n",
    )
    .unwrap();

    frontend_check(&source);
    let output = frontend_run(&source);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"42\n");
    assert!(output.stderr.is_empty());

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn control_flow_rejections_are_single_source_located_diagnostics() {
    for (name, line, column, category) in [
        ("diagnostics/non-bool-condition.allen", 2, 7, "Bool"),
        ("diagnostics/missing-else-value.allen", 2, 3, "expected Int"),
        (
            "diagnostics/mismatched-branches.allen",
            2,
            13,
            "exact result type",
        ),
        (
            "diagnostics/non-void-true-branch.allen",
            2,
            13,
            "Void true branch",
        ),
        (
            "diagnostics/value-conditional-statement.allen",
            2,
            3,
            "Void",
        ),
        ("diagnostics/bare-return-non-void.allen", 2, 3, "Void"),
        (
            "diagnostics/branch-local-escape.allen",
            5,
            3,
            "unknown local value",
        ),
        (
            "diagnostics/arbitrary-expression-statement.allen",
            3,
            4,
            "expected",
        ),
        ("ownership-one-branch.allen", 7, 3, "ownership state"),
    ] {
        let source = control_flow_fixture(name);
        let output = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("check")
            .arg(&source)
            .output()
            .expect("CLI must start");
        frontend_assert_diagnostic(&source, output, line, column, category);
    }
}

#[test]
fn loops_have_source_artifact_parity_in_every_source_mode() {
    let directory = temporary_directory();
    let package = directory.join("package");
    copy_directory(&loops_fixture("parity/package"), &package);
    frontend_lock(&package);
    let modes = [
        loops_fixture("parity/loose.allen"),
        loops_fixture("parity/modules/main.allen"),
        loops_fixture("parity/inline.allen"),
        package,
    ];

    for (index, source) in modes.iter().enumerate() {
        frontend_check(source);
        let source_run = frontend_run(source);
        assert!(
            source_run.status.success(),
            "{}: {}",
            source.display(),
            String::from_utf8_lossy(&source_run.stderr)
        );
        assert_eq!(source_run.stdout, b"12\n", "{}", source.display());
        assert!(source_run.stderr.is_empty(), "{}", source.display());

        let first = directory.join(format!("loop-mode-{index}-first.allenb"));
        let second = directory.join(format!("loop-mode-{index}-second.allenb"));
        frontend_build(source, &first);
        frontend_build(source, &second);
        let first_bytes = std::fs::read(&first).expect("first artifact must exist");
        assert_eq!(first_bytes, std::fs::read(&second).unwrap());
        assert_eq!(&first_bytes[10..12], &13_u16.to_le_bytes());

        let inspected = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("inspect")
            .arg(&first)
            .output()
            .expect("CLI must start");
        assert!(inspected.status.success());
        assert!(inspected.stderr.is_empty());
        assert!(
            String::from_utf8(inspected.stdout)
                .unwrap()
                .starts_with("bytecode_version: 13\n")
        );

        let artifact_run = frontend_run(&first);
        assert!(
            artifact_run.status.success(),
            "{}: {}",
            first.display(),
            String::from_utf8_lossy(&artifact_run.stderr)
        );
        assert_eq!(artifact_run.stdout, source_run.stdout);
        assert_eq!(artifact_run.stderr, source_run.stderr);
    }

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn loops_loop_forms_cover_sequences_ranges_snapshots_and_nested_control() {
    let source = loops_fixture("loop-behavior.allen");
    frontend_check(&source);
    let output = frontend_run(&source);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        b"[10,10,3,3,6,258,12,3,0,-2,9223372036854775806,6]\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn loops_loop_ownership_and_skipped_effects_execute() {
    for (name, expected) in [
        ("await-before-back-edge.allen", b"21\n".as_slice()),
        ("return-transfer.allen", b"7\n".as_slice()),
        ("await-scope-loop-return.allen", b"7\n".as_slice()),
        ("skipped-loop-effect.allen", b"null\n".as_slice()),
    ] {
        let source = loops_fixture(name);
        frontend_check(&source);
        let output = frontend_run(&source);
        assert!(
            output.status.success(),
            "{}: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, expected, "{}", source.display());
        assert!(output.stderr.is_empty(), "{}", source.display());
    }

    let source = loops_fixture("skipped-loop-effect.allen");
    let effects = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("check")
        .arg("--show-effects")
        .arg(&source)
        .output()
        .expect("CLI must start");
    assert!(effects.status.success());
    assert!(effects.stderr.is_empty());
    assert!(
        String::from_utf8(effects.stdout)
            .unwrap()
            .contains("effects [task.spawn]")
    );
}

#[test]
fn loops_rejections_are_single_source_located_diagnostics() {
    for (name, line, column, category) in [
        ("diagnostics/break-outside.allen", 2, 3, "loop"),
        ("diagnostics/continue-outside.allen", 2, 3, "loop"),
        ("diagnostics/closure-break.allen", 3, 40, "loop"),
        ("diagnostics/non-bool-while.allen", 2, 10, "Bool"),
        ("diagnostics/non-iterable.allen", 2, 15, "iterable"),
        ("diagnostics/tuple-arity.allen", 2, 7, "tuple"),
        ("diagnostics/tuple-non-tuple.allen", 2, 7, "tuple"),
        (
            "diagnostics/duplicate-binding.allen",
            2,
            15,
            "duplicate loop binding",
        ),
        (
            "diagnostics/binding-escapes.allen",
            3,
            3,
            "unknown local value",
        ),
        ("diagnostics/task-lost-continue.allen", 6, 5, "affine"),
        ("diagnostics/task-lost-break.allen", 6, 5, "affine"),
        ("diagnostics/task-lost-back-edge.allen", 6, 3, "affine"),
    ] {
        let source = loops_fixture(name);
        let output = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("check")
            .arg(&source)
            .output()
            .expect("CLI must start");
        frontend_assert_diagnostic(&source, output, line, column, category);
    }
}

#[test]
fn loops_loop_body_value_tails_are_exact_source_located_diagnostics() {
    for name in [
        "diagnostics/while-body-value.allen",
        "diagnostics/loop-body-value.allen",
        "diagnostics/for-body-value.allen",
        "diagnostics/loop-body-conditional-value.allen",
    ] {
        let source = loops_fixture(name);
        let output = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("check")
            .arg(&source)
            .output()
            .expect("CLI must start");
        assert!(!output.status.success(), "{} must fail", source.display());
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).expect("diagnostic must be UTF-8"),
            format!(
                "{}:3:5: error[E3007]: loop body must have type Void or Never, found Int\n",
                source.display()
            )
        );
    }
}

#[test]
fn operators_operators_have_source_artifact_parity_in_every_source_mode() {
    let directory = temporary_directory();
    let package = directory.join("package");
    copy_directory(&operators_fixture("parity/package"), &package);
    frontend_lock(&package);
    let modes = [
        operators_fixture("parity/loose.allen"),
        operators_fixture("parity/modules/main.allen"),
        operators_fixture("parity/inline.allen"),
        package,
    ];

    for (index, source) in modes.iter().enumerate() {
        frontend_check(source);
        let source_run = frontend_run(source);
        assert!(
            source_run.status.success(),
            "{}: {}",
            source.display(),
            String::from_utf8_lossy(&source_run.stderr)
        );
        assert_eq!(source_run.stdout, b"118\n", "{}", source.display());
        assert!(source_run.stderr.is_empty(), "{}", source.display());

        let first = directory.join(format!("operator-mode-{index}-first.allenb"));
        let second = directory.join(format!("operator-mode-{index}-second.allenb"));
        frontend_build(source, &first);
        frontend_build(source, &second);
        let first_bytes = std::fs::read(&first).expect("first artifact must exist");
        assert_eq!(first_bytes, std::fs::read(&second).unwrap());
        assert_eq!(&first_bytes[10..12], &13_u16.to_le_bytes());

        let inspected = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("inspect")
            .arg(&first)
            .output()
            .expect("CLI must start");
        assert!(inspected.status.success());
        assert!(inspected.stderr.is_empty());
        assert!(
            String::from_utf8(inspected.stdout)
                .unwrap()
                .starts_with("bytecode_version: 13\n")
        );

        let artifact_run = frontend_run(&first);
        assert!(
            artifact_run.status.success(),
            "{}: {}",
            first.display(),
            String::from_utf8_lossy(&artifact_run.stderr)
        );
        assert_eq!(artifact_run.stdout, source_run.stdout);
        assert_eq!(artifact_run.stderr, source_run.stderr);
    }

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn operators_remainder_semantics_short_circuit_and_errors_are_visible_through_the_cli() {
    let directory = temporary_directory();
    let semantics = directory.join("operators.allen");
    std::fs::write(
        &semantics,
        r"export fn main() returns (Int, Int, Int, Int, Int, Bool, Bool) {
  let skipped_and = false && ((1 % 0) == 0);
  let skipped_or = true || ((1 % 0) == 0);
  (5 % 2, -5 % 2, 5 % -2, -5 % -2, -9223372036854775808 % -1, skipped_and, skipped_or)
}
",
    )
    .unwrap();
    frontend_check(&semantics);
    let output = frontend_run(&semantics);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"[1,-1,1,-1,0,false,true]\n");
    assert!(output.stderr.is_empty());

    for (name, source, expected) in [
        (
            "remainder-by-zero",
            "export fn main() returns Int { 1 % 0 }\n",
            "runtime error[arithmetic.division_by_zero]: division by zero\n",
        ),
        (
            "float-remainder",
            "export fn main() returns Float { 1.0 % 2.0 }\n",
            "remainder requires Int operands",
        ),
        (
            "immutable-compound",
            "export fn main() returns Int { let value = 1; value += 1; value }\n",
            "immutable",
        ),
        (
            "float-remainder-compound",
            "export fn main() returns Float { mut value: Float = 1.0; value %= 2; value }\n",
            "remainder compound assignment requires Int",
        ),
    ] {
        let path = directory.join(format!("{name}.allen"));
        std::fs::write(&path, source).unwrap();
        let output = frontend_run(&path);
        assert!(!output.status.success(), "{name} must fail");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(expected), "{name}: {stderr}");
    }

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn strings_templates_and_capability_inspection_have_source_artifact_parity() {
    let directory = temporary_directory();
    let package = directory.join("package");
    copy_directory(&strings_fixture("parity/package"), &package);
    frontend_lock(&package);
    let modes = [
        strings_fixture("parity/loose.allen"),
        strings_fixture("parity/modules/main.allen"),
        strings_fixture("parity/inline.allen"),
        package,
    ];

    for (index, source) in modes.iter().enumerate() {
        frontend_check(source);
        let source_run = frontend_run(source);
        assert!(
            source_run.status.success(),
            "{}: {}",
            source.display(),
            String::from_utf8_lossy(&source_run.stderr)
        );
        assert_eq!(source_run.stdout, b"true\n", "{}", source.display());
        assert!(source_run.stderr.is_empty(), "{}", source.display());

        let first = directory.join(format!("string-mode-{index}-first.allenb"));
        let second = directory.join(format!("string-mode-{index}-second.allenb"));
        frontend_build(source, &first);
        frontend_build(source, &second);
        let first_bytes = std::fs::read(&first).expect("first artifact must exist");
        assert_eq!(first_bytes, std::fs::read(&second).unwrap());
        assert_eq!(&first_bytes[10..12], &13_u16.to_le_bytes());

        let inspected = Command::new(env!("CARGO_BIN_EXE_allen"))
            .arg("inspect")
            .arg(&first)
            .output()
            .expect("CLI must start");
        assert!(inspected.status.success());
        assert!(inspected.stderr.is_empty());
        assert!(
            String::from_utf8(inspected.stdout)
                .unwrap()
                .starts_with("bytecode_version: 13\n")
        );

        let artifact_run = frontend_run(&first);
        assert!(
            artifact_run.status.success(),
            "{}: {}",
            first.display(),
            String::from_utf8_lossy(&artifact_run.stderr)
        );
        assert_eq!(artifact_run.stdout, source_run.stdout);
        assert_eq!(artifact_run.stderr, source_run.stderr);
    }

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}

#[test]
fn strings_standalone_exposes_optional_filesystem_grants_to_inspection() {
    let directory = temporary_directory();
    let package = directory.join("package");
    let workspace = directory.join("workspace");
    std::fs::create_dir_all(package.join("src")).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
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
    std::fs::write(
        package.join("src/main.allen"),
        r"export fn main() returns List<String> effects [capability.inspect] {
  capability.granted()
}
",
    )
    .unwrap();
    frontend_lock(&package);

    let missing_entry = Command::new(env!("CARGO_BIN_EXE_allen"))
        .args(["run", "--entry", "missing"])
        .arg(&package)
        .output()
        .expect("CLI must start");
    assert!(!missing_entry.status.success());
    assert!(missing_entry.stdout.is_empty());
    assert_eq!(
        String::from_utf8(missing_entry.stderr).unwrap(),
        "runtime error[runtime.entry_not_found]: entry is not declared\n"
    );

    let missing_workdir = frontend_run(&package);
    assert!(!missing_workdir.status.success());
    assert!(missing_workdir.stdout.is_empty());
    assert!(
        String::from_utf8(missing_workdir.stderr)
            .unwrap()
            .contains("filesystem entries require --workdir <directory>")
    );

    let granted = Command::new(env!("CARGO_BIN_EXE_allen"))
        .arg("run")
        .arg("--workdir")
        .arg(&workspace)
        .arg(&package)
        .output()
        .expect("CLI must start");
    assert!(
        granted.status.success(),
        "{}",
        String::from_utf8_lossy(&granted.stderr)
    );
    assert_eq!(granted.stdout, b"[\"fs.read\"]\n");
    assert!(granted.stderr.is_empty());

    std::fs::remove_dir_all(directory).expect("temporary test directory must be removed");
}
