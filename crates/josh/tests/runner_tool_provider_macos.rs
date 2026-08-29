#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const ERROR_SCHEMA: &str = r#"{"additionalProperties":false,"properties":{"code":{"maxLength":128,"minLength":1,"type":"string"},"message":{"maxLength":2048,"minLength":1,"type":"string"}},"required":["code","message"],"type":"object"}"#;

#[test]
fn executor_tool_grants_fail_closed_before_execution_on_macos() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "josh-executor-macos-test-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&root).unwrap();
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
                "source": "runner-tool-provider-macos-test",
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

    let output = Command::new(env!("CARGO_BIN_EXE_josh"))
        .arg("run")
        .arg("--executor")
        .arg("--catalog")
        .arg(&catalog)
        .arg("--grant-tool")
        .arg("example_echo")
        .arg("--input")
        .arg(r#""request""#)
        .arg(&source)
        .output()
        .unwrap();

    let _cleanup = Cleanup(root);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--executor tool grants are unsupported on this platform"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
