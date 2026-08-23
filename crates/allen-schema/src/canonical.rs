use sha2::{Digest, Sha256};

/// Encode a JSON value using the tool-schema canonical JSON rules.
#[must_use]
pub fn canonical_json(value: &serde_json::Value) -> Vec<u8> {
    let mut output = Vec::new();
    write_value(value, &mut output);
    output
}

fn write_value(value: &serde_json::Value, output: &mut Vec<u8>) {
    match value {
        serde_json::Value::Null => output.extend_from_slice(b"null"),
        serde_json::Value::Bool(false) => output.extend_from_slice(b"false"),
        serde_json::Value::Bool(true) => output.extend_from_slice(b"true"),
        serde_json::Value::Number(number) => {
            let mut text = number.to_string();
            if number.is_f64() && text.ends_with(".0") && text != "-0.0" {
                text.truncate(text.len() - 2);
            }
            output.extend_from_slice(text.as_bytes());
        }
        serde_json::Value::String(string) => write_string(string, output),
        serde_json::Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_value(value, output);
            }
            output.push(b']');
        }
        serde_json::Value::Object(object) => {
            output.push(b'{');
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_string(key, output);
                output.push(b':');
                write_value(value, output);
            }
            output.push(b'}');
        }
    }
}

fn write_string(value: &str, output: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    output.push(b'"');
    for byte in value.as_bytes() {
        match byte {
            b'"' => output.extend_from_slice(br#"\""#),
            b'\\' => output.extend_from_slice(br"\\"),
            0x00..=0x1f => {
                output.extend_from_slice(b"\\u00");
                output.push(HEX[usize::from(byte >> 4)]);
                output.push(HEX[usize::from(byte & 0x0f)]);
            }
            _ => output.push(*byte),
        }
    }
    output.push(b'"');
}

/// Return lower-case `sha256:` digest text for canonical or binary bytes.
#[must_use]
pub fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_and_uses_only_required_escapes() {
        let value = serde_json::json!({"z": 1.0, "a": "line\n/é", "n": -0.0});
        assert_eq!(
            canonical_json(&value),
            r#"{"a":"line\u000a/é","n":-0.0,"z":1}"#.as_bytes()
        );
    }
}
