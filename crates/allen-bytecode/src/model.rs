//! Canonical in-memory bytecode types and instruction model.

use std::fmt;

pub type Register = u16;
pub type EnumTypeId = u32;
pub type FunctionId = u32;
pub type EffectSetId = u32;

pub const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;
pub const MAX_VALUE_NESTING: usize = 128;
pub const TRANSCRIPT_PART_ENUM_NAME: &str = "pkg://allen@0.1.1/src/standard.allen::TranscriptPart";

/// Return whether `bits` encodes any IEEE 754 binary64 NaN.
#[must_use]
pub const fn is_nan_bits(bits: u64) -> bool {
    let exponent = bits & 0x7ff0_0000_0000_0000;
    let fraction = bits & 0x000f_ffff_ffff_ffff;
    exponent == 0x7ff0_0000_0000_0000 && fraction != 0
}

/// Replace every IEEE 754 binary64 NaN representation with the language NaN.
#[must_use]
pub const fn canonical_float_bits(bits: u64) -> u64 {
    if is_nan_bits(bits) {
        CANONICAL_NAN_BITS
    } else {
        bits
    }
}

/// Alias for [`canonical_float_bits`] for callers that normalize raw bits.
#[must_use]
pub const fn normalize_float_bits(bits: u64) -> u64 {
    canonical_float_bits(bits)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RecordField {
    pub name: String,
    pub value_type: ValueType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnumPayloadType {
    Unit,
    Tuple(Vec<ValueType>),
    Record(Vec<RecordField>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnumVariant {
    pub name: String,
    pub payload: EnumPayloadType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnumType {
    pub name: String,
    pub variants: Vec<EnumVariant>,
}

/// One scalar hole in an embedded package template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateHole {
    pub name: String,
    pub value_type: ValueType,
}

/// One whole `{{name}}` marker in template content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateMarker {
    pub start: u32,
    pub end: u32,
    pub hole: u32,
}

/// One verified, package-qualified template resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateResource {
    pub identity: String,
    pub content: String,
    pub digest: [u8; 32],
    pub holes: Vec<TemplateHole>,
    pub markers: Vec<TemplateMarker>,
}

/// One bounded, typed expression evaluated only at an entry boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidatorExpr {
    Bool(bool),
    Field {
        field: u32,
        value_type: ValueType,
    },
    Not(Box<Self>),
    BoolBinary {
        operation: BoolBinaryOp,
        left: Box<Self>,
        right: Box<Self>,
    },
    Compare {
        operation: CompareOp,
        left: Box<Self>,
        right: Box<Self>,
    },
}

/// A source-nominal record contract. Record values themselves remain structural.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordInvariantDefinition {
    pub identity: String,
    pub fields: Vec<RecordField>,
    pub predicate: ValidatorExpr,
}

/// One deterministic traversal step from an entry value to a named record.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EntryValidatorPathSegment {
    Field(u32),
    ListElement,
    MapKey,
    MapValue,
    TupleElement(u32),
    OptionSome,
    ResultOk,
    ResultError,
    EnumPayload { variant: u32, element: u32 },
    NewtypeValue,
}

/// One nominal invariant application site in an entry's directional contract.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EntryValidatorSite {
    pub path: Vec<EntryValidatorPathSegment>,
    pub invariant: u32,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ValueType {
    Int,
    Bool,
    Float,
    String,
    Bytes,
    Range,
    Unit,
    Never,
    List(Box<Self>),
    Map(Box<Self>, Box<Self>),
    Tuple(Vec<Self>),
    Record(Vec<RecordField>),
    Enum(EnumTypeId),
    Newtype {
        name: String,
        underlying: Box<Self>,
    },
    Option(Box<Self>),
    Result(Box<Self>, Box<Self>),
    Future(Box<Self>),
    Task(Box<Self>),
    Sequence(Box<Self>),
    Workspace,
    ExternalFsAccess,
    /// An execution-scoped opaque child-agent handle.
    SubAgent,
    Function {
        parameters: Vec<Self>,
        return_type: Box<Self>,
        effects: EffectSetId,
    },
    Unknown,
}

/// Return the exact structural type produced by [`Instruction::TaskSnapshot`].
///
/// The field order is canonical UTF-8 name order. The values identify a task
/// only within one execution and are intended for diagnostic use.
#[must_use]
pub fn task_snapshot_type() -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: "function".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "id".to_owned(),
            value_type: ValueType::Int,
        },
        RecordField {
            name: "location".to_owned(),
            value_type: ValueType::Option(Box::new(ValueType::String)),
        },
        RecordField {
            name: "owner_id".to_owned(),
            value_type: ValueType::Int,
        },
        RecordField {
            name: "state".to_owned(),
            value_type: ValueType::String,
        },
    ])
}

