use std::collections::BTreeSet;
use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StrictJsonError {
    Invalid,
    DuplicateKey,
}

impl fmt::Display for StrictJsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "invalid JSON",
            Self::DuplicateKey => "JSON object contains a duplicate key",
        })
    }
}

impl std::error::Error for StrictJsonError {}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum StrictValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl StrictValue {
    pub(crate) fn into_json(self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(value) => serde_json::Value::Bool(value),
            Self::Number(value) => serde_json::Value::Number(value),
            Self::String(value) => serde_json::Value::String(value),
            Self::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(Self::into_json).collect())
            }
            Self::Object(entries) => serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, value.into_json()))
                    .collect(),
            ),
        }
    }
}

struct StrictValueSeed;

impl<'de> DeserializeSeed<'de> for StrictValueSeed {
    type Value = StrictValue;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("one JSON value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(StrictValue::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictValue::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue::String(value))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(4096));
        while let Some(value) = sequence.next_element_seed(StrictValueSeed)? {
            values.push(value);
        }
        Ok(StrictValue::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen = BTreeSet::new();
        let mut values = Vec::with_capacity(map.size_hint().unwrap_or(0).min(4096));
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom("duplicate object key"));
            }
            let value = map.next_value_seed(StrictValueSeed)?;
            values.push((key, value));
        }
        Ok(StrictValue::Object(values))
    }
}

pub(crate) fn parse_strict_value(input: &str) -> Result<StrictValue, StrictJsonError> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = StrictValueSeed
        .deserialize(&mut deserializer)
        .map_err(|error| classify_error(&error))?;
    deserializer.end().map_err(|error| classify_error(&error))?;
    Ok(value)
}

fn classify_error(error: &serde_json::Error) -> StrictJsonError {
    if error.to_string().contains("duplicate object key") {
        StrictJsonError::DuplicateKey
    } else {
        StrictJsonError::Invalid
    }
}

/// Parse exactly one JSON value and reject duplicate object keys at every depth.
///
/// # Errors
///
/// Returns [`StrictJsonError::DuplicateKey`] for a repeated key and
/// [`StrictJsonError::Invalid`] for every other syntax or data-model error.
pub fn parse_json_strict(input: &str) -> Result<serde_json::Value, StrictJsonError> {
    parse_strict_value(input).map(StrictValue::into_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_keys_fail_at_every_depth() {
        assert_eq!(
            parse_json_strict(r#"{"a":{"b":1,"b":2}}"#),
            Err(StrictJsonError::DuplicateKey)
        );
        assert_eq!(
            parse_json_strict(r#"[{"a":1,"a":2}]"#),
            Err(StrictJsonError::DuplicateKey)
        );
    }

    #[test]
    fn trailing_json_fails() {
        assert_eq!(
            parse_json_strict("null null"),
            Err(StrictJsonError::Invalid)
        );
    }
}
