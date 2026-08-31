use std::collections::BTreeMap;

use josh_protocol::{
    AgentAskParams, AgentMessageParams, AgentMessageResult, AgentTranscriptParams,
    AgentTranscriptResult, CatalogSetParams, CatalogSetResult, EventKind, EventSequenceTracker,
    ExecutionEventParams, ExecutionStartParams, FEATURES, FileEncoding, GrantDuration,
    HostProjectionSetParams, InitializeParams, InitializeResult, ModelRequestParams,
    PROTOCOL_VERSION, PeerInfo, PermissionRequestParams, PermissionRequestResult,
    PermissionRevokeParams, PermissionRight, PermissionTargetKind, ProgramLoadParams,
    ProjectionSectionKind, ProtocolError, ProtocolLimits, SessionBindingLevel, SourceFile,
    SubAgentAskParams, SubAgentCreateParams, SubAgentCreateResult, SubAgentMessageParams,
    SubAgentRunParams, ToolInvokeResult, TranscriptPart, Validate, request_params,
};
use serde_json::json;

fn initialize() -> InitializeParams {
    serde_json::from_value(json!({
        "host": {"name":"host","version":"1.0.0"},
        "protocol_versions":[PROTOCOL_VERSION],
        "language_versions":[">=0.1.0, <0.2.0"],
        "execution_mode":"unattended",
        "standard_capabilities":["fs.read","fs.write","net.http_get"],
        "limits": {
            "max_frame_bytes":4_194_304,
            "max_active_requests":64,
            "max_loaded_programs":32,
            "max_total_executions":1024,
            "max_catalog_tools":256,
            "max_catalog_bytes":3_145_728
        },
        "extensions":[]
    }))
    .unwrap()
}

fn initialize_for(mode: &str, session_id: &serde_json::Value) -> InitializeParams {
    serde_json::from_value(json!({
        "host": {"name":"host","version":"1.0.0"},
        "protocol_versions":[PROTOCOL_VERSION],
        "language_versions":[">=0.1.0, <0.2.0"],
        "execution_mode":mode,
        "invoking_session_id":session_id,
        "standard_capabilities":["fs.read","fs.write","net.http_get"],
        "limits": {
            "max_frame_bytes":4_194_304,
            "max_active_requests":64,
            "max_loaded_programs":32,
            "max_total_executions":1024,
            "max_catalog_tools":256,
            "max_catalog_bytes":3_145_728
        },
        "extensions":[]
    }))
    .unwrap()
}

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

fn host_projection() -> HostProjectionSetParams {
    let catalog: CatalogSetParams = serde_json::from_value(json!({
        "schema_dialect": "https://json-schema.org/draft/2020-12/schema",
        "metadata": {
            "source": "test-host",
            "source_revision": "revision-7",
            "observed_at_unix_ms": 1,
            "freshness": "current",
            "complete": true
        },
        "tools": []
    }))
    .unwrap();
    HostProjectionSetParams::complete_for_catalog(
        "projection-1",
        PeerInfo {
            name: "host".to_owned(),
            version: "1.0.0".to_owned(),
        },
        SessionBindingLevel::None,
        &catalog,
    )
}

#[test]
fn validates_initialize_exactly() {
    initialize_for("unattended", &json!(null))
        .validate()
        .unwrap();
    let mut bad = initialize();
    bad.protocol_versions.push("josh/unsupported".into());
    assert!(bad.validate().is_err());
    let unknown: Result<InitializeParams, _> = serde_json::from_value(json!({
        "host":{"name":"h","version":"1.0.0"}, "extra": true
    }));
    assert!(unknown.is_err());
}

