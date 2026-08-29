#![no_main]

use std::rc::Rc;

use allen_bytecode::{
    BYTECODE_VERSION, EffectOperation, EnumPayloadType, EnumType, EnumVariant, RecordField,
    ValueType, agent_error_type,
};
use allen_testkit::{
    EffectKind, EffectOutcome, EffectRequest, RecordAll, RecordedVmError, Recorder,
    RefuseSensitive, ReplayExecutionOutcome, ReplayHeader, ReplayLimits, ReplayLog, ReplaySession,
    ReplayingEffectProvider, ToolResultSchema,
};
use allen_vm::{
    CancellationSource, EffectProvider, EnumIdentity, EnumPayload, EnumValue, PendingEffectId,
    Value, VmError, decode_canonical_with_limit, encode_canonical_with_limit,
};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 256 * 1024;
const LIMITS: ReplayLimits = ReplayLimits {
    entries: 64,
    payload_bytes: 64 * 1024,
    document_bytes: MAX_INPUT_BYTES,
};

fn canonical(value: &Value) -> Vec<u8> {
    encode_canonical_with_limit(value, LIMITS.payload_bytes as u64).expect("bounded fixture value")
}

fn result_value(ok: bool, payload: Value) -> Value {
    Value::Enum(Rc::new(EnumValue {
        identity: EnumIdentity::Result,
        type_name: Rc::from("Result"),
        variant: u32::from(!ok),
        variant_name: Rc::from(if ok { "Ok" } else { "Err" }),
        payload: EnumPayload::Tuple(Rc::from([payload])),
    }))
}

fn standard_error(code: &str, message: &str) -> Value {
    Value::Record(Rc::from([
        (Rc::from("code"), Value::String(Rc::from(code))),
        (Rc::from("message"), Value::String(Rc::from(message))),
    ]))
}

fn generated_tool_error(selector: u8) -> Value {
    let (variant, name, payload) = match selector % 3 {
        0 => (
            0,
            "Declared",
            EnumPayload::Tuple(Rc::from([Value::String(Rc::from("declared"))])),
        ),
        1 => (
            1,
            "Unavailable",
            EnumPayload::Record(Rc::from([
                (
                    Rc::from("code"),
                    Value::String(Rc::from("tool.unavailable")),
                ),
                (
                    Rc::from("message"),
                    Value::String(Rc::from("tool provider is unavailable")),
                ),
            ])),
        ),
        _ => (
            2,
            "Schema",
            EnumPayload::Record(Rc::from([
                (Rc::from("code"), Value::String(Rc::from("tool.schema"))),
                (
                    Rc::from("message"),
                    Value::String(Rc::from("tool result failed schema validation")),
                ),
            ])),
        ),
    };
    Value::Enum(Rc::new(EnumValue {
        identity: EnumIdentity::User(0),
        type_name: Rc::from("pkg://fuzz/root.allen::_tool_demo_echo_Error"),
        variant,
        variant_name: Rc::from(name),
        payload,
    }))
}

fn fuzz_enum_types() -> Vec<EnumType> {
    let standard_error = EnumPayloadType::Record(vec![
        RecordField {
            name: "code".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "message".to_owned(),
            value_type: ValueType::String,
        },
    ]);
    vec![
        EnumType {
            name: "pkg://fuzz/root.allen::_tool_demo_echo_Error".to_owned(),
            variants: vec![
                EnumVariant {
                    name: "Declared".to_owned(),
                    payload: EnumPayloadType::Tuple(vec![ValueType::String]),
                },
                EnumVariant {
                    name: "Unavailable".to_owned(),
                    payload: standard_error.clone(),
                },
                EnumVariant {
                    name: "Schema".to_owned(),
                    payload: standard_error,
                },
            ],
        },
        EnumType {
            name: "pkg://fuzz/root.allen::Response".to_owned(),
            variants: vec![
                EnumVariant {
                    name: "Wrapped".to_owned(),
                    payload: EnumPayloadType::Tuple(vec![ValueType::Enum(2)]),
                },
                EnumVariant {
                    name: "Batch".to_owned(),
                    payload: EnumPayloadType::Record(vec![RecordField {
                        name: "items".to_owned(),
                        value_type: ValueType::List(Box::new(ValueType::Enum(2))),
                    }]),
                },
            ],
        },
        EnumType {
            name: "pkg://fuzz/root.allen::Detail".to_owned(),
            variants: vec![
                EnumVariant {
                    name: "Text".to_owned(),
                    payload: EnumPayloadType::Tuple(vec![ValueType::String]),
                },
                EnumVariant {
                    name: "Count".to_owned(),
                    payload: EnumPayloadType::Record(vec![RecordField {
                        name: "count".to_owned(),
                        value_type: ValueType::Int,
                    }]),
                },
            ],
        },
    ]
}

