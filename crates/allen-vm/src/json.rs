use crate::{
    EnumIdentity, EnumPayload, EnumValue, FloatValue, NewtypeValue, Value, compare_map_keys,
};
use allen_bytecode::{EnumPayloadType, EnumType, ValueType};
use base64::Engine as _;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeSet;
use std::fmt;
use std::rc::Rc;

const DUPLICATE_MARKER: &str = "__allen_duplicate_key__";
const DEPTH_MARKER: &str = "__allen_decode_depth__";
const MAX_ERROR_MESSAGE_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonDecodeErrorKind {
    InvalidUtf8,
    InvalidJson,
    DuplicateKey,
    TypeMismatch,
    ResourceDepth,
}

impl JsonDecodeErrorKind {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidUtf8 => "invalid_utf8",
            Self::InvalidJson => "invalid_json",
            Self::DuplicateKey => "duplicate_key",
            Self::TypeMismatch => "type_mismatch",
            Self::ResourceDepth => "resource.limit",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonDecodeError {
    kind: JsonDecodeErrorKind,
    message: String,
}

impl JsonDecodeError {
    fn new(kind: JsonDecodeErrorKind, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_ERROR_MESSAGE_BYTES {
            message.truncate(MAX_ERROR_MESSAGE_BYTES);
            while !message.is_char_boundary(message.len()) {
                message.pop();
            }
        }
        Self { kind, message }
    }

    #[must_use]
    pub const fn kind(&self) -> JsonDecodeErrorKind {
        self.kind
    }

    #[must_use]
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug)]
enum JsonNumber {
    Int(i64),
    Float(f64),
    OutOfRange,
}

#[derive(Clone, Debug)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(JsonNumber),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

struct JsonSeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for JsonSeed {
    type Value = JsonValue;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        if self.depth > super::MAXIMUM_DECODE_DEPTH {
            return Err(de::Error::custom(DEPTH_MARKER));
        }
        deserializer.deserialize_any(JsonVisitor { depth: self.depth })
    }
}

struct JsonVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = JsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(JsonValue::Bool(value))
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(JsonNumber::Int(value)))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(
            i64::try_from(value).map_or(JsonNumber::OutOfRange, JsonNumber::Int),
        ))
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(JsonNumber::Float(value)))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(JsonValue::String(value.to_owned()))
    }
    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(JsonValue::String(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(JsonSeed {
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(JsonValue::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        let mut names = BTreeSet::new();
        while let Some(name) = map.next_key::<String>()? {
            if !names.insert(name.clone()) {
                return Err(de::Error::custom(DUPLICATE_MARKER));
            }
            let value = map.next_value_seed(JsonSeed {
                depth: self.depth + 1,
            })?;
            values.push((name, value));
        }
        Ok(JsonValue::Object(values))
    }
}

/// # Errors
///
/// Returns an error when the input is not valid UTF-8, is not one complete JSON
/// value, exceeds the JSON nesting limit, contains duplicate object keys, or
/// cannot be projected into the requested ALLEN value type.
pub fn decode_json(
    bytes: &[u8],
    target: &ValueType,
    enums: &[EnumType],
) -> Result<Value, JsonDecodeError> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        JsonDecodeError::new(JsonDecodeErrorKind::InvalidUtf8, "input is not valid UTF-8")
    })?;
    if text.starts_with('\u{feff}') {
        return Err(JsonDecodeError::new(
            JsonDecodeErrorKind::InvalidJson,
            "JSON must not begin with a byte-order mark",
        ));
    }
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = JsonSeed { depth: 0 }
        .deserialize(&mut deserializer)
        .map_err(|error| {
            let message = error.to_string();
            if message.contains(DUPLICATE_MARKER) {
                JsonDecodeError::new(
                    JsonDecodeErrorKind::DuplicateKey,
                    "JSON object contains a duplicate key",
                )
            } else if message.contains(DEPTH_MARKER) || message.contains("recursion limit exceeded")
            {
                JsonDecodeError::new(
                    JsonDecodeErrorKind::ResourceDepth,
                    "JSON nesting exceeds 128",
                )
            } else {
                JsonDecodeError::new(
                    JsonDecodeErrorKind::InvalidJson,
                    "input is not one complete JSON value",
                )
            }
        })?;
    deserializer.end().map_err(|_| {
        JsonDecodeError::new(
            JsonDecodeErrorKind::InvalidJson,
            "input is not one complete JSON value",
        )
    })?;
    project(&value, target, enums)
}

/// # Errors
///
/// Returns an error when the JSON value cannot be projected into the requested
/// ALLEN value type.
pub fn project_json_value(
    value: &serde_json::Value,
    target: &ValueType,
    enums: &[EnumType],
) -> Result<Value, JsonDecodeError> {
    project(&from_serde(value), target, enums)
}