/// Return the exact structural filesystem error type.
#[must_use]
pub fn file_error_type() -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: "code".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "message".to_owned(),
            value_type: ValueType::String,
        },
    ])
}

/// Return the exact structural network error type.
#[must_use]
pub fn network_error_type() -> ValueType {
    file_error_type()
}

/// Return the canonical structural shape shared by standard recoverable errors.
#[must_use]
pub fn standard_error_type() -> ValueType {
    file_error_type()
}

#[must_use]
pub fn agent_error_type() -> ValueType {
    standard_error_type()
}

#[must_use]
pub fn user_error_type() -> ValueType {
    standard_error_type()
}

#[must_use]
pub fn sub_agent_error_type() -> ValueType {
    standard_error_type()
}

#[must_use]
pub fn model_error_type() -> ValueType {
    standard_error_type()
}

#[must_use]
pub fn permission_error_type() -> ValueType {
    standard_error_type()
}

/// Return the exact structural error shape used by clockless time operations.
#[must_use]
pub fn time_error_type() -> ValueType {
    standard_error_type()
}

/// Return the exact structural error shape used by strict integer parsing.
#[must_use]
pub fn parse_error_type() -> ValueType {
    standard_error_type()
}

/// Return the exact structural error shape used by fixed-decimal formatting.
#[must_use]
pub fn format_error_type() -> ValueType {
    standard_error_type()
}

/// Return the exact structural error shape used by strict JSON decoding.
#[must_use]
pub fn decode_error_type() -> ValueType {
    standard_error_type()
}

/// Return the exact structural error shape used by whitelisted subprocess execution.
#[must_use]
pub fn exec_error_type() -> ValueType {
    standard_error_type()
}

/// Return the exact successful subprocess response shape.
#[must_use]
pub fn exec_response_type() -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: "status".to_owned(),
            value_type: ValueType::Int,
        },
        RecordField {
            name: "stderr".to_owned(),
            value_type: ValueType::Bytes,
        },
        RecordField {
            name: "stdout".to_owned(),
            value_type: ValueType::Bytes,
        },
    ])
}

/// Return the exact structural response type produced by HTTP GET.
#[must_use]
pub fn http_response_type() -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: "body".to_owned(),
            value_type: ValueType::Bytes,
        },
        RecordField {
            name: "final_url".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "headers".to_owned(),
            value_type: ValueType::Map(
                Box::new(ValueType::String),
                Box::new(ValueType::List(Box::new(ValueType::String))),
            ),
        },
        RecordField {
            name: "status".to_owned(),
            value_type: ValueType::Int,
        },
    ])
}

/// Return the exact structural match type produced by `fs.search`.
#[must_use]
pub fn search_match_type() -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: "column".to_owned(),
            value_type: ValueType::Int,
        },
        RecordField {
            name: "line".to_owned(),
            value_type: ValueType::Int,
        },
        RecordField {
            name: "path".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "text".to_owned(),
            value_type: ValueType::String,
        },
    ])
}

/// Return the exact request type accepted by `permission.request_file`.
#[must_use]
pub fn external_file_request_type() -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: "access".to_owned(),
            value_type: ValueType::ExternalFsAccess,
        },
        RecordField {
            name: "path".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "reason".to_owned(),
            value_type: ValueType::String,
        },
    ])
}

/// Return the exact request type accepted by `permission.request_directory`.
#[must_use]
pub fn external_directory_request_type() -> ValueType {
    let ValueType::Record(mut fields) = external_file_request_type() else {
        unreachable!("external file request type is a record")
    };
    fields.push(RecordField {
        name: "recursive".to_owned(),
        value_type: ValueType::Bool,
    });
    ValueType::Record(fields)
}

/// Return the exact query type accepted by `agent.transcript`.
#[must_use]
pub fn transcript_query_type() -> ValueType {
    ValueType::Record(vec![RecordField {
        name: "limit".to_owned(),
        value_type: ValueType::Int,
    }])
}