#[test]
fn catalog_payloads_preserve_provenance_and_projection() {
    let params: CatalogSetParams = serde_json::from_value(json!({
        "schema_dialect": "https://json-schema.org/draft/2020-12/schema",
        "metadata": {
            "source": "test-host",
            "source_revision": "revision-7",
            "observed_at_unix_ms": 1,
            "freshness": "current",
            "complete": true
        },
        "tools": [{
            "name": "example.lookup",
            "version": "1.2.3",
            "description": "Look up one example value.",
            "input_schema": {"type":"string"},
            "output_schema": {"type":"string"},
            "error_schema": {"type":"string"},
            "effects": [],
            "idempotency": "idempotent"
        }]
    }))
    .unwrap();
    params.validate().unwrap();

    let result: CatalogSetResult = serde_json::from_value(json!({
        "catalog_digest": format!("sha256:{}", "a".repeat(64)),
        "schema_profile": "allen.tool-schema/0.1",
        "tool_count": 1,
        "metadata": params.metadata,
        "tools": [{
            "name": "example.lookup",
            "version": "1.2.3",
            "description": "Look up one example value."
        }]
    }))
    .unwrap();
    result.validate().unwrap();

    let mut wrong_count = result.clone();
    wrong_count.tool_count = 2;
    assert!(wrong_count.validate().is_err());
    let mut incomplete = result;
    incomplete.metadata.complete = false;
    assert!(incomplete.validate().is_err());
}

#[test]
fn host_projection_requires_every_complete_canonical_section() {
    let projection = host_projection();
    projection.validate().unwrap();
    assert_eq!(projection.sections.len(), ProjectionSectionKind::ALL.len());
    assert_eq!(
        projection.section(ProjectionSectionKind::Tools).item_count,
        0
    );

    for index in 0..ProjectionSectionKind::ALL.len() {
        let mut missing = projection.clone();
        missing.sections.remove(index);
        assert!(missing.validate().is_err(), "missing section {index}");

        let mut duplicate = projection.clone();
        let replacement = (index + 1) % duplicate.sections.len();
        duplicate.sections[index] = duplicate.sections[replacement].clone();
        assert!(duplicate.validate().is_err(), "duplicate section {index}");

        let mut reordered = projection.clone();
        let other = (index + 1) % reordered.sections.len();
        reordered.sections.swap(index, other);
        assert!(reordered.validate().is_err(), "reordered section {index}");

        let mut incomplete = projection.clone();
        incomplete.sections[index].complete = false;
        assert!(incomplete.validate().is_err(), "incomplete section {index}");

        for invalid in [
            {
                let mut invalid = projection.clone();
                invalid.sections[index].source.clear();
                invalid
            },
            {
                let mut invalid = projection.clone();
                invalid.sections[index].source_revision = "bad\nrevision".to_owned();
                invalid
            },
            {
                let mut invalid = projection.clone();
                invalid.sections[index].observed_at_unix_ms = 0;
                invalid
            },
            {
                let mut invalid = projection.clone();
                invalid.sections[index].item_count = 1_048_577;
                invalid
            },
        ] {
            assert!(invalid.validate().is_err(), "invalid section {index}");
        }
    }
}

#[test]
fn host_projection_rejects_invalid_top_level_and_wire_fields() {
    let projection = host_projection();

    let mut wrong_profile = projection.clone();
    wrong_profile.profile = "josh.host-projection/other".to_owned();
    assert!(wrong_profile.validate().is_err());
    let mut invalid_id = projection.clone();
    invalid_id.projection_id.clear();
    assert!(invalid_id.validate().is_err());
    let mut invalid_host = projection.clone();
    invalid_host.host.name.clear();
    assert!(invalid_host.validate().is_err());

    let original = serde_json::to_value(&projection).unwrap();
    for mutate in [
        |value: &mut serde_json::Value| {
            value["sections"][0]["freshness"] = json!("stale");
        },
        |value: &mut serde_json::Value| {
            value["sections"][0]["kind"] = json!("unknown");
        },
        |value: &mut serde_json::Value| {
            value["sections"][0]
                .as_object_mut()
                .unwrap()
                .remove("source");
        },
        |value: &mut serde_json::Value| {
            value["sections"][0]["unexpected"] = json!(true);
        },
    ] {
        let mut value = original.clone();
        mutate(&mut value);
        assert!(serde_json::from_value::<HostProjectionSetParams>(value).is_err());
    }
}

#[test]
fn host_projection_section_lookup_does_not_depend_on_enum_discriminants() {
    let mut projection = host_projection();
    projection.sections.reverse();
    for kind in ProjectionSectionKind::ALL {
        assert_eq!(projection.section(kind).kind, kind);
    }
}