fn nominal_enum(type_id: u32, variant: u32, payload: EnumPayload) -> Value {
    Value::Enum(Rc::new(EnumValue {
        identity: EnumIdentity::User(type_id),
        type_name: Rc::from("<canonical>"),
        variant,
        variant_name: Rc::from("<canonical>"),
        payload,
    }))
}

fn header(data: &[u8]) -> ReplayHeader {
    let digest = |offset: usize| {
        let mut output = [0_u8; 32];
        for (index, byte) in output.iter_mut().enumerate() {
            *byte = data.get(offset + index).copied().unwrap_or(index as u8) | 1;
        }
        output
    };
    let grants = match data.first().copied().unwrap_or_default() % 4 {
        0 => vec![],
        1 => vec!["fs.read".to_owned()],
        2 => vec!["fs.write".to_owned(), "net.http_get".to_owned()],
        _ => vec!["permission.request_external_fs".to_owned()],
    };
    ReplayHeader {
        bytecode_version: BYTECODE_VERSION,
        artifact_digest: digest(0),
        contract_digest: digest(1),
        language_digest: digest(2),
        runtime_digest: digest(3),
        policy_digest: digest(4),
        catalog_digest: digest(5),
        capability_digest: digest(6),
        error_registry_digest: digest(7),
        effective_manifest_grants: grants,
        requested_exec_commands: Vec::new(),
        requested_exec_environment: Vec::new(),
        effective_exec_grants: Vec::new(),
        effective_exec_environment: Vec::new(),
        effective_exec_environment_digest: [0; 32],
        pinned_exec_identity_digest: [0; 32],
        scheduler_completion_order: Vec::new(),
    }
}

fn request(kind: EffectKind, input: Value, result_type: &ValueType) -> EffectRequest {
    EffectRequest::from_value(kind, &input, result_type, LIMITS).expect("bounded fixture request")
}

struct NeverCancelled;

impl CancellationSource for NeverCancelled {
    fn is_cancelled(&mut self) -> bool {
        false
    }
}

struct FuzzToolSchemas;

impl ToolResultSchema for FuzzToolSchemas {
    fn result_type(&self, tool: u32) -> Option<ValueType> {
        (tool == 0).then_some(ValueType::Result(
            Box::new(ValueType::String),
            Box::new(ValueType::Enum(0)),
        ))
    }

    fn validate_result(&self, tool: u32, value: &Value) -> bool {
        if tool != 0 {
            return false;
        }
        let Value::Enum(result) = value else {
            return false;
        };
        let EnumPayload::Tuple(payload) = &result.payload else {
            return false;
        };
        match (result.variant, payload.first()) {
            (0, Some(Value::String(value))) => value.len() <= 32,
            (1, Some(Value::Enum(error))) if error.variant == 0 => {
                matches!(&error.payload, EnumPayload::Tuple(payload) if matches!(payload.first(), Some(Value::String(value)) if value.len() <= 32))
            }
            (1, Some(Value::Enum(error))) if matches!(error.variant, 1 | 2) => {
                matches!(&error.payload, EnumPayload::Record(fields) if matches!(fields.as_ref(), [(_, Value::String(_)), (_, Value::String(_))]))
            }
            _ => false,
        }
    }
}