fn from_serde(value: &serde_json::Value) -> JsonValue {
    match value {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(value) => JsonValue::Bool(*value),
        serde_json::Value::Number(value) if value.is_i64() => {
            JsonValue::Number(JsonNumber::Int(value.as_i64().unwrap()))
        }
        serde_json::Value::Number(value) if value.is_u64() => JsonValue::Number(
            value
                .as_u64()
                .and_then(|value| i64::try_from(value).ok())
                .map_or(JsonNumber::OutOfRange, JsonNumber::Int),
        ),
        serde_json::Value::Number(value) => JsonValue::Number(
            value
                .as_f64()
                .map_or(JsonNumber::OutOfRange, JsonNumber::Float),
        ),
        serde_json::Value::String(value) => JsonValue::String(value.clone()),
        serde_json::Value::Array(values) => {
            JsonValue::Array(values.iter().map(from_serde).collect())
        }
        serde_json::Value::Object(values) => JsonValue::Object(
            values
                .iter()
                .map(|(name, value)| (name.clone(), from_serde(value)))
                .collect(),
        ),
    }
}

fn mismatch() -> JsonDecodeError {
    JsonDecodeError::new(
        JsonDecodeErrorKind::TypeMismatch,
        "JSON value does not match the target type",
    )
}
fn member<'a>(object: &'a [(String, JsonValue)], name: &str) -> Option<&'a JsonValue> {
    object
        .iter()
        .find_map(|(candidate, value)| (candidate == name).then_some(value))
}

#[allow(clippy::too_many_lines)]
fn project(
    value: &JsonValue,
    target: &ValueType,
    enums: &[EnumType],
) -> Result<Value, JsonDecodeError> {
    if let ValueType::Newtype { name, underlying } = target {
        return Ok(Value::Newtype(Rc::new(NewtypeValue::new(
            name.as_str(),
            project(value, underlying, enums)?,
        ))));
    }
    match (target, value) {
        (ValueType::Int, JsonValue::Number(JsonNumber::Int(value))) => Ok(Value::Int(*value)),
        (ValueType::Bool, JsonValue::Bool(value)) => Ok(Value::Bool(*value)),
        (ValueType::Float, JsonValue::Number(JsonNumber::Float(value))) => {
            Ok(Value::Float(FloatValue::new(*value)))
        }
        (ValueType::Float, JsonValue::String(value)) => match value.as_str() {
            "NaN" => Ok(Value::Float(FloatValue::new(f64::NAN))),
            "Infinity" => Ok(Value::Float(FloatValue::new(f64::INFINITY))),
            "-Infinity" => Ok(Value::Float(FloatValue::new(f64::NEG_INFINITY))),
            _ => Err(mismatch()),
        },
        (ValueType::String, JsonValue::String(value)) => Ok(Value::String(value.as_str().into())),
        (ValueType::Bytes, JsonValue::Object(object)) if object.len() == 1 => {
            let JsonValue::String(encoded) = member(object, "$bytes").ok_or_else(mismatch)? else {
                return Err(mismatch());
            };
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| mismatch())?;
            if base64::engine::general_purpose::STANDARD.encode(&decoded) != *encoded {
                return Err(mismatch());
            }
            Ok(Value::Bytes(decoded.into()))
        }
        (ValueType::Unit, JsonValue::Null) => Ok(Value::Unit),
        (ValueType::List(item), JsonValue::Array(values)) => Ok(Value::List(
            values
                .iter()
                .map(|value| project(value, item, enums))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        )),
        (ValueType::Tuple(items), JsonValue::Array(values)) if items.len() == values.len() => {
            Ok(Value::Tuple(
                values
                    .iter()
                    .zip(items)
                    .map(|(value, item)| project(value, item, enums))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ))
        }
        (ValueType::Record(fields), JsonValue::Object(object))
            if fields.len() == object.len()
                && fields
                    .iter()
                    .all(|field| member(object, &field.name).is_some()) =>
        {
            Ok(Value::Record(
                fields
                    .iter()
                    .map(|field| {
                        Ok((
                            Rc::from(field.name.as_str()),
                            project(
                                member(object, &field.name).unwrap(),
                                &field.value_type,
                                enums,
                            )?,
                        ))
                    })
                    .collect::<Result<Vec<_>, JsonDecodeError>>()?
                    .into(),
            ))
        }
        (ValueType::Map(key_type, value_type), JsonValue::Array(pairs)) => {
            let mut output = Vec::with_capacity(pairs.len());
            let mut previous: Option<Value> = None;
            for pair in pairs {
                let JsonValue::Array(pair) = pair else {
                    return Err(mismatch());
                };
                if pair.len() != 2 {
                    return Err(mismatch());
                }
                let key = project(&pair[0], key_type, enums)?;
                if previous.as_ref().is_some_and(|previous| {
                    compare_map_keys(previous, &key) != std::cmp::Ordering::Less
                }) {
                    return Err(mismatch());
                }
                previous = Some(key.clone());
                output.push((key, project(&pair[1], value_type, enums)?));
            }
            Ok(Value::Map(output.into()))
        }
        (ValueType::Option(item), JsonValue::Object(object)) => {
            project_builtin_tag(object, EnumIdentity::Option, item, None, enums)
        }
        (ValueType::Result(ok, error), JsonValue::Object(object)) => {
            project_result_tag(object, ok, error, enums)
        }
        (ValueType::Enum(id), JsonValue::Object(object)) => project_user_enum(object, *id, enums),
        _ => Err(mismatch()),
    }
}

