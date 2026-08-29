use allen_bytecode::{CANONICAL_NAN_BITS, MAX_VALUE_NESTING, is_nan_bits};
use std::cmp::Ordering;
use std::fmt;
use std::rc::Rc;

use super::{
    EnumIdentity, EnumPayload, EnumValue, FloatValue, NewtypeValue, RecordValues, Value,
    compare_map_keys, language_equal,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanonicalEncodeError {
    LengthOverflow,
    ResourceLimit,
    InvalidValue,
}

impl fmt::Display for CanonicalEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LengthOverflow => formatter.write_str("value is too large to encode"),
            Self::ResourceLimit => formatter.write_str("canonical output exceeds its byte limit"),
            Self::InvalidValue => formatter.write_str("value is not a valid ALLEN value"),
        }
    }
}

impl std::error::Error for CanonicalEncodeError {}

/// Failure while decoding a canonical value payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanonicalDecodeError {
    /// The input ended before a complete value could be read.
    Truncated,
    /// Input or decoded allocation exceeds the caller-provided bound.
    ResourceLimit,
    /// A tag, scalar, layout, or opaque value is not canonical.
    InvalidValue,
    /// Bytes remain after a complete top-level value.
    TrailingBytes,
}

impl fmt::Display for CanonicalDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Truncated => "canonical value is truncated",
            Self::ResourceLimit => "canonical value exceeds the decode limit",
            Self::InvalidValue => "canonical value is invalid",
            Self::TrailingBytes => "canonical value has trailing bytes",
        })
    }
}

impl std::error::Error for CanonicalDecodeError {}

/// Encode a value with the stable binary value encoding.
///
/// # Errors
///
/// Returns an error when a length exceeds the format or address-space limit.
pub fn encode_canonical(value: &Value) -> Result<Vec<u8>, CanonicalEncodeError> {
    encode_canonical_with_limit(value, u64::MAX)
}

/// Encode a value after checking the complete output allocation charge.
///
/// # Errors
///
/// Returns `ResourceLimit` before output allocation when the value is too large.
pub fn encode_canonical_with_limit(
    value: &Value,
    maximum_bytes: u64,
) -> Result<Vec<u8>, CanonicalEncodeError> {
    let length = encoded_length(value)?;
    let length_u64 = u64::try_from(length).map_err(|_| CanonicalEncodeError::LengthOverflow)?;
    if length_u64 > maximum_bytes {
        return Err(CanonicalEncodeError::ResourceLimit);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| CanonicalEncodeError::ResourceLimit)?;
    encode_into(value, &mut output)?;
    Ok(output)
}

/// Decode one strict, bounded canonical value payload.
///
/// Opaque capabilities, futures, tasks, closures, workspaces, and sub-agent
/// handles have no canonical tags and are rejected. The decoder also rejects
/// noncanonical layouts instead of normalizing hostile input.
///
/// # Errors
///
/// Returns an error for malformed, noncanonical, trailing, or oversized input.
pub fn decode_canonical_with_limit(
    bytes: &[u8],
    maximum_bytes: u64,
) -> Result<Value, CanonicalDecodeError> {
    if u64::try_from(bytes.len()).map_err(|_| CanonicalDecodeError::ResourceLimit)? > maximum_bytes
    {
        return Err(CanonicalDecodeError::ResourceLimit);
    }
    let maximum = usize::try_from(maximum_bytes).unwrap_or(usize::MAX);
    let mut reader = CanonicalReader {
        bytes,
        offset: 0,
        maximum,
        allocated: 0,
    };
    let value = reader.value(0)?;
    if reader.offset != bytes.len() {
        return Err(CanonicalDecodeError::TrailingBytes);
    }
    Ok(value)
}

/// Decode one strict canonical value with no practical byte bound.
///
/// # Errors
///
/// Returns an error for malformed, noncanonical, or trailing input.
pub fn decode_canonical(bytes: &[u8]) -> Result<Value, CanonicalDecodeError> {
    decode_canonical_with_limit(bytes, u64::MAX)
}

