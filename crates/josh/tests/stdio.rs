use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::process::{Command, Stdio};

use allen_bytecode::{
    Artifact, ArtifactMetadata, EntryContract, EnumPayloadType, EnumType, EnumVariant, Function,
    Instruction, ManifestContract, Module, StrictSchema, ToolContract, ValueType,
    compute_tool_contract_digest, encode,
};
use allen_schema::{ExactVersion, SchemaLimits, ToolName, ToolSchema, generated_tool_effect};
use base64::Engine as _;
use josh_protocol::{
    CatalogSetParams, CatalogTool, ExecutionMode, ExecutionStartParams, FrameReader, Idempotency,
    InitializeParams, InvokingSessionId, PeerInfo, ProgramLoadParams, ProtocolLimits,
    ToolInvokeResult, WireMessage, encode_frame,
};
use serde_json::json;

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

fn request<T: serde::Serialize>(id: &str, method: &str, params: &T) -> Vec<u8> {
    encode_frame(
        &WireMessage::Request {
            id: id.to_owned(),
            method: method.to_owned(),
            params: serde_json::to_value(params).unwrap(),
        },
        josh_protocol::DEFAULT_MAX_FRAME_BYTES,
    )
    .unwrap()
}

fn send<T: serde::Serialize>(input: &mut impl Write, id: &str, method: &str, params: &T) {
    input.write_all(&request(id, method, params)).unwrap();
    input.flush().unwrap();
}

fn receive(reader: &mut FrameReader<impl std::io::Read>) -> WireMessage {
    reader.read_message().unwrap().unwrap()
}

#[allow(clippy::too_many_lines)]
fn tool_artifact(schema: &ToolSchema) -> Vec<u8> {
    let name = ToolName::parse("example.lookup").unwrap();
    let version = ExactVersion::parse("1.2.3").unwrap();
    let digest_bytes = |text: &str| {
        let hex = text.strip_prefix("sha256:").unwrap();
        let mut bytes = [0; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap();
        }
        bytes
    };
    let ValueType::Record(standard_error_fields) = allen_bytecode::standard_error_type() else {
        unreachable!()
    };
    let tool_error = EnumType {
        name: "pkg://test@0.1.0/src/main.allen::_tool_tools_x2E_example_x2E_lookup_x3A__x3A_Error"
            .to_owned(),
        variants: vec![
            EnumVariant {
                name: "Declared".to_owned(),
                payload: EnumPayloadType::Tuple(vec![ValueType::String]),
            },
            EnumVariant {
                name: "Unavailable".to_owned(),
                payload: EnumPayloadType::Record(standard_error_fields.clone()),
            },
            EnumVariant {
                name: "Schema".to_owned(),
                payload: EnumPayloadType::Record(standard_error_fields),
            },
        ],
    };
    let result_type = ValueType::Result(Box::new(ValueType::String), Box::new(ValueType::Enum(0)));
    let contract = ToolContract {
        name: name.as_str().to_owned(),
        version: version.to_string(),
        version_requirement: ">=1.0.0, <2.0.0".to_owned(),
        effect: generated_tool_effect(&name, version).unwrap(),
        input_schema: 0,
        output_schema: 0,
        error_schema: 0,
        input_digest: digest_bytes(schema.digest()),
        output_digest: digest_bytes(schema.digest()),
        error_digest: digest_bytes(schema.digest()),
    };
    encode(&Artifact {
        metadata: ArtifactMetadata::default(),
        module: Module {
            constants: Vec::new(),
            enum_types: vec![tool_error],
            effect_sets: vec![vec![contract.effect.clone()]],
            functions: vec![Function {
                name: "pkg/x74657374/x302e312e30/x737263/x6d61696e.allen::main".to_owned(),
                parameters: vec![0],
                captures: Vec::new(),
                registers: vec![
                    ValueType::String,
                    ValueType::Future(Box::new(result_type.clone())),
                    result_type.clone(),
                ],
                return_type: result_type.clone(),
                effects: 0,
                code: vec![
                    Instruction::ToolInvoke {
                        destination: 1,
                        tool: 0,
                        input: 0,
                    },
                    Instruction::Await {
                        destination: 2,
                        source: 1,
                    },
                    Instruction::Return { source: 2 },
                ],
            }],
            async_functions: vec![0],
            entry: 0,
        },
        debug: None,
        schemas: vec![
            StrictSchema {
                value_type: ValueType::String,
            },
            StrictSchema {
                value_type: result_type,
            },
        ],
        entries: vec![EntryContract {
            name: "main".to_owned(),
            function: 0,
            input_schema: 0,
            output_schema: 1,
        }],
        imports: Vec::new(),
        manifest: Some(ManifestContract {
            package: "test".to_owned(),
            version: "0.1.0".to_owned(),
            language_requirement: "0.1".to_owned(),
            required_capabilities: Vec::new(),
            optional_capabilities: Vec::new(),
            limits: Vec::new(),
            https_origins: Vec::new(),
            required_tools: vec![contract.clone()],
            tool_contract_digest: compute_tool_contract_digest(&[contract]),
        }),
    })
    .unwrap()
}

