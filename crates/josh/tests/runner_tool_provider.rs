#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const ERROR_SCHEMA: &str = r#"{"additionalProperties":false,"properties":{"code":{"maxLength":128,"minLength":1,"type":"string"},"message":{"maxLength":2048,"minLength":1,"type":"string"}},"required":["code","message"],"type":"object"}"#;

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    catalog: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "josh-executor-test-{name}-{}-{nonce}",
            std::process::id()
        ));
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let source = root.join("tool.allen");
        fs::write(
            &source,
            r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
  tools: {
    required: [
      { name: "example_echo", version: ">=1.0.0, <2.0.0" }
    ]
  }
}

export async fn main(value: String) returns String effects [tool.example_echo@1] {
  match await tools.example_echo.call({ text: value }) {
    Ok(output) => output.text
    Err(_) => "tool error"
  }
}
"#,
        )
        .unwrap();
        let catalog = root.join("catalog.json");
        let error_schema: Value = serde_json::from_str(ERROR_SCHEMA).unwrap();
        fs::write(
            &catalog,
            serde_json::to_vec(&json!({
                "schema_dialect": "https://json-schema.org/draft/2020-12/schema",
                "metadata": {
                    "source": "runner-tool-provider-test",
                    "source_revision": "1",
                    "observed_at_unix_ms": 1,
                    "freshness": "current",
                    "complete": true
                },
                "tools": [{
                    "name": "example_echo",
                    "version": "1.2.3",
                    "description": "Test echo tool",
                    "input_schema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}},
                        "required": ["text"],
                        "additionalProperties": false
                    },
                    "output_schema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}},
                        "required": ["text"],
                        "additionalProperties": false
                    },
                    "error_schema": error_schema,
                    "effects": [],
                    "idempotency": "unknown"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        Self {
            root,
            source,
            catalog,
            bin,
        }
    }

    fn install_executor(&self, body: &str) {
        let executable = self.bin.join("executor");
        fs::write(&executable, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(executable, permissions).unwrap();
    }

    fn run(&self, extra: &[&str]) -> Output {
        self.command_with_executor_path(extra).output().unwrap()
    }

    fn command_with_executor_path(&self, extra: &[&str]) -> Command {
        let current_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(&current_path));
        let path = std::env::join_paths(paths).unwrap();
        let mut command = self.command(extra);
        command
            .env("PATH", path)
            .env("JOSH_EXECUTOR_TEST_BIN", &self.bin);
        command
    }

    fn command(&self, extra: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_josh"));
        command
            .arg("run")
            .arg("--executor")
            .arg("--catalog")
            .arg(&self.catalog);
        command.args(extra).arg(&self.source);
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn completed_value(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["outcome"], "completed");
    outcome["output"].clone()
}

fn recorded_input_path(fixture: &Fixture) -> PathBuf {
    let argument = fs::read_to_string(fixture.bin.join("input-path")).unwrap();
    PathBuf::from(argument.strip_prefix('@').unwrap())
}

#[test]
fn executor_success_uses_exact_argv_private_input_and_cleans_up_without_shell_expansion() {
    let fixture = Fixture::new("success");
    let shell_target = fixture.root.join("must-not-exist");
    fixture.install_executor(
        r#"base=$JOSH_EXECUTOR_TEST_BIN
printf '%s\n%s\n%s' "$1" "$2" "$3" > "$base/argv"
	printf '%s' "$3" > "$base/input-path"
	input=${3#@}
	cp "$input" "$base/input-copy"
	input_dir=${input%/*}
	cp "$input" "$input_dir/input.copy"
	case $(uname) in
	  Darwin) directory_mode=$(stat -f %Lp "$input_dir"); file_mode=$(stat -f %Lp "$input") ;;
	  *) directory_mode=$(stat -c %a "$input_dir"); file_mode=$(stat -c %a "$input") ;;
	esac
	printf '%s\n%s' "$directory_mode" "$file_mode" > "$base/permissions"
	printf '%s' '{"ok":true,"data":{"text":"executor success"}}'"#,
    );
    let hostile = format!("$(touch {})", shell_target.display());
    let input = serde_json::to_string(&hostile).unwrap();
    let output = fixture.run(&["--grant-tool", "example_echo", "--input", &input]);

    assert_eq!(completed_value(&output), "executor success");
    assert_eq!(
        fs::read_to_string(fixture.bin.join("argv")).unwrap(),
        format!(
            "call\nexample_echo\n@{}",
            recorded_input_path(&fixture).display()
        )
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(fixture.bin.join("input-copy")).unwrap())
            .unwrap(),
        json!({"text": hostile})
    );
    assert!(
        !shell_target.exists(),
        "input text must never reach a shell"
    );
    assert!(
        !recorded_input_path(&fixture).exists(),
        "private provider input must be removed after the call"
    );
    assert_eq!(
        fs::read_to_string(fixture.bin.join("permissions")).unwrap(),
        "700\n600"
    );
    assert!(!recorded_input_path(&fixture).parent().unwrap().exists());
}

#[test]
fn executor_declared_error_is_returned_through_the_tool_contract() {
    let fixture = Fixture::new("declared-error");
    fs::write(
        &fixture.source,
        r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
  tools: {
    required: [
      { name: "example_echo", version: ">=1.0.0, <2.0.0" }
    ]
  }
}

export async fn main(value: String)
  returns Result<tools.example_echo.Output, tools.example_echo.Error>
  effects [tool.example_echo@1] {
  await tools.example_echo.call({ text: value })
}
"#,
    )
    .unwrap();
    fixture.install_executor(
        r#"printf '%s' '{"ok":false,"error":{"code":"example.rejected","message":"declared rejection"}}'"#,
    );
    let output = fixture.run(&["--grant-tool", "example_echo", "--input", r#""request""#]);
    let value = completed_value(&output);
    assert_eq!(value["tag"], "Err");
    assert_eq!(value["value"]["tag"], "Declared");
    assert_eq!(value["value"]["value"][0]["code"], "example.rejected");
    assert_eq!(value["value"]["value"][0]["message"], "declared rejection");
}

#[test]
fn executor_is_required_on_path_when_a_tool_is_granted() {
    let fixture = Fixture::new("missing");
    let output = fixture
        .command(&["--grant-tool", "example_echo", "--input", r#""request""#])
        .env("PATH", fixture.root.join("empty-path"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("requires an executable named 'executor'")
    );
}

#[test]
fn non_executable_path_entry_is_rejected_before_entry_execution() {
    let fixture = Fixture::new("not-executable");
    fixture.install_executor(r#"printf '%s' '{"ok":true,"data":{"text":"unexpected"}}'"#);
    let executable = fixture.bin.join("executor");
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(executable, permissions).unwrap();

    let output = fixture.run(&["--grant-tool", "example_echo", "--input", r#""request""#]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("requires an executable named 'executor'")
    );
}

#[test]
fn invalid_and_ungranted_tools_never_reach_the_executor() {
    let fixture = Fixture::new("grants");
    fixture.install_executor(
        r#"base=$JOSH_EXECUTOR_TEST_BIN
touch "$base/invoked"
printf '%s' '{"ok":true,"data":{"text":"unexpected"}}'"#,
    );
    let invalid = fixture.run(&["--grant-tool", "not canonical", "--input", r#""request""#]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("not a canonical tool name"));

    let ungranted = fixture.run(&["--input", r#""request""#]);
    assert!(!ungranted.status.success());
    assert!(String::from_utf8_lossy(&ungranted.stderr).contains("execution preflight failed"));
    assert!(!fixture.bin.join("invoked").exists());
}

#[test]
fn malformed_and_oversized_executor_output_is_rejected() {
    let fixture = Fixture::new("bad-output");
    for body in [
        r#"printf '%s' '{"ok":true,"data":{"text":"first"}} trailing'"#,
        r"dd if=/dev/zero bs=1048577 count=1 2>/dev/null | tr '\000' x",
    ] {
        fixture.install_executor(body);
        let output = fixture.run(&["--grant-tool", "example_echo", "--input", r#""request""#]);
        assert_eq!(completed_value(&output), "tool error");
    }
}

#[test]
fn executor_timeout_kills_the_call() {
    let fixture = Fixture::new("timeout");
    fixture.install_executor(
        r#"base=$JOSH_EXECUTOR_TEST_BIN
	printf '%s' "$3" > "$base/input-path"
	while :; do :; done"#,
    );
    let started = std::time::Instant::now();
    let output = fixture.run(&[
        "--grant-tool",
        "example_echo",
        "--wall-ms",
        "3000",
        "--input",
        r#""request""#,
    ]);
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    assert_eq!(completed_value(&output), "tool error");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("late"));
    let input_path = recorded_input_path(&fixture);
    assert!(!input_path.exists());
    assert!(!input_path.parent().unwrap().exists());
}

#[test]
fn executor_timeout_terminates_descendants_that_hold_output_pipes_open() {
    let fixture = Fixture::new("descendant-timeout");
    fixture.install_executor(
        r#"base=$JOSH_EXECUTOR_TEST_BIN
	printf '%s' "$3" > "$base/input-path"
	(trap '' HUP; sleep 10) &
	printf '%s' "$!" > "$base/descendant-pid"
	printf '%s' '{"ok":true,"data":{"text":"late"}}'
	exit 0"#,
    );
    let started = std::time::Instant::now();
    let output = fixture.run(&[
        "--grant-tool",
        "example_echo",
        "--wall-ms",
        "3000",
        "--input",
        r#""request""#,
    ]);

    assert_eq!(completed_value(&output), "tool error");
    assert!(started.elapsed() < Duration::from_secs(4));
    let raw_pid = fs::read_to_string(fixture.bin.join("descendant-pid"))
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(raw_pid).unwrap();
    for _ in 0..500 {
        if rustix::process::test_kill_process(pid).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(rustix::process::test_kill_process(pid).is_err());
    let input_path = recorded_input_path(&fixture);
    assert!(!input_path.exists());
    assert!(!input_path.parent().unwrap().exists());
}

#[test]
fn a_replaced_executor_path_cannot_change_the_preflight_image() {
    let fixture = Fixture::new("replaced");
    fs::write(
        &fixture.source,
        r#"manifest {
  language: "0.1"
  entry: main
  capabilities: [fs.write(workdir)]
  tools: {
    required: [
      { name: "example_echo", version: ">=1.0.0, <2.0.0" }
    ]
  }
}

export async fn main(value: String) returns String effects [fs.write, tool.example_echo@1] {
  let workspace = fs.workspace();
  let ignored = await fs.write_text(workspace, "ready", "ready");
  mut delay = 0;
  while (delay < 10000) { delay = delay + 1; }
  match await tools.example_echo.call({ text: value }) {
    Ok(output) => output.text
    Err(_) => "tool error"
  }
}
"#,
    )
    .unwrap();
    fixture.install_executor(
        r#"base=$JOSH_EXECUTOR_TEST_BIN
	touch "$base/original-invoked"
	printf '%s' '{"ok":true,"data":{"text":"original"}}'"#,
    );
    let replacement = fixture.bin.join("replacement");
    fs::write(
        &replacement,
        r#"#!/bin/sh
set -eu
base=$JOSH_EXECUTOR_TEST_BIN
touch "$base/replacement-invoked"
printf '%s' '{"ok":true,"data":{"text":"replacement"}}'
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&replacement).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&replacement, permissions).unwrap();

    let ready = fixture.root.join("ready");
    let executable = fixture.bin.join("executor");
    let replacer = thread::spawn(move || {
        for _ in 0..5000 {
            if ready.exists() {
                fs::rename(replacement, executable).unwrap();
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        false
    });
    let output = fixture.run(&[
        "--grant-tool",
        "example_echo",
        "--grant",
        "fs.write",
        "--workdir",
        fixture.root.to_str().unwrap(),
        "--input",
        r#""request""#,
    ]);

    assert!(
        replacer.join().unwrap(),
        "entry did not reach replacement gate"
    );
    assert_eq!(completed_value(&output), "original");
    assert!(fixture.bin.join("original-invoked").exists());
    assert!(!fixture.bin.join("replacement-invoked").exists());
}

#[test]
fn provider_failure_is_not_retried_and_removes_private_input() {
    let fixture = Fixture::new("no-retry");
    fixture.install_executor(
        r#"base=$JOSH_EXECUTOR_TEST_BIN
	printf x >> "$base/invocations"
	printf '%s' "$3" > "$base/input-path"
	exit 7"#,
    );
    let output = fixture.run(&["--grant-tool", "example_echo", "--input", r#""request""#]);

    assert_eq!(completed_value(&output), "tool error");
    assert_eq!(fs::read(fixture.bin.join("invocations")).unwrap(), b"x");
    let input_path = recorded_input_path(&fixture);
    assert!(!input_path.exists());
    assert!(!input_path.parent().unwrap().exists());
}

#[test]
fn mismatched_error_schema_is_rejected_before_invocation() {
    let fixture = Fixture::new("schema-mismatch");
    let mut catalog: Value = serde_json::from_slice(&fs::read(&fixture.catalog).unwrap()).unwrap();
    catalog["tools"][0]["error_schema"]["properties"]["code"]["maxLength"] = json!(127);
    fs::write(&fixture.catalog, serde_json::to_vec(&catalog).unwrap()).unwrap();
    fixture.install_executor(
        r#"base=$JOSH_EXECUTOR_TEST_BIN
	touch "$base/invoked"
	printf '%s' '{"ok":true,"data":{"text":"unexpected"}}'"#,
    );

    let output = fixture.run(&["--grant-tool", "example_echo", "--input", r#""request""#]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("fixed executor error schema"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.bin.join("invoked").exists());
}

#[test]
fn runtime_rejects_executor_data_that_violates_the_output_schema() {
    let fixture = Fixture::new("invalid-output-schema");
    fixture.install_executor(r#"printf '%s' '{"ok":true,"data":{"text":7}}'"#);

    let output = fixture.run(&["--grant-tool", "example_echo", "--input", r#""request""#]);
    assert_eq!(completed_value(&output), "tool error");
}

#[test]
fn executor_stderr_is_bounded_and_never_exposed() {
    let fixture = Fixture::new("stderr");
    fixture.install_executor("printf '%s' 'secret-provider-credential' >&2\nexit 7");
    let output = fixture.run(&["--grant-tool", "example_echo", "--input", r#""request""#]);
    assert_eq!(completed_value(&output), "tool error");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret-provider-credential"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-provider-credential"));
}