struct CanonicalReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    maximum: usize,
    allocated: usize,
}

impl<'a> CanonicalReader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], CanonicalDecodeError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or(CanonicalDecodeError::ResourceLimit)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(CanonicalDecodeError::Truncated)?;
        self.offset = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, CanonicalDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, CanonicalDecodeError> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .expect("fixed four-byte canonical scalar"),
        ))
    }

    fn count(&mut self) -> Result<usize, CanonicalDecodeError> {
        let count =
            usize::try_from(self.u32()?).map_err(|_| CanonicalDecodeError::ResourceLimit)?;
        if count > self.bytes.len().saturating_sub(self.offset) {
            return Err(CanonicalDecodeError::Truncated);
        }
        Ok(count)
    }

    fn charge<T>(&mut self, count: usize) -> Result<(), CanonicalDecodeError> {
        let bytes = std::mem::size_of::<T>()
            .checked_mul(count)
            .ok_or(CanonicalDecodeError::ResourceLimit)?;
        self.allocated = self
            .allocated
            .checked_add(bytes)
            .ok_or(CanonicalDecodeError::ResourceLimit)?;
        if self.allocated > self.maximum {
            return Err(CanonicalDecodeError::ResourceLimit);
        }
        Ok(())
    }

    fn text(&mut self) -> Result<Rc<str>, CanonicalDecodeError> {
        let length = self.count()?;
        let bytes = self.take(length)?;
        let text = std::str::from_utf8(bytes).map_err(|_| CanonicalDecodeError::InvalidValue)?;
        self.charge::<u8>(length)?;
        Ok(Rc::<str>::from(text))
    }

    fn value(&mut self, depth: usize) -> Result<Value, CanonicalDecodeError> {
        if depth > MAX_VALUE_NESTING {
            return Err(CanonicalDecodeError::InvalidValue);
        }
        match self.byte()? {
            0x00 => Ok(Value::Unit),
            0x01 => Ok(Value::Bool(false)),
            0x02 => Ok(Value::Bool(true)),
            0x03 => Ok(Value::Int(i64::from_be_bytes(
                self.take(8)?
                    .try_into()
                    .expect("fixed eight-byte canonical scalar"),
            ))),
            0x04 => {
                let bits = u64::from_be_bytes(
                    self.take(8)?
                        .try_into()
                        .expect("fixed eight-byte canonical scalar"),
                );
                if is_nan_bits(bits) && bits != CANONICAL_NAN_BITS {
                    return Err(CanonicalDecodeError::InvalidValue);
                }
                Ok(Value::Float(FloatValue::from_canonical_bits(bits)))
            }
            0x05 => Ok(Value::String(self.text()?)),
            0x06 => {
                let length = self.count()?;
                let bytes = self.take(length)?;
                self.charge::<u8>(length)?;
                Ok(Value::Bytes(Rc::from(bytes)))
            }
            0x07 => self.sequence(depth, false),
            0x08 => self.map(depth),
            0x09 => self.sequence(depth, true),
            0x0a => self.record(depth).map(Value::Record),
            0x0b => self.enum_value(depth),
            0x0c => Ok(Value::Unknown(Rc::new(self.value(depth + 1)?))),
            0x0d => {
                let identity = self.text()?;
                if !valid_newtype_identity(&identity) {
                    return Err(CanonicalDecodeError::InvalidValue);
                }
                let value = self.value(depth + 1)?;
                Ok(Value::Newtype(Rc::new(NewtypeValue::new(identity, value))))
            }
            _ => Err(CanonicalDecodeError::InvalidValue),
        }
    }

    fn sequence(&mut self, depth: usize, tuple: bool) -> Result<Value, CanonicalDecodeError> {
        let count = self.count()?;
        if tuple && count == 0 {
            return Err(CanonicalDecodeError::InvalidValue);
        }
        self.charge::<Value>(count)?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| CanonicalDecodeError::ResourceLimit)?;
        for _ in 0..count {
            values.push(self.value(depth + 1)?);
        }
        let values: Rc<[Value]> = values.into();
        Ok(if tuple {
            Value::Tuple(values)
        } else {
            Value::List(values)
        })
    }

    fn map_key_kind(value: &Value) -> Option<u8> {
        match value {
            Value::Bool(_) => Some(0),
            Value::Int(_) => Some(1),
            Value::String(_) => Some(2),
            Value::Bytes(_) => Some(3),
            Value::Newtype(value) => Self::map_key_kind(value.value()).map(|_| 4),
            _ => None,
        }
    }

    fn map(&mut self, depth: usize) -> Result<Value, CanonicalDecodeError> {
        let count = self.count()?;
        self.charge::<(Value, Value)>(count)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| CanonicalDecodeError::ResourceLimit)?;
        let mut kind = None;
        let mut previous: Option<Value> = None;
        for _ in 0..count {
            let key = self.value(depth + 1)?;
            let key_kind = Self::map_key_kind(&key).ok_or(CanonicalDecodeError::InvalidValue)?;
            if kind.is_some_and(|expected| expected != key_kind)
                || previous
                    .as_ref()
                    .is_some_and(|previous| !same_map_key_kind(previous, &key))
                || previous
                    .as_ref()
                    .is_some_and(|previous| compare_map_keys(previous, &key) != Ordering::Less)
            {
                return Err(CanonicalDecodeError::InvalidValue);
            }
            kind = Some(key_kind);
            previous = Some(key.clone());
            entries.push((key, self.value(depth + 1)?));
        }
        Ok(Value::Map(entries.into()))
    }

    fn record(&mut self, depth: usize) -> Result<RecordValues, CanonicalDecodeError> {
        let count = self.count()?;
        self.charge::<(Rc<str>, Value)>(count)?;
        let mut fields = Vec::new();
        fields
            .try_reserve_exact(count)
            .map_err(|_| CanonicalDecodeError::ResourceLimit)?;
        let mut previous: Option<Rc<str>> = None;
        for _ in 0..count {
            let name = self.text()?;
            if previous
                .as_ref()
                .is_some_and(|previous| previous.as_bytes() >= name.as_bytes())
            {
                return Err(CanonicalDecodeError::InvalidValue);
            }
            previous = Some(name.clone());
            fields.push((name, self.value(depth + 1)?));
        }
        Ok(fields.into())
    }

    fn enum_value(&mut self, depth: usize) -> Result<Value, CanonicalDecodeError> {
        let identity = match self.byte()? {
            0x00 => EnumIdentity::User(self.u32()?),
            0x01 => EnumIdentity::Option,
            0x02 => EnumIdentity::Result,
            _ => return Err(CanonicalDecodeError::InvalidValue),
        };
        let variant = self.u32()?;
        let payload = match self.byte()? {
            0x00 => {
                if self.count()? != 0 {
                    return Err(CanonicalDecodeError::InvalidValue);
                }
                EnumPayload::Unit
            }
            0x01 => {
                let count = self.count()?;
                self.charge::<Value>(count)?;
                let mut values = Vec::new();
                values
                    .try_reserve_exact(count)
                    .map_err(|_| CanonicalDecodeError::ResourceLimit)?;
                for _ in 0..count {
                    values.push(self.value(depth + 1)?);
                }
                EnumPayload::Tuple(values.into())
            }
            0x02 => EnumPayload::Record(self.record(depth + 1)?),
            _ => return Err(CanonicalDecodeError::InvalidValue),
        };
        Ok(Value::Enum(Rc::new(EnumValue {
            identity,
            type_name: Rc::from("<canonical>"),
            variant,
            variant_name: Rc::from("<canonical>"),
            payload,
        })))
    }
}