/// Return the exact synthetic nominal enum used by transcript parts.
#[must_use]
pub fn transcript_part_enum_type() -> EnumType {
    let option_unknown = || ValueType::Option(Box::new(ValueType::Unknown));
    let option_string = || ValueType::Option(Box::new(ValueType::String));
    EnumType {
        name: TRANSCRIPT_PART_ENUM_NAME.to_owned(),
        variants: vec![
            EnumVariant {
                name: "Text".to_owned(),
                payload: EnumPayloadType::Record(vec![RecordField {
                    name: "text".to_owned(),
                    value_type: ValueType::String,
                }]),
            },
            EnumVariant {
                name: "Json".to_owned(),
                payload: EnumPayloadType::Record(vec![RecordField {
                    name: "value".to_owned(),
                    value_type: ValueType::Unknown,
                }]),
            },
            EnumVariant {
                name: "ToolCall".to_owned(),
                payload: EnumPayloadType::Record(vec![
                    RecordField {
                        name: "call_id".to_owned(),
                        value_type: ValueType::String,
                    },
                    RecordField {
                        name: "input".to_owned(),
                        value_type: option_unknown(),
                    },
                    RecordField {
                        name: "name".to_owned(),
                        value_type: ValueType::String,
                    },
                ]),
            },
            EnumVariant {
                name: "ToolResult".to_owned(),
                payload: EnumPayloadType::Record(vec![
                    RecordField {
                        name: "call_id".to_owned(),
                        value_type: ValueType::String,
                    },
                    RecordField {
                        name: "is_error".to_owned(),
                        value_type: ValueType::Bool,
                    },
                    RecordField {
                        name: "output".to_owned(),
                        value_type: option_unknown(),
                    },
                ]),
            },
            EnumVariant {
                name: "Attachment".to_owned(),
                payload: EnumPayloadType::Record(vec![
                    RecordField {
                        name: "content_ref".to_owned(),
                        value_type: option_string(),
                    },
                    RecordField {
                        name: "media_type".to_owned(),
                        value_type: ValueType::String,
                    },
                    RecordField {
                        name: "name".to_owned(),
                        value_type: option_string(),
                    },
                ]),
            },
            EnumVariant {
                name: "Redacted".to_owned(),
                payload: EnumPayloadType::Record(vec![RecordField {
                    name: "reason_code".to_owned(),
                    value_type: ValueType::String,
                }]),
            },
            EnumVariant {
                name: "Omitted".to_owned(),
                payload: EnumPayloadType::Record(vec![
                    RecordField {
                        name: "content_kind".to_owned(),
                        value_type: ValueType::String,
                    },
                    RecordField {
                        name: "count".to_owned(),
                        value_type: ValueType::Int,
                    },
                ]),
            },
        ],
    }
}

/// Return the exact structural transcript message type for a transcript-part enum ID.
#[must_use]
pub fn transcript_message_type(transcript_part: EnumTypeId) -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: "content".to_owned(),
            value_type: ValueType::List(Box::new(ValueType::Enum(transcript_part))),
        },
        RecordField {
            name: "id".to_owned(),
            value_type: ValueType::Option(Box::new(ValueType::String)),
        },
        RecordField {
            name: "role".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "time".to_owned(),
            value_type: ValueType::Option(Box::new(ValueType::String)),
        },
    ])
}

/// Return the exact structural result type produced by `agent.transcript`.
#[must_use]
pub fn transcript_snapshot_type(transcript_part: EnumTypeId) -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: "captured_at".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "messages".to_owned(),
            value_type: ValueType::List(Box::new(transcript_message_type(transcript_part))),
        },
        RecordField {
            name: "policy_version".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "session_id".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "snapshot_id".to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: "truncated".to_owned(),
            value_type: ValueType::Bool,
        },
    ])
}

/// Find the exact synthetic transcript-part enum in a module.
#[must_use]
pub fn transcript_part_enum_id(module: &Module) -> Option<EnumTypeId> {
    module
        .enum_types
        .iter()
        .position(|enum_type| enum_type == &transcript_part_enum_type())
        .and_then(|index| u32::try_from(index).ok())
}

impl ValueType {
    #[must_use]
    pub const fn is_map_key(&self) -> bool {
        match self {
            Self::Bool | Self::Int | Self::String | Self::Bytes => true,
            Self::Newtype { underlying, .. } => underlying.is_map_key(),
            _ => false,
        }
    }

    #[must_use]
    pub fn is_equatable(&self) -> bool {
        match self {
            Self::Int
            | Self::Bool
            | Self::Float
            | Self::String
            | Self::Bytes
            | Self::Range
            | Self::Unit
            | Self::ExternalFsAccess
            | Self::Enum(_)
            | Self::Never => true,
            Self::List(element) => element.is_equatable(),
            Self::Map(key, value) => key.is_equatable() && value.is_equatable(),
            Self::Tuple(elements) => elements.iter().all(Self::is_equatable),
            Self::Record(fields) => fields.iter().all(|field| field.value_type.is_equatable()),
            Self::Option(value) => value.is_equatable(),
            Self::Result(ok, error) => ok.is_equatable() && error.is_equatable(),
            Self::Newtype { underlying, .. } => underlying.is_equatable(),
            Self::Function { .. }
            | Self::Future(_)
            | Self::Task(_)
            | Self::Sequence(_)
            | Self::Workspace
            | Self::SubAgent
            | Self::Unknown => false,
        }
    }

    #[must_use]
    pub const fn is_ordered(&self) -> bool {
        match self {
            Self::Int | Self::Float | Self::String | Self::Bytes => true,
            Self::Newtype { underlying, .. } => underlying.is_ordered(),
            _ => false,
        }
    }
}