#[test]
fn validates_current_protocol_and_exact_session_shapes() {
    let attached = initialize_for("attached", &json!("session-7"));
    attached.validate().unwrap();
    attached.validate_protocol_version().unwrap();
    assert_eq!(attached.bound_session_id(), Some("session-7"));
    assert_eq!(
        serde_json::to_value(&attached).unwrap()["invoking_session_id"],
        "session-7"
    );

    let unattended = initialize_for("unattended", &serde_json::Value::Null);
    unattended.validate().unwrap();
    assert_eq!(
        serde_json::to_value(&unattended).unwrap()["invoking_session_id"],
        serde_json::Value::Null
    );

    for bad in [
        json!({
            "protocol_versions":[PROTOCOL_VERSION, "josh/unsupported"],
            "execution_mode":"attached",
            "invoking_session_id":"session"
        }),
        json!({
            "protocol_versions":[PROTOCOL_VERSION],
            "execution_mode":"attached",
            "invoking_session_id":null
        }),
        json!({
            "protocol_versions":[PROTOCOL_VERSION],
            "execution_mode":"unattended",
            "invoking_session_id":"session"
        }),
    ] {
        let mut value = serde_json::to_value(&attached).unwrap();
        let object = value.as_object_mut().unwrap();
        for (key, value) in bad.as_object().unwrap() {
            object.insert(key.clone(), value.clone());
        }
        let params: InitializeParams = serde_json::from_value(value).unwrap();
        assert!(params.validate().is_err());
    }

    let missing_session = {
        let mut value = serde_json::to_value(&attached).unwrap();
        value.as_object_mut().unwrap().remove("invoking_session_id");
        serde_json::from_value::<InitializeParams>(value).unwrap()
    };
    assert!(missing_session.validate().is_err());

    let too_long = initialize_for("attached", &json!("x".repeat(257)));
    assert!(too_long.validate().is_err());
    let control = initialize_for("attached", &json!("bad\nsession"));
    assert!(control.validate().is_err());
}

#[test]
fn initialize_result_features_are_exact() {
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION.into(),
        runtime: PeerInfo {
            name: "runtime".into(),
            version: "0.1.0".into(),
        },
        language_version: "0.1.1".into(),
        features: FEATURES.iter().map(ToString::to_string).collect(),
        limits: limits(),
    };
    result.validate().unwrap();
    let mut bad = result;
    bad.features.push("unexpected".into());
    assert!(bad.validate().is_err());
}

#[test]
fn exec_grants_use_the_strict_josh_1_6_start_contract() {
    assert_eq!(PROTOCOL_VERSION, "josh/1.6");
    assert!(FEATURES.contains(&"exec-run"));
    let value = json!({
        "execution_id":"exec-1",
        "program_id":"program-1",
        "artifact_digest":format!("sha256:{}", "a".repeat(64)),
        "entry":"main",
        "input":null,
        "working_directory":null,
        "granted_capabilities":[],
        "granted_tools":[],
        "allowed_http_origins":[],
        "granted_exec":["git show --stat", "git status"],
        "granted_exec_environment":["GIT_CONFIG_NOSYSTEM", "HOME"],
        "limits":{"wall_ms":5000}
    });
    let params: ExecutionStartParams = serde_json::from_value(value.clone()).unwrap();
    params.validate().unwrap();

    for replacement in [
        json!(["git status", "git show --stat"]),
        json!(["git status", "git status"]),
        json!(["/usr/bin/git status"]),
        json!(["git 'status'"]),
    ] {
        let mut invalid = value.clone();
        invalid["granted_exec"] = replacement;
        let params: ExecutionStartParams = serde_json::from_value(invalid).unwrap();
        assert!(params.validate().is_err());
    }
    for replacement in [
        json!(["HOME", "GIT_CONFIG_NOSYSTEM"]),
        json!(["HOME", "HOME"]),
        json!(["TZ"]),
        json!(["BAD-NAME"]),
    ] {
        let mut invalid = value.clone();
        invalid["granted_exec_environment"] = replacement;
        let params: ExecutionStartParams = serde_json::from_value(invalid).unwrap();
        assert!(params.validate().is_err());
    }

    let mut missing = value;
    missing.as_object_mut().unwrap().remove("granted_exec");
    assert!(serde_json::from_value::<ExecutionStartParams>(missing).is_err());
}

