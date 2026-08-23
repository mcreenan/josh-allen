use josh_protocol::{
    ProtocolError, WireError, WireErrorCode, WireMessage, decode_message, encode_message,
    validate_id, validate_method,
};
use serde_json::{Value, json};

#[test]
fn exact_message_shapes_round_trip() {
    let messages = [
        WireMessage::Request {
            id: "h-1".into(),
            method: "program/load".into(),
            params: json!({}),
        },
        WireMessage::Response {
            id: "h-1".into(),
            result: Some(Value::Null),
            error: None,
        },
        WireMessage::Response {
            id: "h-2".into(),
            result: None,
            error: Some(WireError {
                code: WireErrorCode::RequestInvalid,
                message: "invalid".into(),
                data: None,
            }),
        },
        WireMessage::Notification {
            method: "runtime/ready".into(),
            params: json!({}),
        },
        WireMessage::Cancel {
            id: "r-9".into(),
            reason: None,
        },
        WireMessage::Cancel {
            id: "r-10".into(),
            reason: Some("deadline".into()),
        },
    ];
    for message in messages {
        let bytes = encode_message(&message).unwrap();
        assert_eq!(decode_message(&bytes).unwrap(), message);
    }
}

#[test]
fn domain_refusal_codes_use_the_existing_wire_error_envelope() {
    for (code, text) in [
        (WireErrorCode::AgentDenied, "agent.denied"),
        (WireErrorCode::ModelDenied, "model.denied"),
        (WireErrorCode::UserDenied, "user.denied"),
        (WireErrorCode::SubAgentDenied, "sub_agent.denied"),
        (WireErrorCode::ToolDenied, "tool.denied"),
    ] {
        let message = WireMessage::Response {
            id: "provider-1".into(),
            result: None,
            error: Some(WireError {
                code,
                message: "operation denied".into(),
                data: None,
            }),
        };
        let encoded = encode_message(&message).unwrap();
        let value: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(value["protocol"], "josh/1");
        assert_eq!(value["error"]["code"], text);
        assert_eq!(decode_message(&encoded).unwrap(), message);
    }
}

#[test]
fn permission_denial_is_not_a_wire_error_code() {
    assert!(
        decode_message(
            br#"{"protocol":"josh/1","kind":"response","id":"permission-1","error":{"code":"permission.denied","message":"denied"}}"#,
        )
        .is_err()
    );
}

#[test]
fn null_result_is_present_not_missing() {
    let decoded =
        decode_message(br#"{"protocol":"josh/1","kind":"response","id":"x","result":null}"#)
            .unwrap();
    assert_eq!(
        decoded,
        WireMessage::Response {
            id: "x".into(),
            result: Some(Value::Null),
            error: None,
        }
    );
}

#[test]
fn rejects_duplicate_keys_at_every_depth() {
    for bytes in [
        br#"{"protocol":"josh/1","protocol":"josh/1","kind":"notification","method":"runtime/ready","params":{}}"#.as_slice(),
        br#"{"protocol":"josh/1","kind":"notification","method":"runtime/ready","params":{"nested":{"x":1,"x":2}}}"#.as_slice(),
        br#"{"protocol":"josh/1","kind":"notification","method":"runtime/ready","params":[{"x":1,"x":2}]}"#.as_slice(),
    ] {
        assert!(matches!(decode_message(bytes), Err(ProtocolError::InvalidJson(_))));
    }
}

#[test]
fn rejects_unknown_fields_protocol_kinds_and_response_ambiguity() {
    for text in [
        r#"{"protocol":"josh/1","kind":"request","id":"x","method":"initialize","params":{},"extra":0}"#,
        r#"{"protocol":"josh/2","kind":"request","id":"x","method":"initialize","params":{}}"#,
        r#"{"protocol":"josh/1","kind":"wat","id":"x"}"#,
        r#"{"protocol":"josh/1","kind":"response","id":"x"}"#,
        r#"{"protocol":"josh/1","kind":"response","id":"x","result":{},"error":{"code":"request.invalid","message":"x"}}"#,
        r#"{"protocol":"josh/1","kind":"response","id":"x","error":{"code":"unknown","message":"x"}}"#,
        r#"{"protocol":"josh/1","kind":"cancel","id":"x","reason":null}"#,
    ] {
        // Null optional reason is accepted by Serde and has the same wire meaning as absent.
        if text.contains("reason\":null") {
            assert!(decode_message(text.as_bytes()).is_ok());
        } else {
            assert!(decode_message(text.as_bytes()).is_err(), "{text}");
        }
    }
}

#[test]
fn validates_ids_methods_reasons_and_error_text() {
    for id in ["", "has space", "\n", &"a".repeat(65)] {
        assert!(validate_id(id).is_err());
    }
    for method in [
        "",
        "/load",
        "program/",
        "program//load",
        "Program/load",
        "program-load",
    ] {
        assert!(validate_method(method).is_err(), "{method}");
    }
    assert!(validate_method("future/method").is_ok());
    assert!(
        encode_message(&WireMessage::Cancel {
            id: "x".into(),
            reason: Some("é".repeat(513)),
        })
        .is_err()
    );
    assert!(
        encode_message(&WireMessage::Response {
            id: "x".into(),
            result: None,
            error: Some(WireError {
                code: WireErrorCode::RequestInvalid,
                message: "x".repeat(1_025),
                data: None,
            }),
        })
        .is_err()
    );
}