impl fmt::Display for ValueType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int => formatter.write_str("Int"),
            Self::Bool => formatter.write_str("Bool"),
            Self::Float => formatter.write_str("Float"),
            Self::String => formatter.write_str("String"),
            Self::Bytes => formatter.write_str("Bytes"),
            Self::Range => formatter.write_str("Range<Int>"),
            Self::Unit => formatter.write_str("Void"),
            Self::Never => formatter.write_str("Never"),
            Self::List(element) => write!(formatter, "List<{element}>"),
            Self::Map(key, value) => write!(formatter, "Map<{key}, {value}>"),
            Self::Tuple(elements) => {
                formatter.write_str("Tuple<")?;
                for (index, element) in elements.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{element}")?;
                }
                formatter.write_str(">")
            }
            Self::Record(fields) => {
                formatter.write_str("{")?;
                for (index, field) in fields.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{}: {}", field.name, field.value_type)?;
                }
                formatter.write_str("}")
            }
            Self::Enum(id) => write!(formatter, "Enum#{id}"),
            Self::Newtype { name, .. } => formatter.write_str(name),
            Self::Option(value) => write!(formatter, "Option<{value}>"),
            Self::Result(ok, error) => write!(formatter, "Result<{ok}, {error}>"),
            Self::Future(value) => write!(formatter, "Future<{value}>"),
            Self::Task(value) => write!(formatter, "Task<{value}>"),
            Self::Sequence(value) => write!(formatter, "Sequence<{value}>"),
            Self::Workspace => formatter.write_str("Workspace"),
            Self::ExternalFsAccess => formatter.write_str("ExternalFsAccess"),
            Self::SubAgent => formatter.write_str("SubAgent"),
            Self::Function {
                parameters,
                return_type,
                effects,
            } => {
                formatter.write_str("fn(")?;
                for (index, parameter) in parameters.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{parameter}")?;
                }
                write!(formatter, ") returns {return_type} effects #{effects}")
            }
            Self::Unknown => formatter.write_str("unknown"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Constant {
    Int(i64),
    Bool(bool),
    Float(u64),
    String(String),
    Bytes(Vec<u8>),
    Unit,
    ExternalFsAccess(ExternalFsAccess),
}

impl Constant {
    #[must_use]
    pub const fn float_bits(bits: u64) -> Self {
        Self::Float(canonical_float_bits(bits))
    }

    #[must_use]
    pub fn float(value: f64) -> Self {
        Self::float_bits(value.to_bits())
    }

    #[must_use]
    pub fn value_type(&self) -> ValueType {
        match self {
            Self::Int(_) => ValueType::Int,
            Self::Bool(_) => ValueType::Bool,
            Self::Float(_) => ValueType::Float,
            Self::String(_) => ValueType::String,
            Self::Bytes(_) => ValueType::Bytes,
            Self::Unit => ValueType::Unit,
            Self::ExternalFsAccess(_) => ValueType::ExternalFsAccess,
        }
    }
}

/// The exact access mode requested for one external filesystem grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalFsAccess {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NumericBinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareOp {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoolBinaryOp {
    And,
    Or,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Conversion {
    IntToFloat,
    ToString,
    StringToBytes,
}

/// One pure String operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StringOperation {
    ByteLength,
    Concat,
    Get,
    Slice,
    Find,
    Contains,
    StartsWith,
    EndsWith,
    Split,
    Join,
    TrimAscii,
    FromUtf8,
    Replace,
    /// Compiler-only lowering for a nonempty sequence of template segments.
    TemplateConcat,
}

/// One pure scalar operation outside the String namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StandardOperation {
    ToInt,
    FloatFormat,
    TimeFormatUtc,
    TimeParseUtc,
    TimeBucket,
}

/// One synchronous observation of the frozen manifest grant set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityOperation {
    IsGranted,
    Granted,
}

/// Non-trapping collection operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafeCollectionOperation {
    ListGet,
    ListTrySet,
    BytesGet,
    MapGet,
    MapInsert,
    MapRemove,
    MapKeys,
}

/// Pure collection operations which may allocate or trap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionOperation {
    Zip,
    ListMin,
    ListMax,
    ListSumInt,
    ListSumFloat,
}

/// Eager list operations which call a pure closure once per visited item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListCombinator {
    Map,
    Filter,
    FlatMap,
    FilterMap,
    Find,
    Any,
    All,
    Partition,
    Scan,
}

/// Non-trapping checked integer operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckedIntOperation {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Negate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectOperation {
    ReadText,
    ReadBytes,
    WriteText,
    WriteBytes,
    List,
    HttpGet,
    PermissionRequestFile,
    PermissionRequestDirectory,
    AgentMessage,
    AgentAsk,
    AgentTranscript,
    ModelRequest,
    UserAsk,
    SubAgentCreate,
    SubAgentRun,
    SubAgentMessage,
    SubAgentAsk,
    Search,
    ExecRun,
}

