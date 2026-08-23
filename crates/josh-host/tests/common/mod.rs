#![allow(dead_code)]

use std::collections::BTreeMap;

use allen_bytecode::{
    Artifact, ArtifactMetadata, Constant, EntryContract, Function, Instruction, ManifestContract,
    Module, StrictSchema, ValueType, compute_tool_contract_digest, encode,
};
use base64::Engine as _;
use josh_host::Session;
use josh_protocol::{
    CatalogSetParams, ExecutionMode, ExecutionStartParams, InitializeParams, InvokingSessionId,
    PeerInfo, ProgramLoadParams, ProgramLoadResult, ProtocolLimits,
};

pub fn initialize_params() -> InitializeParams {
    InitializeParams {
        host: PeerInfo {
            name: "integration-host".to_owned(),
            version: "1.0.0".to_owned(),
        },
        protocol_versions: vec![josh_protocol::PROTOCOL_VERSION.to_owned()],
        language_versions: vec![">=0.1.0, <0.2.0".to_owned()],
        execution_mode: ExecutionMode::Unattended,
        invoking_session_id: InvokingSessionId::Null,
        standard_capabilities: Vec::new(),
        limits: ProtocolLimits {
            max_frame_bytes: 4_194_304,
            max_active_requests: 64,
            max_loaded_programs: 32,
            max_total_executions: 1_024,
            max_catalog_tools: 256,
            max_catalog_bytes: 3_145_728,
        },
        extensions: Vec::new(),
    }
}

pub fn initialized_session() -> Session {
    let mut session = Session::new();
    session.initialize(&initialize_params()).unwrap();
    session
        .set_catalog(&CatalogSetParams {
            schema_dialect: josh_protocol::SCHEMA_DIALECT.to_owned(),
            metadata: josh_protocol::CatalogMetadata::complete("test-host", "1", 1),
            tools: Vec::new(),
        })
        .unwrap();
    session
}

pub fn load_unit_program(session: &mut Session) -> ProgramLoadResult {
    let artifact = Artifact {
        metadata: ArtifactMetadata::default(),
        module: Module {
            constants: vec![Constant::Unit],
            enum_types: Vec::new(),
            effect_sets: vec![Vec::new()],
            functions: vec![Function {
                name: "pkg/x74657374/x302e312e30/x737263/x6d61696e.allen::main".to_owned(),
                parameters: vec![0],
                captures: Vec::new(),
                registers: vec![ValueType::Unit],
                return_type: ValueType::Unit,
                effects: 0,
                code: vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Return { source: 0 },
                ],
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
    };
    session
        .load_program(&ProgramLoadParams::Bytecode {
            artifact: base64::engine::general_purpose::STANDARD.encode(encode(&artifact).unwrap()),
        })
        .unwrap()
}

pub fn execution_params(loaded: ProgramLoadResult, execution_id: &str) -> ExecutionStartParams {
    ExecutionStartParams {
        execution_id: execution_id.to_owned(),
        program_id: loaded.program_id,
        artifact_digest: loaded.artifact_digest,
        entry: "main".to_owned(),
        input: serde_json::Value::Null,
        working_directory: None,
        granted_capabilities: Vec::new(),
        granted_tools: Vec::new(),
        allowed_http_origins: Vec::new(),
        limits: BTreeMap::from([("wall_ms".to_owned(), 1_000)]),
    }
}
