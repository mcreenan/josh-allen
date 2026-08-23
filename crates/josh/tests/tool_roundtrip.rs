use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use allen_bytecode::{
    Artifact, ArtifactMetadata, Constant, DecodeLimits, EntryContract, EnumPayloadType,
    EnumSwitchArm, EnumType, EnumVariant, Function, Instruction, ManifestContract, Module,
    StrictSchema, ToolContract, ValueType, compute_tool_contract_digest, decode_and_verify, encode,
};
use allen_compiler::assemble_root_source_package;
use allen_package::LoadLimits;
use allen_schema::{
    CatalogLimits, ExactVersion, FrozenCatalog, SchemaLimits, ToolDefinition, ToolName, ToolSchema,
    generated_tool_effect,
};
use base64::Engine as _;
use josh_protocol::{
    CatalogSetParams, CatalogTool, ExecutionMode, ExecutionStartParams, FileEncoding, FrameReader,
    Idempotency, InitializeParams, InvokingSessionId, PeerInfo, ProgramLoadParams, ProtocolLimits,
    SourceFile, ToolInvokeResult, WireError, WireErrorCode, WireMessage, encode_frame,
};
use serde_json::json;

const CANARY: &str = "JOSH_PRIVATE_CANARY_7df0";

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

fn receive(reader: &mut FrameReader<impl std::io::Read>) -> WireMessage {
    reader.read_message().unwrap().unwrap()
}