#[test]
fn validates_sub_agent_payloads_and_projection_exactly() {
    let projection = json!({
        "capabilities":["fs.read","net.http_get"],
        "limits":{"instructions":1000,"wall_ms":500},
        "tools":["github.issue.create"]
    });
    let prompt = json!({
        "system":"Review the explicit context only.",
        "context":{"tag":"Some","value":{"visible":true}},
        "data":{"tag":"None"},
        "policy":{"max_attempts":2}
    });
    let schema = json!({
        "digest":format!("sha256:{}", "d".repeat(64)),
        "descriptor":{"type":"boolean"}
    });

    let create: SubAgentCreateParams = serde_json::from_value(json!({
        "execution_id":"exec-1","operation_id":"op-1","prompt":prompt,
        "projection":projection,"deadline_ms":1000
    }))
    .unwrap();
    create.validate().unwrap();

    let run: SubAgentRunParams = serde_json::from_value(json!({
        "execution_id":"exec-1","operation_id":"op-2","interaction_id":"interaction-1",
        "prompt":prompt,"projection":projection,"response_schema":schema,
        "attempt":1,"validation_issues":[],"deadline_ms":1000
    }))
    .unwrap();
    run.validate().unwrap();

    let message: SubAgentMessageParams = serde_json::from_value(json!({
        "execution_id":"exec-1","operation_id":"op-3","sub_agent_id":"child-1",
        "message":"continue","deadline_ms":1000
    }))
    .unwrap();
    message.validate().unwrap();

    let ask: SubAgentAskParams = serde_json::from_value(json!({
        "execution_id":"exec-1","operation_id":"op-4","sub_agent_id":"child-1",
        "interaction_id":"interaction-2","prompt":prompt,"response_schema":schema,
        "attempt":2,"validation_issues":[{"path":"/","code":"type"}],"deadline_ms":1000
    }))
    .unwrap();
    ask.validate().unwrap();

    SubAgentCreateResult {
        sub_agent_id: "child-1".into(),
    }
    .validate()
    .unwrap();
    let mut unsorted = create;
    unsorted.projection.capabilities.reverse();
    assert!(unsorted.validate().is_err());
    let mut widening = run;
    widening.projection.limits.insert("wall_ms".into(), 0);
    assert!(widening.validate().is_err());
    let mut invalid_id = message;
    invalid_id.sub_agent_id = "bad child".into();
    assert!(invalid_id.validate().is_err());

    initialize_for("unattended", &serde_json::Value::Null)
        .validate()
        .unwrap();
}

#[test]
fn validates_typed_response_payloads_exactly() {
    let typed_agent: AgentAskParams = serde_json::from_value(json!({
        "execution_id":"exec-1",
        "operation_id":"op-2",
        "session_id":"session-1",
        "interaction_id":"interaction-1",
        "prompt":{
            "system":"Review only the supplied evidence.",
            "context":{"tag":"None"},
            "data":{"tag":"Some","value":{"change":"safe"}},
            "policy":{"max_attempts":3}
        },
        "response_schema":{
            "digest":format!("sha256:{}", "a".repeat(64)),
            "descriptor":{"type":"object","additionalProperties":false}
        },
        "attempt":2,
        "validation_issues":[{"path":"/decision","code":"required"}],
        "deadline_ms":1000
    }))
    .unwrap();
    typed_agent.validate().unwrap();

    let model: ModelRequestParams = serde_json::from_value(json!({
        "execution_id":"exec-1",
        "operation_id":"op-3",
        "interaction_id":"interaction-2",
        "prompt":{
            "system":"Summarize.",
            "context":{"tag":"Some","value":{"visible":true}},
            "data":{"tag":"Some","value":{"records":[1,2]}},
            "policy":{"max_attempts":2}
        },
        "response_schema":{
            "digest":format!("sha256:{}", "b".repeat(64)),
            "descriptor":{"type":"string"}
        },
        "attempt":1,
        "validation_issues":[],
        "deadline_ms":1000
    }))
    .unwrap();
    model.validate().unwrap();

    let mut extra = serde_json::to_value(&model).unwrap();
    extra
        .as_object_mut()
        .unwrap()
        .insert("session_id".into(), json!("wrong"));
    assert!(serde_json::from_value::<ModelRequestParams>(extra).is_err());

    let mut bad_attempt = model.clone();
    bad_attempt.attempt = 3;
    assert!(bad_attempt.validate().is_err());
    let mut bad_first_issues = model;
    bad_first_issues
        .validation_issues
        .push(josh_protocol::ValidationIssuePayload {
            path: "/value".into(),
            code: "type".into(),
        });
    assert!(bad_first_issues.validate().is_err());

    let _ = initialize_for("unattended", &serde_json::Value::Null);
}