/// Operation type used by the VM and provider ABI.
pub type FsOperation = EffectOperation;

impl EffectOperation {
    #[must_use]
    pub const fn required_effect(self) -> &'static str {
        match self {
            Self::ReadText | Self::ReadBytes | Self::List | Self::Search => "fs.read",
            Self::WriteText | Self::WriteBytes => "fs.write",
            Self::HttpGet => "net.http_get",
            Self::PermissionRequestFile | Self::PermissionRequestDirectory => {
                "permission.request_external_fs"
            }
            Self::AgentMessage => "agent.message",
            Self::AgentAsk => "agent.ask",
            Self::AgentTranscript => "agent.transcript",
            Self::ModelRequest => "model.request",
            Self::UserAsk => "user.ask",
            Self::SubAgentCreate => "sub_agent.create",
            Self::SubAgentRun => "sub_agent.run",
            Self::SubAgentMessage => "sub_agent.message",
            Self::SubAgentAsk => "sub_agent.ask",
            Self::ExecRun => "exec.run",
        }
    }
}

/// Return the value stored inside the future produced by an effect operation.
///
/// `transcript_part` is required only for `AgentTranscript` because its result
/// contains the module-local synthetic transcript-part enum ID.
#[must_use]
pub fn effect_result_type(
    operation: EffectOperation,
    transcript_part: Option<EnumTypeId>,
) -> Option<ValueType> {
    let (ok, error) = match operation {
        EffectOperation::ReadText => (ValueType::String, file_error_type()),
        EffectOperation::ReadBytes => (ValueType::Bytes, file_error_type()),
        EffectOperation::WriteText | EffectOperation::WriteBytes => {
            (ValueType::Unit, file_error_type())
        }
        EffectOperation::List => (
            ValueType::List(Box::new(ValueType::String)),
            file_error_type(),
        ),
        EffectOperation::Search => (
            ValueType::List(Box::new(search_match_type())),
            file_error_type(),
        ),
        EffectOperation::HttpGet => (http_response_type(), network_error_type()),
        EffectOperation::ExecRun => (exec_response_type(), exec_error_type()),
        EffectOperation::PermissionRequestFile | EffectOperation::PermissionRequestDirectory => {
            (ValueType::Workspace, permission_error_type())
        }
        EffectOperation::AgentMessage => (ValueType::Unit, agent_error_type()),
        EffectOperation::AgentAsk => (ValueType::String, agent_error_type()),
        EffectOperation::AgentTranscript => (
            transcript_snapshot_type(transcript_part?),
            agent_error_type(),
        ),
        EffectOperation::SubAgentCreate => (ValueType::SubAgent, sub_agent_error_type()),
        EffectOperation::SubAgentMessage => (ValueType::Unit, sub_agent_error_type()),
        EffectOperation::SubAgentRun
        | EffectOperation::SubAgentAsk
        | EffectOperation::ModelRequest
        | EffectOperation::UserAsk => return None,
    };
    Some(ValueType::Result(Box::new(ok), Box::new(error)))
}

const PROMPT_SYSTEM_FIELD: &str = "$prompt_0_system";
const PROMPT_CONTEXT_FIELD: &str = "$prompt_1_context";
const PROMPT_DATA_FIELD: &str = "$prompt_2_data";
const PROMPT_OUTPUT_FIELD: &str = "$prompt_3_output";
const PROMPT_ATTEMPTS_FIELD: &str = "$prompt_4_max_attempts";

/// Internal structural representation of the first-class `Prompt<T>` value.
///
/// The `$`-prefixed field names cannot be written as source identifiers, so a
/// source record cannot impersonate a prompt. Context and data remain separate
/// unknown-value slots and the never-populated output slot carries `T` through
/// bytecode verification without exposing a language value.
#[must_use]
pub fn prompt_type(output: ValueType) -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: PROMPT_SYSTEM_FIELD.to_owned(),
            value_type: ValueType::String,
        },
        RecordField {
            name: PROMPT_CONTEXT_FIELD.to_owned(),
            value_type: ValueType::Option(Box::new(ValueType::Unknown)),
        },
        RecordField {
            name: PROMPT_DATA_FIELD.to_owned(),
            value_type: ValueType::Option(Box::new(ValueType::Unknown)),
        },
        RecordField {
            name: PROMPT_OUTPUT_FIELD.to_owned(),
            value_type: ValueType::Option(Box::new(output)),
        },
        RecordField {
            name: PROMPT_ATTEMPTS_FIELD.to_owned(),
            value_type: ValueType::Int,
        },
    ])
}

