use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

use allen_compiler::assemble_root_source_package;
use allen_package::LoadLimits;
use josh_protocol::{
    CatalogSetParams, ExecutionMode, ExecutionStartParams, FileEncoding, FrameReader,
    HostProjectionSetParams, InitializeParams, InvokingSessionId, PeerInfo, ProgramLoadParams,
    ProtocolLimits, SessionBindingLevel, SourceFile, WireMessage, encode_frame,
};
use serde_json::json;

const MANIFEST: &str = r#"[package]
name = "sub-agent-roundtrip"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Projection"
output = "Bool"

[capabilities]
required = []
optional = ["sub_agent.ask", "sub_agent.create", "sub_agent.message", "sub_agent.run"]
"#;

const SOURCE: &str = r#"record Projection {
  capabilities: List<String>,
  limits: Map<String, Int>,
  tools: List<String>
}

export async fn main(projection: Projection) returns Bool
  effects [sub_agent.ask, sub_agent.create, sub_agent.message, sub_agent.run] {
  let child = match await sub_agent.create(prompt { system: "seed", output: Void }, projection) {
    Ok(value) => value,
    Err(_) => stop("sub-agent create unavailable"),
  };
  let _message = match await sub_agent.message(child, "status") {
    Ok(_) => (),
    Err(_) => stop("sub-agent message unavailable"),
  };
  let answered = match await sub_agent.ask<Bool>(child, prompt { system: "ask", output: Bool }) {
    Ok(value) => value,
    Err(_) => stop("sub-agent ask unavailable"),
  };
  let ran = match await sub_agent.run<Bool>(prompt { system: "run", output: Bool }, projection) {
    Ok(value) => value,
    Err(_) => stop("sub-agent run unavailable"),
  };
  answered && ran
}
"#;

fn limits() -> ProtocolLimits {
    ProtocolLimits {
        max_frame_bytes: 4_194_304,
        max_active_requests: 64,
        max_loaded_programs: 32,
        max_total_executions: 1_024,
        max_catalog_tools: 256,
        max_catalog_bytes: 3_145_728,
    }
}

fn send<T: serde::Serialize>(stdin: &mut impl Write, id: &str, method: &str, params: &T) {
    let frame = encode_frame(
        &WireMessage::Request {
            id: id.to_owned(),
            method: method.to_owned(),
            params: serde_json::to_value(params).unwrap(),
        },
        josh_protocol::DEFAULT_MAX_FRAME_BYTES,
    )
    .unwrap();
    stdin.write_all(&frame).unwrap();
    stdin.flush().unwrap();
}

fn respond(stdin: &mut impl Write, id: String, result: serde_json::Value) {
    let frame = encode_frame(
        &WireMessage::Response {
            id,
            result: Some(result),
            error: None,
        },
        josh_protocol::DEFAULT_MAX_FRAME_BYTES,
    )
    .unwrap();
    stdin.write_all(&frame).unwrap();
    stdin.flush().unwrap();
}

fn receive(reader: &mut FrameReader<impl Read>) -> WireMessage {
    reader.read_message().unwrap().unwrap()
}