fn project_builtin_tag(
    object: &[(String, JsonValue)],
    identity: EnumIdentity,
    item: &ValueType,
    _: Option<&ValueType>,
    enums: &[EnumType],
) -> Result<Value, JsonDecodeError> {
    let JsonValue::String(tag) = member(object, "tag").ok_or_else(mismatch)? else {
        return Err(mismatch());
    };
    let (variant, payload) = match tag.as_str() {
        "None" if object.len() == 1 => (0, EnumPayload::Unit),
        "Some" if object.len() == 2 => (
            1,
            EnumPayload::Tuple(
                vec![project(
                    member(object, "value").ok_or_else(mismatch)?,
                    item,
                    enums,
                )?]
                .into(),
            ),
        ),
        _ => return Err(mismatch()),
    };
    Ok(Value::Enum(Rc::new(EnumValue {
        identity,
        type_name: "Option".into(),
        variant_name: tag.as_str().into(),
        variant,
        payload,
    })))
}

fn project_result_tag(
    object: &[(String, JsonValue)],
    ok: &ValueType,
    error: &ValueType,
    enums: &[EnumType],
) -> Result<Value, JsonDecodeError> {
    let JsonValue::String(tag) = member(object, "tag").ok_or_else(mismatch)? else {
        return Err(mismatch());
    };
    let (variant, target) = match tag.as_str() {
        "Ok" => (0, ok),
        "Err" => (1, error),
        _ => return Err(mismatch()),
    };
    let payload = if *target == ValueType::Unit {
        if object.len() != 1 {
            return Err(mismatch());
        }
        Value::Unit
    } else if object.len() == 2 {
        project(member(object, "value").ok_or_else(mismatch)?, target, enums)?
    } else {
        return Err(mismatch());
    };
    Ok(Value::Enum(Rc::new(EnumValue {
        identity: EnumIdentity::Result,
        type_name: "Result".into(),
        variant_name: tag.as_str().into(),
        variant,
        payload: EnumPayload::Tuple(vec![payload].into()),
    })))
}

fn project_user_enum(
    object: &[(String, JsonValue)],
    id: u32,
    enums: &[EnumType],
) -> Result<Value, JsonDecodeError> {
    let definition = enums.get(id as usize).ok_or_else(mismatch)?;
    let JsonValue::String(tag) = member(object, "tag").ok_or_else(mismatch)? else {
        return Err(mismatch());
    };
    let (variant, shape) = definition
        .variants
        .iter()
        .enumerate()
        .find(|(_, variant)| variant.name == *tag)
        .ok_or_else(mismatch)?;
    let payload = match &shape.payload {
        EnumPayloadType::Unit if object.len() == 1 => EnumPayload::Unit,
        EnumPayloadType::Tuple(items) if object.len() == 2 => {
            let JsonValue::Array(values) = member(object, "value").ok_or_else(mismatch)? else {
                return Err(mismatch());
            };
            if values.len() != items.len() {
                return Err(mismatch());
            }
            EnumPayload::Tuple(
                values
                    .iter()
                    .zip(items)
                    .map(|(value, item)| project(value, item, enums))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            )
        }
        EnumPayloadType::Record(fields) if object.len() == 2 => {
            let Value::Record(values) = project(
                member(object, "value").ok_or_else(mismatch)?,
                &ValueType::Record(fields.clone()),
                enums,
            )?
            else {
                unreachable!()
            };
            EnumPayload::Record(values)
        }
        _ => return Err(mismatch()),
    };
    Ok(Value::Enum(Rc::new(EnumValue {
        identity: EnumIdentity::User(id),
        type_name: definition.name.as_str().into(),
        variant_name: tag.as_str().into(),
        variant: u32::try_from(variant).map_err(|_| mismatch())?,
        payload,
    })))
}