fn one_entry_log(data: &[u8], request: EffectRequest, outcome: EffectOutcome) -> ReplayLog {
    let replay_header = header(data);
    let mut recorder = Recorder::with_header(LIMITS, replay_header);
    recorder
        .record(request, outcome, false, &RefuseSensitive)
        .expect("bounded replay entry");
    recorder
        .finish_with_execution_outcome(ReplayExecutionOutcome::Completed)
        .expect("valid replay envelope")
}

fn exercise_operation_result_registry(data: &[u8]) {
    let selector = data.first().copied().unwrap_or_default();
    let (operation, valid_code, wrong_code) = match selector % 3 {
        0 => (
            EffectOperation::AgentAsk,
            "agent.unavailable",
            "tool.unavailable",
        ),
        1 => (
            EffectOperation::ModelRequest,
            "model.validation_failed",
            "permission.unavailable",
        ),
        _ => (
            EffectOperation::UserAsk,
            "user.validation_failed",
            "sub_agent.unavailable",
        ),
    };
    let valid = selector & 4 == 0;
    let result_type = ValueType::Result(Box::new(ValueType::String), Box::new(agent_error_type()));
    let arguments = [Value::String(Rc::from("request"))];
    let replay_request = request(
        EffectKind::Agent(operation.required_effect().to_owned()),
        Value::Tuple(Rc::from(arguments.clone())),
        &result_type,
    );
    let completion = result_value(
        false,
        standard_error(
            if valid { valid_code } else { wrong_code },
            "bounded provider error",
        ),
    );
    let log = one_entry_log(
        data,
        replay_request,
        EffectOutcome::Ok(canonical(&completion)),
    );
    let mut replay = ReplayingEffectProvider::new(
        &log,
        log.header(),
        LIMITS,
        FuzzToolSchemas,
        &fuzz_enum_types(),
    )
    .unwrap();
    replay
        .start_agent(
            PendingEffectId(1),
            operation,
            &arguments,
            &result_type,
            &mut NeverCancelled,
        )
        .unwrap();
    let polled = replay.poll_effect(PendingEffectId(1), &mut NeverCancelled);
    assert_eq!(
        polled.is_ok(),
        valid,
        "wrong-domain standard Result was released"
    );
    if !valid {
        assert_eq!(polled, Err(VmError::ReplayRuntimeDiverged));
    }
}

fn exercise_tool_wrapper_registry(data: &[u8]) {
    let selector = data.get(1).copied().unwrap_or_default();
    let (variant, name, valid_code, wrong_code) = if selector & 1 == 0 {
        (1, "Unavailable", "tool.unavailable", "agent.unavailable")
    } else {
        (2, "Schema", "tool.schema", "tool.unavailable")
    };
    let valid = selector & 2 == 0;
    let tool_error = Value::Enum(Rc::new(EnumValue {
        identity: EnumIdentity::User(0),
        type_name: Rc::from("pkg://fuzz/root.allen::_tool_demo_echo_Error"),
        variant,
        variant_name: Rc::from(name),
        payload: EnumPayload::Record(Rc::from([
            (
                Rc::from("code"),
                Value::String(Rc::from(if valid { valid_code } else { wrong_code })),
            ),
            (Rc::from("message"), Value::String(Rc::from("bounded"))),
        ])),
    }));
    let result_type = FuzzToolSchemas.result_type(0).unwrap();
    let input = Value::String(Rc::from("input"));
    let replay_request = request(EffectKind::Tool(0), input.clone(), &result_type);
    let completion = result_value(false, tool_error);
    let log = one_entry_log(
        data,
        replay_request,
        EffectOutcome::Ok(canonical(&completion)),
    );
    let mut replay = ReplayingEffectProvider::new(
        &log,
        log.header(),
        LIMITS,
        FuzzToolSchemas,
        &fuzz_enum_types(),
    )
    .unwrap();
    replay
        .start_tool(
            PendingEffectId(1),
            0,
            &input,
            &result_type,
            &mut NeverCancelled,
        )
        .unwrap();
    let polled = replay.poll_effect(PendingEffectId(1), &mut NeverCancelled);
    assert_eq!(
        polled.is_ok(),
        valid,
        "wrong generated-tool operational code was released"
    );
    if !valid {
        assert_eq!(polled, Err(VmError::ReplayRuntimeDiverged));
    }
}