fn assert_typed_response_request(params: &serde_json::Value) {
    assert!(
        params["interaction_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(params["attempt"], 1);
    assert_eq!(params["validation_issues"], json!([]));
    assert!(params["response_schema"]["descriptor"].is_object());
    assert!(
        params["response_schema"]["digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:"))
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn raw_stdio_routes_all_sub_agent_operations_without_an_invoking_session() {
    let sources = BTreeMap::from([("src/main.allen".to_owned(), SOURCE.to_owned())]);
    assemble_root_source_package(MANIFEST, &sources, None, None, &LoadLimits::default())
        .unwrap_or_else(|error| panic!("sub-agent source must compile: {error:?}"));

    let initialize = InitializeParams {
        host: PeerInfo {
            name: "sub-agent-stdio-test".to_owned(),
            version: "1.0.0".to_owned(),
        },
        protocol_versions: vec![josh_protocol::PROTOCOL_VERSION.to_owned()],
        language_versions: vec![">=0.1.0, <0.2.0".to_owned()],
        execution_mode: ExecutionMode::Unattended,
        invoking_session_id: InvokingSessionId::Null,
        standard_capabilities: Vec::new(),
        limits: limits(),
        extensions: Vec::new(),
    };
    let catalog = CatalogSetParams {
        schema_dialect: josh_protocol::SCHEMA_DIALECT.to_owned(),
        metadata: josh_protocol::CatalogMetadata::complete("test-host", "1", 1),
        tools: Vec::new(),
    };
    let load = ProgramLoadParams::SourceBundle {
        files: vec![
            SourceFile {
                path: "allen.toml".to_owned(),
                encoding: FileEncoding::Utf8,
                content: MANIFEST.to_owned(),
            },
            SourceFile {
                path: "src/main.allen".to_owned(),
                encoding: FileEncoding::Utf8,
                content: SOURCE.to_owned(),
            },
        ],
    };

    let mut child = Command::new(env!("CARGO_BIN_EXE_josh"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = FrameReader::new(
        child.stdout.take().unwrap(),
        josh_protocol::DEFAULT_MAX_FRAME_BYTES,
    );
    let mut stderr = child.stderr.take().unwrap();

    send(&mut stdin, "h-1", "initialize", &initialize);
    assert!(matches!(
        receive(&mut stdout),
        WireMessage::Notification { method, .. } if method == "runtime/ready"
    ));
    match receive(&mut stdout) {
        WireMessage::Response {
            id,
            result: Some(_),
            error: None,
        } if id == "h-1" => {}
        message => panic!("unexpected initialize response: {message:?}"),
    }

    let projection = HostProjectionSetParams::complete_for_catalog(
        "sub-agent-stdio-projection",
        initialize.host.clone(),
        SessionBindingLevel::None,
        &catalog,
    );
    send(&mut stdin, "h-p", "host/project", &projection);
    assert!(matches!(
        receive(&mut stdout),
        WireMessage::Response { id, result: Some(_), error: None } if id == "h-p"
    ));

    send(&mut stdin, "h-2", "catalog/set", &catalog);
    assert!(matches!(
        receive(&mut stdout),
        WireMessage::Response { id, result: Some(_), error: None } if id == "h-2"
    ));

    send(&mut stdin, "h-3", "program/load", &load);
    let artifact_digest = match receive(&mut stdout) {
        WireMessage::Response {
            id,
            result: Some(result),
            error: None,
        } if id == "h-3" => result["artifact_digest"].as_str().unwrap().to_owned(),
        message => panic!("unexpected program/load response: {message:?}"),
    };
    let start = ExecutionStartParams {
        execution_id: "exec-sub-agent-stdio".to_owned(),
        program_id: "program-1".to_owned(),
        artifact_digest,
        entry: "main".to_owned(),
        input: json!({"capabilities": [], "limits": [], "tools": []}),
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools: Vec::new(),
        allowed_http_origins: Vec::new(),
        granted_exec: Vec::new(),
        granted_exec_environment: Vec::new(),
        limits: BTreeMap::from([("wall_ms".to_owned(), 5_000)]),
    };
    send(&mut stdin, "h-4", "execution/start", &start);

    let mut methods = Vec::new();
    let mut replayed_events = 0;
    loop {
        match receive(&mut stdout) {
            WireMessage::Notification { method, params } if method == "execution/event" => {
                assert_eq!(params["replayed"], false);
                replayed_events += 1;
            }
            WireMessage::Request { id, method, params } => {
                methods.push(method.clone());
                match method.as_str() {
                    "sub_agent/create" => {
                        assert_eq!(
                            params["projection"],
                            json!({
                                "capabilities": [], "limits": {}, "tools": []
                            })
                        );
                        assert_eq!(params["prompt"]["context"], json!({"tag":"None"}));
                        assert_eq!(params["prompt"]["data"], json!({"tag":"None"}));
                        respond(&mut stdin, id, json!({"sub_agent_id":"child-stdio-1"}));
                    }
                    "sub_agent/message" => {
                        assert_eq!(params["sub_agent_id"], "child-stdio-1");
                        assert_eq!(params["message"], "status");
                        respond(&mut stdin, id, json!({"accepted":true}));
                    }
                    "sub_agent/ask" => {
                        assert_eq!(params["sub_agent_id"], "child-stdio-1");
                        assert_typed_response_request(&params);
                        respond(&mut stdin, id, json!({"value":true}));
                    }
                    "sub_agent/run" => {
                        assert_eq!(
                            params["projection"],
                            json!({
                                "capabilities": [], "limits": {}, "tools": []
                            })
                        );
                        assert_typed_response_request(&params);
                        respond(&mut stdin, id, json!({"value":true}));
                    }
                    other => panic!("unexpected outbound request: {other}"),
                }
            }
            WireMessage::Response {
                id,
                result: Some(result),
                error: None,
            } if id == "h-4" => {
                assert_eq!(result, json!({"outcome":"completed","output":true}));
                break;
            }
            message => panic!("unexpected stdio message: {message:?}"),
        }
    }

    assert_eq!(
        methods,
        [
            "sub_agent/create",
            "sub_agent/message",
            "sub_agent/ask",
            "sub_agent/run"
        ]
    );
    assert!(replayed_events > 0, "missing v1.3 execution events");

    drop(stdin);
    drop(stdout);
    assert!(child.wait().unwrap().success());
    let mut stderr_bytes = Vec::new();
    stderr.read_to_end(&mut stderr_bytes).unwrap();
    assert!(
        stderr_bytes.is_empty(),
        "unexpected stderr: {stderr_bytes:?}"
    );
}