#[test]
fn prompt_segment_presence_distinguishes_absent_from_present_null() {
    use josh_protocol::PromptSegmentPayload;

    let absent = PromptSegmentPayload::from_option(None);
    let present_null = PromptSegmentPayload::from_option(Some(serde_json::Value::Null));
    assert_eq!(
        serde_json::to_value(&absent).unwrap(),
        json!({"tag":"None"})
    );
    assert_eq!(
        serde_json::to_value(&present_null).unwrap(),
        json!({"tag":"Some","value":null})
    );
    assert_ne!(absent, present_null);
    assert!(absent.as_option().is_none());
    assert_eq!(present_null.as_option(), Some(&serde_json::Value::Null));
    assert!(
        serde_json::from_value::<PromptSegmentPayload>(json!({"tag":"None","value":null})).is_err()
    );
    assert!(serde_json::from_value::<PromptSegmentPayload>(json!({"tag":"Some"})).is_err());
}

#[test]
fn validates_exact_agent_payloads() {
    let message: AgentMessageParams = serde_json::from_value(json!({
        "execution_id":"exec-1",
        "operation_id":"op-1",
        "session_id":"session-1",
        "message":"ready",
        "deadline_ms":1000
    }))
    .unwrap();
    message.validate().unwrap();
    AgentMessageResult { accepted: true }.validate().unwrap();
    assert!(AgentMessageResult { accepted: false }.validate().is_err());

    assert!(
        serde_json::from_value::<AgentAskParams>(json!({
            "execution_id":"exec-1",
            "operation_id":"op-2",
            "session_id":"session-1",
            "interaction_id":"interaction-1",
            "message":"review",
            "response_schema":"allen:string/0.1",
            "attempt":1,
            "deadline_ms":1000
        }))
        .is_err()
    );

    let transcript = AgentTranscriptParams {
        execution_id: "exec-1".into(),
        operation_id: "op-3".into(),
        session_id: "session-1".into(),
        limit: 20,
        deadline_ms: 1_000,
    };
    transcript.validate().unwrap();
    let mut invalid = transcript;
    invalid.limit = 101;
    assert!(invalid.validate().is_err());

    let extra: Result<AgentMessageParams, _> = serde_json::from_value(json!({
        "execution_id":"exec-1",
        "operation_id":"op-1",
        "session_id":"session-1",
        "message":"ready",
        "deadline_ms":1000,
        "answer":"not allowed"
    }));
    assert!(extra.is_err());
}

#[test]
fn validates_structured_transcript_and_required_nulls() {
    let result: AgentTranscriptResult = serde_json::from_value(json!({
        "snapshot":{
            "snapshot_id":"snapshot-1",
            "session_id":"session-1",
            "policy_version":"policy-1",
            "captured_at":"2026-08-14T12:30:45.123Z",
            "truncated":true,
            "messages":[
                {
                    "id":null,
                    "role":"user",
                    "time":"2026-08-14T12:00:00Z",
                    "content":[
                        {"kind":"text","text":"hello"},
                        {"kind":"json","value":{"safe":true}},
                        {"kind":"tool_call","name":"lookup","call_id":"call-1","input":null},
                        {"kind":"tool_result","call_id":"call-1","output":{"ok":true},"is_error":false},
                        {"kind":"attachment","media_type":"text/plain","name":null,"content_ref":"attachment-1"},
                        {"kind":"redacted","reason_code":"policy.hidden"},
                        {"kind":"omitted","content_kind":"assistant","count":2}
                    ]
                },
                {
                    "id":"message-2",
                    "role":"assistant",
                    "time":null,
                    "content":[]
                }
            ]
        }
    }))
    .unwrap();
    result.validate_for("session-1", 2).unwrap();
    assert!(result.validate_for("other-session", 2).is_err());
    assert!(result.validate_for("session-1", 1).is_err());
    assert!(matches!(
        result.snapshot.messages[0].content[2],
        TranscriptPart::ToolCall { input: None, .. }
    ));

    let missing_nullable: Result<AgentTranscriptResult, _> = serde_json::from_value(json!({
        "snapshot":{
            "snapshot_id":"snapshot-1",
            "session_id":"session-1",
            "policy_version":"policy-1",
            "captured_at":"2026-08-14T12:30:45Z",
            "truncated":false,
            "messages":[{"role":"user","time":null,"content":[]}]
        }
    }));
    assert!(missing_nullable.is_err());

    let mut reversed = result.clone();
    reversed.snapshot.messages[1].time = Some("2026-08-14T11:00:00Z".into());
    assert!(reversed.validate().is_err());
    let mut bad_timestamp = result;
    bad_timestamp.snapshot.captured_at = "2026-08-14T12:30:45+00:00".into();
    assert!(bad_timestamp.validate().is_err());
}