fn encoded_length(value: &Value) -> Result<usize, CanonicalEncodeError> {
    encoded_length_at(value, 0)
}

fn encoded_length_at(value: &Value, depth: usize) -> Result<usize, CanonicalEncodeError> {
    if depth > MAX_VALUE_NESTING {
        return Err(CanonicalEncodeError::InvalidValue);
    }
    match value {
        Value::Unit | Value::Bool(_) => Ok(1),
        Value::Int(_) => Ok(9),
        Value::Float(value) => {
            if is_nan_bits(value.bits()) && value.bits() != CANONICAL_NAN_BITS {
                return Err(CanonicalEncodeError::InvalidValue);
            }
            Ok(9)
        }
        Value::String(value) => sized_payload_length(value.len()),
        Value::Bytes(value) => sized_payload_length(value.len()),
        Value::List(values) => sequence_encoded_length(values, depth),
        Value::Tuple(values) => {
            if values.is_empty() {
                return Err(CanonicalEncodeError::InvalidValue);
            }
            sequence_encoded_length(values, depth)
        }
        Value::Map(entries) => {
            u32::try_from(entries.len()).map_err(|_| CanonicalEncodeError::LengthOverflow)?;
            validate_map(entries)?;
            entries.iter().try_fold(5_usize, |length, (key, value)| {
                let length = length
                    .checked_add(encoded_length_at(key, depth + 1)?)
                    .ok_or(CanonicalEncodeError::LengthOverflow)?;
                length
                    .checked_add(encoded_length_at(value, depth + 1)?)
                    .ok_or(CanonicalEncodeError::LengthOverflow)
            })
        }
        Value::Record(fields) => record_encoded_length(fields, depth),
        Value::Enum(value) => enum_encoded_length(value, depth),
        Value::Newtype(value) => {
            if !valid_newtype_identity(value.identity()) {
                return Err(CanonicalEncodeError::InvalidValue);
            }
            let value_length = encoded_length_at(value.value(), depth + 1)?;
            1_usize
                .checked_add(field_name_length(value.identity().len())?)
                .and_then(|length| length.checked_add(value_length))
                .ok_or(CanonicalEncodeError::LengthOverflow)
        }
        Value::Unknown(value) => 1_usize
            .checked_add(encoded_length_at(value, depth + 1)?)
            .ok_or(CanonicalEncodeError::LengthOverflow),
        Value::ExternalFsAccess(_)
        | Value::Range(_)
        | Value::Sequence(_)
        | Value::Closure(_)
        | Value::Future(_)
        | Value::Task(_)
        | Value::Workspace(_)
        | Value::SubAgent(_) => Err(CanonicalEncodeError::InvalidValue),
    }
}

