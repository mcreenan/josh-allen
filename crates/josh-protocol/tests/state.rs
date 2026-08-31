use josh_protocol::{
    ConnectionState, InitializeParams, PeerRole, ProtocolTracker, ReceiveAction, WireErrorCode,
    WireMessage,
};
use serde_json::json;

fn request(id: &str, method: &str) -> WireMessage {
    WireMessage::Request {
        id: id.into(),
        method: method.into(),
        params: json!({}),
    }
}

fn response(id: &str) -> WireMessage {
    WireMessage::Response {
        id: id.into(),
        result: Some(json!({})),
        error: None,
    }
}

fn initialize_for(mode: &str, session_id: &serde_json::Value) -> InitializeParams {
    serde_json::from_value(json!({
        "host":{"name":"host","version":"1.0.0"},
        "protocol_versions":[josh_protocol::PROTOCOL_VERSION],
        "language_versions":[">=0.1.0, <0.2.0"],
        "execution_mode":mode,
        "invoking_session_id":session_id,
        "standard_capabilities":[],
        "limits":{
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

fn active_host(params: &InitializeParams) -> ProtocolTracker {
    let mut host = ProtocolTracker::new(PeerRole::Host, 16);
    host.register_outgoing_request("init", "initialize")
        .unwrap();
    host.initialize_succeeded_with(params).unwrap();
    host.register_outgoing_request("projection", "host/project")
        .unwrap();
    host.projection_succeeded().unwrap();
    host.register_outgoing_request("catalog", "catalog/set")
        .unwrap();
    host.catalog_succeeded().unwrap();
    host.register_outgoing_request("load", "program/load")
        .unwrap();
    host.program_loaded().unwrap();
    host.register_outgoing_request("run", "execution/start")
        .unwrap();
    host.execution_started_with("exec-1").unwrap();
    host
}

fn active_runtime(params: &InitializeParams) -> ProtocolTracker {
    let mut runtime = ProtocolTracker::new(PeerRole::Runtime, 16);
    runtime.initialize_succeeded_with(params).unwrap();
    runtime.projection_succeeded().unwrap();
    runtime.catalog_succeeded().unwrap();
    runtime.program_loaded().unwrap();
    runtime.execution_started_with("exec-1").unwrap();
    runtime
}

fn agent_message(execution_id: &str, session_id: &str) -> WireMessage {
    WireMessage::Request {
        id: "agent-1".into(),
        method: "agent/message".into(),
        params: json!({
            "execution_id":execution_id,
            "operation_id":"op-1",
            "session_id":session_id,
            "message":"ready",
            "deadline_ms":1000
        }),
    }
}

fn bound_requests() -> Vec<WireMessage> {
    vec![
        agent_message("exec-1", "session-1"),
        typed_response_request("agent/ask", Some("session-1")),
        WireMessage::Request {
            id: "agent-transcript".into(),
            method: "agent/transcript".into(),
            params: json!({
                "execution_id":"exec-1",
                "operation_id":"op-3",
                "session_id":"session-1",
                "limit":20,
                "deadline_ms":1000
            }),
        },
        WireMessage::Request {
            id: "permission".into(),
            method: "permission/request".into(),
            params: json!({
                "execution_id":"exec-1",
                "operation_id":"op-4",
                "session_id":"session-1",
                "pending_target_id":"pending-1",
                "kind":"directory",
                "path":"/outside/reports",
                "rights":["read","list"],
                "recursive":false,
                "max_bytes":1000,
                "duration":"execution",
                "reason":"Read selected reports."
            }),
        },
    ]
}

fn typed_response_request(method: &str, session_id: Option<&str>) -> WireMessage {
    let mut params = json!({
        "execution_id":"exec-1",
        "operation_id":"op-typed",
        "interaction_id":"interaction-typed",
        "prompt":{
            "system":"Review.",
            "context":{"tag":"None"},
            "data":{"tag":"Some","value":{"value":1}},
            "policy":{"max_attempts":2}
        },
        "response_schema":{
            "digest":format!("sha256:{}", "c".repeat(64)),
            "descriptor":{"type":"boolean"}
        },
        "attempt":1,
        "validation_issues":[],
        "deadline_ms":1000
    });
    if let Some(session_id) = session_id {
        params
            .as_object_mut()
            .unwrap()
            .insert("session_id".into(), json!(session_id));
    }
    WireMessage::Request {
        id: format!("{method}-1"),
        method: method.into(),
        params,
    }
}

fn sub_agent_request(method: &str) -> WireMessage {
    let prompt = json!({
        "system":"Use only projected context.",
        "context":{"tag":"None"},
        "data":{"tag":"None"},
        "policy":{"max_attempts":1}
    });
    let mut params = match method {
        "sub_agent/create" => json!({
            "execution_id":"exec-1","operation_id":"op-sub","prompt":prompt,
            "projection":{"capabilities":[],"limits":{},"tools":[]},"deadline_ms":1000
        }),
        "sub_agent/run" => json!({
            "execution_id":"exec-1","operation_id":"op-sub","interaction_id":"int-sub",
            "prompt":prompt,"projection":{"capabilities":[],"limits":{},"tools":[]},
            "response_schema":{"digest":format!("sha256:{}", "a".repeat(64)),"descriptor":{"type":"boolean"}},
            "attempt":1,"validation_issues":[],"deadline_ms":1000
        }),
        "sub_agent/message" => json!({
            "execution_id":"exec-1","operation_id":"op-sub","sub_agent_id":"child-1",
            "message":"continue","deadline_ms":1000
        }),
        "sub_agent/ask" => json!({
            "execution_id":"exec-1","operation_id":"op-sub","sub_agent_id":"child-1",
            "interaction_id":"int-sub","prompt":prompt,
            "response_schema":{"digest":format!("sha256:{}", "a".repeat(64)),"descriptor":{"type":"boolean"}},
            "attempt":1,"validation_issues":[],"deadline_ms":1000
        }),
        _ => unreachable!(),
    };
    params["execution_id"] = json!("exec-1");
    WireMessage::Request {
        id: format!("{method}-1"),
        method: method.into(),
        params,
    }
}

#[test]
fn sub_agent_routes_work_with_or_without_an_invoking_session() {
    for mode in ["attached", "unattended"] {
        let session = if mode == "attached" {
            json!("session-1")
        } else {
            json!(null)
        };
        let params = initialize_for(mode, &session);
        let mut host = active_host(&params);
        for method in [
            "sub_agent/create",
            "sub_agent/run",
            "sub_agent/message",
            "sub_agent/ask",
        ] {
            host.receive(&sub_agent_request(method)).unwrap();
            host.commit_response(&format!("{method}-1")).unwrap();
        }
    }
}

fn accepted_event(replayed: Option<bool>) -> WireMessage {
    let mut params = json!({
        "execution_id":"exec-1",
        "sequence":1,
        "elapsed_ms":0,
        "kind":"accepted",
        "fields":{
            "artifact_digest":format!("sha256:{}", "a".repeat(64)),
            "catalog_digest":format!("sha256:{}", "b".repeat(64)),
            "entry":"main",
            "program_id":"program-1"
        }
    });
    if let Some(replayed) = replayed {
        params
            .as_object_mut()
            .unwrap()
            .insert("replayed".into(), json!(replayed));
    }
    WireMessage::Notification {
        method: "execution/event".into(),
        params,
    }
}

#[test]
fn replay_markers_are_required() {
    for replayed in [false, true] {
        let mut host = active_host(&initialize_for("unattended", &json!(null)));
        assert_eq!(
            host.receive(&accepted_event(Some(replayed))).unwrap(),
            ReceiveAction::NotificationAccepted
        );
    }
    let mut missing = active_host(&initialize_for("unattended", &json!(null)));
    assert_eq!(
        missing.receive(&accepted_event(None)).unwrap_err().code,
        WireErrorCode::ProtocolViolation
    );
}

#[test]
fn provider_routes_preserve_identity_rules() {
    for mode in ["attached", "unattended"] {
        let session = if mode == "attached" {
            json!("session-1")
        } else {
            json!(null)
        };
        let params = initialize_for(mode, &session);
        let mut host = active_host(&params);
        for method in ["model/request", "user/ask"] {
            let message = typed_response_request(method, None);
            assert_eq!(
                host.receive(&message).unwrap(),
                ReceiveAction::RequestAccepted
            );
            host.commit_response(match method {
                "model/request" => "model/request-1",
                _ => "user/ask-1",
            })
            .unwrap();
        }
    }

    let params = initialize_for("attached", &json!("session-1"));
    let mut host = active_host(&params);
    host.receive(&typed_response_request("agent/ask", Some("session-1")))
        .unwrap();

    let mut wrong_execution = typed_response_request("model/request", None);
    let WireMessage::Request { params, .. } = &mut wrong_execution else {
        unreachable!()
    };
    params["execution_id"] = json!("exec-other");
    assert!(host.receive(&wrong_execution).unwrap_err().fatal);
}

#[test]
fn tracks_same_text_id_independently_by_direction() {
    let mut runtime = ProtocolTracker::new(PeerRole::Runtime, 4);
    runtime.receive(&request("init", "initialize")).unwrap();
    runtime.initialize_succeeded().unwrap();
    runtime.commit_response("init").unwrap();
    runtime
        .receive(&request("projection", "host/project"))
        .unwrap();
    runtime.projection_succeeded().unwrap();
    runtime.commit_response("projection").unwrap();
    runtime.receive(&request("catalog", "catalog/set")).unwrap();
    runtime.catalog_succeeded().unwrap();
    runtime.commit_response("catalog").unwrap();
    runtime.receive(&request("load", "program/load")).unwrap();
    runtime.program_loaded().unwrap();
    runtime.commit_response("load").unwrap();
    runtime
        .receive(&request("same", "execution/start"))
        .unwrap();
    runtime.execution_started().unwrap();
    runtime
        .register_outgoing_request("same", "tool/invoke")
        .unwrap();
    assert_eq!(runtime.active_incoming(), 1);
    assert_eq!(runtime.active_outgoing(), 1);
}

#[test]
fn rejects_duplicates_unknown_responses_and_active_overflow() {
    let mut runtime = ProtocolTracker::new(PeerRole::Runtime, 1);
    runtime.receive(&request("one", "initialize")).unwrap();
    let duplicate = runtime.receive(&request("one", "initialize")).unwrap_err();
    assert_eq!(duplicate.code, WireErrorCode::ProtocolViolation);
    assert!(duplicate.fatal);

    let mut host = ProtocolTracker::new(PeerRole::Host, 1);
    let unknown = host.receive(&response("absent")).unwrap_err();
    assert!(unknown.fatal);

    let mut bounded = ProtocolTracker::new(PeerRole::Runtime, 1);
    bounded.receive(&request("init", "initialize")).unwrap();
    bounded.initialize_succeeded().unwrap();
    bounded.commit_response("init").unwrap();
    bounded
        .receive(&request("projection", "host/project"))
        .unwrap();
    bounded.projection_succeeded().unwrap();
    bounded.commit_response("projection").unwrap();
    bounded.receive(&request("catalog", "catalog/set")).unwrap();
    bounded.catalog_succeeded().unwrap();
    bounded.commit_response("catalog").unwrap();
    bounded.receive(&request("first", "program/load")).unwrap();
    let overflow = bounded
        .receive(&request("second", "program/load"))
        .unwrap_err();
    assert_eq!(overflow.code, WireErrorCode::RequestLimit);
    assert!(!overflow.fatal);
}

#[test]
fn enforces_method_direction_and_state() {
    let mut runtime = ProtocolTracker::new(PeerRole::Runtime, 4);
    assert_eq!(
        runtime
            .receive(&request("x", "tool/invoke"))
            .unwrap_err()
            .code,
        WireErrorCode::RequestMethodNotFound
    );
    assert_eq!(
        runtime
            .receive(&request("x", "catalog/set"))
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalidState
    );
    assert_eq!(
        runtime
            .receive(&request("x", "unknown/method"))
            .unwrap_err()
            .code,
        WireErrorCode::RequestMethodNotFound
    );
}

#[test]
fn host_projection_cannot_be_skipped_duplicated_repeated_or_sent_after_catalog() {
    let mut runtime = ProtocolTracker::new(PeerRole::Runtime, 8);
    runtime.receive(&request("init", "initialize")).unwrap();
    runtime.initialize_succeeded().unwrap();
    runtime.commit_response("init").unwrap();

    assert_eq!(
        runtime
            .receive(&request("catalog-skipped", "catalog/set"))
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalidState
    );

    runtime
        .receive(&request("projection-active", "host/project"))
        .unwrap();
    assert_eq!(
        runtime
            .receive(&request("projection-duplicate", "host/project"))
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalidState
    );
    runtime.projection_succeeded().unwrap();
    runtime.commit_response("projection-active").unwrap();

    assert_eq!(
        runtime
            .receive(&request("projection-repeated", "host/project"))
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalidState
    );

    runtime.receive(&request("catalog", "catalog/set")).unwrap();
    runtime.catalog_succeeded().unwrap();
    runtime.commit_response("catalog").unwrap();
    assert_eq!(
        runtime
            .receive(&request("projection-after-catalog", "host/project"))
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalidState
    );
}

#[test]
fn host_projection_has_exactly_one_wire_direction() {
    let initialized = initialize_for("unattended", &json!(null));

    let mut host_receiver = ProtocolTracker::new(PeerRole::Host, 4);
    host_receiver
        .initialize_succeeded_with(&initialized)
        .unwrap();
    assert_eq!(
        host_receiver
            .receive(&request("projection", "host/project"))
            .unwrap_err()
            .code,
        WireErrorCode::RequestMethodNotFound
    );

    let mut runtime_sender = ProtocolTracker::new(PeerRole::Runtime, 4);
    runtime_sender
        .initialize_succeeded_with(&initialized)
        .unwrap();
    assert_eq!(
        runtime_sender
            .register_outgoing_request("projection", "host/project")
            .unwrap_err()
            .code,
        WireErrorCode::RequestMethodNotFound
    );
}

#[test]
fn state_allows_program_load_during_an_active_execution() {
    let mut runtime = ProtocolTracker::new(PeerRole::Runtime, 8);
    runtime.receive(&request("init", "initialize")).unwrap();
    runtime.initialize_succeeded().unwrap();
    runtime.commit_response("init").unwrap();
    runtime
        .receive(&request("projection", "host/project"))
        .unwrap();
    runtime.projection_succeeded().unwrap();
    runtime.commit_response("projection").unwrap();
    runtime.receive(&request("catalog", "catalog/set")).unwrap();
    runtime.catalog_succeeded().unwrap();
    runtime.commit_response("catalog").unwrap();
    runtime.receive(&request("load-1", "program/load")).unwrap();
    runtime.program_loaded().unwrap();
    runtime.commit_response("load-1").unwrap();
    runtime.receive(&request("run", "execution/start")).unwrap();
    runtime.execution_started().unwrap();
    runtime.receive(&request("load-2", "program/load")).unwrap();
    assert_eq!(runtime.active_incoming(), 2);
}

#[test]
fn cancellation_is_idempotent_and_late_responses_are_fatal() {
    let mut runtime = ProtocolTracker::new(PeerRole::Runtime, 4);
    runtime.receive(&request("init", "initialize")).unwrap();
    assert_eq!(
        runtime
            .receive(&WireMessage::Cancel {
                id: "init".into(),
                reason: None
            })
            .unwrap(),
        ReceiveAction::CancelObserved {
            active: true,
            first: true
        }
    );
    assert_eq!(
        runtime
            .receive(&WireMessage::Cancel {
                id: "init".into(),
                reason: None
            })
            .unwrap(),
        ReceiveAction::CancelObserved {
            active: true,
            first: false
        }
    );
    runtime.commit_response("init").unwrap();
    assert_eq!(
        runtime
            .receive(&WireMessage::Cancel {
                id: "init".into(),
                reason: None
            })
            .unwrap(),
        ReceiveAction::CancelObserved {
            active: false,
            first: false
        }
    );

    let mut host = ProtocolTracker::new(PeerRole::Host, 4);
    host.register_outgoing_request("h", "initialize").unwrap();
    assert!(host.cancel_outgoing("h"));
    assert_eq!(host.active_outgoing(), 0);
    let late = host.receive(&response("h")).unwrap_err();
    assert_eq!(late.code, WireErrorCode::ProtocolViolation);
    assert!(late.fatal);
    assert_eq!(
        late.message,
        "response arrived after its outgoing request was cancelled"
    );
    let duplicate = host.receive(&response("h")).unwrap_err();
    assert_eq!(duplicate.code, WireErrorCode::ProtocolViolation);
    assert!(duplicate.fatal);
}

#[test]
fn cancelled_response_tombstones_are_bounded_without_weakening_fatality() {
    let mut host = ProtocolTracker::new(PeerRole::Host, 1);
    host.register_outgoing_request("old", "initialize").unwrap();
    assert!(host.cancel_outgoing("old"));
    host.register_outgoing_request("current", "initialize")
        .unwrap();
    assert!(host.cancel_outgoing("current"));

    let expired = host.receive(&response("old")).unwrap_err();
    assert!(expired.fatal);
    assert_eq!(expired.code, WireErrorCode::ProtocolViolation);

    let current = host.receive(&response("current")).unwrap_err();
    assert!(current.fatal);
    assert_eq!(current.code, WireErrorCode::ProtocolViolation);
    assert_eq!(
        current.message,
        "response arrived after its outgoing request was cancelled"
    );
}

#[test]
fn disconnect_clears_all_protocol_state() {
    let mut runtime = ProtocolTracker::new(PeerRole::Runtime, 4);
    runtime.receive(&request("init", "initialize")).unwrap();
    runtime.disconnect();
    assert_eq!(runtime.state(), ConnectionState::Disconnected);
    assert_eq!(runtime.active_incoming(), 0);
    assert_eq!(runtime.active_outgoing(), 0);
}

#[test]
fn invoking_agent_requests_require_attached_mode_and_exact_binding() {
    let attached = initialize_for("attached", &json!("session-1"));
    let mut host = active_host(&attached);
    assert_eq!(host.invoking_session_id(), Some("session-1"));
    assert_eq!(
        host.receive(&agent_message("exec-1", "session-1")).unwrap(),
        ReceiveAction::RequestAccepted
    );

    let mut wrong_session = active_host(&attached);
    let error = wrong_session
        .receive(&agent_message("exec-1", "session-2"))
        .unwrap_err();
    assert!(error.fatal);
    assert_eq!(error.code, WireErrorCode::ProtocolViolation);

    let mut wrong_execution = active_host(&attached);
    assert!(
        wrong_execution
            .receive(&agent_message("exec-2", "session-1"))
            .unwrap_err()
            .fatal
    );

    let mut unattended = active_host(&initialize_for("unattended", &serde_json::Value::Null));
    assert_eq!(
        unattended
            .receive(&agent_message("exec-1", "session-1"))
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalidState
    );
}

#[test]
fn runtime_registers_complete_bound_requests_only() {
    let attached = initialize_for("attached", &json!("session-1"));
    let mut runtime = active_runtime(&attached);
    assert_eq!(
        runtime
            .register_outgoing_request("agent-1", "agent/message")
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalid
    );
    runtime
        .register_outgoing_message(&agent_message("exec-1", "session-1"))
        .unwrap();
    assert_eq!(runtime.active_outgoing(), 1);

    let mut wrong = active_runtime(&attached);
    assert!(
        wrong
            .register_outgoing_message(&agent_message("exec-1", "session-other"))
            .unwrap_err()
            .fatal
    );
}

#[test]
fn every_bound_request_has_one_direction_and_active_state() {
    let attached = initialize_for("attached", &json!("session-1"));
    let mut host = active_host(&attached);
    for message in bound_requests() {
        assert_eq!(
            host.receive(&message).unwrap(),
            ReceiveAction::RequestAccepted
        );
    }

    let mut runtime = active_runtime(&attached);
    for message in bound_requests() {
        assert_eq!(
            runtime.receive(&message).unwrap_err().code,
            WireErrorCode::RequestMethodNotFound
        );
    }

    let mut outgoing = active_runtime(&attached);
    for message in bound_requests() {
        outgoing.register_outgoing_message(&message).unwrap();
    }
    assert_eq!(outgoing.active_outgoing(), 4);

    let mut inactive = ProtocolTracker::new(PeerRole::Host, 8);
    inactive.initialize_succeeded_with(&attached).unwrap();
    inactive.projection_succeeded().unwrap();
    inactive.catalog_succeeded().unwrap();
    inactive.program_loaded().unwrap();
    assert_eq!(
        inactive
            .receive(&agent_message("exec-1", "session-1"))
            .unwrap_err()
            .code,
        WireErrorCode::RequestInvalidState
    );
}

#[test]
fn permission_revoke_has_one_direction_and_bound_identities() {
    let attached = initialize_for("attached", &json!("session-1"));
    let revoke = WireMessage::Notification {
        method: "permission/revoke".into(),
        params: json!({
            "execution_id":"exec-1",
            "session_id":"session-1",
            "grant_id":"grant-1"
        }),
    };
    let mut runtime = active_runtime(&attached);
    assert_eq!(
        runtime.receive(&revoke).unwrap(),
        ReceiveAction::NotificationAccepted
    );

    let mut wrong = active_runtime(&attached);
    let wrong_revoke = WireMessage::Notification {
        method: "permission/revoke".into(),
        params: json!({
            "execution_id":"exec-1",
            "session_id":"session-other",
            "grant_id":"grant-1"
        }),
    };
    assert!(wrong.receive(&wrong_revoke).unwrap_err().fatal);

    let mut host = active_host(&attached);
    assert!(host.receive(&revoke).unwrap_err().fatal);
    host.validate_outgoing_notification(&revoke).unwrap();
    assert!(runtime.validate_outgoing_notification(&revoke).is_err());
}

#[test]
fn permission_decision_event_requires_an_active_execution() {
    let event = WireMessage::Notification {
        method: "execution/event".into(),
        params: json!({
            "execution_id":"exec-1",
            "sequence":1,
            "elapsed_ms":0,
            "kind":"permission_decision",
            "replayed":false,
            "fields":{
                "operation_id":"op-1",
                "decision":"allow",
                "reason_code":"approved"
            }
        }),
    };
    let attached = initialize_for("attached", &json!("session-1"));
    let mut host = active_host(&attached);
    assert_eq!(
        host.receive(&event).unwrap(),
        ReceiveAction::NotificationAccepted
    );
}