/// Return the response type encoded by an internal `Prompt<T>` shape.
#[must_use]
pub fn prompt_output_type(value_type: &ValueType) -> Option<&ValueType> {
    let ValueType::Record(fields) = value_type else {
        return None;
    };
    if fields.len() != 5
        || fields[0].name != PROMPT_SYSTEM_FIELD
        || fields[0].value_type != ValueType::String
        || fields[1].name != PROMPT_CONTEXT_FIELD
        || fields[1].value_type != ValueType::Option(Box::new(ValueType::Unknown))
        || fields[2].name != PROMPT_DATA_FIELD
        || fields[2].value_type != ValueType::Option(Box::new(ValueType::Unknown))
        || fields[3].name != PROMPT_OUTPUT_FIELD
        || fields[4].name != PROMPT_ATTEMPTS_FIELD
        || fields[4].value_type != ValueType::Int
    {
        return None;
    }
    match &fields[3].value_type {
        ValueType::Option(output) => Some(output),
        _ => None,
    }
}

pub(crate) fn sub_agent_projection_type() -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: "capabilities".to_owned(),
            value_type: ValueType::List(Box::new(ValueType::String)),
        },
        RecordField {
            name: "limits".to_owned(),
            value_type: ValueType::Map(Box::new(ValueType::String), Box::new(ValueType::Int)),
        },
        RecordField {
            name: "tools".to_owned(),
            value_type: ValueType::List(Box::new(ValueType::String)),
        },
    ])
}