fn record_encoded_length(
    fields: &[(Rc<str>, Value)],
    depth: usize,
) -> Result<usize, CanonicalEncodeError> {
    u32::try_from(fields.len()).map_err(|_| CanonicalEncodeError::LengthOverflow)?;
    validate_record_fields(fields)?;
    let mut length = 5_usize;
    for (name, value) in fields {
        length = length
            .checked_add(field_name_length(name.len())?)
            .ok_or(CanonicalEncodeError::LengthOverflow)?;
        length = length
            .checked_add(encoded_length_at(value, depth + 1)?)
            .ok_or(CanonicalEncodeError::LengthOverflow)?;
    }
    Ok(length)
}

fn enum_encoded_length(value: &EnumValue, depth: usize) -> Result<usize, CanonicalEncodeError> {
    let identity_length = match value.identity {
        EnumIdentity::User(_) => 5_usize,
        EnumIdentity::Option | EnumIdentity::Result => 1,
    };
    let mut length = identity_length
        .checked_add(10)
        .ok_or(CanonicalEncodeError::LengthOverflow)?;
    match &value.payload {
        EnumPayload::Unit => {}
        EnumPayload::Tuple(values) => {
            u32::try_from(values.len()).map_err(|_| CanonicalEncodeError::LengthOverflow)?;
            for value in values.iter() {
                length = length
                    .checked_add(encoded_length_at(value, depth + 1)?)
                    .ok_or(CanonicalEncodeError::LengthOverflow)?;
            }
        }
        EnumPayload::Record(fields) => {
            u32::try_from(fields.len()).map_err(|_| CanonicalEncodeError::LengthOverflow)?;
            validate_record_fields(fields)?;
            for (name, value) in fields.iter() {
                length = length
                    .checked_add(field_name_length(name.len())?)
                    .ok_or(CanonicalEncodeError::LengthOverflow)?;
                length = length
                    .checked_add(encoded_length_at(value, depth + 1)?)
                    .ok_or(CanonicalEncodeError::LengthOverflow)?;
            }
        }
    }
    Ok(length)
}