#[test]
fn validates_file_and_directory_permission_decisions() {
    let file_request = PermissionRequestParams {
        execution_id: "exec-1".into(),
        operation_id: "op-file".into(),
        session_id: "session-1".into(),
        pending_target_id: "pending-1".into(),
        kind: PermissionTargetKind::File,
        path: "/outside/report.txt".into(),
        rights: vec![PermissionRight::Read],
        recursive: false,
        max_bytes: 1_000,
        duration: GrantDuration::Execution,
        reason: "Read the selected report.".into(),
    };
    file_request.validate().unwrap();
    let file_allow = PermissionRequestResult::Allow {
        grant_id: "grant-file".into(),
        path: file_request.path.clone(),
        rights: vec![PermissionRight::Read],
        recursive: false,
        max_bytes: 500,
        duration: GrantDuration::Execution,
    };
    file_allow.validate_for(&file_request).unwrap();
    let file_broaden = PermissionRequestResult::Allow {
        grant_id: "grant-file".into(),
        path: "/outside".into(),
        rights: vec![PermissionRight::Read],
        recursive: false,
        max_bytes: 500,
        duration: GrantDuration::Execution,
    };
    assert!(file_broaden.validate_for(&file_request).is_err());

    let directory_request = PermissionRequestParams {
        execution_id: "exec-1".into(),
        operation_id: "op-dir".into(),
        session_id: "session-1".into(),
        pending_target_id: "pending-2".into(),
        kind: PermissionTargetKind::Directory,
        path: "/outside/reports".into(),
        rights: vec![PermissionRight::Read, PermissionRight::List],
        recursive: true,
        max_bytes: 2_000,
        duration: GrantDuration::Execution,
        reason: "Read selected reports.".into(),
    };
    directory_request.validate().unwrap();
    let directory_allow = PermissionRequestResult::Allow {
        grant_id: "grant-dir".into(),
        path: "/outside/reports/q2".into(),
        rights: vec![PermissionRight::Read],
        recursive: false,
        max_bytes: 1_000,
        duration: GrantDuration::Execution,
    };
    directory_allow.validate_for(&directory_request).unwrap();
    let directory_broaden = PermissionRequestResult::Allow {
        grant_id: "grant-dir".into(),
        path: "/outside/reports-other".into(),
        rights: vec![PermissionRight::Read],
        recursive: false,
        max_bytes: 1_000,
        duration: GrantDuration::Execution,
    };
    assert!(directory_broaden.validate_for(&directory_request).is_err());

    PermissionRequestResult::Deny {
        reason_code: "user.denied".into(),
    }
    .validate_for(&directory_request)
    .unwrap();
    PermissionRevokeParams {
        execution_id: "exec-1".into(),
        session_id: "session-1".into(),
        grant_id: "grant-dir".into(),
    }
    .validate()
    .unwrap();
}