fn send_message(stdin: &mut impl Write, message: &WireMessage) {
    let frame = encode_frame(message, josh_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap();
    stdin.write_all(&frame).unwrap();
    stdin.flush().unwrap();
}

#[derive(Clone, Default)]
struct TranscriptOutput(Arc<(Mutex<Vec<u8>>, Condvar)>);

impl Write for TranscriptOutput {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let (bytes, changed) = &*self.0;
        bytes.lock().unwrap().extend_from_slice(buffer);
        changed.notify_all();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct GatedCursor {
    cursor: Cursor<Vec<u8>>,
    gates: Vec<(u64, &'static str)>,
    output: TranscriptOutput,
    final_marker: &'static str,
}

impl Read for GatedCursor {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        while let Some((offset, marker)) = self.gates.first().copied() {
            let position = self.cursor.position();
            if position == offset {
                wait_for_marker(&self.output, marker);
                self.gates.remove(0);
                continue;
            }
            let remaining = usize::try_from(offset.saturating_sub(position)).unwrap_or(usize::MAX);
            if remaining > 0 {
                let available = buffer.len().min(remaining);
                return self.cursor.read(&mut buffer[..available]);
            }
            self.gates.remove(0);
        }
        let read = self.cursor.read(buffer)?;
        if read == 0 {
            wait_for_marker(&self.output, self.final_marker);
        }
        Ok(read)
    }
}

fn wait_for_marker(output: &TranscriptOutput, marker: &str) {
    let (bytes, changed) = &*output.0;
    let mut bytes = bytes.lock().unwrap();
    while !String::from_utf8_lossy(&bytes).contains(marker) {
        let (next, timeout) = changed.wait_timeout(bytes, Duration::from_secs(2)).unwrap();
        assert!(!timeout.timed_out(), "transcript did not reach {marker}");
        bytes = next;
    }
}

fn tool_artifact(schema: &ToolSchema) -> Vec<u8> {
    tool_artifact_with_retry(schema, false)
}

fn tool_artifact_with_matched_retry(schema: &ToolSchema) -> Vec<u8> {
    tool_artifact_with_retry(schema, true)
}

#[allow(clippy::too_many_lines)]
fn tool_artifact_with_retry(schema: &ToolSchema, retry_on_error: bool) -> Vec<u8> {
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
    let (registers, code) = if retry_on_error {
        (
            vec![
                ValueType::String,
                ValueType::Future(Box::new(result_type.clone())),
                result_type.clone(),
                ValueType::String,
                ValueType::Enum(0),
                ValueType::Future(Box::new(result_type.clone())),
                result_type.clone(),
            ],
            vec![
                Instruction::ToolInvoke {
                    destination: 1,
                    tool: 0,
                    input: 0,
                },
                Instruction::Await {
                    destination: 2,
                    source: 1,
                },
                Instruction::SwitchEnum {
                    source: 2,
                    arms: vec![
                        EnumSwitchArm {
                            variant: 0,
                            target: 6,
                            bindings: vec![3],
                        },
                        EnumSwitchArm {
                            variant: 1,
                            target: 3,
                            bindings: vec![4],
                        },
                    ],
                },
                Instruction::ToolInvoke {
                    destination: 5,
                    tool: 0,
                    input: 0,
                },
                Instruction::Await {
                    destination: 6,
                    source: 5,
                },
                Instruction::Return { source: 6 },
                Instruction::Return { source: 2 },
            ],
        )
    } else {
        (
            vec![
                ValueType::String,
                ValueType::Future(Box::new(result_type.clone())),
                result_type.clone(),
            ],
            vec![
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
        )
    };
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
    let artifact = Artifact {
        metadata: ArtifactMetadata {
            bytecode_version: allen_bytecode::BYTECODE_VERSION,
            ..ArtifactMetadata::default()
        },
        module: Module {
            constants: Vec::new(),
            enum_types: vec![tool_error],
            effect_sets: vec![vec![contract.effect.clone()]],
            functions: vec![Function {
                name: "pkg/x74657374/x302e312e30/x737263/x6d61696e.allen::main".to_owned(),
                parameters: vec![0],
                captures: Vec::new(),
                registers,
                return_type: result_type.clone(),
                effects: 0,
                code,
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
    };
    encode(&artifact).unwrap()
}

fn unit_artifact(stopped: bool) -> Vec<u8> {
    let (constants, registers, code) = if stopped {
        (
            vec![Constant::String("requested stop".to_owned())],
            vec![ValueType::Unit, ValueType::String],
            vec![
                Instruction::Const {
                    destination: 1,
                    constant: 0,
                },
                Instruction::Stop { reason: 1 },
            ],
        )
    } else {
        (
            vec![Constant::Unit],
            vec![ValueType::Unit],
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Return { source: 0 },
            ],
        )
    };
    encode(&Artifact {
        metadata: ArtifactMetadata::default(),
        module: Module {
            constants,
            enum_types: Vec::new(),
            effect_sets: vec![Vec::new()],
            functions: vec![Function {
                name: "pkg/x74657374/x302e312e30/x737263/x6d61696e.allen::main".to_owned(),
                parameters: vec![0],
                captures: Vec::new(),
                registers,
                return_type: ValueType::Unit,
                effects: 0,
                code,
            }],
            async_functions: Vec::new(),
            entry: 0,
        },
        debug: None,
        schemas: vec![StrictSchema {
            value_type: ValueType::Unit,
        }],
        entries: vec![EntryContract {
            name: "main".to_owned(),
            function: 0,
            input_schema: 0,
            output_schema: 0,
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
            required_tools: Vec::new(),
            tool_contract_digest: compute_tool_contract_digest(&[]),
        }),
    })
    .unwrap()
}

fn source_and_current_bytecode() -> (ProgramLoadParams, ProgramLoadParams) {
    const MANIFEST: &str = r#"[package]
name = "parity"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "String"
output = "Result<String, tools.example.lookup.Error>"

[capabilities]
required = []
optional = []

[[tools.required]]
name = "example.lookup"
version = ">=1.0.0, <2.0.0"
"#;
    const SOURCE: &str = r"export async fn main(value: String) returns Result<String, tools.example.lookup.Error>
  effects [tool.example.lookup@1] {
  await tools.example.lookup.call(value)
}
";
    let definition = ToolDefinition::parse(
        "example.lookup",
        "1.2.3",
        r#"{"type":"string"}"#,
        r#"{"type":"string"}"#,
        r#"{"type":"string"}"#,
        Vec::new(),
        allen_schema::Idempotency::Unknown,
        &SchemaLimits::default(),
    )
    .unwrap();
    let catalog = FrozenCatalog::freeze(vec![definition], &CatalogLimits::default()).unwrap();
    let sources = BTreeMap::from([("src/main.allen".to_owned(), SOURCE.to_owned())]);
    let compiled = assemble_root_source_package(
        MANIFEST,
        &sources,
        None,
        Some(&catalog),
        &LoadLimits::default(),
    )
    .unwrap();
    let artifact = encode(&compiled.artifact).unwrap();
    (
        ProgramLoadParams::SourceBundle {
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
        },
        ProgramLoadParams::Bytecode {
            artifact: base64::engine::general_purpose::STANDARD.encode(artifact),
        },
    )
}

fn run_parity_transcript(
    load: &ProgramLoadParams,
    artifact_digest: &str,
) -> Vec<serde_json::Value> {
    let schema_json = json!({"type":"string"});
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "parity-host".to_owned(),
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
        tools: vec![CatalogTool {
            name: "example.lookup".to_owned(),
            version: "1.2.3".to_owned(),
            description: "Look up one example value.".to_owned(),
            input_schema: schema_json.clone(),
            output_schema: schema_json.clone(),
            error_schema: schema_json,
            effects: Vec::new(),
            idempotency: Idempotency::Unknown,
        }],
    };
    let start = ExecutionStartParams {
        execution_id: "exec-parity".to_owned(),
        program_id: "program-1".to_owned(),
        artifact_digest: artifact_digest.to_owned(),
        entry: "main".to_owned(),
        input: json!("question"),
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools: vec!["example.lookup".to_owned()],
        allowed_http_origins: Vec::new(),
        limits: BTreeMap::from([("wall_ms".to_owned(), 5_000)]),
    };
    let request = |id: &str, method: &str, params: serde_json::Value| {
        encode_frame(
            &WireMessage::Request {
                id: id.to_owned(),
                method: method.to_owned(),
                params,
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap()
    };
    let mut input = Vec::new();
    input.extend(request("h-1", "initialize", json!(initialize)));
    input.extend(request("h-2", "catalog/set", json!(catalog)));
    input.extend(request("h-3", "program/load", json!(load)));
    input.extend(request("h-4", "execution/start", json!(start)));
    let response_offset = u64::try_from(input.len()).unwrap();
    input.extend(
        encode_frame(
            &WireMessage::Response {
                id: "r-1".to_owned(),
                result: Some(json!({"outcome":"ok","value":"answer"})),
                error: None,
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap(),
    );
    let output = TranscriptOutput::default();
    let reader = GatedCursor {
        cursor: Cursor::new(input),
        gates: vec![(response_offset, "\"method\":\"tool/invoke\"")],
        output: output.clone(),
        final_marker: "\"id\":\"h-4\"",
    };
    josh_host::run_connection(reader, output.clone()).unwrap();
    let bytes = output.0.0.lock().unwrap().clone();
    let mut reader = FrameReader::new(Cursor::new(bytes), josh_protocol::DEFAULT_MAX_FRAME_BYTES);
    let mut normalized = Vec::new();
    while let Some(message) = reader.read_message().unwrap() {
        match message {
            WireMessage::Response {
                id,
                result: Some(result),
                ..
            } if id == "h-3" || id == "h-4" => {
                normalized.push(json!({"response":id,"result":result}));
            }
            WireMessage::Notification { method, mut params } if method == "execution/event" => {
                params.as_object_mut().unwrap().remove("elapsed_ms");
                normalized.push(json!({"event":params}));
            }
            WireMessage::Request { method, params, .. } if method == "tool/invoke" => {
                normalized.push(json!({"tool":params}));
            }
            _ => {}
        }
    }
    normalized
}

#[test]
fn source_and_current_bytecode_have_tool_and_event_parity() {
    let (source, bytecode) = source_and_current_bytecode();
    let ProgramLoadParams::Bytecode { artifact } = &bytecode else {
        unreachable!();
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(artifact)
        .unwrap();
    let verified = decode_and_verify(&bytes, &DecodeLimits::default()).unwrap();
    assert_eq!(
        verified.metadata().bytecode_version,
        allen_bytecode::BYTECODE_VERSION
    );
    let mut digest = "sha256:".to_owned();
    for byte in verified.content_digest() {
        use std::fmt::Write as _;
        write!(digest, "{byte:02x}").unwrap();
    }

    let source_transcript = run_parity_transcript(&source, &digest);
    let bytecode_transcript = run_parity_transcript(&bytecode, &digest);
    assert_eq!(source_transcript, bytecode_transcript);
    assert!(
        source_transcript
            .iter()
            .any(|item| item.get("tool").is_some())
    );
    assert!(source_transcript.iter().any(|item| {
        item.pointer("/event/kind") == Some(&serde_json::Value::String("completed".to_owned()))
    }));
}

#[test]
#[allow(clippy::too_many_lines)]
fn attached_agent_ask_classifies_exact_wire_errors_and_preserves_session_identity() {
    const MANIFEST: &str = r#"[package]
name = "agent-roundtrip"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "String"

[capabilities]
required = ["agent.ask"]
optional = []
"#;
    const SOURCE: &str = r#"export async fn main() returns String effects [agent.ask] {
  match await agent.ask(prompt { system: "continue?", output: String }) {
    Ok(value) => value,
    Err(error) => error.code,
  }
}
"#;
    let sources = BTreeMap::from([("src/main.allen".to_owned(), SOURCE.to_owned())]);
    let compiled =
        assemble_root_source_package(MANIFEST, &sources, None, None, &LoadLimits::default())
            .unwrap();
    let artifact = encode(&compiled.artifact).unwrap();
    let verified = decode_and_verify(&artifact, &DecodeLimits::default()).unwrap();
    let mut artifact_digest = "sha256:".to_owned();
    for byte in verified.content_digest() {
        use std::fmt::Write as _;
        write!(artifact_digest, "{byte:02x}").unwrap();
    }
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "attached-host".to_owned(),
            version: "1.0.0".to_owned(),
        },
        protocol_versions: vec![josh_protocol::PROTOCOL_VERSION.to_owned()],
        language_versions: vec![">=0.1.0, <0.2.0".to_owned()],
        execution_mode: ExecutionMode::Attached,
        invoking_session_id: InvokingSessionId::Id("session-current".to_owned()),
        standard_capabilities: Vec::new(),
        limits: limits(),
        extensions: Vec::new(),
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
    let start = ExecutionStartParams {
        execution_id: "exec-agent".to_owned(),
        program_id: "program-1".to_owned(),
        artifact_digest,
        entry: "main".to_owned(),
        input: serde_json::Value::Null,
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools: Vec::new(),
        allowed_http_origins: Vec::new(),
        limits: BTreeMap::from([("wall_ms".to_owned(), 5_000)]),
    };
    let request = |id: &str, method: &str, params: serde_json::Value| {
        encode_frame(
            &WireMessage::Request {
                id: id.to_owned(),
                method: method.to_owned(),
                params,
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap()
    };
    for (case, response_error, expected_output, expected_failure) in [
        ("ok", None, Some("yes"), None),
        (
            "unavailable",
            Some(WireErrorCode::AgentUnavailable),
            Some("agent.unavailable"),
            None,
        ),
        (
            "denied",
            Some(WireErrorCode::AgentDenied),
            Some("agent.denied"),
            None,
        ),
        (
            "cancelled",
            Some(WireErrorCode::RequestCancelled),
            None,
            Some("runtime.cancelled"),
        ),
        (
            "wrong-domain",
            Some(WireErrorCode::UserDenied),
            None,
            Some("protocol.violation"),
        ),
    ] {
        let mut input = Vec::new();
        input.extend(request("h-1", "initialize", json!(initialize)));
        input.extend(request(
            "h-2",
            "catalog/set",
            json!({"schema_dialect":josh_protocol::SCHEMA_DIALECT,"metadata":{"source":"test-host","source_revision":"1","observed_at_unix_ms":1,"freshness":"current","complete":true},"tools":[]}),
        ));
        input.extend(request("h-3", "program/load", json!(load)));
        input.extend(request("h-4", "execution/start", json!(start)));
        let response_offset = u64::try_from(input.len()).unwrap();
        let provider_response = response_error.map_or_else(
            || WireMessage::Response {
                id: "r-1".to_owned(),
                result: Some(json!({"value":"yes"})),
                error: None,
            },
            |code| WireMessage::Response {
                id: "r-1".to_owned(),
                result: None,
                error: Some(WireError {
                    code,
                    message: CANARY.to_owned(),
                    data: Some(json!({"secret":CANARY})),
                }),
            },
        );
        input.extend(
            encode_frame(&provider_response, josh_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap(),
        );
        let output = TranscriptOutput::default();
        let reader = GatedCursor {
            cursor: Cursor::new(input),
            gates: vec![(response_offset, "\"method\":\"agent/ask\"")],
            output: output.clone(),
            final_marker: "\"id\":\"h-4\"",
        };
        josh_host::run_connection(reader, output.clone()).unwrap();
        let bytes = output.0.0.lock().unwrap().clone();
        assert!(!String::from_utf8_lossy(&bytes).contains(CANARY));
        let mut reader =
            FrameReader::new(Cursor::new(bytes), josh_protocol::DEFAULT_MAX_FRAME_BYTES);
        let mut saw_request = false;
        let mut saw_result = false;
        let mut terminal_events = 0;
        while let Some(message) = reader.read_message().unwrap() {
            match message {
                WireMessage::Request { method, params, .. } if method == "agent/ask" => {
                    saw_request = true;
                    assert_eq!(params["execution_id"], "exec-agent");
                    assert_eq!(params["session_id"], "session-current");
                    assert_eq!(params["prompt"]["system"], "continue?");
                    assert_eq!(params["attempt"], 1);
                }
                WireMessage::Notification { params, .. }
                    if matches!(
                        params["kind"].as_str(),
                        Some("completed" | "failed" | "cancelled" | "stopped")
                    ) =>
                {
                    terminal_events += 1;
                }
                WireMessage::Response {
                    id,
                    result: Some(result),
                    ..
                } if id == "h-4" => {
                    saw_result = true;
                    if let Some(expected) = expected_output {
                        assert_eq!(
                            result,
                            json!({"outcome":"completed","output":expected}),
                            "{case}"
                        );
                    } else {
                        assert_eq!(result["outcome"], "failed", "{case}");
                        assert_eq!(result["error"]["code"], expected_failure.unwrap(), "{case}");
                    }
                }
                _ => {}
            }
        }
        assert!(saw_request, "{case}");
        assert!(saw_result, "{case}");
        assert_eq!(terminal_events, 1, "{case}");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn model_and_user_requests_work_without_an_invoking_session() {
    #[derive(Clone, Copy)]
    enum Expected {
        Output(&'static str),
        Failure(&'static str),
    }

    for (case, effect, method, response_error, expected) in [
        (
            "model-ok",
            "model.request",
            "model/request",
            None,
            Expected::Output("yes"),
        ),
        (
            "user-ok",
            "user.ask",
            "user/ask",
            None,
            Expected::Output("yes"),
        ),
        (
            "model-unavailable",
            "model.request",
            "model/request",
            Some(WireErrorCode::ModelUnavailable),
            Expected::Output("model.unavailable"),
        ),
        (
            "user-unavailable",
            "user.ask",
            "user/ask",
            Some(WireErrorCode::UserUnavailable),
            Expected::Output("user.unavailable"),
        ),
        (
            "model-denied",
            "model.request",
            "model/request",
            Some(WireErrorCode::ModelDenied),
            Expected::Output("model.denied"),
        ),
        (
            "user-denied",
            "user.ask",
            "user/ask",
            Some(WireErrorCode::UserDenied),
            Expected::Output("user.denied"),
        ),
        (
            "model-cancelled",
            "model.request",
            "model/request",
            Some(WireErrorCode::RequestCancelled),
            Expected::Failure("runtime.cancelled"),
        ),
        (
            "model-wrong-domain",
            "model.request",
            "model/request",
            Some(WireErrorCode::UserUnavailable),
            Expected::Failure("protocol.violation"),
        ),
        (
            "user-wrong-domain-denied",
            "user.ask",
            "user/ask",
            Some(WireErrorCode::ModelDenied),
            Expected::Failure("protocol.violation"),
        ),
    ] {
        let manifest = format!(
            r#"[package]
name = "typed-roundtrip"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "String"

[capabilities]
required = ["{effect}"]
optional = []
"#
        );
        let source = format!(
            r#"export async fn main() returns String effects [{effect}] {{
  match await {effect}<String>(prompt {{
    system: "Decide using only supplied data."
    context: "visible context"
    data: {{ canary: "{CANARY}" }}
    output: String
    policy: {{ max_attempts: 1 }}
  }}) {{
    Ok(value) => value,
    Err(error) => error.code,
  }}
}}
"#
        );
        let sources = BTreeMap::from([("src/main.allen".to_owned(), source.clone())]);
        let compiled =
            assemble_root_source_package(&manifest, &sources, None, None, &LoadLimits::default())
                .unwrap();
        let artifact = encode(&compiled.artifact).unwrap();
        let verified = decode_and_verify(&artifact, &DecodeLimits::default()).unwrap();
        let mut artifact_digest = "sha256:".to_owned();
        for byte in verified.content_digest() {
            use std::fmt::Write as _;
            write!(artifact_digest, "{byte:02x}").unwrap();
        }
        let initialize = InitializeParams {
            host: PeerInfo {
                name: "typed-host".to_owned(),
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
        let load = ProgramLoadParams::SourceBundle {
            files: vec![
                SourceFile {
                    path: "allen.toml".to_owned(),
                    encoding: FileEncoding::Utf8,
                    content: manifest,
                },
                SourceFile {
                    path: "src/main.allen".to_owned(),
                    encoding: FileEncoding::Utf8,
                    content: source,
                },
            ],
        };
        let start = ExecutionStartParams {
            execution_id: format!("exec-{case}"),
            program_id: "program-1".to_owned(),
            artifact_digest,
            entry: "main".to_owned(),
            input: serde_json::Value::Null,
            working_directory: None,
            granted_capabilities: Vec::new(),
            granted_tools: Vec::new(),
            allowed_http_origins: Vec::new(),
            limits: BTreeMap::from([("wall_ms".to_owned(), 5_000)]),
        };
        let request = |id: &str, request_method: &str, params: serde_json::Value| {
            encode_frame(
                &WireMessage::Request {
                    id: id.to_owned(),
                    method: request_method.to_owned(),
                    params,
                },
                josh_protocol::DEFAULT_MAX_FRAME_BYTES,
            )
            .unwrap()
        };
        let mut input = Vec::new();
        input.extend(request("h-1", "initialize", json!(initialize)));
        input.extend(request(
            "h-2",
            "catalog/set",
            json!({"schema_dialect":josh_protocol::SCHEMA_DIALECT,"metadata":{"source":"test-host","source_revision":"1","observed_at_unix_ms":1,"freshness":"current","complete":true},"tools":[]}),
        ));
        input.extend(request("h-3", "program/load", json!(load)));
        input.extend(request("h-4", "execution/start", json!(start)));
        let response_offset = u64::try_from(input.len()).unwrap();
        let provider_response = if let Some(code) = response_error {
            WireMessage::Response {
                id: "r-1".to_owned(),
                result: None,
                error: Some(WireError {
                    code,
                    message: CANARY.to_owned(),
                    data: Some(json!({"secret": CANARY})),
                }),
            }
        } else {
            WireMessage::Response {
                id: "r-1".to_owned(),
                result: Some(json!({"value":"yes"})),
                error: None,
            }
        };
        input.extend(
            encode_frame(&provider_response, josh_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap(),
        );
        let output = TranscriptOutput::default();
        let marker: &'static str = if method == "model/request" {
            "\"method\":\"model/request\""
        } else {
            "\"method\":\"user/ask\""
        };
        let reader = GatedCursor {
            cursor: Cursor::new(input),
            gates: vec![(response_offset, marker)],
            output: output.clone(),
            final_marker: "\"id\":\"h-4\"",
        };
        josh_host::run_connection(reader, output.clone()).unwrap();
        let bytes = output.0.0.lock().unwrap().clone();
        let mut reader =
            FrameReader::new(Cursor::new(bytes), josh_protocol::DEFAULT_MAX_FRAME_BYTES);
        let mut saw_request = false;
        let mut saw_result = false;
        let mut terminal_events = 0;
        while let Some(message) = reader.read_message().unwrap() {
            match message {
                WireMessage::Request {
                    method: request_method,
                    params,
                    ..
                } if request_method == method => {
                    saw_request = true;
                    assert!(params.get("session_id").is_none());
                    assert_eq!(
                        params["prompt"]["system"],
                        "Decide using only supplied data."
                    );
                    assert_eq!(
                        params["prompt"]["context"],
                        json!({"tag":"Some","value":"visible context"})
                    );
                    assert_eq!(params["prompt"]["data"]["tag"], "Some");
                    assert_eq!(params["prompt"]["data"]["value"]["canary"], CANARY);
                    assert_eq!(params["attempt"], 1);
                    assert_eq!(params["validation_issues"], json!([]));
                    assert!(
                        params["response_schema"]["digest"]
                            .as_str()
                            .unwrap()
                            .starts_with("sha256:")
                    );
                }
                WireMessage::Response {
                    id,
                    result: Some(result),
                    ..
                } if id == "h-4" => {
                    saw_result = true;
                    assert!(
                        !serde_json::to_string(&result).unwrap().contains(CANARY),
                        "provider detail leaked in {case}"
                    );
                    match expected {
                        Expected::Output(output) => {
                            assert_eq!(result, json!({"outcome":"completed","output":output}));
                        }
                        Expected::Failure(code) => {
                            assert_eq!(result["outcome"], "failed");
                            assert_eq!(result["error"]["code"], code);
                            if code == "protocol.violation" {
                                assert_eq!(
                                    result["error"]["message"],
                                    "runtime protocol violation"
                                );
                            }
                        }
                    }
                }
                WireMessage::Notification { params, .. }
                    if matches!(
                        params["kind"].as_str(),
                        Some("completed" | "failed" | "cancelled" | "stopped")
                    ) =>
                {
                    assert!(
                        !serde_json::to_string(&params).unwrap().contains(CANARY),
                        "provider detail leaked in terminal event for {case}"
                    );
                    terminal_events += 1;
                }
                _ => {}
            }
        }
        assert!(saw_request, "missing {method} request");
        assert!(saw_result, "missing {method} result");
        assert_eq!(terminal_events, 1, "terminal count for {case}");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn late_agent_response_after_stop_is_fatal_without_a_second_terminal() {
    const MANIFEST: &str = r#"[package]
name = "agent-stop"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Void"

[capabilities]
required = ["agent.ask"]
optional = []
"#;
    const SOURCE: &str = r#"export async fn main() returns Void effects [agent.ask, task.spawn] {
  await {
    let task = spawn agent.ask(prompt { system: "wait", output: String });
    stop("finished")
  }
}
"#;
    let sources = BTreeMap::from([("src/main.allen".to_owned(), SOURCE.to_owned())]);
    let compiled =
        assemble_root_source_package(MANIFEST, &sources, None, None, &LoadLimits::default())
            .unwrap();
    let stopped_bytes = encode(&compiled.artifact).unwrap();
    let stopped = decode_and_verify(&stopped_bytes, &DecodeLimits::default()).unwrap();
    let mut stopped_digest = "sha256:".to_owned();
    for byte in stopped.content_digest() {
        use std::fmt::Write as _;
        write!(stopped_digest, "{byte:02x}").unwrap();
    }
    let unit_bytes = unit_artifact(false);
    let unit = decode_and_verify(&unit_bytes, &DecodeLimits::default()).unwrap();
    let mut unit_digest = "sha256:".to_owned();
    for byte in unit.content_digest() {
        use std::fmt::Write as _;
        write!(unit_digest, "{byte:02x}").unwrap();
    }
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "attached-stop-host".to_owned(),
            version: "1.0.0".to_owned(),
        },
        protocol_versions: vec![josh_protocol::PROTOCOL_VERSION.to_owned()],
        language_versions: vec![">=0.1.0, <0.2.0".to_owned()],
        execution_mode: ExecutionMode::Attached,
        invoking_session_id: InvokingSessionId::Id("session-stays-open".to_owned()),
        standard_capabilities: Vec::new(),
        limits: limits(),
        extensions: Vec::new(),
    };
    let stopped_load = ProgramLoadParams::SourceBundle {
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
    let request = |id: &str, method: &str, params: serde_json::Value| {
        encode_frame(
            &WireMessage::Request {
                id: id.to_owned(),
                method: method.to_owned(),
                params,
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap()
    };
    let start = |execution_id: &str, program_id: &str, digest: &str| {
        json!({
            "execution_id":execution_id,
            "program_id":program_id,
            "artifact_digest":digest,
            "entry":"main",
            "input":null,
            "working_directory":null,
            "granted_capabilities":[],
            "granted_tools":[],
            "allowed_http_origins":[],
            "limits":{"wall_ms":5000}
        })
    };
    let mut input = Vec::new();
    input.extend(request("h-1", "initialize", json!(initialize)));
    input.extend(request(
        "h-2",
        "catalog/set",
        json!({"schema_dialect":josh_protocol::SCHEMA_DIALECT,"metadata":{"source":"test-host","source_revision":"1","observed_at_unix_ms":1,"freshness":"current","complete":true},"tools":[]}),
    ));
    input.extend(request("h-3", "program/load", json!(stopped_load)));
    input.extend(request(
        "h-4",
        "execution/start",
        start("exec-stop-agent", "program-1", &stopped_digest),
    ));
    let late_offset = u64::try_from(input.len()).unwrap();
    input.extend(
        encode_frame(
            &WireMessage::Response {
                id: "r-1".to_owned(),
                result: Some(json!({"value":"too late"})),
                error: None,
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap(),
    );
    input.extend(request(
        "h-5",
        "program/load",
        json!({
            "format":"bytecode",
            "artifact":base64::engine::general_purpose::STANDARD.encode(unit_bytes)
        }),
    ));
    let next_offset = u64::try_from(input.len()).unwrap();
    input.extend(request(
        "h-6",
        "execution/start",
        start("exec-after-stop", "program-2", &unit_digest),
    ));
    let output = TranscriptOutput::default();
    let reader = GatedCursor {
        cursor: Cursor::new(input),
        gates: vec![
            (late_offset, "\"id\":\"h-4\""),
            (next_offset, "\"id\":\"h-4\""),
        ],
        output: output.clone(),
        final_marker: "\"id\":\"h-6\"",
    };
    let error = josh_host::run_connection(reader, output.clone()).unwrap_err();
    assert_eq!(error.code, WireErrorCode::ProtocolViolation);
    let bytes = output.0.0.lock().unwrap().clone();
    let mut reader = FrameReader::new(Cursor::new(bytes), josh_protocol::DEFAULT_MAX_FRAME_BYTES);
    let mut messages = Vec::new();
    while let Some(message) = reader.read_message().unwrap() {
        messages.push(message);
    }
    let ask_index = messages
        .iter()
        .position(|message| {
            matches!(message, WireMessage::Request { method, params, .. }
            if method == "agent/ask" && params["session_id"] == "session-stays-open")
        })
        .unwrap();
    let cancel_index = messages
        .iter()
        .position(|message| matches!(message, WireMessage::Cancel { id, .. } if id == "r-1"))
        .unwrap();
    let stopped_event_index = messages
        .iter()
        .position(|message| {
            matches!(message, WireMessage::Notification { method, params }
            if method == "execution/event" && params["kind"] == "stopped")
        })
        .unwrap();
    let stopped_response_index = messages
        .iter()
        .position(|message| {
            matches!(message, WireMessage::Response { id, result: Some(result), .. }
            if id == "h-4" && result == &json!({"outcome":"stopped","reason":"finished"}))
        })
        .unwrap();
    assert!(ask_index < cancel_index);
    assert!(cancel_index < stopped_event_index);
    assert!(stopped_event_index < stopped_response_index);
    assert!(matches!(
        &messages[stopped_event_index],
        WireMessage::Notification { params, .. } if params["fields"] == json!({})
    ));
    assert!(!messages.iter().any(|message| matches!(message,
        WireMessage::Request { id, .. } | WireMessage::Response { id, .. } if id == "h-6"
    )));
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message,
                WireMessage::Notification { method, params }
                if method == "execution/event"
                    && matches!(params["kind"].as_str(), Some("completed" | "stopped" | "failed" | "cancelled"))
            ))
            .count(),
        1
    );
}

struct OpenCursor(Cursor<Vec<u8>>);

impl Read for OpenCursor {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.0.read(buffer)?;
        if read != 0 {
            return Ok(read);
        }
        loop {
            std::thread::park_timeout(Duration::from_secs(60));
        }
    }
}

struct FailAfterFrames(usize);

impl Write for FailAfterFrames {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.0 == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "broken transcript",
            ));
        }
        self.0 -= 1;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn active_connection_returns_when_writer_breaks_and_input_stays_open() {
    let artifact = unit_artifact(false);
    let verified = decode_and_verify(&artifact, &DecodeLimits::default()).unwrap();
    let mut artifact_digest = "sha256:".to_owned();
    for byte in verified.content_digest() {
        use std::fmt::Write as _;
        write!(artifact_digest, "{byte:02x}").unwrap();
    }
    let requests = [
        (
            "h-1",
            "initialize",
            json!({
                "host":{"name":"broken-host","version":"1.0.0"},
                "protocol_versions":[josh_protocol::PROTOCOL_VERSION],
                "language_versions":[">=0.1.0, <0.2.0"],
                "execution_mode":"unattended",
                "standard_capabilities":[],
                "limits":limits(),
                "extensions":[]
            }),
        ),
        (
            "h-2",
            "catalog/set",
            json!({"schema_dialect":josh_protocol::SCHEMA_DIALECT,"metadata":{"source":"test-host","source_revision":"1","observed_at_unix_ms":1,"freshness":"current","complete":true},"tools":[]}),
        ),
        (
            "h-3",
            "program/load",
            json!({"format":"bytecode","artifact":base64::engine::general_purpose::STANDARD.encode(artifact)}),
        ),
        (
            "h-4",
            "execution/start",
            json!({
                "execution_id":"exec-broken","program_id":"program-1",
                "artifact_digest":artifact_digest,"entry":"main","input":null,
                "working_directory":null,"granted_capabilities":[],"granted_tools":[],
                "allowed_http_origins":[],"limits":{"wall_ms":5000}
            }),
        ),
    ];
    let mut input = Vec::new();
    for (id, method, params) in requests {
        input.extend(
            encode_frame(
                &WireMessage::Request {
                    id: id.to_owned(),
                    method: method.to_owned(),
                    params,
                },
                josh_protocol::DEFAULT_MAX_FRAME_BYTES,
            )
            .unwrap(),
        );
    }
    let (done, result) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = done.send(josh_host::run_connection(
            OpenCursor(Cursor::new(input)),
            FailAfterFrames(5),
        ));
    });
    assert!(
        result.recv_timeout(Duration::from_secs(2)).is_ok(),
        "broken writer did not terminate the active connection"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn in_memory_connection_matches_the_golden_tool_transcript() {
    let schema_json = json!({"type":"string"});
    let schema = ToolSchema::from_value(&schema_json, &SchemaLimits::default()).unwrap();
    let artifact_bytes = tool_artifact(&schema);
    let verified = decode_and_verify(&artifact_bytes, &DecodeLimits::default()).unwrap();
    let mut artifact_digest = "sha256:".to_owned();
    for byte in verified.content_digest() {
        use std::fmt::Write as _;
        write!(artifact_digest, "{byte:02x}").unwrap();
    }
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "memory-host".to_owned(),
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
        tools: vec![CatalogTool {
            name: "example.lookup".to_owned(),
            version: "1.2.3".to_owned(),
            description: "Look up one example value.".to_owned(),
            input_schema: schema_json.clone(),
            output_schema: schema_json.clone(),
            error_schema: schema_json,
            effects: Vec::new(),
            idempotency: Idempotency::Unknown,
        }],
    };
    let load = ProgramLoadParams::Bytecode {
        artifact: base64::engine::general_purpose::STANDARD.encode(artifact_bytes),
    };
    let start = ExecutionStartParams {
        execution_id: "exec-memory".to_owned(),
        program_id: "program-1".to_owned(),
        artifact_digest,
        entry: "main".to_owned(),
        input: json!("question"),
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools: vec!["example.lookup".to_owned()],
        allowed_http_origins: Vec::new(),
        limits: BTreeMap::from([("wall_ms".to_owned(), 5_000)]),
    };
    let request = |id: &str, method: &str, params: serde_json::Value| {
        encode_frame(
            &WireMessage::Request {
                id: id.to_owned(),
                method: method.to_owned(),
                params,
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap()
    };
    let mut input = Vec::new();
    input.extend(request("h-1", "initialize", json!(initialize)));
    input.extend(request("h-2", "catalog/set", json!(catalog)));
    input.extend(request("h-3", "program/load", json!(load)));
    input.extend(request("h-4", "execution/start", json!(start)));
    let reentrant_offset = u64::try_from(input.len()).unwrap();
    input.extend(request("h-5", "program/load", json!(load)));
    let response_offset = u64::try_from(input.len()).unwrap();
    input.extend(
        encode_frame(
            &WireMessage::Response {
                id: "r-1".to_owned(),
                result: Some(json!({"outcome":"ok","value":"answer"})),
                error: None,
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap(),
    );
    let output = TranscriptOutput::default();
    let reader = GatedCursor {
        cursor: Cursor::new(input),
        gates: vec![
            (reentrant_offset, "\"method\":\"tool/invoke\""),
            (response_offset, "\"id\":\"h-5\""),
        ],
        output: output.clone(),
        final_marker: "\"id\":\"h-4\"",
    };
    josh_host::run_connection(reader, output.clone()).unwrap();
    let bytes = output.0.0.lock().unwrap().clone();
    let mut reader = FrameReader::new(Cursor::new(bytes), josh_protocol::DEFAULT_MAX_FRAME_BYTES);
    let mut labels = Vec::new();
    while let Some(message) = reader.read_message().unwrap() {
        labels.push(match message {
            WireMessage::Notification { method, params } if method == "execution/event" => {
                params["kind"].as_str().unwrap().to_owned()
            }
            WireMessage::Notification { method, .. } => method,
            WireMessage::Request { id, .. } | WireMessage::Response { id, .. } => id,
            WireMessage::Cancel { id, .. } => format!("cancel:{id}"),
        });
    }
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

fn run_terminal_script(
    artifact_bytes: Vec<u8>,
    catalog: &CatalogSetParams,
    granted_tools: Vec<String>,
    execution_limits: BTreeMap<String, u64>,
    cancel_at_tool: bool,
) -> Vec<WireMessage> {
    let verified = decode_and_verify(&artifact_bytes, &DecodeLimits::default()).unwrap();
    let mut artifact_digest = "sha256:".to_owned();
    for byte in verified.content_digest() {
        use std::fmt::Write as _;
        write!(artifact_digest, "{byte:02x}").unwrap();
    }
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "terminal-matrix-host".to_owned(),
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
    let load = ProgramLoadParams::Bytecode {
        artifact: base64::engine::general_purpose::STANDARD.encode(artifact_bytes),
    };
    let input = if granted_tools.is_empty() {
        serde_json::Value::Null
    } else {
        json!("question")
    };
    let start = ExecutionStartParams {
        execution_id: "exec-terminal".to_owned(),
        program_id: "program-1".to_owned(),
        artifact_digest,
        entry: "main".to_owned(),
        input,
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools,
        allowed_http_origins: Vec::new(),
        limits: execution_limits,
    };
    let request = |id: &str, method: &str, params: serde_json::Value| {
        encode_frame(
            &WireMessage::Request {
                id: id.to_owned(),
                method: method.to_owned(),
                params,
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap()
    };
    let mut input = Vec::new();
    input.extend(request("h-1", "initialize", json!(initialize)));
    input.extend(request("h-2", "catalog/set", json!(catalog)));
    input.extend(request("h-3", "program/load", json!(load)));
    input.extend(request("h-4", "execution/start", json!(start)));
    let cancel_offset = u64::try_from(input.len()).unwrap();
    if cancel_at_tool {
        input.extend(
            encode_frame(
                &WireMessage::Cancel {
                    id: "h-4".to_owned(),
                    reason: None,
                },
                josh_protocol::DEFAULT_MAX_FRAME_BYTES,
            )
            .unwrap(),
        );
    }
    let output = TranscriptOutput::default();
    let reader = GatedCursor {
        cursor: Cursor::new(input),
        gates: cancel_at_tool
            .then_some((cancel_offset, "\"method\":\"tool/invoke\""))
            .into_iter()
            .collect(),
        output: output.clone(),
        final_marker: "\"id\":\"h-4\"",
    };
    josh_host::run_connection(reader, output.clone()).unwrap();
    let bytes = output.0.0.lock().unwrap().clone();
    let mut reader = FrameReader::new(Cursor::new(bytes), josh_protocol::DEFAULT_MAX_FRAME_BYTES);
    let mut messages = Vec::new();
    while let Some(message) = reader.read_message().unwrap() {
        messages.push(message);
    }
    messages
}

#[test]
#[allow(clippy::too_many_lines)]
fn all_four_terminal_outcomes_cross_the_wire_once() {
    let empty_catalog = || CatalogSetParams {
        schema_dialect: josh_protocol::SCHEMA_DIALECT.to_owned(),
        metadata: josh_protocol::CatalogMetadata::complete("test-host", "1", 1),
        tools: Vec::new(),
    };
    let schema_json = json!({"type":"string"});
    let schema = ToolSchema::from_value(&schema_json, &SchemaLimits::default()).unwrap();
    let tool_catalog = CatalogSetParams {
        schema_dialect: josh_protocol::SCHEMA_DIALECT.to_owned(),
        metadata: josh_protocol::CatalogMetadata::complete("test-host", "1", 1),
        tools: vec![CatalogTool {
            name: "example.lookup".to_owned(),
            version: "1.2.3".to_owned(),
            description: "Look up one example value.".to_owned(),
            input_schema: schema_json.clone(),
            output_schema: schema_json.clone(),
            error_schema: schema_json,
            effects: Vec::new(),
            idempotency: Idempotency::Unknown,
        }],
    };
    let cases = [
        (
            "completed",
            run_terminal_script(
                unit_artifact(false),
                &empty_catalog(),
                Vec::new(),
                BTreeMap::from([("wall_ms".to_owned(), 1_000)]),
                false,
            ),
        ),
        (
            "stopped",
            run_terminal_script(
                unit_artifact(true),
                &empty_catalog(),
                Vec::new(),
                BTreeMap::from([("wall_ms".to_owned(), 1_000)]),
                false,
            ),
        ),
        (
            "failed",
            run_terminal_script(
                unit_artifact(false),
                &empty_catalog(),
                Vec::new(),
                BTreeMap::from([
                    ("instructions".to_owned(), 1),
                    ("wall_ms".to_owned(), 1_000),
                ]),
                false,
            ),
        ),
        (
            "cancelled",
            run_terminal_script(
                tool_artifact(&schema),
                &tool_catalog,
                vec!["example.lookup".to_owned()],
                BTreeMap::from([("wall_ms".to_owned(), 1_000)]),
                true,
            ),
        ),
    ];

    for (expected, messages) in cases {
        let terminals = messages
            .iter()
            .filter_map(|message| match message {
                WireMessage::Notification { method, params }
                    if method == "execution/event"
                        && matches!(
                            params["kind"].as_str(),
                            Some("completed" | "stopped" | "failed" | "cancelled")
                        ) =>
                {
                    params["kind"].as_str()
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(terminals, [expected], "wire messages: {messages:#?}");
        let response = messages.iter().find_map(|message| match message {
            WireMessage::Response {
                id,
                result: Some(result),
                ..
            } if id == "h-4" => Some(result),
            _ => None,
        });
        assert_eq!(response.unwrap()["outcome"], expected);
        if expected == "failed" {
            let warning = messages.iter().find_map(|message| match message {
                WireMessage::Notification { method, params }
                    if method == "execution/event" && params["kind"] == "budget_warning" =>
                {
                    Some(&params["fields"])
                }
                _ => None,
            });
            assert_eq!(
                warning,
                Some(&json!({"resource":"instructions","used":1,"limit":1}))
            );
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn late_tool_response_after_cancel_is_fatal_without_a_second_terminal() {
    let schema_json = json!({"type":"string"});
    let schema = ToolSchema::from_value(&schema_json, &SchemaLimits::default()).unwrap();
    let artifact_bytes = tool_artifact(&schema);
    let verified = decode_and_verify(&artifact_bytes, &DecodeLimits::default()).unwrap();
    let mut artifact_digest = "sha256:".to_owned();
    for byte in verified.content_digest() {
        use std::fmt::Write as _;
        write!(artifact_digest, "{byte:02x}").unwrap();
    }
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "cancel-race-host".to_owned(),
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
        tools: vec![CatalogTool {
            name: "example.lookup".to_owned(),
            version: "1.2.3".to_owned(),
            description: "Look up one example value.".to_owned(),
            input_schema: schema_json.clone(),
            output_schema: schema_json.clone(),
            error_schema: schema_json,
            effects: Vec::new(),
            idempotency: Idempotency::Unknown,
        }],
    };
    let load = ProgramLoadParams::Bytecode {
        artifact: base64::engine::general_purpose::STANDARD.encode(artifact_bytes),
    };
    let start = |execution_id: &str| ExecutionStartParams {
        execution_id: execution_id.to_owned(),
        program_id: "program-1".to_owned(),
        artifact_digest: artifact_digest.clone(),
        entry: "main".to_owned(),
        input: json!("question"),
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools: vec!["example.lookup".to_owned()],
        allowed_http_origins: Vec::new(),
        limits: BTreeMap::from([("wall_ms".to_owned(), 5_000)]),
    };
    let request = |id: &str, method: &str, params: serde_json::Value| {
        encode_frame(
            &WireMessage::Request {
                id: id.to_owned(),
                method: method.to_owned(),
                params,
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap()
    };
    let frame = |message: &WireMessage| {
        encode_frame(message, josh_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap()
    };
    let mut input = Vec::new();
    input.extend(request("h-1", "initialize", json!(initialize)));
    input.extend(request("h-2", "catalog/set", json!(catalog)));
    input.extend(request("h-3", "program/load", json!(load)));
    input.extend(request(
        "h-4",
        "execution/start",
        json!(start("exec-cancel")),
    ));
    let cancel_offset = u64::try_from(input.len()).unwrap();
    input.extend(frame(&WireMessage::Cancel {
        id: "unknown".to_owned(),
        reason: None,
    }));
    input.extend(frame(&WireMessage::Cancel {
        id: "h-4".to_owned(),
        reason: None,
    }));
    input.extend(frame(&WireMessage::Cancel {
        id: "h-4".to_owned(),
        reason: None,
    }));
    let late_response_offset = u64::try_from(input.len()).unwrap();
    input.extend(frame(&WireMessage::Response {
        id: "r-1".to_owned(),
        result: None,
        error: Some(WireError {
            code: WireErrorCode::RequestInvalid,
            message: CANARY.to_owned(),
            data: Some(json!({"private": CANARY})),
        }),
    }));
    let restart_offset = u64::try_from(input.len()).unwrap();
    input.extend(request(
        "h-6",
        "execution/start",
        json!(start("exec-after-cancel")),
    ));
    let response_offset = u64::try_from(input.len()).unwrap();
    input.extend(frame(&WireMessage::Response {
        id: "r-2".to_owned(),
        result: Some(json!({"outcome":"ok","value":"answer"})),
        error: None,
    }));
    let terminal_cancel_offset = u64::try_from(input.len()).unwrap();
    input.extend(frame(&WireMessage::Cancel {
        id: "h-6".to_owned(),
        reason: None,
    }));

    let output = TranscriptOutput::default();
    let reader = GatedCursor {
        cursor: Cursor::new(input),
        gates: vec![
            (cancel_offset, "\"method\":\"tool/invoke\""),
            (late_response_offset, "\"id\":\"h-4\""),
            (restart_offset, "\"id\":\"h-4\""),
            (response_offset, "\"id\":\"r-2\""),
            (terminal_cancel_offset, "\"id\":\"h-6\""),
        ],
        output: output.clone(),
        final_marker: "\"id\":\"h-6\"",
    };
    let result = josh_host::run_connection(reader, output.clone());
    assert_eq!(result.unwrap_err().code, WireErrorCode::ProtocolViolation);
    let bytes = output.0.0.lock().unwrap().clone();
    assert!(!String::from_utf8_lossy(&bytes).contains(CANARY));
    let mut reader = FrameReader::new(Cursor::new(bytes), josh_protocol::DEFAULT_MAX_FRAME_BYTES);
    let mut terminal_kinds = Vec::new();
    let mut tool_cancels = Vec::new();
    let mut execution_responses = Vec::new();
    while let Some(message) = reader.read_message().unwrap() {
        match message {
            WireMessage::Notification { method, params }
                if method == "execution/event"
                    && matches!(
                        params["kind"].as_str(),
                        Some("completed" | "stopped" | "failed" | "cancelled")
                    ) =>
            {
                terminal_kinds.push(params["kind"].as_str().unwrap().to_owned());
            }
            WireMessage::Cancel { id, .. } => tool_cancels.push(id),
            WireMessage::Response {
                id,
                result: Some(result),
                ..
            } if id == "h-4" || id == "h-6" => {
                execution_responses.push((id, result["outcome"].as_str().unwrap().to_owned()));
            }
            _ => {}
        }
    }
    assert_eq!(terminal_kinds, ["cancelled"]);
    assert_eq!(tool_cancels, ["r-1"]);
    assert_eq!(
        execution_responses,
        [("h-4".to_owned(), "cancelled".to_owned())]
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn wrong_domain_tool_error_stops_before_a_matched_retry() {
    let schema_json = json!({"type":"string"});
    let schema = ToolSchema::from_value(&schema_json, &SchemaLimits::default()).unwrap();
    let artifact = tool_artifact_with_matched_retry(&schema);
    let verified = decode_and_verify(&artifact, &DecodeLimits::default()).unwrap();
    let mut artifact_digest = "sha256:".to_owned();
    for byte in verified.content_digest() {
        use std::fmt::Write as _;
        write!(artifact_digest, "{byte:02x}").unwrap();
    }
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "terminal-tool-host".to_owned(),
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
        tools: vec![CatalogTool {
            name: "example.lookup".to_owned(),
            version: "1.2.3".to_owned(),
            description: "Look up one example value.".to_owned(),
            input_schema: schema_json.clone(),
            output_schema: schema_json.clone(),
            error_schema: schema_json,
            effects: Vec::new(),
            idempotency: Idempotency::Unknown,
        }],
    };
    let load = ProgramLoadParams::Bytecode {
        artifact: base64::engine::general_purpose::STANDARD.encode(artifact),
    };
    let start = ExecutionStartParams {
        execution_id: "exec-terminal-tool-response".to_owned(),
        program_id: "program-1".to_owned(),
        artifact_digest,
        entry: "main".to_owned(),
        input: json!("question"),
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools: vec!["example.lookup".to_owned()],
        allowed_http_origins: Vec::new(),
        limits: BTreeMap::from([("wall_ms".to_owned(), 250)]),
    };
    let request = |id: &str, method: &str, params: serde_json::Value| {
        encode_frame(
            &WireMessage::Request {
                id: id.to_owned(),
                method: method.to_owned(),
                params,
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap()
    };
    let mut input = Vec::new();
    input.extend(request("h-1", "initialize", json!(initialize)));
    input.extend(request("h-2", "catalog/set", json!(catalog)));
    input.extend(request("h-3", "program/load", json!(load)));
    input.extend(request("h-4", "execution/start", json!(start)));
    let response_offset = u64::try_from(input.len()).unwrap();
    input.extend(
        encode_frame(
            &WireMessage::Response {
                id: "r-1".to_owned(),
                result: None,
                error: Some(WireError {
                    code: WireErrorCode::UserUnavailable,
                    message: CANARY.to_owned(),
                    data: Some(json!({"secret":CANARY})),
                }),
            },
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap(),
    );
    let output = TranscriptOutput::default();
    let reader = GatedCursor {
        cursor: Cursor::new(input),
        gates: vec![(response_offset, "\"method\":\"tool/invoke\"")],
        output: output.clone(),
        final_marker: "\"id\":\"h-4\"",
    };

    josh_host::run_connection(reader, output.clone()).unwrap();

    let bytes = output.0.0.lock().unwrap().clone();
    let encoded = String::from_utf8_lossy(&bytes);
    assert!(!encoded.contains(CANARY));
    let mut reader = FrameReader::new(Cursor::new(bytes), josh_protocol::DEFAULT_MAX_FRAME_BYTES);
    let mut tool_requests = 0;
    let mut effect_starts = 0;
    let mut terminal_events = 0;
    let mut terminal_result = None;
    while let Some(message) = reader.read_message().unwrap() {
        match message {
            WireMessage::Request { method, .. } if method == "tool/invoke" => {
                tool_requests += 1;
            }
            WireMessage::Notification { params, .. } if params["kind"] == "effect_started" => {
                effect_starts += 1;
            }
            WireMessage::Notification { params, .. }
                if matches!(
                    params["kind"].as_str(),
                    Some("completed" | "failed" | "cancelled" | "stopped")
                ) =>
            {
                terminal_events += 1;
                assert_eq!(params["kind"], "failed");
            }
            WireMessage::Response {
                id,
                result: Some(result),
                ..
            } if id == "h-4" => terminal_result = Some(result),
            _ => {}
        }
    }
    assert_eq!(
        tool_requests, 1,
        "matched error branch issued a later tool call"
    );
    assert_eq!(
        effect_starts, 1,
        "matched error branch started a later effect"
    );
    assert_eq!(terminal_events, 1);
    let result = terminal_result.expect("missing execution response");
    assert_eq!(result["outcome"], "failed");
    assert_eq!(result["error"]["code"], "protocol.violation");
    assert_eq!(result["error"]["message"], "runtime protocol violation");
}

#[test]
#[allow(clippy::too_many_lines)]
fn tool_call_and_reentrant_program_load_complete_over_raw_stdio() {
    let schema_json = json!({"type":"string"});
    let schema = ToolSchema::from_value(&schema_json, &SchemaLimits::default()).unwrap();
    let initialize = InitializeParams {
        host: PeerInfo {
            name: "raw-mock-host".to_owned(),
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
        tools: vec![CatalogTool {
            name: "example.lookup".to_owned(),
            version: "1.2.3".to_owned(),
            description: "Look up one example value.".to_owned(),
            input_schema: schema_json.clone(),
            output_schema: schema_json.clone(),
            error_schema: schema_json,
            effects: Vec::new(),
            idempotency: Idempotency::Unknown,
        }],
    };
    let artifact = base64::engine::general_purpose::STANDARD.encode(tool_artifact(&schema));
    let load = ProgramLoadParams::Bytecode {
        artifact: artifact.clone(),
    };

    let mut child = Command::new(env!("CARGO_BIN_EXE_josh"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = FrameReader::new(stdout, josh_protocol::DEFAULT_MAX_FRAME_BYTES);
    assert!(matches!(
        receive(&mut reader),
        WireMessage::Notification { ref method, .. } if method == "runtime/ready"
    ));
    send(&mut stdin, "h-1", "initialize", &initialize);
    assert!(
        matches!(receive(&mut reader), WireMessage::Response { ref id, result: Some(_), .. } if id == "h-1")
    );
    send(&mut stdin, "h-2", "catalog/set", &catalog);
    assert!(
        matches!(receive(&mut reader), WireMessage::Response { ref id, result: Some(_), .. } if id == "h-2")
    );
    send(&mut stdin, "h-3", "program/load", &load);
    let loaded = receive(&mut reader);
    let WireMessage::Response {
        result: Some(loaded),
        ..
    } = loaded
    else {
        panic!("program load failed");
    };
    let program_id = loaded["program_id"].as_str().unwrap().to_owned();
    let artifact_digest = loaded["artifact_digest"].as_str().unwrap().to_owned();
    let start = ExecutionStartParams {
        execution_id: "exec-1".to_owned(),
        program_id,
        artifact_digest,
        entry: "main".to_owned(),
        input: json!("question"),
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools: vec!["example.lookup".to_owned()],
        allowed_http_origins: Vec::new(),
        limits: BTreeMap::from([("wall_ms".to_owned(), 5_000)]),
    };
    send(&mut stdin, "h-4", "execution/start", &start);
    for expected in ["accepted", "started"] {
        let WireMessage::Notification { params, .. } = receive(&mut reader) else {
            panic!("missing execution event");
        };
        assert_eq!(params["kind"], expected);
    }
    for expected in ["task_started", "effect_started"] {
        let WireMessage::Notification { params, .. } = receive(&mut reader) else {
            panic!("missing execution detail event");
        };
        assert_eq!(params["kind"], expected);
    }
    let invocation = receive(&mut reader);
    let WireMessage::Request {
        id: tool_id,
        method,
        ..
    } = invocation
    else {
        panic!("missing tool invocation");
    };
    assert_eq!(method, "tool/invoke");
    assert_eq!(tool_id, "r-1");

    let tool_result = serde_json::to_value(ToolInvokeResult::Ok {
        value: json!("answer"),
    })
    .unwrap();
    let response = WireMessage::Response {
        id: tool_id,
        result: Some(tool_result),
        error: None,
    };
    stdin
        .write_all(&encode_frame(&response, josh_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap())
        .unwrap();
    stdin.flush().unwrap();
    let WireMessage::Notification { params, .. } = receive(&mut reader) else {
        panic!("missing effect-completed event");
    };
    assert_eq!(params["kind"], "effect_completed");
    let WireMessage::Notification { params, .. } = receive(&mut reader) else {
        panic!("missing terminal event");
    };
    assert_eq!(params["kind"], "completed");
    let WireMessage::Response {
        id,
        result: Some(result),
        ..
    } = receive(&mut reader)
    else {
        panic!("missing execution response");
    };
    assert_eq!(id, "h-4");
    assert_eq!(
        result,
        json!({"outcome":"completed","output":{"tag":"Ok","value":"answer"}})
    );

    let mut timeout_start = start.clone();
    timeout_start.execution_id = "exec-timeout".to_owned();
    timeout_start.limits = BTreeMap::from([("wall_ms".to_owned(), 25)]);
    send(&mut stdin, "h-timeout", "execution/start", &timeout_start);
    for expected in ["accepted", "started", "task_started", "effect_started"] {
        let WireMessage::Notification { params, .. } = receive(&mut reader) else {
            panic!("missing timeout execution event");
        };
        assert_eq!(params["kind"], expected);
    }
    assert!(matches!(
        receive(&mut reader),
        WireMessage::Request { ref id, ref method, .. } if id == "r-2" && method == "tool/invoke"
    ));
    let next = receive(&mut reader);
    let timeout_cancel = if let WireMessage::Notification { params, .. } = next {
        assert_eq!(params["kind"], "budget_warning");
        assert_eq!(params["fields"]["resource"], "wall_time");
        assert_eq!(params["fields"]["limit"], 25);
        assert!(
            params["fields"]["used"]
                .as_u64()
                .is_some_and(|used| used < 25)
        );
        receive(&mut reader)
    } else {
        next
    };
    assert!(
        matches!(
            timeout_cancel,
            WireMessage::Cancel { ref id, .. } if id == "r-2"
        ),
        "expected cancellation for r-2, received {timeout_cancel:?}"
    );
    let effect_failed = receive(&mut reader);
    assert!(
        !serde_json::to_string(&effect_failed)
            .unwrap()
            .contains(CANARY)
    );
    let WireMessage::Notification { params, .. } = effect_failed else {
        panic!("missing timeout effect event");
    };
    assert_eq!(params["kind"], "effect_failed");
    let terminal = receive(&mut reader);
    assert!(!serde_json::to_string(&terminal).unwrap().contains(CANARY));
    let WireMessage::Notification { params, .. } = terminal else {
        panic!("missing timeout terminal event");
    };
    assert_eq!(params["kind"], "failed");
    let timeout_response = receive(&mut reader);
    assert!(
        !serde_json::to_string(&timeout_response)
            .unwrap()
            .contains(CANARY)
    );
    let WireMessage::Response {
        id,
        result: Some(result),
        ..
    } = timeout_response
    else {
        panic!("missing timeout response");
    };
    assert_eq!(id, "h-timeout");
    assert_eq!(result["outcome"], "failed");
    assert_eq!(result["error"]["code"], "runtime.timeout");
    assert_eq!(result["error"]["message"], "execution timed out");
    assert!(!serde_json::to_string(&result).unwrap().contains(CANARY));
    assert!(!serde_json::to_string(&result).unwrap().contains("wall"));

    let mut invalid_start = start.clone();
    invalid_start.execution_id = "exec-invalid-tool-output".to_owned();
    send(&mut stdin, "h-invalid", "execution/start", &invalid_start);
    for expected in ["accepted", "started", "task_started", "effect_started"] {
        let WireMessage::Notification { params, .. } = receive(&mut reader) else {
            panic!("missing invalid-outcome execution event");
        };
        assert_eq!(params["kind"], expected);
    }
    assert!(matches!(
        receive(&mut reader),
        WireMessage::Request { ref id, ref method, .. } if id == "r-3" && method == "tool/invoke"
    ));
    send_message(
        &mut stdin,
        &WireMessage::Response {
            id: "r-3".to_owned(),
            result: Some(json!({"outcome":"ok","value":{"secret":CANARY}})),
            error: None,
        },
    );
    let WireMessage::Notification { params, .. } = receive(&mut reader) else {
        panic!("missing invalid-outcome effect event");
    };
    assert_eq!(params["kind"], "effect_failed");
    let WireMessage::Notification { params, .. } = receive(&mut reader) else {
        panic!("missing invalid-outcome terminal event");
    };
    assert_eq!(params["kind"], "completed");
    let WireMessage::Response {
        id,
        result: Some(result),
        ..
    } = receive(&mut reader)
    else {
        panic!("missing invalid-outcome response");
    };
    assert_eq!(id, "h-invalid");
    assert_eq!(result["outcome"], "completed");
    assert_eq!(result["output"]["tag"], "Err");
    assert_eq!(result["output"]["value"]["tag"], "Schema");
    assert_eq!(result["output"]["value"]["value"]["code"], "tool.schema");
    assert_eq!(
        result["output"]["value"]["value"]["message"],
        "the tool response failed schema validation"
    );
    assert!(!serde_json::to_string(&result).unwrap().contains(CANARY));

    let mut unavailable_start = start.clone();
    unavailable_start.execution_id = "exec-tool-unavailable".to_owned();
    send(
        &mut stdin,
        "h-tool-unavailable",
        "execution/start",
        &unavailable_start,
    );
    for expected in ["accepted", "started", "task_started", "effect_started"] {
        let WireMessage::Notification { params, .. } = receive(&mut reader) else {
            panic!("missing unavailable execution event");
        };
        assert_eq!(params["kind"], expected);
    }
    assert!(matches!(
        receive(&mut reader),
        WireMessage::Request { ref id, ref method, .. } if id == "r-4" && method == "tool/invoke"
    ));
    send_message(
        &mut stdin,
        &WireMessage::Response {
            id: "r-4".to_owned(),
            result: None,
            error: Some(WireError {
                code: WireErrorCode::ToolUnavailable,
                message: CANARY.to_owned(),
                data: Some(json!({"secret": CANARY})),
            }),
        },
    );
    let WireMessage::Notification { params, .. } = receive(&mut reader) else {
        panic!("missing unavailable effect event");
    };
    assert_eq!(params["kind"], "effect_failed");
    let WireMessage::Notification { params, .. } = receive(&mut reader) else {
        panic!("missing unavailable terminal event");
    };
    assert_eq!(params["kind"], "completed");
    let unavailable_response = receive(&mut reader);
    assert!(
        !serde_json::to_string(&unavailable_response)
            .unwrap()
            .contains(CANARY)
    );
    let WireMessage::Response {
        id,
        result: Some(result),
        ..
    } = unavailable_response
    else {
        panic!("missing unavailable response");
    };
    assert_eq!(id, "h-tool-unavailable");
    assert_eq!(result["outcome"], "completed");
    assert_eq!(result["output"]["tag"], "Err");
    assert_eq!(result["output"]["value"]["tag"], "Unavailable");
    assert_eq!(
        result["output"]["value"]["value"]["code"],
        "tool.unavailable"
    );

    let mut denied_start = start.clone();
    denied_start.execution_id = "exec-tool-denied".to_owned();
    send(
        &mut stdin,
        "h-tool-denied",
        "execution/start",
        &denied_start,
    );
    for expected in ["accepted", "started", "task_started", "effect_started"] {
        let WireMessage::Notification { params, .. } = receive(&mut reader) else {
            panic!("missing denied execution event");
        };
        assert_eq!(params["kind"], expected);
    }
    assert!(matches!(
        receive(&mut reader),
        WireMessage::Request { ref id, ref method, .. } if id == "r-5" && method == "tool/invoke"
    ));
    send_message(
        &mut stdin,
        &WireMessage::Response {
            id: "r-5".to_owned(),
            result: None,
            error: Some(WireError {
                code: WireErrorCode::ToolDenied,
                message: CANARY.to_owned(),
                data: Some(json!({"secret": CANARY})),
            }),
        },
    );
    let WireMessage::Notification { params, .. } = receive(&mut reader) else {
        panic!("missing denied effect event");
    };
    assert_eq!(params["kind"], "effect_failed");
    let WireMessage::Notification { params, .. } = receive(&mut reader) else {
        panic!("missing denied terminal event");
    };
    assert_eq!(params["kind"], "completed");
    let denied_response = receive(&mut reader);
    assert!(
        !serde_json::to_string(&denied_response)
            .unwrap()
            .contains(CANARY)
    );
    let WireMessage::Response {
        id,
        result: Some(result),
        ..
    } = denied_response
    else {
        panic!("missing denied response");
    };
    assert_eq!(id, "h-tool-denied");
    assert_eq!(result["outcome"], "completed");
    assert_eq!(result["output"]["tag"], "Err");
    assert_eq!(result["output"]["value"]["tag"], "Unavailable");
    assert_eq!(result["output"]["value"]["value"]["code"], "tool.denied");

    let mut cancelled_start = start.clone();
    cancelled_start.execution_id = "exec-tool-cancelled".to_owned();
    send(
        &mut stdin,
        "h-tool-cancelled",
        "execution/start",
        &cancelled_start,
    );
    for expected in ["accepted", "started", "task_started", "effect_started"] {
        let WireMessage::Notification { params, .. } = receive(&mut reader) else {
            panic!("missing provider-cancelled execution event");
        };
        assert_eq!(params["kind"], expected);
    }
    assert!(matches!(
        receive(&mut reader),
        WireMessage::Request { ref id, ref method, .. } if id == "r-6" && method == "tool/invoke"
    ));
    send_message(
        &mut stdin,
        &WireMessage::Response {
            id: "r-6".to_owned(),
            result: None,
            error: Some(WireError {
                code: WireErrorCode::RequestCancelled,
                message: CANARY.to_owned(),
                data: Some(json!({"secret": CANARY})),
            }),
        },
    );
    let WireMessage::Notification { params, .. } = receive(&mut reader) else {
        panic!("missing provider-cancelled effect event");
    };
    assert_eq!(params["kind"], "effect_failed");
    let WireMessage::Notification { params, .. } = receive(&mut reader) else {
        panic!("missing provider-cancelled terminal event");
    };
    assert_eq!(params["kind"], "failed");
    let cancelled_response = receive(&mut reader);
    assert!(
        !serde_json::to_string(&cancelled_response)
            .unwrap()
            .contains(CANARY)
    );
    let WireMessage::Response {
        id,
        result: Some(result),
        ..
    } = cancelled_response
    else {
        panic!("missing provider-cancelled response");
    };
    assert_eq!(id, "h-tool-cancelled");
    assert_eq!(result["outcome"], "failed");
    assert_eq!(result["error"]["code"], "runtime.cancelled");

    let mut wrong_domain_start = start.clone();
    wrong_domain_start.execution_id = "exec-wrong-domain-tool-error".to_owned();
    send(
        &mut stdin,
        "h-wrong-domain",
        "execution/start",
        &wrong_domain_start,
    );
    for expected in ["accepted", "started", "task_started", "effect_started"] {
        let WireMessage::Notification { params, .. } = receive(&mut reader) else {
            panic!("missing wrong-domain execution event");
        };
        assert_eq!(params["kind"], expected);
    }
    assert!(matches!(
        receive(&mut reader),
        WireMessage::Request { ref id, ref method, .. } if id == "r-7" && method == "tool/invoke"
    ));
    send_message(
        &mut stdin,
        &WireMessage::Response {
            id: "r-7".to_owned(),
            result: None,
            error: Some(WireError {
                code: WireErrorCode::UserUnavailable,
                message: CANARY.to_owned(),
                data: Some(json!({"secret": CANARY})),
            }),
        },
    );
    let wrong_domain_effect = receive(&mut reader);
    assert!(
        !serde_json::to_string(&wrong_domain_effect)
            .unwrap()
            .contains(CANARY)
    );
    let WireMessage::Notification { params, .. } = wrong_domain_effect else {
        panic!("missing wrong-domain effect event");
    };
    assert_eq!(params["kind"], "effect_failed");
    let wrong_domain_terminal = receive(&mut reader);
    assert!(
        !serde_json::to_string(&wrong_domain_terminal)
            .unwrap()
            .contains(CANARY)
    );
    let WireMessage::Notification { params, .. } = wrong_domain_terminal else {
        panic!("missing wrong-domain terminal event");
    };
    assert_eq!(params["kind"], "failed");
    let wrong_domain_response = receive(&mut reader);
    assert!(
        !serde_json::to_string(&wrong_domain_response)
            .unwrap()
            .contains(CANARY)
    );
    let WireMessage::Response {
        id,
        result: Some(result),
        ..
    } = wrong_domain_response
    else {
        panic!("missing wrong-domain response");
    };
    assert_eq!(id, "h-wrong-domain");
    assert_eq!(result["outcome"], "failed");
    assert_eq!(result["error"]["code"], "protocol.violation");
    assert_eq!(result["error"]["message"], "runtime protocol violation");

    let mut disconnected_start = start;
    disconnected_start.execution_id = "exec-2".to_owned();
    send(&mut stdin, "h-6", "execution/start", &disconnected_start);
    for expected in ["accepted", "started"] {
        let WireMessage::Notification { params, .. } = receive(&mut reader) else {
            panic!("missing execution event before disconnect");
        };
        assert_eq!(params["kind"], expected);
    }
    for expected in ["task_started", "effect_started"] {
        let WireMessage::Notification { params, .. } = receive(&mut reader) else {
            panic!("missing execution detail event before disconnect");
        };
        assert_eq!(params["kind"], expected);
    }
    assert!(matches!(
        receive(&mut reader),
        WireMessage::Request { ref id, ref method, .. } if id == "r-8" && method == "tool/invoke"
    ));
    drop(stdin);
    let after_disconnect = reader.read_message().unwrap();
    assert!(
        after_disconnect.is_none(),
        "late message: {after_disconnect:?}"
    );
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
}