fn field_name_length(length: usize) -> Result<usize, CanonicalEncodeError> {
    u32::try_from(length).map_err(|_| CanonicalEncodeError::LengthOverflow)?;
    4_usize
        .checked_add(length)
        .ok_or(CanonicalEncodeError::LengthOverflow)
}

fn validate_record_fields(fields: &[(Rc<str>, Value)]) -> Result<(), CanonicalEncodeError> {
    for pair in fields.windows(2) {
        if pair[0].0.as_bytes() >= pair[1].0.as_bytes() {
            return Err(CanonicalEncodeError::InvalidValue);
        }
    }
    Ok(())
}

fn sized_payload_length(length: usize) -> Result<usize, CanonicalEncodeError> {
    u32::try_from(length).map_err(|_| CanonicalEncodeError::LengthOverflow)?;
    5_usize
        .checked_add(length)
        .ok_or(CanonicalEncodeError::LengthOverflow)
}

fn sequence_encoded_length(values: &[Value], depth: usize) -> Result<usize, CanonicalEncodeError> {
    u32::try_from(values.len()).map_err(|_| CanonicalEncodeError::LengthOverflow)?;
    values.iter().try_fold(5_usize, |length, value| {
        length
            .checked_add(encoded_length_at(value, depth + 1)?)
            .ok_or(CanonicalEncodeError::LengthOverflow)
    })
}

#[allow(clippy::too_many_lines)]
fn encode_into(value: &Value, output: &mut Vec<u8>) -> Result<(), CanonicalEncodeError> {
    match value {
        Value::Unit => output.push(0x00),
        Value::Bool(false) => output.push(0x01),
        Value::Bool(true) => output.push(0x02),
        Value::Int(value) => {
            output.push(0x03);
            output.extend_from_slice(&value.to_be_bytes());
        }
        Value::Float(value) => {
            output.push(0x04);
            output.extend_from_slice(&value.bits().to_be_bytes());
        }
        Value::String(value) => encode_sized_value(0x05, value.as_bytes(), output)?,
        Value::Bytes(value) => encode_sized_value(0x06, value, output)?,
        Value::List(values) => {
            output.push(0x07);
            encode_length(values.len(), output)?;
            for value in values.iter() {
                encode_into(value, output)?;
            }
        }
        Value::Map(entries) => {
            output.push(0x08);
            encode_length(entries.len(), output)?;
            let mut previous = None;
            for _ in 0..entries.len() {
                let next = next_map_entry(entries, previous)?;
                let (key, value) = &entries[next];
                encode_into(key, output)?;
                encode_into(value, output)?;
                previous = Some(next);
            }
        }
        Value::Tuple(values) => {
            output.push(0x09);
            encode_length(values.len(), output)?;
            for value in values.iter() {
                encode_into(value, output)?;
            }
        }
        Value::Record(fields) => {
            output.push(0x0a);
            encode_length(fields.len(), output)?;
            for (name, value) in fields.iter() {
                encode_field_name(name, output)?;
                encode_into(value, output)?;
            }
        }
        Value::Enum(value) => {
            output.push(0x0b);
            match value.identity {
                EnumIdentity::User(type_id) => {
                    output.push(0x00);
                    output.extend_from_slice(&type_id.to_be_bytes());
                }
                EnumIdentity::Option => output.push(0x01),
                EnumIdentity::Result => output.push(0x02),
            }
            output.extend_from_slice(&value.variant.to_be_bytes());
            match &value.payload {
                EnumPayload::Unit => {
                    output.push(0x00);
                    output.extend_from_slice(&0_u32.to_be_bytes());
                }
                EnumPayload::Tuple(values) => {
                    output.push(0x01);
                    encode_length(values.len(), output)?;
                    for value in values.iter() {
                        encode_into(value, output)?;
                    }
                }
                EnumPayload::Record(fields) => {
                    output.push(0x02);
                    encode_length(fields.len(), output)?;
                    for (name, value) in fields.iter() {
                        encode_field_name(name, output)?;
                        encode_into(value, output)?;
                    }
                }
            }
        }
        Value::Unknown(value) => {
            output.push(0x0c);
            encode_into(value, output)?;
        }
        Value::Newtype(value) => {
            output.push(0x0d);
            encode_field_name(value.identity(), output)?;
            encode_into(value.value(), output)?;
        }
        Value::ExternalFsAccess(_)
        | Value::Range(_)
        | Value::Sequence(_)
        | Value::Closure(_)
        | Value::Future(_)
        | Value::Task(_)
        | Value::Workspace(_)
        | Value::SubAgent(_) => {
            return Err(CanonicalEncodeError::InvalidValue);
        }
    }
    Ok(())
}