fn exercise_nominal_enum_validation(data: &[u8]) {
    let selector = data.get(2).copied().unwrap_or_default();
    let valid = selector % 7 == 0;
    let completion = if valid {
        nominal_enum(
            1,
            0,
            EnumPayload::Tuple(Rc::from([nominal_enum(
                2,
                0,
                EnumPayload::Tuple(Rc::from([Value::String(Rc::from("bounded"))])),
            )])),
        )
    } else {
        match selector % 6 {
            0 => nominal_enum(1, 99, EnumPayload::Unit),
            1 => nominal_enum(1, 0, EnumPayload::Unit),
            2 => nominal_enum(1, 0, EnumPayload::Tuple(Rc::from([]))),
            3 => nominal_enum(
                1,
                0,
                EnumPayload::Tuple(Rc::from([nominal_enum(2, 99, EnumPayload::Unit)])),
            ),
            4 => nominal_enum(
                1,
                0,
                EnumPayload::Tuple(Rc::from([nominal_enum(
                    2,
                    0,
                    EnumPayload::Tuple(Rc::from([Value::Int(7)])),
                )])),
            ),
            _ => nominal_enum(
                1,
                1,
                EnumPayload::Record(Rc::from([(Rc::from("wrong"), Value::List(Rc::from([])))])),
            ),
        }
    };
    let result_type = ValueType::Result(Box::new(ValueType::Enum(1)), Box::new(agent_error_type()));
    let arguments = [Value::String(Rc::from("request"))];
    let replay_request = request(
        EffectKind::Agent("agent.ask".to_owned()),
        Value::Tuple(Rc::from(arguments.clone())),
        &result_type,
    );
    let log = one_entry_log(
        data,
        replay_request,
        EffectOutcome::Ok(canonical(&result_value(true, completion))),
    );
    let mut replay = ReplayingEffectProvider::new(
        &log,
        log.header(),
        LIMITS,
        FuzzToolSchemas,
        &fuzz_enum_types(),
    )
    .unwrap();
    replay
        .start_agent(
            PendingEffectId(1),
            EffectOperation::AgentAsk,
            &arguments,
            &result_type,
            &mut NeverCancelled,
        )
        .unwrap();
    let polled = replay.poll_effect(PendingEffectId(1), &mut NeverCancelled);
    assert_eq!(polled.is_ok(), valid, "malformed nominal enum was released");
    if !valid {
        assert_eq!(polled, Err(VmError::ReplayRuntimeDiverged));
    }
}

fn exercise_raw_error_mutation(data: &[u8]) {
    let result_type = ValueType::Result(Box::new(ValueType::String), Box::new(agent_error_type()));
    let replay_request = request(
        EffectKind::Agent("agent.ask".to_owned()),
        Value::String(Rc::from("request")),
        &result_type,
    );
    let log = one_entry_log(
        data,
        replay_request,
        EffectOutcome::Err {
            error: RecordedVmError::AgentUnavailable,
        },
    );
    let json = log.to_json().unwrap();
    let forged = json.replacen("AgentUnavailable", "ToolUnavailable", 1);
    assert_eq!(
        ReplayLog::from_json(&forged, LIMITS),
        Err(allen_testkit::ReplayError::InvalidJournal),
        "wrong-domain raw provider error survived journal validation"
    );
}