#[test]
fn source_bundle_rejects_unsorted_paths_traversal_and_noncanonical_base64() {
    let good = ProgramLoadParams::SourceBundle {
        files: vec![
            SourceFile {
                path: "allen.toml".into(),
                encoding: FileEncoding::Utf8,
                content: "[package]".into(),
            },
            SourceFile {
                path: "src/main.allen".into(),
                encoding: FileEncoding::Utf8,
                content: "export fn main() {}".into(),
            },
        ],
    };
    good.validate().unwrap();

    let bad = ProgramLoadParams::SourceBundle {
        files: vec![
            SourceFile {
                path: "allen.toml".into(),
                encoding: FileEncoding::Utf8,
                content: String::new(),
            },
            SourceFile {
                path: "../secret".into(),
                encoding: FileEncoding::Base64,
                content: "eA==".into(),
            },
        ],
    };
    assert!(bad.validate().is_err());
    assert!(
        ProgramLoadParams::Bytecode {
            artifact: "eA".into()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn source_bundle_accepts_one_loose_main_source() {
    ProgramLoadParams::SourceBundle {
        files: vec![SourceFile {
            path: "src/main.allen".into(),
            encoding: FileEncoding::Utf8,
            content: "export fn main() returns Int { 42 }".into(),
        }],
    }
    .validate()
    .expect("one loose source file is valid");
}

#[test]
fn tool_outcomes_are_exact_tagged_objects() {
    let ok: ToolInvokeResult =
        serde_json::from_value(json!({"outcome":"ok","value":null})).unwrap();
    assert_eq!(
        ok,
        ToolInvokeResult::Ok {
            value: serde_json::Value::Null
        }
    );
    assert!(
        serde_json::from_value::<ToolInvokeResult>(json!({"outcome":"ok","value":1,"extra":0}))
            .is_err()
    );
    assert!(
        serde_json::from_value::<ToolInvokeResult>(json!({"outcome":"error","value":1})).is_err()
    );
}

#[test]
fn event_fields_are_closed_per_kind() {
    let accepted = ExecutionEventParams {
        execution_id: "exec-1".into(),
        sequence: 1,
        elapsed_ms: 0,
        kind: EventKind::Accepted,
        replayed: false,
        fields: BTreeMap::from([
            (
                "artifact_digest".into(),
                json!(format!("sha256:{}", "a".repeat(64))),
            ),
            (
                "catalog_digest".into(),
                json!(format!("sha256:{}", "b".repeat(64))),
            ),
            ("entry".into(), json!("main")),
            ("program_id".into(), json!("program-1")),
        ]),
    };
    accepted.validate().unwrap();
    let mut leaked = accepted.clone();
    leaked.fields.insert("reason".into(), json!("secret"));
    assert!(leaked.validate().is_err());

    let terminal = ExecutionEventParams {
        execution_id: "exec-1".into(),
        sequence: 2,
        elapsed_ms: 1,
        kind: EventKind::Completed,
        replayed: false,
        fields: BTreeMap::new(),
    };
    terminal.validate().unwrap();

    let permission = ExecutionEventParams {
        execution_id: "exec-1".into(),
        sequence: 2,
        elapsed_ms: 1,
        kind: EventKind::PermissionDecision,
        replayed: false,
        fields: BTreeMap::from([
            ("decision".into(), json!("deny")),
            ("operation_id".into(), json!("op-1")),
            ("reason_code".into(), json!("policy.denied")),
        ]),
    };
    permission.validate().unwrap();
    let mut permission_leak = permission.clone();
    permission_leak
        .fields
        .insert("path".into(), json!("/secret"));
    assert!(permission_leak.validate().is_err());

    let mut sequence = EventSequenceTracker::new("exec-1".into());
    sequence.record(&accepted).unwrap();
    sequence.record(&permission).unwrap();
    let mut terminal = terminal;
    terminal.sequence = 3;
    terminal.elapsed_ms = 2;
    sequence.record(&terminal).unwrap();
    assert!(sequence.is_terminal());
    assert!(sequence.record(&terminal).is_err());
}

#[test]
fn request_param_helper_does_not_reparse_wire_bytes() {
    let initialize = initialize_for("unattended", &json!(null));
    let message = josh_protocol::WireMessage::Request {
        id: "h-1".into(),
        method: "initialize".into(),
        params: serde_json::to_value(&initialize).unwrap(),
    };
    let parsed: InitializeParams = request_params(&message, "initialize").unwrap();
    assert_eq!(parsed, initialize);
    let error: ProtocolError =
        request_params::<InitializeParams>(&message, "catalog/set").unwrap_err();
    assert!(matches!(error, ProtocolError::InvalidMessage(_)));
}