fn encode_sized_value(
    tag: u8,
    value: &[u8],
    output: &mut Vec<u8>,
) -> Result<(), CanonicalEncodeError> {
    output.push(tag);
    encode_length(value.len(), output)?;
    output.extend_from_slice(value);
    Ok(())
}

fn encode_field_name(name: &str, output: &mut Vec<u8>) -> Result<(), CanonicalEncodeError> {
    encode_length(name.len(), output)?;
    output.extend_from_slice(name.as_bytes());
    Ok(())
}

fn encode_length(length: usize, output: &mut Vec<u8>) -> Result<(), CanonicalEncodeError> {
    let length = u32::try_from(length).map_err(|_| CanonicalEncodeError::LengthOverflow)?;
    output.extend_from_slice(&length.to_be_bytes());
    Ok(())
}

fn validate_map(entries: &[(Value, Value)]) -> Result<(), CanonicalEncodeError> {
    for (index, (key, _)) in entries.iter().enumerate() {
        if !is_map_key(key) {
            return Err(CanonicalEncodeError::InvalidValue);
        }
        if entries[..index]
            .iter()
            .any(|(previous, _)| language_equal(previous, key))
        {
            return Err(CanonicalEncodeError::InvalidValue);
        }
        if entries
            .first()
            .is_some_and(|(first, _)| !same_map_key_kind(first, key))
        {
            return Err(CanonicalEncodeError::InvalidValue);
        }
    }
    Ok(())
}

fn is_map_key(value: &Value) -> bool {
    match value {
        Value::Bool(_) | Value::Int(_) | Value::String(_) | Value::Bytes(_) => true,
        Value::Newtype(value) => is_map_key(value.value()),
        _ => false,
    }
}

fn same_map_key_kind(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Bool(_), Value::Bool(_))
        | (Value::Int(_), Value::Int(_))
        | (Value::String(_), Value::String(_))
        | (Value::Bytes(_), Value::Bytes(_)) => true,
        (Value::Newtype(left), Value::Newtype(right)) => {
            left.identity() == right.identity() && same_map_key_kind(left.value(), right.value())
        }
        _ => false,
    }
}

fn valid_newtype_identity(value: &str) -> bool {
    let Some((owner, declaration)) = value.rsplit_once("::") else {
        return false;
    };
    !owner.is_empty()
        && owner.bytes().all(|byte| byte.is_ascii_graphic())
        && is_source_identifier(declaration)
}

fn is_source_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn next_map_entry(
    entries: &[(Value, Value)],
    previous: Option<usize>,
) -> Result<usize, CanonicalEncodeError> {
    let mut candidate: Option<usize> = None;
    for (index, (key, _)) in entries.iter().enumerate() {
        if previous.is_some_and(|previous| {
            compare_map_keys(key, &entries[previous].0) != Ordering::Greater
        }) {
            continue;
        }
        if candidate
            .is_none_or(|candidate| compare_map_keys(key, &entries[candidate].0) == Ordering::Less)
        {
            candidate = Some(index);
        }
    }
    candidate.ok_or(CanonicalEncodeError::InvalidValue)
}