fn synthesize_log(data: &[u8]) -> ReplayLog {
    let selector = data.first().copied().unwrap_or_default();
    let tool_result_type =
        ValueType::Result(Box::new(ValueType::String), Box::new(ValueType::Enum(0)));
    let agent_result_type =
        ValueType::Result(Box::new(ValueType::String), Box::new(agent_error_type()));
    let requests = [
        request(
            EffectKind::Tool(u32::from(selector % 4)),
            Value::Record(Rc::from([(
                Rc::from("value"),
                Value::String(Rc::from("request")),
            )])),
            &tool_result_type,
        ),
        request(
            EffectKind::Agent("agent.ask".to_owned()),
            Value::String(Rc::from("question")),
            &agent_result_type,
        ),
        request(
            EffectKind::Call("permission.request_file".to_owned()),
            Value::Unit,
            &ValueType::Unit,
        ),
    ];
    let tool_value = if selector & 1 == 0 {
        result_value(true, Value::String(Rc::from("tool output")))
    } else {
        result_value(false, generated_tool_error(selector >> 1))
    };
    let outcomes = [
        EffectOutcome::Ok(canonical(&tool_value)),
        EffectOutcome::Ok(canonical(&result_value(
            false,
            standard_error("agent.unavailable", "agent provider is unavailable"),
        ))),
        EffectOutcome::Err {
            error: match selector % 4 {
                0 => RecordedVmError::ProtocolViolation,
                1 => RecordedVmError::CapabilityMissing,
                2 => RecordedVmError::Cancelled,
                _ => RecordedVmError::AgentUnavailable,
            },
        },
    ];

    let mut recorder = Recorder::with_header(LIMITS, header(data));
    let sequences = requests.map(|request| recorder.start(request, false).unwrap());
    let order = match selector % 3 {
        0 => [0, 1, 2],
        1 => [2, 0, 1],
        _ => [1, 2, 0],
    };
    for index in order {
        recorder
            .complete(sequences[index], outcomes[index].clone(), &RefuseSensitive)
            .unwrap();
    }
    let final_outcome = match selector % 3 {
        0 => ReplayExecutionOutcome::Completed,
        1 => ReplayExecutionOutcome::Stopped {
            reason: "stopped by fuzz fixture".to_owned(),
        },
        _ => ReplayExecutionOutcome::Terminal {
            error: RecordedVmError::ProtocolViolation,
        },
    };
    recorder
        .finish_with_execution_outcome_policy(final_outcome, &RecordAll, &RefuseSensitive)
        .expect("bounded canonical replay")
}

fn exercise_valid_log(log: &ReplayLog) {
    let json = log.to_json().expect("serializable replay");
    let reparsed = ReplayLog::from_json(&json, LIMITS).expect("canonical replay reparses");
    assert_eq!(&reparsed, log);
    for entry in log.entries() {
        entry
            .outcome
            .validate(LIMITS)
            .expect("validated replay outcome");
        if let EffectOutcome::Ok(bytes) = &entry.outcome {
            let value = decode_canonical_with_limit(bytes, LIMITS.payload_bytes as u64)
                .expect("validated canonical outcome");
            assert_eq!(canonical(&value), *bytes);
        }
    }

    assert!(ReplayLog::from_json(&format!(" {json}"), LIMITS).is_err());
    if json.contains("ALLEN-REPLAY/3") {
        assert!(
            ReplayLog::from_json(
                &json.replacen("ALLEN-REPLAY/3", "ALLEN-REPLAY/2", 1),
                LIMITS
            )
            .is_err()
        );
        assert!(
            ReplayLog::from_json(
                &json.replacen("\"bytecode_version\":16", "\"bytecode_version\":15", 1),
                LIMITS
            )
            .is_err()
        );
    }
}

fn exercise(data: &[u8]) {
    let input = &data[..data.len().min(MAX_INPUT_BYTES)];
    if let Ok(text) = std::str::from_utf8(input) {
        if let Ok(log) = ReplayLog::from_json(text, LIMITS) {
            assert_eq!(log.to_json().unwrap(), text);
            exercise_valid_log(&log);
            if log.entries().is_empty() && log.execution_outcome().is_some() {
                ReplaySession::new(&log)
                    .finish_with_execution_outcome(log.execution_outcome().unwrap())
                    .expect("empty replay reaches its exact final channel");
            }
        }
    }

    let synthesized = synthesize_log(input);
    exercise_valid_log(&synthesized);
    exercise_raw_error_mutation(input);
    exercise_operation_result_registry(input);
    exercise_tool_wrapper_registry(input);
    exercise_nominal_enum_validation(input);

    let json = synthesized.to_json().unwrap();
    if !json.is_empty() {
        let cut = usize::from(input.first().copied().unwrap_or_default()) % json.len();
        let _ = ReplayLog::from_json(&json[..cut], LIMITS);
    }
}

fuzz_target!(|data: &[u8]| exercise(data));