#[test]
fn executable_serves_the_exact_stdio_handshake() {
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "stdio-test".to_owned(),
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
        tools: Vec::new(),
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_josh"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(&request("h-1", "initialize", &initialize))
        .unwrap();
    stdin
        .write_all(&request("h-2", "catalog/set", &catalog))
        .unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());

    let mut reader = FrameReader::new(
        Cursor::new(output.stdout),
        josh_protocol::DEFAULT_MAX_FRAME_BYTES,
    );
    let ready = reader.read_message().unwrap().unwrap();
    assert!(matches!(
        ready,
        WireMessage::Notification { ref method, .. } if method == "runtime/ready"
    ));
    for expected_id in ["h-1", "h-2"] {
        assert!(matches!(
            reader.read_message().unwrap().unwrap(),
            WireMessage::Response { ref id, result: Some(_), error: None } if id == expected_id
        ));
    }
    assert!(reader.read_message().unwrap().is_none());
}

#[test]
#[allow(clippy::too_many_lines)]
fn independent_stdio_host_matches_the_golden_tool_transcript() {
    let schema_json = json!({"type":"string"});
    let schema = ToolSchema::from_value(&schema_json, &SchemaLimits::default()).unwrap();
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "independent-stdio-host".to_owned(),
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
        tools: vec![CatalogTool {
            name: "example.lookup".to_owned(),
            version: "1.2.3".to_owned(),
            input_schema: schema_json.clone(),
            output_schema: schema_json.clone(),
            error_schema: schema_json,
            effects: Vec::new(),
            idempotency: Idempotency::Unknown,
        }],
    };
    let load = ProgramLoadParams::Bytecode {
        artifact: base64::engine::general_purpose::STANDARD.encode(tool_artifact(&schema)),
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_josh"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = FrameReader::new(
        child.stdout.take().unwrap(),
        josh_protocol::DEFAULT_MAX_FRAME_BYTES,
    );
    let mut labels = Vec::new();
    let mut record = |message: WireMessage| -> WireMessage {
        labels.push(match &message {
            WireMessage::Notification { method, params } if method == "execution/event" => {
                params["kind"].as_str().unwrap().to_owned()
            }
            WireMessage::Notification { method, .. } => method.clone(),
            WireMessage::Request { id, .. } | WireMessage::Response { id, .. } => id.clone(),
            WireMessage::Cancel { id, .. } => format!("cancel:{id}"),
        });
        message
    };

    record(receive(&mut output));
    send(&mut input, "h-1", "initialize", &initialize);
    record(receive(&mut output));
    send(&mut input, "h-2", "catalog/set", &catalog);
    record(receive(&mut output));
    send(&mut input, "h-3", "program/load", &load);
    let loaded = record(receive(&mut output));
    let WireMessage::Response {
        result: Some(loaded),
        ..
    } = loaded
    else {
        panic!("program load failed");
    };
    let start = ExecutionStartParams {
        execution_id: "exec-independent".to_owned(),
        program_id: loaded["program_id"].as_str().unwrap().to_owned(),
        artifact_digest: loaded["artifact_digest"].as_str().unwrap().to_owned(),
        entry: "main".to_owned(),
        input: json!("question"),
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools: vec!["example.lookup".to_owned()],
        allowed_http_origins: Vec::new(),
        limits: BTreeMap::from([("wall_ms".to_owned(), 5_000)]),
    };
    send(&mut input, "h-4", "execution/start", &start);
    for _ in 0..4 {
        record(receive(&mut output));
    }
    let invocation = record(receive(&mut output));
    let WireMessage::Request { id: tool_id, .. } = invocation else {
        panic!("tool invocation is missing");
    };
    send(&mut input, "h-5", "program/load", &load);
    record(receive(&mut output));
    let response = WireMessage::Response {
        id: tool_id,
        result: Some(
            serde_json::to_value(ToolInvokeResult::Ok {
                value: json!("answer"),
            })
            .unwrap(),
        ),
        error: None,
    };
    input
        .write_all(&encode_frame(&response, josh_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap())
        .unwrap();
    input.flush().unwrap();
    for _ in 0..3 {
        record(receive(&mut output));
    }
    drop(input);
    let process = child.wait_with_output().unwrap();
    assert!(process.status.success());
    assert!(process.stderr.is_empty());
    assert_eq!(
        labels,
        [
            "runtime/ready",
            "h-1",
            "h-2",
            "h-3",
            "accepted",
            "started",
            "task_started",
            "effect_started",
            "r-1",
            "h-5",
            "effect_completed",
            "completed",
            "h-4",
        ]
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn active_partial_frame_eof_cleans_up_and_reports_only_a_safe_error() {
    const PARTIAL_CANARY: &str = "PARTIAL_PRIVATE_CANARY_b0b5";
    let schema_json = json!({"type":"string"});
    let schema = ToolSchema::from_value(&schema_json, &SchemaLimits::default()).unwrap();
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "partial-frame-host".to_owned(),
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
        tools: vec![CatalogTool {
            name: "example.lookup".to_owned(),
            version: "1.2.3".to_owned(),
            input_schema: schema_json.clone(),
            output_schema: schema_json.clone(),
            error_schema: schema_json,
            effects: Vec::new(),
            idempotency: Idempotency::Unknown,
        }],
    };
    let load = ProgramLoadParams::Bytecode {
        artifact: base64::engine::general_purpose::STANDARD.encode(tool_artifact(&schema)),
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_josh"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = FrameReader::new(
        child.stdout.take().unwrap(),
        josh_protocol::DEFAULT_MAX_FRAME_BYTES,
    );
    assert!(
        matches!(receive(&mut output), WireMessage::Notification { ref method, .. } if method == "runtime/ready")
    );
    send(&mut input, "h-1", "initialize", &initialize);
    assert!(matches!(receive(&mut output), WireMessage::Response { ref id, .. } if id == "h-1"));
    send(&mut input, "h-2", "catalog/set", &catalog);
    assert!(matches!(receive(&mut output), WireMessage::Response { ref id, .. } if id == "h-2"));
    send(&mut input, "h-3", "program/load", &load);
    let WireMessage::Response {
        result: Some(loaded),
        ..
    } = receive(&mut output)
    else {
        panic!("program load failed");
    };
    let start = ExecutionStartParams {
        execution_id: "exec-partial".to_owned(),
        program_id: loaded["program_id"].as_str().unwrap().to_owned(),
        artifact_digest: loaded["artifact_digest"].as_str().unwrap().to_owned(),
        entry: "main".to_owned(),
        input: json!("question"),
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools: vec!["example.lookup".to_owned()],
        allowed_http_origins: Vec::new(),
        limits: BTreeMap::from([("wall_ms".to_owned(), 5_000)]),
    };
    send(&mut input, "h-4", "execution/start", &start);
    loop {
        if matches!(receive(&mut output), WireMessage::Request { ref method, .. } if method == "tool/invoke")
        {
            break;
        }
    }
    write!(
        input,
        "Content-Length: 100\r\n\r\n{{\"private\":\"{PARTIAL_CANARY}"
    )
    .unwrap();
    input.flush().unwrap();
    drop(input);

    assert!(output.read_message().unwrap().is_none());
    let process = child.wait_with_output().unwrap();
    assert!(!process.status.success());
    assert_eq!(
        String::from_utf8(process.stderr).unwrap(),
        "josh: protocol frame is invalid\n"
    );
}