/// Whether a value type can be represented by the exact external-boundary
/// schema profile used for entries, tools, and typed responses.
#[must_use]
pub fn is_strict_schema_type(value_type: &ValueType) -> bool {
    match value_type {
        ValueType::Function { .. }
        | ValueType::Future(_)
        | ValueType::Task(_)
        | ValueType::Workspace
        | ValueType::ExternalFsAccess
        | ValueType::SubAgent
        | ValueType::Range
        | ValueType::Sequence(_)
        | ValueType::Unknown
        | ValueType::Never => false,
        ValueType::List(value) | ValueType::Option(value) => is_strict_schema_type(value),
        ValueType::Newtype { underlying, .. } => is_strict_schema_type(underlying),
        ValueType::Map(key, value) | ValueType::Result(key, value) => {
            is_strict_schema_type(key) && is_strict_schema_type(value)
        }
        ValueType::Tuple(values) => values.iter().all(is_strict_schema_type),
        ValueType::Record(fields) => fields
            .iter()
            .all(|field| is_strict_schema_type(&field.value_type)),
        ValueType::Int
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::Unit
        | ValueType::Enum(_) => true,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnumSwitchArm {
    pub variant: u32,
    pub target: u32,
    pub bindings: Vec<Register>,
}

/// One source-ordered item in an atomic list literal build.
///
/// The compiler evaluates every operand before emitting the build instruction;
/// the instruction only describes whether that value is appended directly or
/// expanded as a list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListLiteralItem {
    Element(Register),
    Spread(Register),
}

/// One source-ordered item in an atomic map literal build.
///
/// Ordinary entries carry separate key and value registers.  A spread carries
/// one map register and replaces existing keys as it is applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MapLiteralItem {
    Entry { key: Register, value: Register },
    Spread(Register),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Instruction {
    Const {
        destination: Register,
        constant: u32,
    },
    Move {
        destination: Register,
        source: Register,
    },
    IntBinary {
        destination: Register,
        left: Register,
        right: Register,
        operation: NumericBinaryOp,
    },
    /// Checked integer remainder with truncating-division semantics.
    IntRemainder {
        destination: Register,
        left: Register,
        right: Register,
    },
    FloatBinary {
        destination: Register,
        left: Register,
        right: Register,
        operation: NumericBinaryOp,
    },
    IntNegate {
        destination: Register,
        source: Register,
    },
    FloatNegate {
        destination: Register,
        source: Register,
    },
    Compare {
        destination: Register,
        left: Register,
        right: Register,
        operation: CompareOp,
    },
    BoolNot {
        destination: Register,
        source: Register,
    },
    BoolBinary {
        destination: Register,
        left: Register,
        right: Register,
        operation: BoolBinaryOp,
    },
    ListNew {
        destination: Register,
        elements: Vec<Register>,
    },
    /// Build a list atomically from source-ordered ordinary and spread items.
    ListLiteralBuild {
        destination: Register,
        items: Vec<ListLiteralItem>,
    },
    RangeNew {
        destination: Register,
        start: Register,
        end: Register,
        inclusive: bool,
    },
    RangeStart {
        destination: Register,
        range: Register,
    },
    RangeEnd {
        destination: Register,
        range: Register,
    },
    RangeInclusive {
        destination: Register,
        range: Register,
    },
    Length {
        destination: Register,
        collection: Register,
    },
    ListAppend {
        destination: Register,
        values: Register,
        value: Register,
    },
    ListSet {
        destination: Register,
        values: Register,
        index: Register,
        value: Register,
    },
    MapNew {
        destination: Register,
        entries: Vec<(Register, Register)>,
    },
    /// Build a map atomically from source-ordered ordinary and spread items.
    MapLiteralBuild {
        destination: Register,
        items: Vec<MapLiteralItem>,
    },
    TupleNew {
        destination: Register,
        elements: Vec<Register>,
    },
    IndexGet {
        destination: Register,
        collection: Register,
        index: Register,
    },
    SliceGet {
        destination: Register,
        collection: Register,
        start: Register,
        end: Register,
    },
    /// Read the canonical sorted map entry at `index` as an exact `(K, V)` tuple.
    MapEntryAt {
        destination: Register,
        map: Register,
        index: Register,
    },
    TupleGet {
        destination: Register,
        tuple: Register,
        index: u32,
    },
    Convert {
        destination: Register,
        source: Register,
        conversion: Conversion,
    },
    RecordNew {
        destination: Register,
        fields: Vec<(u32, Register)>,
    },
    FieldGet {
        destination: Register,
        record: Register,
        field: u32,
    },
    NewtypeWrap {
        destination: Register,
        source: Register,
    },
    NewtypeUnwrap {
        destination: Register,
        source: Register,
    },
    EnumNew {
        destination: Register,
        variant: u32,
        payload: Vec<Register>,
    },
    BranchBool {
        condition: Register,
        true_target: u32,
        false_target: u32,
    },
    SwitchEnum {
        source: Register,
        arms: Vec<EnumSwitchArm>,
    },
    Jump {
        target: u32,
    },
    TryResult {
        destination: Register,
        source: Register,
    },
    TryOption {
        destination: Register,
        source: Register,
    },
    ToUnknown {
        destination: Register,
        source: Register,
    },
    Narrow {
        destination: Register,
        source: Register,
        target: ValueType,
    },
    Decode {
        destination: Register,
        source: Register,
        target: ValueType,
    },
    DirectCall {
        destination: Register,
        function: FunctionId,
        arguments: Vec<Register>,
    },
    ClosureNew {
        destination: Register,
        function: FunctionId,
        captures: Vec<Register>,
    },
    ClosureCall {
        destination: Register,
        closure: Register,
        arguments: Vec<Register>,
    },
    AsyncCall {
        destination: Register,
        function: FunctionId,
        arguments: Vec<Register>,
    },
    Spawn {
        destination: Register,
        future: Register,
        scope: u32,
    },
    Await {
        destination: Register,
        source: Register,
    },
    /// Observe a live task without consuming its affine handle.
    TaskSnapshot {
        destination: Register,
        source: Register,
    },
    WorkspaceGet {
        destination: Register,
    },
    EffectCall {
        destination: Register,
        operation: EffectOperation,
        arguments: Vec<Register>,
    },
    StringCall {
        destination: Register,
        operation: StringOperation,
        arguments: Vec<Register>,
    },
    StandardCall {
        destination: Register,
        operation: StandardOperation,
        arguments: Vec<Register>,
    },
    CapabilityInspect {
        destination: Register,
        operation: CapabilityOperation,
        arguments: Vec<Register>,
    },
    SafeCollectionCall {
        destination: Register,
        operation: SafeCollectionOperation,
        arguments: Vec<Register>,
    },
    CheckedIntCall {
        destination: Register,
        operation: CheckedIntOperation,
        arguments: Vec<Register>,
    },
    CollectionCall {
        destination: Register,
        operation: CollectionOperation,
        arguments: Vec<Register>,
    },
    ListFold {
        destination: Register,
        values: Register,
        initial: Register,
        callback: Register,
    },
    ListCombinator {
        destination: Register,
        operation: ListCombinator,
        values: Register,
        initial: Option<Register>,
        callback: Register,
        callback_result: Register,
    },
    SequenceFromList {
        destination: Register,
        values: Register,
    },
    SequenceMap {
        destination: Register,
        sequence: Register,
        callback: Register,
    },
    SequenceFilter {
        destination: Register,
        sequence: Register,
        callback: Register,
    },
    SequenceTake {
        destination: Register,
        sequence: Register,
        count: Register,
    },
    SequenceFind {
        destination: Register,
        sequence: Register,
        callback: Register,
    },
    SequenceAny {
        destination: Register,
        sequence: Register,
        callback: Register,
    },
    SequenceAll {
        destination: Register,
        sequence: Register,
        callback: Register,
    },
    SequenceFold {
        destination: Register,
        sequence: Register,
        initial: Register,
        callback: Register,
    },
    SequenceToList {
        destination: Register,
        sequence: Register,
    },
    ToolInvoke {
        destination: Register,
        tool: u32,
        input: Register,
    },
    TemplateRender {
        destination: Register,
        template: u32,
        arguments: Vec<Register>,
    },
    TaskScopeEnter {
        scope: u32,
    },
    TaskScopeExit {
        scope: u32,
    },
    Stop {
        reason: Register,
    },
    Fail {
        reason: Register,
    },
    Return {
        source: Register,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Function {
    pub name: String,
    pub parameters: Vec<Register>,
    /// Canonical source parameter names; runtime calls remain positional.
    pub parameter_names: Vec<String>,
    /// Canonical source-semantics digests for declaration-owned defaults.
    ///
    /// The vector is parallel to `parameters`; `None` denotes a required
    /// parameter.  The VM does not interpret these values, but artifacts carry
    /// them so imported declarations can validate their source contract.
    pub parameter_default_digests: Vec<Option<[u8; 32]>>,
    pub captures: Vec<Register>,
    pub registers: Vec<ValueType>,
    pub return_type: ValueType,
    pub effects: EffectSetId,
    pub code: Vec<Instruction>,
}

/// Return the strict response type requested by one typed response operation.
#[must_use]
pub fn typed_response_output_type<'a>(
    function: &'a Function,
    instruction: &Instruction,
) -> Option<&'a ValueType> {
    let Instruction::EffectCall {
        destination,
        operation,
        arguments,
    } = instruction
    else {
        return None;
    };
    let request_index = match operation {
        EffectOperation::ModelRequest
        | EffectOperation::UserAsk
        | EffectOperation::SubAgentRun
        | EffectOperation::AgentAsk => 0,
        EffectOperation::SubAgentAsk => 1,
        _ => return None,
    };
    let requested_output = arguments
        .get(request_index)
        .and_then(|argument| function.registers.get(*argument as usize))
        .and_then(prompt_output_type)?;
    let Some(ValueType::Future(output)) = function.registers.get(*destination as usize) else {
        return None;
    };
    let ValueType::Result(ok, error) = output.as_ref() else {
        return None;
    };
    (ok.as_ref() == requested_output && error.as_ref() == &typed_response_error_type(*operation)?)
        .then_some(requested_output)
}

fn typed_response_error_type(operation: EffectOperation) -> Option<ValueType> {
    Some(match operation {
        EffectOperation::AgentAsk => agent_error_type(),
        EffectOperation::ModelRequest => model_error_type(),
        EffectOperation::UserAsk => user_error_type(),
        EffectOperation::SubAgentRun | EffectOperation::SubAgentAsk => sub_agent_error_type(),
        _ => return None,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Module {
    pub constants: Vec<Constant>,
    pub enum_types: Vec<EnumType>,
    pub effect_sets: Vec<Vec<String>>,
    pub functions: Vec<Function>,
    pub async_functions: Vec<FunctionId>,
    pub entry: u32,
}

/// Exact structural types for one entry in a trusted, frozen tool catalog.
///
/// Slice position is the tool index consumed by [`Instruction::ToolInvoke`].
/// Artifact callers should normally use `decode_and_verify`, which derives this
/// contract from the digest-checked manifest and its embedded schemas.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolVerificationContract {
    /// Exact canonical host tool name for nominal wrapper binding.
    pub tool_name: String,
    /// Exact input type accepted by the tool.
    pub input: ValueType,
    /// Exact successful output type returned by the tool.
    pub output: ValueType,
    /// Exact provider-declared error nested in the tool error wrapper.
    pub declared_error: ValueType,
}

pub(crate) fn tool_declared_error_type<'a>(
    module: &'a Module,
    value_type: &ValueType,
    tool_name: &str,
) -> Option<&'a ValueType> {
    let ValueType::Enum(id) = value_type else {
        return None;
    };
    let wrapper = module.enum_types.get(*id as usize)?;
    let [declared, unavailable, schema] = wrapper.variants.as_slice() else {
        return None;
    };
    let EnumPayloadType::Tuple(declared_payload) = &declared.payload else {
        return None;
    };
    let [declared_error] = declared_payload.as_slice() else {
        return None;
    };
    let ValueType::Record(standard_fields) = standard_error_type() else {
        unreachable!("standard error type is a record")
    };
    let standard_payload = EnumPayloadType::Record(standard_fields);
    let wrapper_type_name = wrapper.name.rsplit_once("::")?.1;
    (wrapper_type_name == tool_error_wrapper_type_name(tool_name)
        && declared.name == "Declared"
        && unavailable.name == "Unavailable"
        && unavailable.payload == standard_payload
        && schema.name == "Schema"
        && schema.payload == standard_payload)
        .then_some(declared_error)
}

fn tool_error_wrapper_type_name(tool_name: &str) -> String {
    let source_name = format!("tools.{tool_name}::Error");
    let mut mangled = String::new();
    for byte in source_name.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            mangled.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(mangled, "_x{byte:02X}_").expect("writing into a String cannot fail");
        }
    }
    format!("_tool_{mangled}")
}
