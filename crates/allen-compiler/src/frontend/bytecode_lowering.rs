//! Coordinated bytecode-v19 lowering after resolution and checking.
//!
//! HIR and MIR are emitted from the same typed expression walk so bytecode,
//! debug spans, ownership transitions, and both IRs cannot observe different
//! control-flow or evaluation order. Their public representations and MIR
//! structural validation live in the focused `hir` and `mir` modules.

use super::checking::{
    SemanticType, concrete_type, contains_affine, contains_stored_sub_agent, contains_sub_agent,
    contains_workspace, effect_id, expected_type_diagnostic_code, is_affine, literal_map_key,
};
use super::resolution::{
    CollectionBuiltin, FunctionInfo, ResolvedBundle, SequenceBuiltin, StandardBuiltin,
    capability_builtin_callee, collection_builtin_callee, default_helper_name,
    direct_capability_inspection_body_span, effect_operation_signature, is_task_snapshot_callee,
    required_body_effects, resolve_extension_functions, resolve_function_name, resolve_named_type,
    semantic_type, standard_builtin_callee, standard_operation_callee, string_builtin_callee,
    sub_agent_projection_type, template_binding, template_callee, tool_callee,
};
use super::{
    BTreeMap, BTreeSet, Binary, CapabilityOperation, CheckedIntOperation, CollectionOperation,
    CompareOp, Constant, Conversion, DebugLocation, Diagnostic, EffectOperation, EffectSetId,
    EnumPayloadType, ExternalFsAccess, Function, FunctionId, HirExpr, HirExprKind, HirForSource,
    HirFunction, HirListItem, HirLoopBinding, HirLoopBindingElement, HirMapItem, HirTemplatePart,
    Instruction, ListCombinator, LocalBinding, LoweredBody, LoweredCallArgument, LoweredElse,
    LoweredEnumValuePayload, LoweredExpr, LoweredExprKind, LoweredForSource, LoweredFunction,
    LoweredLoopBinding, LoweredPattern, LoweredStatement, LoweredTemplatePart, LoweredType,
    MirBlock, MirCleanupKind, MirFunction, MirListItem, MirMapItem, MirOperation, MirOwnership,
    MirOwnershipState, MirSuspension, MirTaskScope, MirTerminator, NumericBinaryOp, RecordField,
    Register, SafeCollectionOperation, SourceSpan, Span, SpanId, StandardOperation,
    StringOperation, SymbolId, TypeId, Unary, ValueType, agent_error_type, format_error_type,
    is_strict_schema_type, model_error_type, parse_error_type, prompt_output_type, prompt_type,
    sub_agent_error_type, task_snapshot_type, template_interpolations, time_error_type,
    user_error_type,
};
use allen_bytecode::{ListLiteralItem, MapLiteralItem, decode_error_type};

#[derive(Clone, Debug, Eq, PartialEq)]
enum PatternLiteralValue {
    Int(i64),
    String(String),
    Bytes(Vec<u8>),
}

impl PatternLiteralValue {
    fn compare(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => Some(left.cmp(right)),
            (Self::String(left), Self::String(right)) => Some(left.chars().cmp(right.chars())),
            (Self::Bytes(left), Self::Bytes(right)) => Some(left.cmp(right)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct PatternInterval {
    start: PatternLiteralValue,
    end: PatternLiteralValue,
    inclusive: bool,
}

fn parameter_default_digest(source_text: &str) -> [u8; 32] {
    let source = allen_syntax::SourceFile::new(allen_syntax::SourceFileId::new(0), source_text)
        .expect("lowered default source fits the syntax range model");
    let lexed = allen_syntax::lex(&source);
    debug_assert!(
        lexed.diagnostics().is_empty(),
        "a lowered default was already parsed successfully"
    );
    let mut canonical = b"allen-default-token-stream-v1\0".to_vec();
    for token in lexed.tokens() {
        let kind = token.kind();
        if matches!(
            kind,
            allen_syntax::SyntaxKind::Eof
                | allen_syntax::SyntaxKind::Whitespace
                | allen_syntax::SyntaxKind::Newline
                | allen_syntax::SyntaxKind::LineComment
                | allen_syntax::SyntaxKind::BlockComment
        ) {
            continue;
        }
        let text = token.text(&source).as_bytes();
        canonical.extend_from_slice(&(kind as u16).to_le_bytes());
        canonical.extend_from_slice(
            &u64::try_from(text.len())
                .expect("token length fits the canonical digest encoding")
                .to_le_bytes(),
        );
        canonical.extend_from_slice(text);
    }
    let digest = allen_schema::digest_bytes(&canonical);
    let hex = digest
        .strip_prefix("sha256:")
        .expect("schema digests use the sha256 prefix");
    let mut bytes = [0_u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&hex[offset..offset + 2], 16)
            .expect("schema digests use lowercase hexadecimal");
    }
    bytes
}

fn template_scalar_type(value_type: &ValueType) -> &ValueType {
    match value_type {
        ValueType::Newtype { underlying, .. } => template_scalar_type(underlying),
        _ => value_type,
    }
}

fn effect_operation_argument_labels(operation: EffectOperation) -> &'static [&'static str] {
    match operation {
        EffectOperation::ReadText | EffectOperation::ReadBytes | EffectOperation::List => {
            &["workspace", "path"]
        }
        EffectOperation::WriteText | EffectOperation::WriteBytes => &["workspace", "path", "value"],
        EffectOperation::Search => &["workspace", "path", "query"],
        EffectOperation::HttpGet => &["url"],
        EffectOperation::ExecRun => &["argv", "stdin"],
        EffectOperation::PermissionRequestFile
        | EffectOperation::PermissionRequestDirectory
        | EffectOperation::AgentAsk
        | EffectOperation::ModelRequest
        | EffectOperation::UserAsk => &["request"],
        EffectOperation::AgentMessage => &["message"],
        EffectOperation::AgentTranscript => &["query"],
        EffectOperation::SubAgentCreate => &["initial", "projection"],
        EffectOperation::SubAgentRun => &["request", "projection"],
        EffectOperation::SubAgentMessage => &["target", "message"],
        EffectOperation::SubAgentAsk => &["target", "request"],
    }
}

fn builtin_argument_labels(callee: &LoweredExpr) -> Option<&'static [&'static str]> {
    if let LoweredExprKind::Variable(name) = &callee.kind {
        if let Some(labels) = match name.as_str() {
            "narrow" | "to_int" => Some(&["value"] as &[_]),
            "stop" | "fail" => Some(&["reason"] as &[_]),
            _ => None,
        } {
            return Some(labels);
        }
    }
    if is_task_snapshot_callee(callee) {
        return Some(&["task"]);
    }
    if template_callee(callee).is_some() || tool_callee(callee).is_some() {
        return Some(&["input"]);
    }
    if let Some(builtin) = standard_builtin_callee(callee) {
        return Some(match builtin {
            StandardBuiltin::Workspace => &[],
            StandardBuiltin::Operation(operation) => effect_operation_argument_labels(operation),
        });
    }
    if let Some(operation) = capability_builtin_callee(callee) {
        return Some(match operation {
            CapabilityOperation::IsGranted => &["name"],
            CapabilityOperation::Granted => &[],
        });
    }
    if let Some(builtin) = collection_builtin_callee(callee) {
        return Some(match builtin {
            CollectionBuiltin::Length
            | CollectionBuiltin::CheckedInt(CheckedIntOperation::Negate) => &["value"],
            CollectionBuiltin::ListAppend => &["values", "value"],
            CollectionBuiltin::ListSet => &["values", "index", "value"],
            CollectionBuiltin::Operation(CollectionOperation::Zip) => &[],
            CollectionBuiltin::Operation(_)
            | CollectionBuiltin::Safe(SafeCollectionOperation::MapKeys) => &["values"],
            CollectionBuiltin::ListFold => &["values", "initial", "callback"],
            CollectionBuiltin::ListCombinator(ListCombinator::Scan) => {
                &["values", "initial", "callback"]
            }
            CollectionBuiltin::ListCombinator(_) => &["values", "callback"],
            CollectionBuiltin::Safe(
                SafeCollectionOperation::ListGet | SafeCollectionOperation::BytesGet,
            ) => &["values", "index"],
            CollectionBuiltin::Safe(SafeCollectionOperation::ListTrySet) => {
                &["values", "index", "value"]
            }
            CollectionBuiltin::Safe(
                SafeCollectionOperation::MapGet | SafeCollectionOperation::MapRemove,
            ) => &["values", "key"],
            CollectionBuiltin::Safe(SafeCollectionOperation::MapInsert) => {
                &["values", "key", "value"]
            }
            CollectionBuiltin::CheckedInt(_) => &["left", "right"],
            CollectionBuiltin::Sequence(operation) => match operation {
                SequenceBuiltin::FromList | SequenceBuiltin::ToList => &["values"],
                SequenceBuiltin::Map
                | SequenceBuiltin::Filter
                | SequenceBuiltin::Find
                | SequenceBuiltin::Any
                | SequenceBuiltin::All => &["values", "callback"],
                SequenceBuiltin::Take => &["values", "count"],
                SequenceBuiltin::Fold => &["values", "initial", "callback"],
            },
        });
    }
    if let Some(operation) = string_builtin_callee(callee) {
        return Some(match operation {
            StringOperation::ByteLength
            | StringOperation::TrimAscii
            | StringOperation::FromUtf8 => &["value"],
            StringOperation::Concat => &["left", "right"],
            StringOperation::Get => &["value", "index"],
            StringOperation::Slice => &["value", "start", "end"],
            StringOperation::Find
            | StringOperation::Contains
            | StringOperation::StartsWith
            | StringOperation::EndsWith
            | StringOperation::Split => &["value", "needle"],
            StringOperation::Join => &["values", "separator"],
            StringOperation::Replace => &["value", "needle", "replacement"],
            StringOperation::TemplateConcat => return None,
        });
    }
    if let Some(operation) = standard_operation_callee(callee) {
        return Some(match operation {
            StandardOperation::FloatFormat => &["value", "precision"],
            StandardOperation::TimeBucket => &["value", "bucket"],
            StandardOperation::TimeFormatUtc
            | StandardOperation::TimeParseUtc
            | StandardOperation::ToInt => &["value"],
        });
    }
    None
}

#[derive(Clone)]
struct DirectPartialContract {
    parameters: Vec<ValueType>,
    result: ValueType,
    effects: Vec<String>,
}

fn string_operation_signature(
    operation: StringOperation,
) -> (&'static str, Vec<ValueType>, ValueType) {
    match operation {
        StringOperation::ByteLength => (
            "string.byte_length",
            vec![ValueType::String],
            ValueType::Int,
        ),
        StringOperation::Concat => (
            "string.concat",
            vec![ValueType::String, ValueType::String],
            ValueType::String,
        ),
        StringOperation::Get => (
            "string.get",
            vec![ValueType::String, ValueType::Int],
            ValueType::Option(Box::new(ValueType::String)),
        ),
        StringOperation::Slice => (
            "string.slice",
            vec![ValueType::String, ValueType::Int, ValueType::Int],
            ValueType::Option(Box::new(ValueType::String)),
        ),
        StringOperation::Find => (
            "string.find",
            vec![ValueType::String, ValueType::String],
            ValueType::Option(Box::new(ValueType::Int)),
        ),
        StringOperation::Contains => (
            "string.contains",
            vec![ValueType::String, ValueType::String],
            ValueType::Bool,
        ),
        StringOperation::StartsWith => (
            "string.starts_with",
            vec![ValueType::String, ValueType::String],
            ValueType::Bool,
        ),
        StringOperation::EndsWith => (
            "string.ends_with",
            vec![ValueType::String, ValueType::String],
            ValueType::Bool,
        ),
        StringOperation::Split => (
            "string.split",
            vec![ValueType::String, ValueType::String],
            ValueType::Option(Box::new(ValueType::List(Box::new(ValueType::String)))),
        ),
        StringOperation::Join => (
            "string.join",
            vec![
                ValueType::List(Box::new(ValueType::String)),
                ValueType::String,
            ],
            ValueType::String,
        ),
        StringOperation::TrimAscii => (
            "string.trim_ascii",
            vec![ValueType::String],
            ValueType::String,
        ),
        StringOperation::FromUtf8 => (
            "string.from_utf8",
            vec![ValueType::Bytes],
            ValueType::Option(Box::new(ValueType::String)),
        ),
        StringOperation::Replace => (
            "string.replace",
            vec![ValueType::String, ValueType::String, ValueType::String],
            ValueType::String,
        ),
        StringOperation::TemplateConcat => {
            unreachable!("template concatenation is not a source builtin")
        }
    }
}

fn standard_operation_signature(
    operation: StandardOperation,
) -> (&'static str, Vec<ValueType>, ValueType) {
    match operation {
        StandardOperation::ToInt => (
            "to_int",
            vec![ValueType::String],
            ValueType::Result(Box::new(ValueType::Int), Box::new(parse_error_type())),
        ),
        StandardOperation::FloatFormat => (
            "float.format",
            vec![ValueType::Float, ValueType::Int],
            ValueType::Result(Box::new(ValueType::String), Box::new(format_error_type())),
        ),
        StandardOperation::TimeFormatUtc => (
            "time.format_utc",
            vec![ValueType::Int],
            ValueType::Result(Box::new(ValueType::String), Box::new(time_error_type())),
        ),
        StandardOperation::TimeParseUtc => (
            "time.parse_utc",
            vec![ValueType::String],
            ValueType::Result(Box::new(ValueType::Int), Box::new(time_error_type())),
        ),
        StandardOperation::TimeBucket => (
            "time.bucket",
            vec![ValueType::Int, ValueType::Int],
            ValueType::Result(Box::new(ValueType::Int), Box::new(time_error_type())),
        ),
    }
}

fn capability_operation_signature(
    operation: CapabilityOperation,
) -> (&'static str, Vec<ValueType>, ValueType) {
    match operation {
        CapabilityOperation::IsGranted => (
            "capability.is_granted",
            vec![ValueType::String],
            ValueType::Bool,
        ),
        CapabilityOperation::Granted => (
            "capability.granted",
            Vec::new(),
            ValueType::List(Box::new(ValueType::String)),
        ),
    }
}

fn reorder_labeled_builtin_arguments(
    arguments: &[LoweredCallArgument],
    labels: &[&str],
    span: Span,
) -> Result<Option<Vec<usize>>, Diagnostic> {
    if !arguments.iter().any(|argument| argument.label.is_some()) {
        return Ok(None);
    }
    if arguments.iter().any(|argument| argument.label.is_none()) {
        return Err(Diagnostic::new(
            "E3010",
            "direct builtin calls cannot mix labeled and positional arguments",
            arguments
                .iter()
                .find(|argument| argument.label.is_none())
                .map_or(span, |argument| argument.span),
        ));
    }
    let mut reordered = (0..labels.len()).map(|_| None).collect::<Vec<_>>();
    for (source_index, argument) in arguments.iter().enumerate() {
        let (label, label_span) = argument.label.as_ref().expect("all arguments are labeled");
        let Some(index) = labels.iter().position(|candidate| *candidate == label) else {
            return Err(Diagnostic::new(
                "E3010",
                format!("direct builtin has no parameter labeled '{label}'"),
                *label_span,
            ));
        };
        if reordered[index].replace(source_index).is_some() {
            return Err(Diagnostic::new(
                "E3010",
                format!("direct builtin received duplicate label '{label}'"),
                *label_span,
            ));
        }
    }
    if reordered.iter().any(Option::is_none) {
        return Err(Diagnostic::new(
            "E3010",
            "direct builtin is missing a labeled argument",
            span,
        ));
    }
    Ok(Some(
        reordered
            .into_iter()
            .map(|argument| argument.expect("all labels are supplied"))
            .collect(),
    ))
}

pub(super) struct GlobalLowering<'a> {
    pub(super) bundle: &'a ResolvedBundle,
    pub(super) effect_sets: Vec<Vec<String>>,
    pub(super) constants: Vec<Constant>,
    pub(super) functions: Vec<Option<Function>>,
    pub(super) monomorphs: Vec<(SymbolId, Vec<ValueType>, FunctionId)>,
    pub(super) hir_modules: BTreeMap<String, Vec<HirFunction>>,
    pub(super) mir_functions: Vec<MirFunction>,
    pub(super) types: Vec<ValueType>,
    pub(super) spans: Vec<SourceSpan>,
    pub(super) debug_sources: Vec<String>,
    pub(super) debug_locations: Vec<DebugLocation>,
    pub(super) next_symbol: SymbolId,
    pub(super) async_functions: BTreeSet<FunctionId>,
    pub(super) constant_values: BTreeMap<SymbolId, allen_vm::Value>,
    pub(super) constant_evaluation: bool,
}

impl GlobalLowering<'_> {
    pub(super) fn allocate_symbol(&mut self) -> SymbolId {
        let symbol = self.next_symbol;
        self.next_symbol = self.next_symbol.checked_add(1).expect("symbol ID fits");
        symbol
    }
    pub(super) fn intern_type(&mut self, value_type: ValueType) -> TypeId {
        if let Some(index) = self.types.iter().position(|item| item == &value_type) {
            return u32::try_from(index).expect("type index fits");
        }
        let id = u32::try_from(self.types.len()).expect("type index fits");
        self.types.push(value_type);
        id
    }

    pub(super) fn intern_span(&mut self, module: &str, span: Span) -> SpanId {
        let source_span = SourceSpan {
            module: module.to_owned(),
            span,
        };
        if let Some(index) = self.spans.iter().position(|item| item == &source_span) {
            return u32::try_from(index).expect("span index fits");
        }
        let id = u32::try_from(self.spans.len()).expect("span index fits");
        self.spans.push(source_span);
        id
    }

    pub(super) fn constant(&mut self, constant: Constant) -> Result<u32, Diagnostic> {
        if let Some(index) = self.constants.iter().position(|item| item == &constant) {
            return u32::try_from(index).map_err(|_| {
                Diagnostic::new("E3005", "too many constants", Span { start: 0, end: 0 })
            });
        }
        let id = u32::try_from(self.constants.len()).map_err(|_| {
            Diagnostic::new("E3005", "too many constants", Span { start: 0, end: 0 })
        })?;
        self.constants.push(constant);
        Ok(id)
    }
}

pub(super) struct CompiledExpr {
    register: Register,
    value_type: ValueType,
    effects: EffectSetId,
    hir: HirExpr,
}

#[derive(Clone, Copy)]
pub(super) struct MirRegionCapture {
    outer_tail: Option<u32>,
    entry_start: usize,
}

#[derive(Clone, Copy, Default)]
pub(super) struct CapturedMirRegion {
    entry: Option<u32>,
    tail: Option<u32>,
}

pub(super) struct FunctionLowering<'a, 'b> {
    global: &'a mut GlobalLowering<'b>,
    info: FunctionInfo,
    return_type: ValueType,
    registers: Vec<ValueType>,
    parameters: Vec<Register>,
    captures: Vec<Register>,
    bindings: BTreeMap<String, LocalBinding>,
    local_functions: BTreeMap<String, FunctionInfo>,
    unavailable_local_functions: BTreeSet<String>,
    local_function_ordinal: usize,
    code: Vec<Instruction>,
    instruction_spans: BTreeMap<usize, Span>,
    mir: Vec<MirOperation>,
    mir_blocks: Vec<MirBlock>,
    mir_suspensions: Vec<MirSuspension>,
    mir_task_scopes: Vec<MirTaskScope>,
    mir_ownership: Vec<MirOwnership>,
    ownership_states: BTreeMap<Register, OwnershipRecord>,
    active_scopes: Vec<u32>,
    next_scope: u32,
    mir_continuations: BTreeSet<u32>,
    mir_entries: Vec<u32>,
    mir_tail: Option<u32>,
    loops: Vec<LoopContext>,
    control_reachable: bool,
    runtime_terminal_values: BTreeSet<Register>,
    sub_agent_value_scopes: BTreeMap<Register, u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OwnershipRecord {
    scope: u32,
    state: MirOwnershipState,
    must_consume: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BindingState {
    moved: bool,
    value_scope: u32,
}

impl From<&LocalBinding> for BindingState {
    fn from(binding: &LocalBinding) -> Self {
        Self {
            moved: binding.moved,
            value_scope: binding.value_scope,
        }
    }
}

#[derive(Clone)]
pub(super) struct LoopEdgeState {
    bindings: BTreeMap<String, BindingState>,
    ownership: BTreeMap<Register, OwnershipRecord>,
}

#[derive(Clone, Copy)]
pub(super) struct LoopEntryStates<'a> {
    repeat: &'a LoopEdgeState,
    exit: &'a LoopEdgeState,
}

fn constant_enum_payload(
    value: &allen_vm::EnumValue,
    payload_types: &[ValueType],
    lowering: &mut FunctionLowering<'_, '_>,
    symbol: SymbolId,
    span: Span,
) -> Result<Vec<Register>, Diagnostic> {
    let values: Vec<&allen_vm::Value> = match &value.payload {
        allen_vm::EnumPayload::Unit => Vec::new(),
        allen_vm::EnumPayload::Tuple(values) => values.iter().collect(),
        allen_vm::EnumPayload::Record(values) => values.iter().map(|(_, value)| value).collect(),
    };
    if values.len() != payload_types.len() {
        return Err(Diagnostic::new(
            "E3007",
            "constant enum payload mismatch",
            span,
        ));
    }
    values
        .into_iter()
        .zip(payload_types)
        .map(|(value, value_type)| {
            lowering
                .materialize_constant(value, value_type, symbol, span)
                .map(|value| value.register)
        })
        .collect()
}

#[derive(Clone)]
pub(super) struct LoopContext {
    scope_depth: usize,
    outer_bindings: BTreeMap<String, LocalBinding>,
    outer_ownership: BTreeMap<Register, OwnershipRecord>,
    break_jumps: Vec<usize>,
    continue_jumps: Vec<usize>,
    break_mir_blocks: Vec<u32>,
    continue_mir_blocks: Vec<u32>,
    break_edges: Vec<LoopEdgeState>,
    continue_edges: Vec<LoopEdgeState>,
}

pub(super) struct CompiledLoop {
    hir: HirExpr,
    register: Register,
    falls_through: bool,
}

impl FunctionLowering<'_, '_> {
    #[allow(clippy::too_many_lines)]
    fn materialize_constant(
        &mut self,
        value: &allen_vm::Value,
        value_type: &ValueType,
        symbol: SymbolId,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let (register, mir) = match (value, value_type) {
            (allen_vm::Value::Int(value), ValueType::Int) => {
                let register = self.allocate(ValueType::Int)?;
                let constant = self.global.constant(Constant::Int(*value))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                (
                    register,
                    MirOperation::Constant {
                        destination: u32::from(register),
                    },
                )
            }
            (allen_vm::Value::Bool(value), ValueType::Bool) => {
                let register = self.allocate(ValueType::Bool)?;
                let constant = self.global.constant(Constant::Bool(*value))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                (
                    register,
                    MirOperation::Constant {
                        destination: u32::from(register),
                    },
                )
            }
            (allen_vm::Value::Float(value), ValueType::Float) => {
                let register = self.allocate(ValueType::Float)?;
                let constant = self.global.constant(Constant::float_bits(value.bits()))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                (
                    register,
                    MirOperation::Constant {
                        destination: u32::from(register),
                    },
                )
            }
            (allen_vm::Value::String(value), ValueType::String) => {
                let register = self.allocate(ValueType::String)?;
                let constant = self.global.constant(Constant::String(value.to_string()))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                (
                    register,
                    MirOperation::Constant {
                        destination: u32::from(register),
                    },
                )
            }
            (allen_vm::Value::Bytes(value), ValueType::Bytes) => {
                let register = self.allocate(ValueType::Bytes)?;
                let constant = self.global.constant(Constant::Bytes(value.to_vec()))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                (
                    register,
                    MirOperation::Constant {
                        destination: u32::from(register),
                    },
                )
            }
            (allen_vm::Value::Unit, ValueType::Unit) => {
                let register = self.allocate(ValueType::Unit)?;
                let constant = self.global.constant(Constant::Unit)?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                (
                    register,
                    MirOperation::Constant {
                        destination: u32::from(register),
                    },
                )
            }
            (allen_vm::Value::List(values), ValueType::List(element_type)) => {
                let elements = values
                    .iter()
                    .map(|value| self.materialize_constant(value, element_type, symbol, span))
                    .collect::<Result<Vec<_>, _>>()?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::ListNew {
                    destination: register,
                    elements: elements.iter().map(|value| value.register).collect(),
                });
                (
                    register,
                    MirOperation::List {
                        destination: u32::from(register),
                        items: Vec::new(),
                    },
                )
            }
            (allen_vm::Value::Map(entries), ValueType::Map(key_type, item_type)) => {
                let entries = entries
                    .iter()
                    .map(|(key, value)| {
                        Ok((
                            self.materialize_constant(key, key_type, symbol, span)?,
                            self.materialize_constant(value, item_type, symbol, span)?,
                        ))
                    })
                    .collect::<Result<Vec<_>, Diagnostic>>()?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::MapNew {
                    destination: register,
                    entries: entries
                        .iter()
                        .map(|(key, value)| (key.register, value.register))
                        .collect(),
                });
                (
                    register,
                    MirOperation::Map {
                        destination: u32::from(register),
                        items: Vec::new(),
                    },
                )
            }
            (allen_vm::Value::Tuple(values), ValueType::Tuple(element_types))
                if values.len() == element_types.len() =>
            {
                let elements = values
                    .iter()
                    .zip(element_types)
                    .map(|(value, value_type)| {
                        self.materialize_constant(value, value_type, symbol, span)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::TupleNew {
                    destination: register,
                    elements: elements.iter().map(|value| value.register).collect(),
                });
                (
                    register,
                    MirOperation::Tuple {
                        destination: u32::from(register),
                    },
                )
            }
            (allen_vm::Value::Record(values), ValueType::Record(fields))
                if values.len() == fields.len() =>
            {
                let mut compiled = Vec::with_capacity(fields.len());
                for (index, field) in fields.iter().enumerate() {
                    let (_, value) = values
                        .iter()
                        .find(|(name, _)| name.as_ref() == field.name)
                        .ok_or_else(|| {
                            Diagnostic::new("E3007", "constant record shape mismatch", span)
                        })?;
                    compiled.push((
                        u32::try_from(index).expect("field index"),
                        self.materialize_constant(value, &field.value_type, symbol, span)?,
                    ));
                }
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::RecordNew {
                    destination: register,
                    fields: compiled
                        .iter()
                        .map(|(index, value)| (*index, value.register))
                        .collect(),
                });
                (
                    register,
                    MirOperation::Record {
                        destination: u32::from(register),
                    },
                )
            }
            (allen_vm::Value::Newtype(value), ValueType::Newtype { name, underlying })
                if value.identity() == name =>
            {
                let inner = self.materialize_constant(value.value(), underlying, symbol, span)?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::NewtypeWrap {
                    destination: register,
                    source: inner.register,
                });
                (
                    register,
                    MirOperation::NewtypeWrap {
                        destination: u32::from(register),
                        source: u32::from(inner.register),
                    },
                )
            }
            (allen_vm::Value::Enum(value), ValueType::Option(element_type)) => {
                let payload_types = if value.variant == 0 {
                    Vec::new()
                } else if value.variant == 1 {
                    vec![element_type.as_ref().clone()]
                } else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "constant Option variant mismatch",
                        span,
                    ));
                };
                let payload = constant_enum_payload(value, &payload_types, self, symbol, span)?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::EnumNew {
                    destination: register,
                    variant: value.variant,
                    payload,
                });
                (
                    register,
                    MirOperation::Enum {
                        destination: u32::from(register),
                    },
                )
            }
            (allen_vm::Value::Enum(value), ValueType::Result(ok_type, error_type)) => {
                let payload_types = match value.variant {
                    0 => vec![ok_type.as_ref().clone()],
                    1 => vec![error_type.as_ref().clone()],
                    _ => {
                        return Err(Diagnostic::new(
                            "E3007",
                            "constant Result variant mismatch",
                            span,
                        ));
                    }
                };
                let payload = constant_enum_payload(value, &payload_types, self, symbol, span)?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::EnumNew {
                    destination: register,
                    variant: value.variant,
                    payload,
                });
                (
                    register,
                    MirOperation::Enum {
                        destination: u32::from(register),
                    },
                )
            }
            (allen_vm::Value::Enum(value), ValueType::Enum(type_id)) => {
                let variant = self
                    .global
                    .bundle
                    .enum_types
                    .get(*type_id as usize)
                    .and_then(|enum_type| enum_type.variants.get(value.variant as usize))
                    .ok_or_else(|| {
                        Diagnostic::new("E3007", "constant enum variant mismatch", span)
                    })?;
                let payload_types = match &variant.payload {
                    EnumPayloadType::Unit => Vec::new(),
                    EnumPayloadType::Tuple(values) => values.clone(),
                    EnumPayloadType::Record(fields) => fields
                        .iter()
                        .map(|field| field.value_type.clone())
                        .collect(),
                };
                let payload = constant_enum_payload(value, &payload_types, self, symbol, span)?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::EnumNew {
                    destination: register,
                    variant: value.variant,
                    payload,
                });
                (
                    register,
                    MirOperation::Enum {
                        destination: u32::from(register),
                    },
                )
            }
            _ => {
                return Err(Diagnostic::new(
                    "E3007",
                    "compile-time constant value does not match its declared type",
                    span,
                ));
            }
        };
        self.mir.push(mir);
        let effects = self.empty_effects();
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Variable,
                Some(symbol),
                value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn runtime_falls_through(&self, value: &CompiledExpr) -> bool {
        value.value_type != ValueType::Never
            && !self.runtime_terminal_values.contains(&value.register)
    }

    pub(super) fn validate_loop_body_type(
        body: &LoweredBody,
        value: &CompiledExpr,
    ) -> Result<(), Diagnostic> {
        if matches!(value.value_type, ValueType::Unit | ValueType::Never) {
            return Ok(());
        }
        Err(Diagnostic::new(
            "E3007",
            format!(
                "loop body must have type Void or Never, found {}",
                value.value_type
            ),
            body.tail.as_ref().map_or(body.span, |tail| tail.span),
        ))
    }

    pub(super) fn current_scope(&self) -> u32 {
        self.active_scopes.last().copied().unwrap_or(0)
    }

    pub(super) fn sub_agent_value_scope(&self, value: &CompiledExpr) -> u32 {
        value
            .hir
            .symbol
            .and_then(|symbol| {
                self.bindings
                    .values()
                    .find(|binding| binding.symbol == symbol)
                    .map(|binding| binding.value_scope)
            })
            .or_else(|| self.sub_agent_value_scopes.get(&value.register).copied())
            .or_else(|| {
                self.bindings
                    .values()
                    .find(|binding| binding.register == value.register)
                    .map(|binding| binding.value_scope)
            })
            .unwrap_or_else(|| self.current_scope())
    }

    pub(super) fn scope_outlives(&self, source: u32, target: u32) -> bool {
        matches!(
            (self.scope_depth(source), self.scope_depth(target)),
            (Some(source), Some(target)) if source <= target
        )
    }

    pub(super) fn scope_depth(&self, scope: u32) -> Option<usize> {
        if scope == 0 {
            Some(0)
        } else {
            self.active_scopes
                .iter()
                .position(|active| *active == scope)
                .map(|index| index + 1)
        }
    }

    pub(super) fn deeper_scope(&self, left: u32, right: u32) -> u32 {
        if self.scope_depth(left).unwrap_or(usize::MAX)
            >= self.scope_depth(right).unwrap_or(usize::MAX)
        {
            left
        } else {
            right
        }
    }

    pub(super) fn mark_instruction(&mut self, instruction: usize, span: Span) {
        self.instruction_spans.insert(instruction, span);
    }

    pub(super) fn mark_last_instruction(&mut self, span: Span) {
        self.mark_instruction(self.code.len() - 1, span);
    }

    pub(super) fn allocate(&mut self, value_type: ValueType) -> Result<Register, Diagnostic> {
        let register = u16::try_from(self.registers.len()).map_err(|_| {
            Diagnostic::new(
                "E3005",
                "function needs too many registers",
                self.info.lowered.name_span,
            )
        })?;
        self.registers.push(value_type);
        Ok(register)
    }

    pub(super) fn empty_effects(&self) -> EffectSetId {
        effect_id(&self.global.effect_sets, &[])
    }

    pub(super) fn union_effects(
        &self,
        effects: impl IntoIterator<Item = EffectSetId>,
    ) -> EffectSetId {
        let mut union = BTreeSet::new();
        for effect_set in effects {
            union.extend(self.global.effect_sets[effect_set as usize].iter().cloned());
        }
        effect_id(
            &self.global.effect_sets,
            &union.into_iter().collect::<Vec<_>>(),
        )
    }

    pub(super) fn record_ownership(
        &mut self,
        register: Register,
        scope: u32,
        state: MirOwnershipState,
        must_consume: bool,
    ) {
        self.ownership_states.insert(
            register,
            OwnershipRecord {
                scope,
                state,
                must_consume,
            },
        );
        self.mir_ownership.push(MirOwnership {
            temporary: u32::from(register),
            scope,
            state,
            must_consume,
        });
    }

    pub(super) fn consume_ownership(&mut self, register: Register, state: MirOwnershipState) {
        if let Some(ownership) = self.ownership_states.get(&register).copied() {
            self.record_ownership(register, ownership.scope, state, ownership.must_consume);
        }
    }

    pub(super) fn must_consume(&self, register: Register) -> bool {
        self.ownership_states
            .get(&register)
            .is_some_and(|ownership| ownership.must_consume)
    }

    pub(super) fn next_mir_block(&self) -> u32 {
        u32::try_from(self.mir_blocks.len() + 1).expect("MIR block ID fits")
    }

    pub(super) fn cleanup_operations(&self, kind: MirCleanupKind) -> Vec<MirOperation> {
        self.active_scopes
            .iter()
            .rev()
            .map(|scope| MirOperation::TaskScopeCleanup {
                scope: *scope,
                kind,
            })
            .collect()
    }

    pub(super) fn invalidate_scope_local_sub_agents(&mut self, scope: u32) {
        self.bindings.retain(|_, binding| {
            binding.scope != scope || !contains_stored_sub_agent(&binding.value_type)
        });
    }

    pub(super) fn terminate_source_dead_path(
        &mut self,
        ownership_at_entry: &BTreeSet<Register>,
        span: Span,
    ) -> Result<Register, Diagnostic> {
        let path_local_live = self
            .ownership_states
            .iter()
            .filter_map(|(register, ownership)| {
                (!ownership_at_entry.contains(register)
                    && ownership.state == MirOwnershipState::Live)
                    .then_some(*register)
            })
            .collect::<Vec<_>>();
        for register in path_local_live {
            self.consume_ownership(register, MirOwnershipState::ScopeJoined);
        }
        let reason = self.allocate(ValueType::String)?;
        let constant = self.global.constant(Constant::String(
            "source-unreachable control flow".to_owned(),
        ))?;
        self.code.push(Instruction::Const {
            destination: reason,
            constant,
        });
        self.mark_last_instruction(span);
        self.code.push(Instruction::Stop { reason });
        self.mark_last_instruction(span);
        Ok(reason)
    }

    pub(super) fn register_mir_region(&mut self, entry: u32, continuation: u32) {
        if let Some(previous) = self.mir_tail {
            self.mir_blocks[previous as usize - 1].terminator =
                MirTerminator::Goto { target: entry };
        }
        self.mir_entries.push(entry);
        self.mir_continuations.insert(continuation);
        self.mir_tail = Some(continuation);
    }

    pub(super) fn begin_nested_mir_region(&mut self) -> MirRegionCapture {
        MirRegionCapture {
            outer_tail: self.mir_tail.take(),
            entry_start: self.mir_entries.len(),
        }
    }

    pub(super) fn finish_nested_mir_region(
        &mut self,
        capture: MirRegionCapture,
    ) -> CapturedMirRegion {
        let entries = self.mir_entries.split_off(capture.entry_start);
        let region = CapturedMirRegion {
            entry: entries.first().copied(),
            tail: self.mir_tail.take(),
        };
        self.mir_tail = capture.outer_tail;
        region
    }

    pub(super) fn set_mir_handoff(&mut self, block: u32, terminator: MirTerminator) {
        self.mir_blocks[block as usize - 1].terminator = terminator;
        self.mir_continuations.remove(&block);
    }

    pub(super) fn compile_without_runtime<T>(
        &mut self,
        compile: impl FnOnce(&mut Self) -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        let global_constants_len = self.global.constants.len();
        let global_functions_len = self.global.functions.len();
        let global_monomorphs_len = self.global.monomorphs.len();
        let global_hir_modules = self.global.hir_modules.clone();
        let global_mir_functions_len = self.global.mir_functions.len();
        let global_debug_locations_len = self.global.debug_locations.len();
        let global_async_functions = self.global.async_functions.clone();
        let code_len = self.code.len();
        let register_len = self.registers.len();
        let mir_len = self.mir.len();
        let mir_block_len = self.mir_blocks.len();
        let suspension_len = self.mir_suspensions.len();
        let task_scope_len = self.mir_task_scopes.len();
        let mir_ownership_len = self.mir_ownership.len();
        let entry_len = self.mir_entries.len();
        let bindings = self.bindings.clone();
        let local_functions = self.local_functions.clone();
        let local_function_ordinal = self.local_function_ordinal;
        let ownership_states = self.ownership_states.clone();
        let loops = self.loops.clone();
        let active_scopes = self.active_scopes.clone();
        let continuations = self.mir_continuations.clone();
        let tail = self.mir_tail.take();
        let runtime_terminal_values = self.runtime_terminal_values.clone();
        let sub_agent_value_scopes = self.sub_agent_value_scopes.clone();
        let control_reachable = self.control_reachable;
        self.control_reachable = false;

        let result = compile(self);

        self.global.constants.truncate(global_constants_len);
        self.global.functions.truncate(global_functions_len);
        self.global.monomorphs.truncate(global_monomorphs_len);
        self.global.hir_modules = global_hir_modules;
        self.global.mir_functions.truncate(global_mir_functions_len);
        self.global
            .debug_locations
            .truncate(global_debug_locations_len);
        self.global.async_functions = global_async_functions;
        self.code.truncate(code_len);
        self.instruction_spans
            .retain(|instruction, _| *instruction < code_len);
        self.registers.truncate(register_len);
        self.mir.truncate(mir_len);
        self.mir_blocks.truncate(mir_block_len);
        self.mir_suspensions.truncate(suspension_len);
        self.mir_task_scopes.truncate(task_scope_len);
        self.mir_ownership.truncate(mir_ownership_len);
        self.mir_entries.truncate(entry_len);
        self.bindings = bindings;
        self.local_functions = local_functions;
        self.local_function_ordinal = local_function_ordinal;
        self.ownership_states = ownership_states;
        self.loops = loops;
        self.active_scopes = active_scopes;
        self.mir_continuations = continuations;
        self.mir_tail = tail;
        self.runtime_terminal_values = runtime_terminal_values;
        self.sub_agent_value_scopes = sub_agent_value_scopes;
        self.control_reachable = control_reachable;
        result
    }

    pub(super) fn compile_static_loop_body(
        &mut self,
        body: &LoweredBody,
    ) -> Result<(HirExpr, EffectSetId), Diagnostic> {
        self.compile_without_runtime(|lowering| {
            lowering.push_loop();
            let (body_hir, body_value, _) = lowering.compile_block_value(body)?;
            Self::validate_loop_body_type(body, &body_value)?;
            let effects = body_hir.effects;
            let context = lowering.loops.pop().expect("static loop remains active");
            lowering.bindings = context.outer_bindings;
            lowering.ownership_states = context.outer_ownership;
            Ok((body_hir, effects))
        })
    }

    pub(super) fn compile_static_for_body(
        &mut self,
        binding: &LoweredLoopBinding,
        yielded_type: &ValueType,
        body: &LoweredBody,
    ) -> Result<(HirLoopBinding, HirExpr, EffectSetId), Diagnostic> {
        self.compile_without_runtime(|lowering| {
            lowering.push_loop();
            let yielded = lowering.allocate(yielded_type.clone())?;
            let hir_binding = lowering.install_loop_binding(binding, yielded, yielded_type)?;
            let (body_hir, body_value, _) = lowering.compile_block_value(body)?;
            Self::validate_loop_body_type(body, &body_value)?;
            let effects = body_hir.effects;
            let context = lowering
                .loops
                .pop()
                .expect("static for loop remains active");
            lowering.bindings = context.outer_bindings;
            lowering.ownership_states = context.outer_ownership;
            Ok((hir_binding, body_hir, effects))
        })
    }

    pub(super) fn hir(
        &mut self,
        kind: HirExprKind,
        symbol: Option<SymbolId>,
        value_type: &ValueType,
        effects: EffectSetId,
        span: Span,
    ) -> HirExpr {
        let ty = self.global.intern_type(value_type.clone());
        let span = self.global.intern_span(&self.info.module, span);
        HirExpr {
            kind,
            symbol,
            ty,
            effects,
            span,
        }
    }

    pub(super) fn loop_entry_state(&self) -> LoopEdgeState {
        LoopEdgeState {
            bindings: self
                .bindings
                .iter()
                .map(|(name, binding)| (name.clone(), BindingState::from(binding)))
                .collect(),
            ownership: self.ownership_states.clone(),
        }
    }

    pub(super) fn loop_edge_state(
        &self,
        context: &LoopContext,
        span: Span,
    ) -> Result<LoopEdgeState, Diagnostic> {
        if self.ownership_states.iter().any(|(register, ownership)| {
            !context.outer_ownership.contains_key(register)
                && ownership.state == MirOwnershipState::Live
                && matches!(
                    self.registers[*register as usize],
                    ValueType::Future(_) | ValueType::Task(_) | ValueType::SubAgent
                )
        }) || self.bindings.iter().any(|(name, binding)| {
            !context.outer_bindings.contains_key(name)
                && contains_stored_sub_agent(&binding.value_type)
        }) {
            return Err(Diagnostic::new(
                "E3011",
                "loop edge leaves a live affine future, task, or SubAgent obligation",
                span,
            ));
        }
        Ok(LoopEdgeState {
            bindings: context
                .outer_bindings
                .keys()
                .filter_map(|name| {
                    self.bindings
                        .get(name)
                        .map(|binding| (name.clone(), BindingState::from(binding)))
                })
                .collect(),
            ownership: context
                .outer_ownership
                .keys()
                .filter_map(|register| {
                    self.ownership_states
                        .get(register)
                        .map(|ownership| (*register, *ownership))
                })
                .collect(),
        })
    }

    pub(super) fn validate_loop_edge(
        expected: &LoopEdgeState,
        found: &LoopEdgeState,
        kind: &str,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if expected.bindings != found.bindings || expected.ownership != found.ownership {
            return Err(Diagnostic::new(
                "E3011",
                format!("{kind} must preserve the loop's affine ownership state"),
                span,
            ));
        }
        Ok(())
    }

    pub(super) fn validate_loop_repeat_edge(
        &self,
        context: &LoopContext,
        entry: &LoopEdgeState,
        found: &LoopEdgeState,
        kind: &str,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let live_binding = found.bindings.iter().any(|(name, state)| {
            context.outer_bindings.get(name).is_some_and(|binding| {
                (!state.moved && is_affine(&binding.value_type))
                    || contains_stored_sub_agent(&binding.value_type)
            })
        });
        let live_ownership = found.ownership.iter().any(|(register, ownership)| {
            ownership.state == MirOwnershipState::Live
                && matches!(
                    self.registers[*register as usize],
                    ValueType::Future(_) | ValueType::Task(_) | ValueType::SubAgent
                )
        });
        if live_binding || live_ownership {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "live affine future, task, or SubAgent ownership cannot cross a reachable {kind}"
                ),
                span,
            ));
        }
        Self::validate_loop_edge(entry, found, kind, span)
    }

    pub(super) fn exit_scopes_for_loop_control(
        &mut self,
        scope_depth: usize,
        span: Span,
    ) -> Result<(), Diagnostic> {
        for scope in self.active_scopes[scope_depth..]
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>()
        {
            let live = self
                .ownership_states
                .iter()
                .filter_map(|(register, ownership)| {
                    (ownership.scope == scope && ownership.state == MirOwnershipState::Live)
                        .then_some(*register)
                })
                .collect::<Vec<_>>();
            for register in live {
                let nested_task_result = matches!(
                    &self.registers[register as usize],
                    ValueType::Task(result) if is_affine(result)
                );
                let hidden_future_obligation =
                    matches!(self.registers[register as usize], ValueType::Future(_))
                        && self.must_consume(register);
                if nested_task_result || hidden_future_obligation {
                    return Err(Diagnostic::new(
                        "E3011",
                        "nested affine result must be awaited before loop control exits its scope",
                        span,
                    ));
                }
                self.record_ownership(register, scope, MirOwnershipState::ScopeJoined, true);
            }
            self.code.push(Instruction::TaskScopeExit { scope });
            self.mark_last_instruction(span);
            self.mir.push(MirOperation::TaskScopeExit { scope });
            self.mir.push(MirOperation::TaskScopeCleanup {
                scope,
                kind: MirCleanupKind::NormalJoin,
            });
            self.invalidate_scope_local_sub_agents(scope);
        }
        Ok(())
    }

    pub(super) fn compile_loop_control(
        &mut self,
        is_break: bool,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let Some(context) = self.loops.last() else {
            return Err(Diagnostic::new(
                "E3005",
                if is_break {
                    "break is only valid inside a loop"
                } else {
                    "continue is only valid inside a loop"
                },
                span,
            ));
        };
        if !self.control_reachable {
            // The source-dead branch is still present in bytecode for static HIR, effect, and
            // diagnostic checking. Terminate that impossible verifier path instead of inventing
            // a break/continue edge, and account for values created only on the dead path as
            // permanently cancelled without changing the loop-entry ownership snapshot.
            let ownership_at_entry = context.outer_ownership.keys().copied().collect();
            let reason = self.terminate_source_dead_path(&ownership_at_entry, span)?;
            self.mir.push(MirOperation::Constant {
                destination: u32::from(reason),
            });
            let register = self.allocate(ValueType::Unit)?;
            let effects = self.empty_effects();
            return Ok(CompiledExpr {
                register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    if is_break {
                        HirExprKind::Break
                    } else {
                        HirExprKind::Continue
                    },
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        let scope_depth = context.scope_depth;
        self.exit_scopes_for_loop_control(scope_depth, span)?;
        let context = self.loops.last().expect("loop context remains active");
        let edge = self.loop_edge_state(context, span)?;
        let jump = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        self.mark_last_instruction(span);
        let mir_block = self.next_mir_block();
        self.mir_blocks.push(MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        });
        if let Some(previous) = self.mir_tail.take() {
            self.set_mir_handoff(previous, MirTerminator::Goto { target: mir_block });
        }
        self.mir_entries.push(mir_block);
        let context = self.loops.last_mut().expect("loop context remains active");
        if is_break {
            context.break_jumps.push(jump);
            context.break_mir_blocks.push(mir_block);
            context.break_edges.push(edge);
        } else {
            context.continue_jumps.push(jump);
            context.continue_mir_blocks.push(mir_block);
            context.continue_edges.push(edge);
        }
        let register = self.allocate(ValueType::Unit)?;
        let effects = self.empty_effects();
        Ok(CompiledExpr {
            register,
            value_type: ValueType::Never,
            effects,
            hir: self.hir(
                if is_break {
                    HirExprKind::Break
                } else {
                    HirExprKind::Continue
                },
                None,
                &ValueType::Never,
                effects,
                span,
            ),
        })
    }

    pub(super) fn push_loop(&mut self) -> LoopEdgeState {
        let entry = self.loop_entry_state();
        self.loops.push(LoopContext {
            scope_depth: self.active_scopes.len(),
            outer_bindings: self.bindings.clone(),
            outer_ownership: self.ownership_states.clone(),
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            break_mir_blocks: Vec::new(),
            continue_mir_blocks: Vec::new(),
            break_edges: Vec::new(),
            continue_edges: Vec::new(),
        });
        entry
    }

    pub(super) fn restore_loop_depth(&mut self, depth: usize) {
        while self.loops.len() > depth {
            let context = self.loops.pop().expect("loop context exceeds saved depth");
            self.bindings = context.outer_bindings;
            self.ownership_states = context.outer_ownership;
        }
    }

    pub(super) fn finish_loop(
        &mut self,
        entries: LoopEntryStates<'_>,
        body_falls_through: bool,
        continue_target: u32,
        break_target: u32,
        zero_iteration: bool,
        span: Span,
    ) -> Result<(LoopContext, bool), Diagnostic> {
        let context = self.loops.pop().expect("loop context is active");
        if body_falls_through {
            let edge = self.loop_edge_state(&context, span)?;
            self.validate_loop_repeat_edge(
                &context,
                entries.repeat,
                &edge,
                "loop back-edge",
                span,
            )?;
        }
        for edge in &context.continue_edges {
            self.validate_loop_repeat_edge(&context, entries.repeat, edge, "continue", span)?;
        }
        let mut joined = zero_iteration.then(|| entries.exit.clone());
        for edge in &context.break_edges {
            if let Some(expected) = &joined {
                Self::validate_loop_edge(expected, edge, "break", span)?;
            } else {
                joined = Some(edge.clone());
            }
        }
        for jump in &context.continue_jumps {
            self.code[*jump] = Instruction::Jump {
                target: continue_target,
            };
        }
        for jump in &context.break_jumps {
            self.code[*jump] = Instruction::Jump {
                target: break_target,
            };
        }
        self.bindings = context.outer_bindings.clone();
        self.ownership_states = context.outer_ownership.clone();
        if let Some(joined) = joined {
            for (name, state) in joined.bindings {
                if let Some(binding) = self.bindings.get_mut(&name) {
                    binding.moved = state.moved;
                    binding.value_scope = state.value_scope;
                }
            }
            self.ownership_states.extend(joined.ownership);
        }
        let falls_through = zero_iteration || !context.break_edges.is_empty();
        Ok((context, falls_through))
    }

    pub(super) fn install_loop_binding(
        &mut self,
        binding: &LoweredLoopBinding,
        value: Register,
        value_type: &ValueType,
    ) -> Result<HirLoopBinding, Diagnostic> {
        let element_types = if binding.tuple {
            let ValueType::Tuple(elements) = value_type else {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("tuple loop binding requires a Tuple value, found {value_type}"),
                    binding.span,
                ));
            };
            if elements.len() != binding.elements.len() {
                return Err(Diagnostic::new(
                    "E3007",
                    format!(
                        "tuple loop binding has {} elements, but the iterator yields {}",
                        binding.elements.len(),
                        elements.len()
                    ),
                    binding.span,
                ));
            }
            elements.clone()
        } else {
            vec![value_type.clone()]
        };
        let mut hir_elements = Vec::with_capacity(binding.elements.len());
        for (index, (element, element_type)) in binding
            .elements
            .iter()
            .zip(element_types.into_iter())
            .enumerate()
        {
            let register = if binding.tuple {
                let register = self.allocate(element_type.clone())?;
                self.code.push(Instruction::TupleGet {
                    destination: register,
                    tuple: value,
                    index: u32::try_from(index).expect("loop tuple binding index fits"),
                });
                self.mark_last_instruction(element.span);
                self.mir.push(MirOperation::Binary {
                    destination: u32::from(register),
                });
                register
            } else {
                value
            };
            let symbol = if let Some(name) = &element.name {
                if self.bindings.contains_key(name) || self.local_name_conflicts(name) {
                    return Err(Diagnostic::new(
                        "E3005",
                        format!("duplicate local binding '{name}'"),
                        element.span,
                    ));
                }
                let symbol = self.global.allocate_symbol();
                let scope = self.active_scopes.last().copied().unwrap_or(0);
                self.bindings.insert(
                    name.clone(),
                    LocalBinding {
                        register,
                        symbol,
                        value_type: element_type.clone(),
                        scope,
                        value_scope: scope,
                        mutable: false,
                        moved: false,
                    },
                );
                Some(symbol)
            } else {
                None
            };
            hir_elements.push(HirLoopBindingElement {
                symbol,
                ty: self.global.intern_type(element_type),
                span: self.global.intern_span(&self.info.module, element.span),
            });
        }
        Ok(HirLoopBinding {
            elements: hir_elements,
            tuple: binding.tuple,
            span: self.global.intern_span(&self.info.module, binding.span),
        })
    }

    #[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
    pub(super) fn register_loop_mir_region(
        &mut self,
        header: u32,
        repeat_target: u32,
        condition_operations: Vec<MirOperation>,
        body_operations: Vec<MirOperation>,
        body_region: CapturedMirRegion,
        body_falls_through: bool,
        backedge_reachable: bool,
        continue_mir_blocks: &[u32],
        break_mir_blocks: &[u32],
        has_zero_iteration: bool,
    ) {
        let body = header + 1;
        let exit = self.next_mir_block();
        let has_continue = !continue_mir_blocks.is_empty();
        let has_break = !break_mir_blocks.is_empty();
        for block in continue_mir_blocks {
            self.mir_blocks[*block as usize - 1].terminator = MirTerminator::Goto {
                target: repeat_target,
            };
        }
        for block in break_mir_blocks {
            self.mir_blocks[*block as usize - 1].terminator = MirTerminator::Goto { target: exit };
        }
        let synthetic_stop = body_falls_through
            && !backedge_reachable
            && matches!(self.code.last(), Some(Instruction::Stop { .. }));
        let body_terminator = if synthetic_stop {
            let Some(Instruction::Stop { reason }) = self.code.last() else {
                unreachable!("synthetic stop was matched")
            };
            MirTerminator::Stop {
                reason: u32::from(*reason),
            }
        } else if backedge_reachable || has_continue {
            MirTerminator::Goto {
                target: repeat_target,
            }
        } else if body_falls_through && has_zero_iteration {
            MirTerminator::Goto { target: exit }
        } else if !body_falls_through {
            match self.code.last() {
                Some(Instruction::Return { source }) => MirTerminator::Return {
                    source: u32::from(*source),
                },
                Some(Instruction::Stop { reason }) => MirTerminator::Stop {
                    reason: u32::from(*reason),
                },
                _ => MirTerminator::Unreachable,
            }
        } else {
            MirTerminator::Unreachable
        };
        self.mir_blocks[header as usize - 1] = MirBlock {
            operations: condition_operations,
            terminator: if has_zero_iteration {
                MirTerminator::SwitchBool {
                    false_target: exit,
                    true_target: body,
                }
            } else {
                MirTerminator::Goto { target: body }
            },
        };
        self.mir_blocks[body as usize - 1] = MirBlock {
            operations: body_operations,
            terminator: body_region.entry.map_or_else(
                || body_terminator.clone(),
                |target| MirTerminator::Goto { target },
            ),
        };
        if let Some(tail) = body_region.tail {
            self.set_mir_handoff(tail, body_terminator);
        }
        if has_zero_iteration || has_break {
            self.mir_blocks.push(MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            });
            self.register_mir_region(header, exit);
        } else {
            if let Some(previous) = self.mir_tail {
                self.mir_blocks[previous as usize - 1].terminator =
                    MirTerminator::Goto { target: header };
            }
            self.mir_entries.push(header);
            if backedge_reachable || has_continue || synthetic_stop || !body_falls_through {
                self.mir_tail = None;
            } else {
                self.mir_continuations.insert(body);
                self.mir_tail = Some(body);
            }
        }
    }

    pub(super) fn compile_while(
        &mut self,
        condition: &LoweredExpr,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledLoop, Diagnostic> {
        let loop_depth = self.loops.len();
        let control_reachable = self.control_reachable;
        let result = self.compile_while_scoped(condition, body, span);
        self.restore_loop_depth(loop_depth);
        self.control_reachable = control_reachable;
        result
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_while_scoped(
        &mut self,
        condition: &LoweredExpr,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledLoop, Diagnostic> {
        let constant_condition = match &condition.kind {
            LoweredExprKind::Bool(value) => Some(*value),
            _ => None,
        };
        let has_zero_iteration = constant_condition != Some(true);
        let outer_control_reachable = self.control_reachable;
        let register = self.allocate(ValueType::Unit)?;
        let header = u32::try_from(self.code.len()).expect("instruction index fits");
        let repeat_entry = self.push_loop();
        let condition_operations_start = self.mir.len();
        let condition_value = self.compile_expression(condition)?;
        if condition_value.value_type == ValueType::Never {
            let (body_hir, body_effects) = self.compile_static_loop_body(body)?;
            let effects = self.union_effects([condition_value.effects, body_effects]);
            return Ok(CompiledLoop {
                register,
                falls_through: false,
                hir: self.hir(
                    HirExprKind::While {
                        condition: Box::new(condition_value.hir),
                        body: Box::new(body_hir),
                    },
                    None,
                    &ValueType::Unit,
                    effects,
                    span,
                ),
            });
        }
        let condition_operations = self.mir.split_off(condition_operations_start);
        if condition_value.value_type != ValueType::Bool {
            return Err(Diagnostic::new(
                "E3007",
                format!(
                    "while condition must be Bool, found {}",
                    condition_value.value_type
                ),
                condition.span,
            ));
        }
        let exit_entry = self.loop_edge_state(
            self.loops
                .last()
                .expect("while loop context remains active"),
            condition.span,
        )?;
        let branch = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        let body_target = u32::try_from(self.code.len()).expect("instruction index fits");
        let mir_header = self.next_mir_block();
        self.mir_blocks.extend((0..2).map(|_| MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        }));
        self.control_reachable = outer_control_reachable && constant_condition != Some(false);
        let body_region_capture = self.begin_nested_mir_region();
        let body_operations_start = self.mir.len();
        let (body_hir, body_value, _) = self.compile_block_value(body)?;
        let body_region = self.finish_nested_mir_region(body_region_capture);
        self.control_reachable = outer_control_reachable;
        Self::validate_loop_body_type(body, &body_value)?;
        let mut body_operations = self.mir.split_off(body_operations_start);
        let body_falls_through = body_value.value_type != ValueType::Never;
        let body_runtime_falls_through = self.runtime_falls_through(&body_value);
        let backedge_reachable = body_runtime_falls_through
            && outer_control_reachable
            && constant_condition != Some(false);
        if backedge_reachable {
            self.code.push(Instruction::Jump { target: header });
            self.mark_last_instruction(body.span);
        } else if body_falls_through && outer_control_reachable && constant_condition != Some(false)
        {
            let ownership_at_entry = repeat_entry.ownership.keys().copied().collect();
            let reason = self.terminate_source_dead_path(&ownership_at_entry, body.span)?;
            body_operations.push(MirOperation::Constant {
                destination: u32::from(reason),
            });
        }
        let emitted_exit = u32::try_from(self.code.len()).expect("instruction index fits");
        let edge_span = Span {
            start: body.span.end.saturating_sub(1),
            end: body.span.end,
        };
        let (context, falls_through) = self.finish_loop(
            LoopEntryStates {
                repeat: &repeat_entry,
                exit: &exit_entry,
            },
            backedge_reachable,
            header,
            emitted_exit,
            has_zero_iteration,
            edge_span,
        )?;
        let exit = if constant_condition == Some(false) {
            self.code.truncate(body_target as usize);
            self.instruction_spans
                .retain(|instruction, _| *instruction < body_target as usize);
            body_target
        } else {
            emitted_exit
        };
        self.code[branch] = Instruction::BranchBool {
            condition: condition_value.register,
            false_target: if has_zero_iteration {
                exit
            } else {
                body_target
            },
            true_target: if constant_condition == Some(false) {
                exit
            } else {
                body_target
            },
        };
        self.mark_instruction(branch, condition.span);
        self.register_loop_mir_region(
            mir_header,
            mir_header,
            condition_operations,
            body_operations,
            body_region,
            body_falls_through,
            backedge_reachable,
            &context.continue_mir_blocks,
            &context.break_mir_blocks,
            has_zero_iteration,
        );
        let effects = self.union_effects([condition_value.effects, body_hir.effects]);
        Ok(CompiledLoop {
            register,
            falls_through,
            hir: self.hir(
                HirExprKind::While {
                    condition: Box::new(condition_value.hir),
                    body: Box::new(body_hir),
                },
                None,
                &ValueType::Unit,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_infinite_loop(
        &mut self,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledLoop, Diagnostic> {
        let loop_reachable = self.control_reachable;
        let register = self.allocate(ValueType::Unit)?;
        let header = u32::try_from(self.code.len()).expect("instruction index fits");
        let entry = self.push_loop();
        let mir_header = self.next_mir_block();
        self.mir_blocks.extend((0..2).map(|_| MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        }));
        let body_region_capture = self.begin_nested_mir_region();
        let body_operations_start = self.mir.len();
        let (body_hir, body_value, _) = self.compile_block_value(body)?;
        let body_region = self.finish_nested_mir_region(body_region_capture);
        Self::validate_loop_body_type(body, &body_value)?;
        let mut body_operations = self.mir.split_off(body_operations_start);
        let body_falls_through = body_value.value_type != ValueType::Never;
        let body_runtime_falls_through = self.runtime_falls_through(&body_value);
        let backedge_reachable = body_runtime_falls_through && loop_reachable;
        if backedge_reachable {
            self.code.push(Instruction::Jump { target: header });
            self.mark_last_instruction(body.span);
        } else if body_falls_through && loop_reachable {
            let ownership_at_entry = entry.ownership.keys().copied().collect();
            let reason = self.terminate_source_dead_path(&ownership_at_entry, body.span)?;
            body_operations.push(MirOperation::Constant {
                destination: u32::from(reason),
            });
        }
        let exit = u32::try_from(self.code.len()).expect("instruction index fits");
        let edge_span = Span {
            start: body.span.end.saturating_sub(1),
            end: body.span.end,
        };
        let (context, falls_through) = self.finish_loop(
            LoopEntryStates {
                repeat: &entry,
                exit: &entry,
            },
            backedge_reachable,
            header,
            exit,
            false,
            edge_span,
        )?;
        self.register_loop_mir_region(
            mir_header,
            mir_header,
            Vec::new(),
            body_operations,
            body_region,
            body_falls_through,
            backedge_reachable,
            &context.continue_mir_blocks,
            &context.break_mir_blocks,
            false,
        );
        let effects = body_hir.effects;
        Ok(CompiledLoop {
            register,
            falls_through,
            hir: self.hir(
                HirExprKind::Loop {
                    body: Box::new(body_hir),
                },
                None,
                &ValueType::Unit,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_terminal_for(
        &mut self,
        register: Register,
        binding: &LoweredLoopBinding,
        source: HirForSource,
        source_effects: EffectSetId,
        yielded_type: &ValueType,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledLoop, Diagnostic> {
        let (hir_binding, body_hir, body_effects) =
            self.compile_static_for_body(binding, yielded_type, body)?;
        let effects = self.union_effects([source_effects, body_effects]);
        Ok(CompiledLoop {
            register,
            falls_through: false,
            hir: self.hir(
                HirExprKind::For {
                    binding: hir_binding,
                    source,
                    body: Box::new(body_hir),
                },
                None,
                &ValueType::Unit,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_for(
        &mut self,
        binding: &LoweredLoopBinding,
        source: &LoweredForSource,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledLoop, Diagnostic> {
        let loop_reachable = self.control_reachable;
        let register = self.allocate(ValueType::Unit)?;
        let (
            source_hir,
            source_effects,
            yielded,
            yielded_type,
            length,
            cursor,
            range_inclusive,
            string_iterable,
        ) = match source {
            LoweredForSource::Iterable(value) => {
                let value_span = value.span;
                let value = self.compile_expression(value)?;
                if value.value_type == ValueType::Never {
                    let yielded_type = if binding.tuple {
                        ValueType::Tuple(vec![ValueType::Never; binding.elements.len()])
                    } else {
                        ValueType::Never
                    };
                    return self.finish_terminal_for(
                        register,
                        binding,
                        HirForSource::Iterable(Box::new(value.hir)),
                        value.effects,
                        &yielded_type,
                        body,
                        span,
                    );
                }
                if value.value_type == ValueType::Range {
                    let start = self.allocate(ValueType::Int)?;
                    self.code.push(Instruction::RangeStart {
                        destination: start,
                        range: value.register,
                    });
                    self.mark_last_instruction(value_span);
                    self.mir.push(MirOperation::FieldGet {
                        destination: u32::from(start),
                        record: u32::from(value.register),
                    });
                    let end = self.allocate(ValueType::Int)?;
                    self.code.push(Instruction::RangeEnd {
                        destination: end,
                        range: value.register,
                    });
                    self.mark_last_instruction(value_span);
                    self.mir.push(MirOperation::FieldGet {
                        destination: u32::from(end),
                        record: u32::from(value.register),
                    });
                    let inclusive = self.allocate(ValueType::Bool)?;
                    self.code.push(Instruction::RangeInclusive {
                        destination: inclusive,
                        range: value.register,
                    });
                    self.mark_last_instruction(value_span);
                    self.mir.push(MirOperation::FieldGet {
                        destination: u32::from(inclusive),
                        record: u32::from(value.register),
                    });
                    let cursor = self.allocate(ValueType::Int)?;
                    self.code.push(Instruction::Move {
                        destination: cursor,
                        source: start,
                    });
                    self.mark_last_instruction(value_span);
                    self.mir.push(MirOperation::Move {
                        destination: u32::from(cursor),
                        source: u32::from(start),
                    });
                    (
                        HirForSource::Iterable(Box::new(value.hir)),
                        value.effects,
                        cursor,
                        ValueType::Int,
                        end,
                        cursor,
                        Some(inclusive),
                        false,
                    )
                } else {
                    let yielded_type = match &value.value_type {
                        ValueType::List(element) => element.as_ref().clone(),
                        ValueType::Bytes => ValueType::Int,
                        ValueType::String => ValueType::String,
                        ValueType::Map(key, value) => {
                            ValueType::Tuple(vec![key.as_ref().clone(), value.as_ref().clone()])
                        }
                        found => {
                            return Err(Diagnostic::new(
                                "E3007",
                                format!(
                                    "for iterable must be String, List<T>, Bytes, or Map<K, V>, found {found}"
                                ),
                                value_span,
                            ));
                        }
                    };
                    let iterable_type = value.value_type.clone();
                    let iterable = self.allocate(iterable_type.clone())?;
                    self.code.push(Instruction::Move {
                        destination: iterable,
                        source: value.register,
                    });
                    self.mark_last_instruction(span);
                    self.mir.push(MirOperation::Move {
                        destination: u32::from(iterable),
                        source: u32::from(value.register),
                    });
                    let length = self.allocate(ValueType::Int)?;
                    self.code.push(Instruction::Length {
                        destination: length,
                        collection: iterable,
                    });
                    self.mark_last_instruction(span);
                    self.mir.push(MirOperation::Length {
                        destination: u32::from(length),
                        collection: u32::from(iterable),
                    });
                    let zero = self.compile_expression(&LoweredExpr {
                        kind: LoweredExprKind::Int(0),
                        span,
                    })?;
                    let cursor = self.allocate(ValueType::Int)?;
                    self.code.push(Instruction::Move {
                        destination: cursor,
                        source: zero.register,
                    });
                    self.mark_last_instruction(span);
                    self.mir.push(MirOperation::Move {
                        destination: u32::from(cursor),
                        source: u32::from(zero.register),
                    });
                    (
                        HirForSource::Iterable(Box::new(value.hir)),
                        value.effects,
                        iterable,
                        yielded_type,
                        length,
                        cursor,
                        None,
                        matches!(iterable_type, ValueType::String),
                    )
                }
            }
        };
        let entry = self.push_loop();
        let header = u32::try_from(self.code.len()).expect("instruction index fits");
        let condition_operations_start = self.mir.len();
        let less = self.allocate(ValueType::Bool)?;
        self.code.push(Instruction::Compare {
            destination: less,
            left: cursor,
            right: length,
            operation: CompareOp::Less,
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::Binary {
            destination: u32::from(less),
        });
        let condition = if let Some(inclusive) = range_inclusive {
            let equal = self.allocate(ValueType::Bool)?;
            self.code.push(Instruction::Compare {
                destination: equal,
                left: cursor,
                right: length,
                operation: CompareOp::Equal,
            });
            self.mark_last_instruction(span);
            self.mir.push(MirOperation::Binary {
                destination: u32::from(equal),
            });
            let inclusive_equal = self.allocate(ValueType::Bool)?;
            self.code.push(Instruction::BoolBinary {
                destination: inclusive_equal,
                left: inclusive,
                right: equal,
                operation: allen_bytecode::BoolBinaryOp::And,
            });
            self.mark_last_instruction(span);
            self.mir.push(MirOperation::Binary {
                destination: u32::from(inclusive_equal),
            });
            let condition = self.allocate(ValueType::Bool)?;
            self.code.push(Instruction::BoolBinary {
                destination: condition,
                left: less,
                right: inclusive_equal,
                operation: allen_bytecode::BoolBinaryOp::Or,
            });
            self.mark_last_instruction(span);
            self.mir.push(MirOperation::Binary {
                destination: u32::from(condition),
            });
            condition
        } else {
            less
        };
        let condition_operations = self.mir.split_off(condition_operations_start);
        let branch = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        let iteration_target = u32::try_from(self.code.len()).expect("instruction index fits");
        let body_operations_start = self.mir.len();
        let string_value = string_iterable
            .then(|| self.allocate(ValueType::String))
            .transpose()?;
        let mut string_none_jump = None;
        if let Some(string_value) = string_value {
            let option = self.allocate(ValueType::Option(Box::new(ValueType::String)))?;
            self.code.push(Instruction::StringCall {
                destination: option,
                operation: StringOperation::Get,
                arguments: vec![yielded, cursor],
            });
            self.mark_last_instruction(binding.span);
            self.mir.push(MirOperation::StringOperation {
                destination: u32::from(option),
                operation: StringOperation::Get,
                arguments: vec![u32::from(yielded), u32::from(cursor)],
            });
            let switch = self.code.len();
            self.code.push(Instruction::Jump { target: 0 });
            let none_target = u32::try_from(self.code.len()).expect("instruction index fits");
            string_none_jump = Some(self.code.len());
            self.code.push(Instruction::Jump { target: 0 });
            let some_target = u32::try_from(self.code.len()).expect("instruction index fits");
            self.code[switch] = Instruction::SwitchEnum {
                source: option,
                arms: vec![
                    allen_bytecode::EnumSwitchArm {
                        variant: 0,
                        target: none_target,
                        bindings: Vec::new(),
                    },
                    allen_bytecode::EnumSwitchArm {
                        variant: 1,
                        target: some_target,
                        bindings: vec![string_value],
                    },
                ],
            };
            self.mark_instruction(switch, binding.span);
            self.mark_last_instruction(binding.span);
            self.mir.push(MirOperation::Binary {
                destination: u32::from(string_value),
            });
        }
        let mir_header = self.next_mir_block();
        self.mir_blocks.extend((0..2).map(|_| MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        }));
        let yielded = if let Some(string_value) = string_value {
            string_value
        } else if range_inclusive.is_some() {
            yielded
        } else {
            let value = self.allocate(yielded_type.clone())?;
            if matches!(self.registers[yielded as usize], ValueType::Map(_, _)) {
                self.code.push(Instruction::MapEntryAt {
                    destination: value,
                    map: yielded,
                    index: cursor,
                });
                self.mir.push(MirOperation::MapEntryAt {
                    destination: u32::from(value),
                    map: u32::from(yielded),
                    index: u32::from(cursor),
                });
            } else {
                self.code.push(Instruction::IndexGet {
                    destination: value,
                    collection: yielded,
                    index: cursor,
                });
                self.mir.push(MirOperation::Binary {
                    destination: u32::from(value),
                });
            }
            self.mark_last_instruction(binding.span);
            value
        };
        let hir_binding = self.install_loop_binding(binding, yielded, &yielded_type)?;
        let body_region_capture = self.begin_nested_mir_region();
        let (body_hir, body_value, _) = self.compile_block_value(body)?;
        let body_region = self.finish_nested_mir_region(body_region_capture);
        Self::validate_loop_body_type(body, &body_value)?;
        let body_falls_through = body_value.value_type != ValueType::Never;
        let body_runtime_falls_through = self.runtime_falls_through(&body_value);
        let step_target = u32::try_from(self.code.len()).expect("instruction index fits");
        let has_continue = self
            .loops
            .last()
            .is_some_and(|loop_| !loop_.continue_jumps.is_empty());
        let backedge_reachable = loop_reachable && (body_runtime_falls_through || has_continue);
        let step_operations_start = self.mir.len();
        let mut range_endpoint_branch = None;
        if backedge_reachable {
            if range_inclusive.is_some() {
                let at_end = self.allocate(ValueType::Bool)?;
                self.code.push(Instruction::Compare {
                    destination: at_end,
                    left: cursor,
                    right: length,
                    operation: CompareOp::Equal,
                });
                self.mark_last_instruction(span);
                self.mir.push(MirOperation::Binary {
                    destination: u32::from(at_end),
                });
                let branch = self.code.len();
                self.code.push(Instruction::Jump { target: 0 });
                let increment = u32::try_from(self.code.len()).expect("instruction index fits");
                range_endpoint_branch = Some((branch, at_end, increment));
            }
            let one = self.compile_expression(&LoweredExpr {
                kind: LoweredExprKind::Int(1),
                span,
            })?;
            self.code.push(Instruction::IntBinary {
                destination: cursor,
                left: cursor,
                right: one.register,
                operation: NumericBinaryOp::Add,
            });
            self.mark_last_instruction(span);
            self.mir.push(MirOperation::Binary {
                destination: u32::from(cursor),
            });
            self.code.push(Instruction::Jump { target: header });
            self.mark_last_instruction(span);
        }
        let step_operations = self.mir.split_off(step_operations_start);
        let mut body_operations = self.mir.split_off(body_operations_start);
        if body_falls_through && !backedge_reachable && loop_reachable {
            let ownership_at_entry = entry.ownership.keys().copied().collect();
            let reason = self.terminate_source_dead_path(&ownership_at_entry, body.span)?;
            body_operations.push(MirOperation::Constant {
                destination: u32::from(reason),
            });
        }
        let exit = u32::try_from(self.code.len()).expect("instruction index fits");
        if let Some((branch, at_end, increment)) = range_endpoint_branch {
            self.code[branch] = Instruction::BranchBool {
                condition: at_end,
                false_target: increment,
                true_target: exit,
            };
            self.mark_instruction(branch, span);
        }
        if let Some(jump) = string_none_jump {
            self.code[jump] = Instruction::Jump { target: exit };
        }
        let edge_span = Span {
            start: body.span.end.saturating_sub(1),
            end: body.span.end,
        };
        let (context, falls_through) = self.finish_loop(
            LoopEntryStates {
                repeat: &entry,
                exit: &entry,
            },
            backedge_reachable,
            step_target,
            exit,
            true,
            edge_span,
        )?;
        self.code[branch] = Instruction::BranchBool {
            condition,
            false_target: exit,
            true_target: iteration_target,
        };
        self.mark_instruction(branch, span);
        let mir_step = if backedge_reachable {
            let step = self.next_mir_block();
            self.mir_blocks.push(MirBlock {
                operations: step_operations,
                terminator: MirTerminator::Goto { target: mir_header },
            });
            step
        } else {
            mir_header
        };
        self.register_loop_mir_region(
            mir_header,
            mir_step,
            condition_operations,
            body_operations,
            body_region,
            body_falls_through,
            backedge_reachable,
            &context.continue_mir_blocks,
            &context.break_mir_blocks,
            true,
        );
        let effects = self.union_effects([source_effects, body_hir.effects]);
        Ok(CompiledLoop {
            register,
            falls_through,
            hir: self.hir(
                HirExprKind::For {
                    binding: hir_binding,
                    source: source_hir,
                    body: Box::new(body_hir),
                },
                None,
                &ValueType::Unit,
                effects,
                span,
            ),
        })
    }

    pub(super) fn collection_value_is_valid(
        value_type: &ValueType,
        collection: &str,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if contains_affine(value_type) || contains_stored_sub_agent(value_type) {
            return Err(Diagnostic::new(
                "E3011",
                format!("future, task, or SubAgent values cannot be stored in {collection}"),
                span,
            ));
        }
        if contains_workspace(value_type) {
            return Err(Diagnostic::new(
                "E3011",
                format!("Workspace cannot be stored in {collection}"),
                span,
            ));
        }
        Ok(())
    }

    pub(super) fn annotation_type(
        &self,
        annotation: &LoweredType,
    ) -> Result<ValueType, Diagnostic> {
        let semantic = semantic_type(
            annotation,
            &BTreeSet::new(),
            &self.info.module,
            &self.global.bundle.modules,
            &self.global.bundle.types,
        )?;
        concrete_type(&semantic, &BTreeMap::new(), &self.global.effect_sets).map_err(|_| {
            Diagnostic::new(
                "E3007",
                "local binding type must be concrete",
                annotation.span(),
            )
        })
    }

    pub(super) fn compile_list(
        &mut self,
        expression: &LoweredExpr,
        elements: &[LoweredExpr],
        expected: Option<&ValueType>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let mut values = Vec::with_capacity(elements.len());
        let element_type = match expected {
            Some(ValueType::List(element_type)) => (**element_type).clone(),
            Some(_) => {
                return Err(Diagnostic::new(
                    "E3010",
                    "list literal requires a List expected type",
                    expression.span,
                ));
            }
            None => {
                let Some(first) = elements.first() else {
                    return Err(Diagnostic::new(
                        "E3010",
                        "empty List requires an expected List type",
                        expression.span,
                    ));
                };
                let first = self.compile_expression(first)?;
                let element_type = first.value_type.clone();
                values.push(first);
                element_type
            }
        };
        Self::collection_value_is_valid(&element_type, "List", expression.span)?;
        for element in elements.iter().skip(usize::from(expected.is_none())) {
            values.push(self.compile_expected(element, &element_type, "list element")?);
        }
        let value_type = ValueType::List(Box::new(element_type));
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::ListNew {
            destination: register,
            elements: values.iter().map(|value| value.register).collect(),
        });
        self.mir.push(MirOperation::List {
            destination: u32::from(register),
            items: values
                .iter()
                .map(|value| MirListItem::Element(u32::from(value.register)))
                .collect(),
        });
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::List(values.into_iter().map(|value| value.hir).collect()),
                None,
                &value_type,
                effects,
                expression.span,
            ),
        })
    }

    pub(super) fn compile_map(
        &mut self,
        expression: &LoweredExpr,
        entries: &[(LoweredExpr, LoweredExpr)],
        expected: Option<&ValueType>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let mut values = Vec::with_capacity(entries.len());
        let (key_type, map_value_type) = match expected {
            Some(ValueType::Map(key, value)) => ((**key).clone(), (**value).clone()),
            Some(_) => {
                return Err(Diagnostic::new(
                    "E3010",
                    "map literal requires a Map expected type",
                    expression.span,
                ));
            }
            None => {
                let Some((key, value)) = entries.first() else {
                    return Err(Diagnostic::new(
                        "E3010",
                        "empty Map requires an expected Map type",
                        expression.span,
                    ));
                };
                let key = self.compile_expression(key)?;
                let value = self.compile_expression(value)?;
                let types = (key.value_type.clone(), value.value_type.clone());
                values.push((key, value));
                types
            }
        };
        if !key_type.is_map_key() {
            return Err(Diagnostic::new(
                "E3011",
                format!("Map key type {key_type} is not allowed"),
                expression.span,
            ));
        }
        Self::collection_value_is_valid(&key_type, "Map", expression.span)?;
        Self::collection_value_is_valid(&map_value_type, "Map", expression.span)?;
        let mut seen = BTreeSet::new();
        if expected.is_none() {
            if let Some((key, _)) = entries.first() {
                if let Some(key) = literal_map_key(key) {
                    seen.insert(key);
                }
            }
        }
        for (key, value) in entries.iter().skip(usize::from(expected.is_none())) {
            if let Some(key) = literal_map_key(key) {
                if !seen.insert(key) {
                    return Err(Diagnostic::new("E3011", "duplicate Map key", value.span));
                }
            }
            values.push((
                self.compile_expected(key, &key_type, "map key")?,
                self.compile_expected(value, &map_value_type, "map value")?,
            ));
        }
        let value_type = ValueType::Map(Box::new(key_type), Box::new(map_value_type));
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::MapNew {
            destination: register,
            entries: values
                .iter()
                .map(|(key, value)| (key.register, value.register))
                .collect(),
        });
        self.mir.push(MirOperation::Map {
            destination: u32::from(register),
            items: values
                .iter()
                .map(|(key, value)| MirMapItem::Entry {
                    key: u32::from(key.register),
                    value: u32::from(value.register),
                })
                .collect(),
        });
        let effects = self.union_effects(
            values
                .iter()
                .flat_map(|(key, value)| [key.effects, value.effects]),
        );
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Map(
                    values
                        .into_iter()
                        .map(|(key, value)| (key.hir, value.hir))
                        .collect(),
                ),
                None,
                &value_type,
                effects,
                expression.span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_list_with_spread(
        &mut self,
        expression: &LoweredExpr,
        items: &[super::LoweredListItem],
        expected: Option<&ValueType>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let mut element_type = match expected {
            Some(ValueType::List(element)) => Some(element.as_ref().clone()),
            Some(_) => {
                return Err(Diagnostic::new(
                    "E3010",
                    "list literal requires a List expected type",
                    expression.span,
                ));
            }
            None => None,
        };
        let mut values = Vec::with_capacity(items.len());
        let mut backend_items = Vec::with_capacity(items.len());
        for item in items {
            let value = if item.spread {
                let expected_spread = element_type
                    .as_ref()
                    .map(|element| ValueType::List(Box::new(element.clone())));
                let value = match expected_spread.as_ref() {
                    Some(expected) => {
                        self.compile_expected(&item.value, expected, "list spread")?
                    }
                    None => self.compile_expression(&item.value)?,
                };
                let ValueType::List(spread_element) = &value.value_type else {
                    return Err(Diagnostic::new(
                        "E3010",
                        "list spread requires a List value",
                        item.span,
                    ));
                };
                if let Some(element) = &element_type {
                    if element != spread_element.as_ref() {
                        return Err(Diagnostic::new(
                            "E3010",
                            format!(
                                "list spread expects List<{element}>, found {}",
                                value.value_type
                            ),
                            item.span,
                        ));
                    }
                } else {
                    element_type = Some(spread_element.as_ref().clone());
                }
                backend_items.push(ListLiteralItem::Spread(value.register));
                value
            } else {
                let value = match element_type.as_ref() {
                    Some(element) => self.compile_expected(&item.value, element, "list element")?,
                    None => self.compile_expression(&item.value)?,
                };
                if let Some(element) = &element_type {
                    if &value.value_type != element {
                        return Err(Diagnostic::new(
                            "E3010",
                            format!("list element expects {element}, found {}", value.value_type),
                            item.span,
                        ));
                    }
                } else {
                    element_type = Some(value.value_type.clone());
                }
                backend_items.push(ListLiteralItem::Element(value.register));
                value
            };
            values.push(value);
        }
        let element_type = element_type.ok_or_else(|| {
            Diagnostic::new(
                "E3010",
                "empty List requires an expected List type",
                expression.span,
            )
        })?;
        Self::collection_value_is_valid(&element_type, "List", expression.span)?;
        let value_type = ValueType::List(Box::new(element_type));
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::ListLiteralBuild {
            destination: register,
            items: backend_items.clone(),
        });
        self.mir.push(MirOperation::List {
            destination: u32::from(register),
            items: backend_items
                .iter()
                .map(|item| match item {
                    ListLiteralItem::Element(register) => {
                        MirListItem::Element(u32::from(*register))
                    }
                    ListLiteralItem::Spread(register) => MirListItem::Spread(u32::from(*register)),
                })
                .collect(),
        });
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::ListWithSpread(
                    items
                        .iter()
                        .zip(values)
                        .map(|(item, value)| {
                            if item.spread {
                                HirListItem::Spread(value.hir)
                            } else {
                                HirListItem::Element(value.hir)
                            }
                        })
                        .collect(),
                ),
                None,
                &value_type,
                effects,
                expression.span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_map_with_spread(
        &mut self,
        expression: &LoweredExpr,
        items: &[super::LoweredMapItem],
        expected: Option<&ValueType>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let (mut key_type, mut value_type) = match expected {
            Some(ValueType::Map(key, value)) => {
                (Some(key.as_ref().clone()), Some(value.as_ref().clone()))
            }
            Some(_) => {
                return Err(Diagnostic::new(
                    "E3010",
                    "map literal requires a Map expected type",
                    expression.span,
                ));
            }
            None => (None, None),
        };
        let mut values: Vec<(CompiledExpr, Option<CompiledExpr>)> = Vec::with_capacity(items.len());
        let mut backend_items = Vec::with_capacity(items.len());
        let mut seen = BTreeSet::new();
        for item in items {
            match item {
                super::LoweredMapItem::Entry { key, value, span } => {
                    let compiled_key = match key_type.as_ref() {
                        Some(expected) => self.compile_expected(key, expected, "map key")?,
                        None => self.compile_expression(key)?,
                    };
                    let compiled_value = match value_type.as_ref() {
                        Some(expected) => self.compile_expected(value, expected, "map value")?,
                        None => self.compile_expression(value)?,
                    };
                    if key_type.is_none() {
                        key_type = Some(compiled_key.value_type.clone());
                    }
                    if value_type.is_none() {
                        value_type = Some(compiled_value.value_type.clone());
                    }
                    if let Some(key) = literal_map_key(key) {
                        if !seen.insert(key) {
                            return Err(Diagnostic::new("E3011", "duplicate Map key", *span));
                        }
                    }
                    backend_items.push(MapLiteralItem::Entry {
                        key: compiled_key.register,
                        value: compiled_value.register,
                    });
                    values.push((compiled_key, Some(compiled_value)));
                }
                super::LoweredMapItem::Spread { value, span } => {
                    let expected_spread = match (key_type.as_ref(), value_type.as_ref()) {
                        (Some(key), Some(value)) => Some(ValueType::Map(
                            Box::new(key.clone()),
                            Box::new(value.clone()),
                        )),
                        _ => None,
                    };
                    let compiled = match expected_spread.as_ref() {
                        Some(expected) => self.compile_expected(value, expected, "map spread")?,
                        None => self.compile_expression(value)?,
                    };
                    let ValueType::Map(spread_key, spread_value) = &compiled.value_type else {
                        return Err(Diagnostic::new(
                            "E3010",
                            "map spread requires a Map value",
                            *span,
                        ));
                    };
                    if let Some(key) = &key_type {
                        if key != spread_key.as_ref() {
                            return Err(Diagnostic::new(
                                "E3010",
                                "map spread key type mismatch",
                                *span,
                            ));
                        }
                    } else {
                        key_type = Some(spread_key.as_ref().clone());
                    }
                    if let Some(value_type) = &value_type {
                        if value_type != spread_value.as_ref() {
                            return Err(Diagnostic::new(
                                "E3010",
                                "map spread value type mismatch",
                                *span,
                            ));
                        }
                    } else {
                        value_type = Some(spread_value.as_ref().clone());
                    }
                    backend_items.push(MapLiteralItem::Spread(compiled.register));
                    values.push((compiled, None));
                }
            }
        }
        let key_type = key_type.ok_or_else(|| {
            Diagnostic::new(
                "E3010",
                "empty Map requires an expected Map type",
                expression.span,
            )
        })?;
        let value_type = value_type.ok_or_else(|| {
            Diagnostic::new(
                "E3010",
                "empty Map requires an expected Map type",
                expression.span,
            )
        })?;
        if !key_type.is_map_key() {
            return Err(Diagnostic::new(
                "E3011",
                format!("Map key type {key_type} is not allowed"),
                expression.span,
            ));
        }
        Self::collection_value_is_valid(&key_type, "Map", expression.span)?;
        Self::collection_value_is_valid(&value_type, "Map", expression.span)?;
        let result_type = ValueType::Map(Box::new(key_type), Box::new(value_type));
        let register = self.allocate(result_type.clone())?;
        self.code.push(Instruction::MapLiteralBuild {
            destination: register,
            items: backend_items.clone(),
        });
        self.mir.push(MirOperation::Map {
            destination: u32::from(register),
            items: backend_items
                .iter()
                .map(|item| match item {
                    MapLiteralItem::Entry { key, value } => MirMapItem::Entry {
                        key: u32::from(*key),
                        value: u32::from(*value),
                    },
                    MapLiteralItem::Spread(register) => MirMapItem::Spread(u32::from(*register)),
                })
                .collect(),
        });
        let effects = self.union_effects(values.iter().flat_map(|(key, value)| {
            value.as_ref().map_or_else(
                || [key.effects, self.empty_effects()],
                |value| [key.effects, value.effects],
            )
        }));
        let hir_values = items
            .iter()
            .zip(values)
            .map(|(item, (key, value))| match item {
                super::LoweredMapItem::Entry { .. } => HirMapItem::Entry {
                    key: key.hir,
                    value: value.expect("map entry has a value").hir,
                },
                super::LoweredMapItem::Spread { .. } => HirMapItem::Spread(key.hir),
            })
            .collect();
        Ok(CompiledExpr {
            register,
            value_type: result_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::MapWithSpread(hir_values),
                None,
                &result_type,
                effects,
                expression.span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_collection_builtin(
        &mut self,
        builtin: CollectionBuiltin,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let (name, arity) = match builtin {
            CollectionBuiltin::Length => ("length", 1),
            CollectionBuiltin::ListAppend => ("list.append", 2),
            CollectionBuiltin::ListSet => ("list.set", 3),
            CollectionBuiltin::Operation(CollectionOperation::Zip) => ("list.zip", 0),
            CollectionBuiltin::Operation(CollectionOperation::ListMin) => ("list.min", 1),
            CollectionBuiltin::Operation(CollectionOperation::ListMax) => ("list.max", 1),
            CollectionBuiltin::Operation(
                CollectionOperation::ListSumInt | CollectionOperation::ListSumFloat,
            ) => ("list.sum", 1),
            CollectionBuiltin::ListFold => ("list.fold", 3),
            CollectionBuiltin::ListCombinator(operation) => (
                match operation {
                    ListCombinator::Map => "list.map",
                    ListCombinator::Filter => "list.filter",
                    ListCombinator::FlatMap => "list.flat_map",
                    ListCombinator::FilterMap => "list.filter_map",
                    ListCombinator::Find => "list.find",
                    ListCombinator::Any => "list.any",
                    ListCombinator::All => "list.all",
                    ListCombinator::Partition => "list.partition",
                    ListCombinator::Scan => "list.scan",
                },
                if operation == ListCombinator::Scan {
                    3
                } else {
                    2
                },
            ),
            CollectionBuiltin::Safe(SafeCollectionOperation::ListGet) => ("list.get", 2),
            CollectionBuiltin::Safe(SafeCollectionOperation::ListTrySet) => ("list.try_set", 3),
            CollectionBuiltin::Safe(SafeCollectionOperation::BytesGet) => ("bytes.get", 2),
            CollectionBuiltin::Safe(SafeCollectionOperation::MapGet) => ("map.get", 2),
            CollectionBuiltin::Safe(SafeCollectionOperation::MapInsert) => ("map.insert", 3),
            CollectionBuiltin::Safe(SafeCollectionOperation::MapRemove) => ("map.remove", 2),
            CollectionBuiltin::Safe(SafeCollectionOperation::MapKeys) => ("map.keys", 1),
            CollectionBuiltin::CheckedInt(CheckedIntOperation::Negate) => ("int.checked_neg", 1),
            CollectionBuiltin::CheckedInt(_) => ("checked integer operation", 2),
            CollectionBuiltin::Sequence(operation) => (
                match operation {
                    SequenceBuiltin::FromList => "seq.from_list",
                    SequenceBuiltin::Map => "seq.map",
                    SequenceBuiltin::Filter => "seq.filter",
                    SequenceBuiltin::Take => "seq.take",
                    SequenceBuiltin::Find => "seq.find",
                    SequenceBuiltin::Any => "seq.any",
                    SequenceBuiltin::All => "seq.all",
                    SequenceBuiltin::Fold => "seq.fold",
                    SequenceBuiltin::ToList => "seq.to_list",
                },
                match operation {
                    SequenceBuiltin::FromList | SequenceBuiltin::ToList => 1,
                    SequenceBuiltin::Map
                    | SequenceBuiltin::Filter
                    | SequenceBuiltin::Take
                    | SequenceBuiltin::Find
                    | SequenceBuiltin::Any
                    | SequenceBuiltin::All => 2,
                    SequenceBuiltin::Fold => 3,
                },
            ),
        };
        if builtin == CollectionBuiltin::Operation(CollectionOperation::Zip) {
            if !(2..=8).contains(&arguments.len()) {
                return Err(Diagnostic::new(
                    "E3011",
                    "list.zip requires from 2 to 8 arguments",
                    span,
                ));
            }
        } else if arguments.len() != arity {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "{name} requires exactly {arity} argument{}",
                    if arity == 1 { "" } else { "s" }
                ),
                span,
            ));
        }
        match builtin {
            CollectionBuiltin::Length => {
                let values = self.compile_expression(&arguments[0])?;
                self.compile_length_builtin(values, arguments[0].span, span)
            }
            CollectionBuiltin::ListAppend | CollectionBuiltin::ListSet => {
                let values = self.compile_expression(&arguments[0])?;
                self.compile_list_builtin(builtin, values, arguments, span, name)
            }
            CollectionBuiltin::Operation(operation) => {
                self.compile_collection_operation(operation, arguments, span, name)
            }
            CollectionBuiltin::ListFold => self.compile_list_fold(arguments, span),
            CollectionBuiltin::ListCombinator(operation) => {
                self.compile_list_combinator(operation, arguments, span)
            }
            CollectionBuiltin::Safe(operation) => {
                self.compile_safe_collection_builtin(operation, arguments, span, name)
            }
            CollectionBuiltin::CheckedInt(operation) => {
                self.compile_checked_int_builtin(operation, arguments, span, name)
            }
            CollectionBuiltin::Sequence(operation) => {
                self.compile_sequence_builtin(operation, arguments, span)
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_safe_collection_builtin(
        &mut self,
        operation: SafeCollectionOperation,
        arguments: &[LoweredExpr],
        span: Span,
        name: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let values = arguments
            .iter()
            .map(|argument| self.compile_expression(argument))
            .collect::<Result<Vec<_>, _>>()?;
        let result = match (operation, values.as_slice()) {
            (SafeCollectionOperation::ListGet, [list, index])
                if matches!(index.value_type, ValueType::Int) =>
            {
                let ValueType::List(item) = &list.value_type else {
                    return Err(Diagnostic::new("E3011", "list.get requires List<T>", span));
                };
                ValueType::Option(item.clone())
            }
            (SafeCollectionOperation::ListTrySet, [list, index, replacement])
                if matches!(index.value_type, ValueType::Int) =>
            {
                let ValueType::List(item) = &list.value_type else {
                    return Err(Diagnostic::new(
                        "E3011",
                        "list.try_set requires List<T>",
                        span,
                    ));
                };
                if item.as_ref() != &replacement.value_type {
                    return Err(Diagnostic::new(
                        "E3011",
                        "list.try_set replacement must match the list element type",
                        span,
                    ));
                }
                ValueType::Option(Box::new(list.value_type.clone()))
            }
            (SafeCollectionOperation::BytesGet, [bytes, index])
                if bytes.value_type == ValueType::Bytes && index.value_type == ValueType::Int =>
            {
                ValueType::Option(Box::new(ValueType::Int))
            }
            (SafeCollectionOperation::MapGet, [map, key]) => {
                let ValueType::Map(expected_key, value) = &map.value_type else {
                    return Err(Diagnostic::new("E3011", "map.get requires Map<K, V>", span));
                };
                if expected_key.as_ref() != &key.value_type {
                    return Err(Diagnostic::new(
                        "E3011",
                        "map.get key must match the map key type",
                        span,
                    ));
                }
                ValueType::Option(value.clone())
            }
            (SafeCollectionOperation::MapInsert, [map, key, replacement]) => {
                let ValueType::Map(expected_key, value) = &map.value_type else {
                    return Err(Diagnostic::new(
                        "E3011",
                        "map.insert requires Map<K, V>",
                        span,
                    ));
                };
                if expected_key.as_ref() != &key.value_type
                    || value.as_ref() != &replacement.value_type
                {
                    return Err(Diagnostic::new(
                        "E3011",
                        "map.insert key and value must match the map types",
                        span,
                    ));
                }
                ValueType::Record(vec![
                    RecordField {
                        name: "previous".to_owned(),
                        value_type: ValueType::Option(value.clone()),
                    },
                    RecordField {
                        name: "values".to_owned(),
                        value_type: map.value_type.clone(),
                    },
                ])
            }
            (SafeCollectionOperation::MapRemove, [map, key]) => {
                let ValueType::Map(expected_key, value) = &map.value_type else {
                    return Err(Diagnostic::new(
                        "E3011",
                        "map.remove requires Map<K, V>",
                        span,
                    ));
                };
                if expected_key.as_ref() != &key.value_type {
                    return Err(Diagnostic::new(
                        "E3011",
                        "map.remove key must match the map key type",
                        span,
                    ));
                }
                ValueType::Record(vec![
                    RecordField {
                        name: "removed".to_owned(),
                        value_type: ValueType::Option(value.clone()),
                    },
                    RecordField {
                        name: "values".to_owned(),
                        value_type: map.value_type.clone(),
                    },
                ])
            }
            (SafeCollectionOperation::MapKeys, [map]) => {
                let ValueType::Map(key, _) = &map.value_type else {
                    return Err(Diagnostic::new(
                        "E3011",
                        "map.keys requires Map<K, V>",
                        span,
                    ));
                };
                ValueType::List(key.clone())
            }
            _ => {
                return Err(Diagnostic::new(
                    "E3011",
                    format!("{name} arguments do not match its exact signature"),
                    span,
                ));
            }
        };
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        let register = self.allocate(result.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::SafeCollectionCall {
            destination: register,
            operation,
            arguments: argument_registers.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::SafeCollectionOperation {
            destination: u32::from(register),
            operation,
            arguments: argument_registers.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::SafeCollectionOperation {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_checked_int_builtin(
        &mut self,
        operation: CheckedIntOperation,
        arguments: &[LoweredExpr],
        span: Span,
        name: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let values = arguments
            .iter()
            .map(|argument| self.compile_expected(argument, &ValueType::Int, name))
            .collect::<Result<Vec<_>, _>>()?;
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        let result = ValueType::Option(Box::new(ValueType::Int));
        let register = self.allocate(result.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::CheckedIntCall {
            destination: register,
            operation,
            arguments: argument_registers.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::CheckedIntOperation {
            destination: u32::from(register),
            operation,
            arguments: argument_registers.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::CheckedIntOperation {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    fn compile_collection_operation(
        &mut self,
        operation: CollectionOperation,
        arguments: &[LoweredExpr],
        span: Span,
        name: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let values = arguments
            .iter()
            .map(|argument| self.compile_expression(argument))
            .collect::<Result<Vec<_>, _>>()?;
        let (operation, result) = match operation {
            CollectionOperation::Zip => {
                let mut elements = Vec::with_capacity(values.len());
                for value in &values {
                    let ValueType::List(element) = &value.value_type else {
                        return Err(Diagnostic::new(
                            "E3011",
                            "list.zip requires List<T> arguments",
                            span,
                        ));
                    };
                    elements.push(element.as_ref().clone());
                }
                (
                    CollectionOperation::Zip,
                    ValueType::List(Box::new(ValueType::Tuple(elements))),
                )
            }
            CollectionOperation::ListMin
            | CollectionOperation::ListMax
            | CollectionOperation::ListSumInt
            | CollectionOperation::ListSumFloat => {
                let [value] = values.as_slice() else {
                    unreachable!("collection arity was checked")
                };
                let ValueType::List(element) = &value.value_type else {
                    return Err(Diagnostic::new(
                        "E3011",
                        format!("{name} requires List<Int> or List<Float>"),
                        span,
                    ));
                };
                match (operation, element.as_ref()) {
                    (
                        CollectionOperation::ListMin | CollectionOperation::ListMax,
                        ValueType::Int | ValueType::Float,
                    ) => (operation, ValueType::Option(element.clone())),
                    (CollectionOperation::ListSumInt, ValueType::Int) => (
                        CollectionOperation::ListSumInt,
                        ValueType::Option(Box::new(ValueType::Int)),
                    ),
                    (CollectionOperation::ListSumInt, ValueType::Float) => {
                        (CollectionOperation::ListSumFloat, ValueType::Float)
                    }
                    _ => {
                        return Err(Diagnostic::new(
                            "E3011",
                            format!("{name} requires List<Int> or List<Float>"),
                            span,
                        ));
                    }
                }
            }
        };
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        let register = self.allocate(result.clone())?;
        let arguments = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::CollectionCall {
            destination: register,
            operation,
            arguments: arguments.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::CollectionOperation {
            destination: u32::from(register),
            operation,
            arguments: arguments.iter().copied().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::CollectionOperation {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    fn compile_list_fold(
        &mut self,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let values = self.compile_expression(&arguments[0])?;
        let ValueType::List(item) = &values.value_type else {
            return Err(Diagnostic::new(
                "E3011",
                "list.fold requires List<T> as its first argument",
                arguments[0].span,
            ));
        };
        let initial = self.compile_expression(&arguments[1])?;
        let callback = self.compile_expression(&arguments[2])?;
        let ValueType::Function {
            parameters,
            return_type,
            effects: callback_effects,
        } = &callback.value_type
        else {
            return Err(Diagnostic::new(
                "E3011",
                "list.fold requires a callback",
                arguments[2].span,
            ));
        };
        if parameters.as_slice() != [initial.value_type.clone(), item.as_ref().clone()]
            || return_type.as_ref() != &initial.value_type
        {
            return Err(Diagnostic::new(
                "E3011",
                "list.fold callback must be (accumulator, item) -> accumulator",
                arguments[2].span,
            ));
        }
        if !self.global.effect_sets[*callback_effects as usize].is_empty() {
            return Err(Diagnostic::new(
                "E2403",
                "list.fold callback must be pure",
                arguments[2].span,
            ));
        }
        let effects = self.union_effects([values.effects, initial.effects, callback.effects]);
        let register = self.allocate(initial.value_type.clone())?;
        self.code.push(Instruction::ListFold {
            destination: register,
            values: values.register,
            initial: initial.register,
            callback: callback.register,
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::ListFold {
            destination: u32::from(register),
            values: u32::from(values.register),
            initial: u32::from(initial.register),
            callback: u32::from(callback.register),
        });
        Ok(CompiledExpr {
            register,
            value_type: initial.value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::ListFold {
                    values: Box::new(values.hir),
                    initial: Box::new(initial.hir),
                    callback: Box::new(callback.hir),
                },
                None,
                &initial.value_type,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn compile_list_combinator(
        &mut self,
        operation: ListCombinator,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let values = self.compile_expression(&arguments[0])?;
        let ValueType::List(item) = &values.value_type else {
            return Err(Diagnostic::new(
                "E3011",
                "list combinator requires List<T> as its first argument",
                arguments[0].span,
            ));
        };
        let item = item.as_ref().clone();
        let initial = (operation == ListCombinator::Scan)
            .then(|| self.compile_expression(&arguments[1]))
            .transpose()?;
        let callback_index = usize::from(initial.is_some()) + 1;
        let pure_effects = effect_id(&self.global.effect_sets, &[]);
        let expected_callback = match operation {
            ListCombinator::Filter
            | ListCombinator::Find
            | ListCombinator::Any
            | ListCombinator::All
            | ListCombinator::Partition => Some(ValueType::Function {
                parameters: vec![item.clone()],
                return_type: Box::new(ValueType::Bool),
                effects: pure_effects,
            }),
            ListCombinator::Scan => {
                let initial = initial.as_ref().expect("scan has initial");
                Some(ValueType::Function {
                    parameters: vec![initial.value_type.clone(), item.clone()],
                    return_type: Box::new(initial.value_type.clone()),
                    effects: pure_effects,
                })
            }
            ListCombinator::Map | ListCombinator::FlatMap | ListCombinator::FilterMap => None,
        };
        let callback = if let Some(expected_callback) = &expected_callback {
            self.compile_expected(
                &arguments[callback_index],
                expected_callback,
                "list combinator callback",
            )?
        } else {
            self.compile_expression(&arguments[callback_index])?
        };
        let ValueType::Function {
            parameters,
            return_type,
            effects: callback_effects,
        } = &callback.value_type
        else {
            return Err(Diagnostic::new(
                "E3011",
                "list combinator requires a callback",
                arguments[callback_index].span,
            ));
        };
        if !self.global.effect_sets[*callback_effects as usize].is_empty() {
            return Err(Diagnostic::new(
                "E2403",
                "list combinator callback must be pure",
                arguments[callback_index].span,
            ));
        }
        let callback_result = return_type.as_ref().clone();
        let (expected_parameters, result) = match operation {
            ListCombinator::Map => {
                Self::collection_value_is_valid(&callback_result, "List", span)?;
                (
                    vec![item.clone()],
                    ValueType::List(Box::new(callback_result.clone())),
                )
            }
            ListCombinator::Filter => (vec![item.clone()], values.value_type.clone()),
            ListCombinator::FlatMap => {
                let ValueType::List(output) = &callback_result else {
                    return Err(Diagnostic::new(
                        "E3011",
                        "list.flat_map callback must return List<U>",
                        arguments[callback_index].span,
                    ));
                };
                Self::collection_value_is_valid(output, "List", span)?;
                (
                    vec![item.clone()],
                    ValueType::List(Box::new(output.as_ref().clone())),
                )
            }
            ListCombinator::FilterMap => {
                let ValueType::Option(output) = &callback_result else {
                    return Err(Diagnostic::new(
                        "E3011",
                        "list.filter_map callback must return Option<U>",
                        arguments[callback_index].span,
                    ));
                };
                Self::collection_value_is_valid(output, "List", span)?;
                (
                    vec![item.clone()],
                    ValueType::List(Box::new(output.as_ref().clone())),
                )
            }
            ListCombinator::Find => (
                vec![item.clone()],
                ValueType::Option(Box::new(item.clone())),
            ),
            ListCombinator::Any | ListCombinator::All => (vec![item.clone()], ValueType::Bool),
            ListCombinator::Partition => (
                vec![item.clone()],
                ValueType::Record(vec![
                    RecordField {
                        name: "matched".to_owned(),
                        value_type: values.value_type.clone(),
                    },
                    RecordField {
                        name: "rest".to_owned(),
                        value_type: values.value_type.clone(),
                    },
                ]),
            ),
            ListCombinator::Scan => {
                let initial = initial.as_ref().expect("scan has initial");
                Self::collection_value_is_valid(&initial.value_type, "List", span)?;
                (
                    vec![initial.value_type.clone(), item.clone()],
                    ValueType::List(Box::new(initial.value_type.clone())),
                )
            }
        };
        let bool_callback = matches!(
            operation,
            ListCombinator::Filter
                | ListCombinator::Find
                | ListCombinator::Any
                | ListCombinator::All
                | ListCombinator::Partition
        );
        let scan_callback = operation == ListCombinator::Scan
            && callback_result != initial.as_ref().expect("scan has initial").value_type;
        if parameters.as_slice() != expected_parameters.as_slice()
            || (bool_callback && callback_result != ValueType::Bool)
            || scan_callback
        {
            return Err(Diagnostic::new(
                "E3011",
                "list combinator callback has the wrong function type",
                arguments[callback_index].span,
            ));
        }
        let callback_result_register = self.allocate(callback_result)?;
        let register = self.allocate(result.clone())?;
        self.code.push(Instruction::ListCombinator {
            destination: register,
            operation,
            values: values.register,
            initial: initial.as_ref().map(|value| value.register),
            callback: callback.register,
            callback_result: callback_result_register,
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::ListCombinator {
            destination: u32::from(register),
            operation,
            values: u32::from(values.register),
            initial: initial.as_ref().map(|value| u32::from(value.register)),
            callback: u32::from(callback.register),
            callback_result: u32::from(callback_result_register),
        });
        let effects = self.union_effects(
            std::iter::once(values.effects)
                .chain(initial.iter().map(|value| value.effects))
                .chain(std::iter::once(callback.effects)),
        );
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::ListCombinator {
                    operation,
                    values: Box::new(values.hir),
                    initial: initial.map(|value| Box::new(value.hir)),
                    callback: Box::new(callback.hir),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn compile_sequence_builtin(
        &mut self,
        operation: SequenceBuiltin,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if operation == SequenceBuiltin::FromList {
            let values = self.compile_expression(&arguments[0])?;
            let ValueType::List(item) = &values.value_type else {
                return Err(Diagnostic::new(
                    "E3011",
                    "seq.from_list requires List<T>",
                    arguments[0].span,
                ));
            };
            let result = ValueType::Sequence(item.clone());
            let register = self.allocate(result.clone())?;
            self.code.push(Instruction::SequenceFromList {
                destination: register,
                values: values.register,
            });
            self.mark_last_instruction(span);
            self.mir.push(MirOperation::SequenceFromList {
                destination: u32::from(register),
                values: u32::from(values.register),
            });
            self.record_ownership(
                register,
                self.current_scope(),
                MirOwnershipState::Live,
                false,
            );
            let effects = values.effects;
            return Ok(CompiledExpr {
                register,
                value_type: result.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::SequenceFromList(Box::new(values.hir)),
                    None,
                    &result,
                    effects,
                    span,
                ),
            });
        }

        let sequence = self.compile_expression(&arguments[0])?;
        let ValueType::Sequence(item) = &sequence.value_type else {
            return Err(Diagnostic::new(
                "E3011",
                "sequence operation requires Sequence<T>",
                arguments[0].span,
            ));
        };
        let item = item.as_ref().clone();
        let pure_effects = self.empty_effects();
        let mut operand_effects = vec![sequence.effects];
        let mut fold_initial = None;

        let (result, instruction, mir, hir) = match operation {
            SequenceBuiltin::FromList => unreachable!("handled above"),
            SequenceBuiltin::Take => {
                let count =
                    self.compile_expected(&arguments[1], &ValueType::Int, "seq.take count")?;
                operand_effects.push(count.effects);
                (
                    sequence.value_type.clone(),
                    Instruction::SequenceTake {
                        destination: 0,
                        sequence: sequence.register,
                        count: count.register,
                    },
                    MirOperation::SequenceTake {
                        destination: 0,
                        sequence: u32::from(sequence.register),
                        count: u32::from(count.register),
                    },
                    HirExprKind::SequenceTake {
                        sequence: Box::new(sequence.hir.clone()),
                        count: Box::new(count.hir),
                    },
                )
            }
            SequenceBuiltin::ToList => (
                ValueType::List(Box::new(item.clone())),
                Instruction::SequenceToList {
                    destination: 0,
                    sequence: sequence.register,
                },
                MirOperation::SequenceToList {
                    destination: 0,
                    sequence: u32::from(sequence.register),
                },
                HirExprKind::SequenceToList(Box::new(sequence.hir.clone())),
            ),
            SequenceBuiltin::Fold => {
                let initial = self.compile_expression(&arguments[1])?;
                if initial.register == sequence.register && is_affine(&initial.value_type) {
                    return Err(Diagnostic::new(
                        "E3011",
                        "sequence fold cannot use one affine value as both source and accumulator",
                        arguments[1].span,
                    ));
                }
                fold_initial = Some(initial.register);
                let expected = ValueType::Function {
                    parameters: vec![initial.value_type.clone(), item.clone()],
                    return_type: Box::new(initial.value_type.clone()),
                    effects: pure_effects,
                };
                let callback =
                    self.compile_expected(&arguments[2], &expected, "seq.fold callback")?;
                operand_effects.extend([initial.effects, callback.effects]);
                (
                    initial.value_type.clone(),
                    Instruction::SequenceFold {
                        destination: 0,
                        sequence: sequence.register,
                        initial: initial.register,
                        callback: callback.register,
                    },
                    MirOperation::SequenceFold {
                        destination: 0,
                        sequence: u32::from(sequence.register),
                        initial: u32::from(initial.register),
                        callback: u32::from(callback.register),
                    },
                    HirExprKind::SequenceFold {
                        sequence: Box::new(sequence.hir.clone()),
                        initial: Box::new(initial.hir),
                        callback: Box::new(callback.hir),
                    },
                )
            }
            SequenceBuiltin::Map
            | SequenceBuiltin::Filter
            | SequenceBuiltin::Find
            | SequenceBuiltin::Any
            | SequenceBuiltin::All => {
                let expected =
                    (!matches!(operation, SequenceBuiltin::Map)).then(|| ValueType::Function {
                        parameters: vec![item.clone()],
                        return_type: Box::new(ValueType::Bool),
                        effects: pure_effects,
                    });
                let callback = if let Some(expected) = &expected {
                    self.compile_expected(&arguments[1], expected, "sequence callback")?
                } else {
                    self.compile_expression(&arguments[1])?
                };
                let ValueType::Function {
                    parameters,
                    return_type,
                    effects: callback_effects,
                } = &callback.value_type
                else {
                    return Err(Diagnostic::new(
                        "E3011",
                        "sequence operation requires a callback",
                        arguments[1].span,
                    ));
                };
                if parameters.as_slice() != [item.clone()] {
                    return Err(Diagnostic::new(
                        "E3011",
                        "sequence callback has the wrong parameter type",
                        arguments[1].span,
                    ));
                }
                if !self.global.effect_sets[*callback_effects as usize].is_empty() {
                    return Err(Diagnostic::new(
                        "E2403",
                        "sequence callbacks must be pure",
                        arguments[1].span,
                    ));
                }
                let callback_result = return_type.as_ref().clone();
                if !matches!(operation, SequenceBuiltin::Map) && callback_result != ValueType::Bool
                {
                    return Err(Diagnostic::new(
                        "E3011",
                        "seq.filter/find/any/all callbacks must return Bool",
                        arguments[1].span,
                    ));
                }
                if operation == SequenceBuiltin::Map {
                    Self::collection_value_is_valid(&callback_result, "Sequence", span)?;
                }
                operand_effects.push(callback.effects);
                let result = match operation {
                    SequenceBuiltin::Map => ValueType::Sequence(Box::new(callback_result.clone())),
                    SequenceBuiltin::Filter => sequence.value_type.clone(),
                    SequenceBuiltin::Find => ValueType::Option(Box::new(item.clone())),
                    SequenceBuiltin::Any | SequenceBuiltin::All => ValueType::Bool,
                    _ => unreachable!(),
                };
                let (instruction, mir, hir) = match operation {
                    SequenceBuiltin::Map => (
                        Instruction::SequenceMap {
                            destination: 0,
                            sequence: sequence.register,
                            callback: callback.register,
                        },
                        MirOperation::SequenceMap {
                            destination: 0,
                            sequence: u32::from(sequence.register),
                            callback: u32::from(callback.register),
                        },
                        HirExprKind::SequenceMap {
                            sequence: Box::new(sequence.hir.clone()),
                            callback: Box::new(callback.hir),
                        },
                    ),
                    SequenceBuiltin::Filter => (
                        Instruction::SequenceFilter {
                            destination: 0,
                            sequence: sequence.register,
                            callback: callback.register,
                        },
                        MirOperation::SequenceFilter {
                            destination: 0,
                            sequence: u32::from(sequence.register),
                            callback: u32::from(callback.register),
                        },
                        HirExprKind::SequenceFilter {
                            sequence: Box::new(sequence.hir.clone()),
                            callback: Box::new(callback.hir),
                        },
                    ),
                    SequenceBuiltin::Find => (
                        Instruction::SequenceFind {
                            destination: 0,
                            sequence: sequence.register,
                            callback: callback.register,
                        },
                        MirOperation::SequenceFind {
                            destination: 0,
                            sequence: u32::from(sequence.register),
                            callback: u32::from(callback.register),
                        },
                        HirExprKind::SequenceFind {
                            sequence: Box::new(sequence.hir.clone()),
                            callback: Box::new(callback.hir),
                        },
                    ),
                    SequenceBuiltin::Any => (
                        Instruction::SequenceAny {
                            destination: 0,
                            sequence: sequence.register,
                            callback: callback.register,
                        },
                        MirOperation::SequenceAny {
                            destination: 0,
                            sequence: u32::from(sequence.register),
                            callback: u32::from(callback.register),
                        },
                        HirExprKind::SequenceAny {
                            sequence: Box::new(sequence.hir.clone()),
                            callback: Box::new(callback.hir),
                        },
                    ),
                    SequenceBuiltin::All => (
                        Instruction::SequenceAll {
                            destination: 0,
                            sequence: sequence.register,
                            callback: callback.register,
                        },
                        MirOperation::SequenceAll {
                            destination: 0,
                            sequence: u32::from(sequence.register),
                            callback: u32::from(callback.register),
                        },
                        HirExprKind::SequenceAll {
                            sequence: Box::new(sequence.hir.clone()),
                            callback: Box::new(callback.hir),
                        },
                    ),
                    _ => unreachable!(),
                };
                (result, instruction, mir, hir)
            }
        };

        let register = self.allocate(result.clone())?;
        let instruction = match instruction {
            Instruction::SequenceMap {
                sequence, callback, ..
            } => Instruction::SequenceMap {
                destination: register,
                sequence,
                callback,
            },
            Instruction::SequenceFilter {
                sequence, callback, ..
            } => Instruction::SequenceFilter {
                destination: register,
                sequence,
                callback,
            },
            Instruction::SequenceTake {
                sequence, count, ..
            } => Instruction::SequenceTake {
                destination: register,
                sequence,
                count,
            },
            Instruction::SequenceFind {
                sequence, callback, ..
            } => Instruction::SequenceFind {
                destination: register,
                sequence,
                callback,
            },
            Instruction::SequenceAny {
                sequence, callback, ..
            } => Instruction::SequenceAny {
                destination: register,
                sequence,
                callback,
            },
            Instruction::SequenceAll {
                sequence, callback, ..
            } => Instruction::SequenceAll {
                destination: register,
                sequence,
                callback,
            },
            Instruction::SequenceFold {
                sequence,
                initial,
                callback,
                ..
            } => Instruction::SequenceFold {
                destination: register,
                sequence,
                initial,
                callback,
            },
            Instruction::SequenceToList { sequence, .. } => Instruction::SequenceToList {
                destination: register,
                sequence,
            },
            _ => unreachable!("sequence instruction"),
        };
        let mir = match mir {
            MirOperation::SequenceMap {
                sequence, callback, ..
            } => MirOperation::SequenceMap {
                destination: u32::from(register),
                sequence,
                callback,
            },
            MirOperation::SequenceFilter {
                sequence, callback, ..
            } => MirOperation::SequenceFilter {
                destination: u32::from(register),
                sequence,
                callback,
            },
            MirOperation::SequenceTake {
                sequence, count, ..
            } => MirOperation::SequenceTake {
                destination: u32::from(register),
                sequence,
                count,
            },
            MirOperation::SequenceFind {
                sequence, callback, ..
            } => MirOperation::SequenceFind {
                destination: u32::from(register),
                sequence,
                callback,
            },
            MirOperation::SequenceAny {
                sequence, callback, ..
            } => MirOperation::SequenceAny {
                destination: u32::from(register),
                sequence,
                callback,
            },
            MirOperation::SequenceAll {
                sequence, callback, ..
            } => MirOperation::SequenceAll {
                destination: u32::from(register),
                sequence,
                callback,
            },
            MirOperation::SequenceFold {
                sequence,
                initial,
                callback,
                ..
            } => MirOperation::SequenceFold {
                destination: u32::from(register),
                sequence,
                initial,
                callback,
            },
            MirOperation::SequenceToList { sequence, .. } => MirOperation::SequenceToList {
                destination: u32::from(register),
                sequence,
            },
            _ => unreachable!("sequence MIR operation"),
        };
        self.code.push(instruction);
        self.mark_last_instruction(span);
        self.mir.push(mir);
        self.consume_ownership(sequence.register, MirOwnershipState::Moved);
        if let Some(initial_register) = fold_initial {
            if is_affine(&result) {
                let ownership = self
                    .ownership_states
                    .get(&initial_register)
                    .copied()
                    .ok_or_else(|| {
                        Diagnostic::new(
                            "E3011",
                            "affine fold accumulator has no ownership state",
                            arguments[1].span,
                        )
                    })?;
                self.consume_ownership(initial_register, MirOwnershipState::Moved);
                self.record_ownership(
                    register,
                    ownership.scope,
                    MirOwnershipState::Live,
                    ownership.must_consume,
                );
            }
        } else if matches!(result, ValueType::Sequence(_)) {
            self.record_ownership(
                register,
                self.current_scope(),
                MirOwnershipState::Live,
                false,
            );
        }
        let effects = self.union_effects(operand_effects);
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(hir, None, &result, effects, span),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_string_builtin(
        &mut self,
        operation: StringOperation,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let (name, expected, result) = string_operation_signature(operation);
        if arguments.len() != expected.len() {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "{name} requires exactly {} argument{}",
                    expected.len(),
                    if expected.len() == 1 { "" } else { "s" }
                ),
                span,
            ));
        }

        let mut values = Vec::with_capacity(arguments.len());
        let mut terminal = None;
        for (argument, expected) in arguments.iter().zip(&expected) {
            let value = if terminal.is_some() {
                self.compile_without_runtime(|lowering| {
                    lowering.compile_expected(argument, expected, name)
                })?
            } else {
                self.compile_expected(argument, expected, name)?
            };
            if terminal.is_none() && value.value_type == ValueType::Never {
                terminal = Some(value.register);
            }
            values.push(value);
        }
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        if let Some(register) = terminal {
            return Ok(CompiledExpr {
                register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::StringOperation {
                        operation,
                        arguments: values.into_iter().map(|value| value.hir).collect(),
                    },
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }

        let register = self.allocate(result.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::StringCall {
            destination: register,
            operation,
            arguments: argument_registers.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::StringOperation {
            destination: u32::from(register),
            operation,
            arguments: argument_registers.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::StringOperation {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_standard_builtin(
        &mut self,
        operation: StandardOperation,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let (name, expected, result) = standard_operation_signature(operation);
        if arguments.len() != expected.len() {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "{name} requires exactly {} argument{}",
                    expected.len(),
                    if expected.len() == 1 { "" } else { "s" }
                ),
                span,
            ));
        }
        let mut values = Vec::with_capacity(arguments.len());
        let mut terminal = None;
        for (argument, expected) in arguments.iter().zip(&expected) {
            let value = if terminal.is_some() {
                self.compile_without_runtime(|lowering| {
                    lowering.compile_expected(argument, expected, name)
                })?
            } else {
                self.compile_expected(argument, expected, name)?
            };
            if terminal.is_none() && value.value_type == ValueType::Never {
                terminal = Some(value.register);
            }
            values.push(value);
        }
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        if let Some(register) = terminal {
            return Ok(CompiledExpr {
                register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::StandardOperation {
                        operation,
                        arguments: values.into_iter().map(|value| value.hir).collect(),
                    },
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        let register = self.allocate(result.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::StandardCall {
            destination: register,
            operation,
            arguments: argument_registers.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::StandardOperation {
            destination: u32::from(register),
            operation,
            arguments: argument_registers.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::StandardOperation {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_capability_builtin(
        &mut self,
        operation: CapabilityOperation,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let (name, expected, result) = capability_operation_signature(operation);
        if arguments.len() != expected.len() {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "{name} requires exactly {} argument{}",
                    expected.len(),
                    if expected.len() == 1 { "" } else { "s" }
                ),
                span,
            ));
        }
        let values = arguments
            .iter()
            .zip(&expected)
            .map(|(argument, expected)| self.compile_expected(argument, expected, name))
            .collect::<Result<Vec<_>, _>>()?;
        let inspect_effect =
            effect_id(&self.global.effect_sets, &["capability.inspect".to_owned()]);
        let effects = self.union_effects(
            values
                .iter()
                .map(|value| value.effects)
                .chain(std::iter::once(inspect_effect)),
        );
        if let Some(value) = values
            .iter()
            .find(|value| value.value_type == ValueType::Never)
        {
            return Ok(CompiledExpr {
                register: value.register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::CapabilityInspect {
                        operation,
                        arguments: values.into_iter().map(|value| value.hir).collect(),
                    },
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        let register = self.allocate(result.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::CapabilityInspect {
            destination: register,
            operation,
            arguments: argument_registers.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::CapabilityInspect {
            destination: u32::from(register),
            operation,
            arguments: argument_registers.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::CapabilityInspect {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_length_builtin(
        &mut self,
        values: CompiledExpr,
        argument_span: Span,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if !matches!(
            values.value_type,
            ValueType::String | ValueType::Bytes | ValueType::List(_) | ValueType::Map(_, _)
        ) {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "length requires String, Bytes, List<T>, or Map<K, V>, found {}",
                    values.value_type
                ),
                argument_span,
            ));
        }
        let register = self.allocate(ValueType::Int)?;
        self.code.push(Instruction::Length {
            destination: register,
            collection: values.register,
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::Length {
            destination: u32::from(register),
            collection: u32::from(values.register),
        });
        let effects = values.effects;
        Ok(CompiledExpr {
            register,
            value_type: ValueType::Int,
            effects,
            hir: self.hir(
                HirExprKind::Length(Box::new(values.hir)),
                None,
                &ValueType::Int,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_list_builtin(
        &mut self,
        builtin: CollectionBuiltin,
        values: CompiledExpr,
        arguments: &[LoweredExpr],
        span: Span,
        name: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let ValueType::List(element_type) = &values.value_type else {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "{name} requires List<T> as its first argument, found {}",
                    values.value_type
                ),
                arguments[0].span,
            ));
        };
        let element_type = element_type.as_ref().clone();
        let value_type = values.value_type.clone();
        match builtin {
            CollectionBuiltin::ListAppend => {
                let value = self.compile_expected(&arguments[1], &element_type, "list value")?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::ListAppend {
                    destination: register,
                    values: values.register,
                    value: value.register,
                });
                self.mir.push(MirOperation::ListAppend {
                    destination: u32::from(register),
                    values: u32::from(values.register),
                    value: u32::from(value.register),
                });
                let effects = self.union_effects([values.effects, value.effects]);
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::ListAppend {
                            values: Box::new(values.hir),
                            value: Box::new(value.hir),
                        },
                        None,
                        &value_type,
                        effects,
                        span,
                    ),
                })
            }
            CollectionBuiltin::ListSet => {
                let index = self.compile_expected(&arguments[1], &ValueType::Int, "list index")?;
                let value = self.compile_expected(&arguments[2], &element_type, "list value")?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::ListSet {
                    destination: register,
                    values: values.register,
                    index: index.register,
                    value: value.register,
                });
                self.mir.push(MirOperation::ListSet {
                    destination: u32::from(register),
                    values: u32::from(values.register),
                    index: u32::from(index.register),
                    value: u32::from(value.register),
                });
                let effects = self.union_effects([values.effects, index.effects, value.effects]);
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::ListSet {
                            values: Box::new(values.hir),
                            index: Box::new(index.hir),
                            value: Box::new(value.hir),
                        },
                        None,
                        &value_type,
                        effects,
                        span,
                    ),
                })
            }
            CollectionBuiltin::Length
            | CollectionBuiltin::Safe(_)
            | CollectionBuiltin::CheckedInt(_)
            | CollectionBuiltin::Operation(_)
            | CollectionBuiltin::ListFold
            | CollectionBuiltin::ListCombinator(_)
            | CollectionBuiltin::Sequence(_) => {
                unreachable!("length builtin is handled before list lowering")
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_user_enum(
        &mut self,
        name: &str,
        variant_name: &str,
        payload: &LoweredEnumValuePayload,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if name == "TranscriptPart" && self.global.bundle.transcript_part.is_some() {
            return Err(Diagnostic::new(
                "E3007",
                "TranscriptPart is a read-only standard type",
                span,
            ));
        }
        if name == "ExternalFsAccess" {
            if !matches!(payload, LoweredEnumValuePayload::Unit) {
                return Err(Diagnostic::new(
                    "E3007",
                    "ExternalFsAccess variants do not accept a payload",
                    span,
                ));
            }
            let access = match variant_name {
                "Read" => ExternalFsAccess::Read,
                "Write" => ExternalFsAccess::Write,
                "ReadWrite" => ExternalFsAccess::ReadWrite,
                _ => {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("ExternalFsAccess has no variant '{variant_name}'"),
                        span,
                    ));
                }
            };
            let register = self.allocate(ValueType::ExternalFsAccess)?;
            let constant = self.global.constant(Constant::ExternalFsAccess(access))?;
            self.code.push(Instruction::Const {
                destination: register,
                constant,
            });
            self.mir.push(MirOperation::Constant {
                destination: u32::from(register),
            });
            let effects = self.empty_effects();
            return Ok(CompiledExpr {
                register,
                value_type: ValueType::ExternalFsAccess,
                effects,
                hir: self.hir(
                    HirExprKind::Enum,
                    None,
                    &ValueType::ExternalFsAccess,
                    effects,
                    span,
                ),
            });
        }
        let value_type = resolve_named_type(
            &self.global.bundle.modules,
            &self.global.bundle.types,
            &self.info.module,
            name,
            span,
        )?;
        let ValueType::Enum(enum_id) = value_type else {
            return Err(Diagnostic::new(
                "E3007",
                format!("'{name}' is not an enum type"),
                span,
            ));
        };
        let metadata = &self.global.bundle.enum_types[enum_id as usize];
        let variant = metadata
            .variants
            .iter()
            .position(|candidate| candidate.name == variant_name)
            .ok_or_else(|| {
                Diagnostic::new(
                    "E3007",
                    format!("enum '{name}' has no variant '{variant_name}'"),
                    span,
                )
            })?;
        let declared_payload = metadata.variants[variant].payload.clone();
        let values = match (declared_payload, payload) {
            (EnumPayloadType::Unit, LoweredEnumValuePayload::Unit) => Vec::new(),
            (EnumPayloadType::Tuple(expected), LoweredEnumValuePayload::Tuple(values)) => {
                if expected.len() != values.len() {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("enum variant '{variant_name}' has the wrong payload count"),
                        span,
                    ));
                }
                values
                    .iter()
                    .zip(&expected)
                    .map(|(value, expected)| self.compile_expected(value, expected, "enum payload"))
                    .collect::<Result<Vec<_>, _>>()?
            }
            (EnumPayloadType::Record(expected), LoweredEnumValuePayload::Record(fields)) => {
                if expected.len() != fields.len() {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("enum variant '{variant_name}' requires every field exactly once"),
                        span,
                    ));
                }
                let mut supplied = BTreeMap::new();
                for (field, value, field_span) in fields {
                    if supplied
                        .insert(field.clone(), (value, *field_span))
                        .is_some()
                    {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!("duplicate enum field '{field}'"),
                            *field_span,
                        ));
                    }
                }
                let mut values = Vec::with_capacity(expected.len());
                for field in &expected {
                    let (value, field_span) = supplied.remove(&field.name).ok_or_else(|| {
                        Diagnostic::new(
                            "E3007",
                            format!("missing enum field '{}'", field.name),
                            span,
                        )
                    })?;
                    values.push(
                        self.compile_expected(value, &field.value_type, "enum field")
                            .map_err(|diagnostic| {
                                if diagnostic.span == value.span {
                                    Diagnostic::new(diagnostic.code, diagnostic.message, field_span)
                                } else {
                                    diagnostic
                                }
                            })?,
                    );
                }
                if let Some((field, (_, field_span))) = supplied.into_iter().next() {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("unknown enum field '{field}'"),
                        field_span,
                    ));
                }
                values
            }
            _ => {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("enum variant '{variant_name}' uses the wrong payload form"),
                    span,
                ));
            }
        };
        let value_type = ValueType::Enum(enum_id);
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::EnumNew {
            destination: register,
            variant: u32::try_from(variant).expect("variant index fits"),
            payload: values.iter().map(|value| value.register).collect(),
        });
        self.mir.push(MirOperation::Enum {
            destination: u32::from(register),
        });
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(HirExprKind::Enum, None, &value_type, effects, span),
        })
    }

    pub(super) fn compile_template(
        &mut self,
        parts: &[LoweredTemplatePart],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let mut values = Vec::with_capacity(parts.len());
        let mut hir_parts = Vec::with_capacity(parts.len());
        let mut terminal = None;
        for part in parts {
            let value = match part {
                LoweredTemplatePart::Literal {
                    value,
                    span: literal_span,
                } => {
                    let literal = LoweredExpr {
                        kind: LoweredExprKind::String(value.clone()),
                        span: *literal_span,
                    };
                    let compiled = if terminal.is_some() {
                        self.compile_without_runtime(|lowering| {
                            lowering.compile_expression(&literal)
                        })?
                    } else {
                        self.compile_expression(&literal)?
                    };
                    hir_parts.push(HirTemplatePart::Literal {
                        value: value.clone(),
                        span: self.global.intern_span(&self.info.module, *literal_span),
                    });
                    compiled
                }
                LoweredTemplatePart::Interpolation(expression) => {
                    let compiled = if terminal.is_some() {
                        self.compile_without_runtime(|lowering| {
                            lowering.compile_expression(expression)
                        })?
                    } else {
                        self.compile_expression(expression)?
                    };
                    if compiled.value_type != ValueType::Never
                        && compiled.value_type != ValueType::String
                    {
                        return Err(Diagnostic::new(
                            "E3011",
                            format!(
                                "template interpolation must be String, found {}",
                                compiled.value_type
                            ),
                            expression.span,
                        ));
                    }
                    hir_parts.push(HirTemplatePart::Interpolation(compiled.hir.clone()));
                    compiled
                }
            };
            if terminal.is_none() && value.value_type == ValueType::Never {
                terminal = Some(value.register);
            }
            values.push(value);
        }
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        if let Some(register) = terminal {
            return Ok(CompiledExpr {
                register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::Template(hir_parts),
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }

        let register = self.allocate(ValueType::String)?;
        let arguments = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::StringCall {
            destination: register,
            operation: StringOperation::TemplateConcat,
            arguments: arguments.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::StringOperation {
            destination: u32::from(register),
            operation: StringOperation::TemplateConcat,
            arguments: arguments.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: ValueType::String,
            effects,
            hir: self.hir(
                HirExprKind::Template(hir_parts),
                None,
                &ValueType::String,
                effects,
                span,
            ),
        })
    }

    fn compile_named_function_value(
        &mut self,
        symbol: SymbolId,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let target = self.global.bundle.functions[symbol as usize].clone();
        if !target.lowered.generics.is_empty() {
            return Err(Diagnostic::new(
                "E3007",
                format!(
                    "generic function '{}' needs an exact instantiation before use as a value",
                    target.lowered.name
                ),
                span,
            ));
        }
        let parameters = target.lowered.parameters.clone();
        let arguments = parameters
            .iter()
            .map(|(name, _, parameter_span)| LoweredCallArgument {
                label: None,
                value: LoweredExpr {
                    kind: LoweredExprKind::Variable(name.clone()),
                    span: *parameter_span,
                },
                placeholder: false,
                trailing: false,
                preceding_call_span: None,
                span: *parameter_span,
            })
            .collect();
        let call = LoweredExpr {
            kind: LoweredExprKind::Call {
                callee: Box::new(LoweredExpr {
                    kind: LoweredExprKind::Variable(target.lowered.name.clone()),
                    span,
                }),
                type_arguments: Vec::new(),
                arguments,
            },
            span,
        };
        let body = LoweredBody {
            statements: Vec::new(),
            tail: Some(call),
            span,
        };
        let return_type = concrete_type(
            &target.return_type,
            &BTreeMap::new(),
            &self.global.effect_sets,
        )?;
        let return_type = if target.lowered.is_async {
            self.lowered_type_for_short_closure(
                &ValueType::Future(Box::new(return_type)),
                target.lowered.return_type.span(),
            )?
        } else {
            target.lowered.return_type.clone()
        };
        self.compile_closure(
            &parameters,
            &return_type,
            Some(&target.effects),
            &body,
            span,
        )
    }

    fn resolve_callable(&self, name: &str) -> Result<Option<(FunctionInfo, bool)>, Diagnostic> {
        if let Some(function) = self.local_functions.get(name) {
            return Ok(Some((function.clone(), true)));
        }
        let Some(symbol) = resolve_function_name(self.global.bundle, &self.info.module, name)?
        else {
            return Ok(None);
        };
        Ok(Some((
            self.global.bundle.functions[symbol as usize].clone(),
            false,
        )))
    }

    fn local_name_conflicts(&self, name: &str) -> bool {
        self.local_functions.contains_key(name) || self.unavailable_local_functions.contains(name)
    }

    fn reserved_local_names(&self) -> BTreeSet<String> {
        self.unavailable_local_functions
            .iter()
            .cloned()
            .chain(self.local_functions.keys().cloned())
            .collect()
    }

    fn compile_local_function_value(
        &mut self,
        target: &FunctionInfo,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let parameters = target
            .parameters
            .iter()
            .map(|parameter| concrete_type(parameter, &BTreeMap::new(), &self.global.effect_sets))
            .collect::<Result<Vec<_>, _>>()?;
        let return_type = concrete_type(
            &target.return_type,
            &BTreeMap::new(),
            &self.global.effect_sets,
        )?;
        let effect = effect_id(&self.global.effect_sets, &target.effects);
        let value_type = ValueType::Function {
            parameters,
            return_type: Box::new(return_type),
            effects: effect,
        };
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::ClosureNew {
            destination: register,
            function: target.bytecode.expect("compiled local function"),
            captures: Vec::new(),
        });
        self.mir.push(MirOperation::ClosureEnvironment {
            destination: u32::from(register),
            function: target.symbol,
            captures: Vec::new(),
        });
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects: effect,
            hir: self.hir(
                HirExprKind::Variable,
                Some(target.symbol),
                &value_type,
                effect,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_expression(
        &mut self,
        expression: &LoweredExpr,
    ) -> Result<CompiledExpr, Diagnostic> {
        match &expression.kind {
            LoweredExprKind::Unit => {
                let register = self.allocate(ValueType::Unit)?;
                let constant = self.global.constant(Constant::Unit)?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Unit,
                    effects,
                    hir: self.hir(
                        HirExprKind::Unit,
                        None,
                        &ValueType::Unit,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Int(value) => {
                let register = self.allocate(ValueType::Int)?;
                let constant = self.global.constant(Constant::Int(*value))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Int,
                    effects,
                    hir: self.hir(
                        HirExprKind::Int(*value),
                        None,
                        &ValueType::Int,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Float(value) => {
                let register = self.allocate(ValueType::Float)?;
                let constant = self.global.constant(Constant::float_bits(*value))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Float,
                    effects,
                    hir: self.hir(
                        HirExprKind::Float(*value),
                        None,
                        &ValueType::Float,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Bool(value) => {
                let register = self.allocate(ValueType::Bool)?;
                let constant = self.global.constant(Constant::Bool(*value))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Bool,
                    effects,
                    hir: self.hir(
                        HirExprKind::Bool(*value),
                        None,
                        &ValueType::Bool,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::String(value) => {
                let register = self.allocate(ValueType::String)?;
                let constant = self.global.constant(Constant::String(value.clone()))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::String,
                    effects,
                    hir: self.hir(
                        HirExprKind::String(value.clone()),
                        None,
                        &ValueType::String,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Template(parts) => self.compile_template(parts, expression.span),
            LoweredExprKind::Bytes(value) => {
                let register = self.allocate(ValueType::Bytes)?;
                let constant = self.global.constant(Constant::Bytes(value.clone()))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Bytes,
                    effects,
                    hir: self.hir(
                        HirExprKind::Bytes(value.clone()),
                        None,
                        &ValueType::Bytes,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Variable(name) => {
                if !self.bindings.contains_key(name) {
                    if let Some((target, local)) = self.resolve_callable(name)? {
                        let symbol = target.symbol;
                        if target.is_const {
                            if let Some(value) = self.global.constant_values.get(&symbol).cloned() {
                                let expected = concrete_type(
                                    &target.return_type,
                                    &BTreeMap::new(),
                                    &self.global.effect_sets,
                                )?;
                                return self.materialize_constant(
                                    &value,
                                    &expected,
                                    symbol,
                                    expression.span,
                                );
                            }
                            if self.global.constant_evaluation {
                                let value_type = concrete_type(
                                    &target.return_type,
                                    &BTreeMap::new(),
                                    &self.global.effect_sets,
                                )?;
                                let register = self.allocate(value_type.clone())?;
                                self.code.push(Instruction::DirectCall {
                                    destination: register,
                                    function: target.bytecode.expect("constant bytecode"),
                                    arguments: Vec::new(),
                                });
                                self.mir.push(MirOperation::DirectCall {
                                    destination: u32::from(register),
                                    function: target.symbol,
                                    arguments: Vec::new(),
                                });
                                let effects = self.empty_effects();
                                return Ok(CompiledExpr {
                                    register,
                                    value_type: value_type.clone(),
                                    effects,
                                    hir: self.hir(
                                        HirExprKind::Variable,
                                        Some(symbol),
                                        &value_type,
                                        effects,
                                        expression.span,
                                    ),
                                });
                            }
                        } else if local {
                            return self.compile_local_function_value(&target, expression.span);
                        } else {
                            return self.compile_named_function_value(symbol, expression.span);
                        }
                    }
                }
                let binding = self.bindings.get_mut(name).ok_or_else(|| {
                    Diagnostic::new(
                        "E3005",
                        format!("unknown local value '{name}'"),
                        expression.span,
                    )
                })?;
                if is_affine(&binding.value_type) {
                    if binding.moved {
                        return Err(Diagnostic::new(
                            "E3011",
                            format!("use of moved {} value '{name}'", binding.value_type),
                            expression.span,
                        ));
                    }
                    binding.moved = true;
                }
                let binding = binding.clone();
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register: binding.register,
                    value_type: binding.value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Variable,
                        Some(binding.symbol),
                        &binding.value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Record { name, fields } => {
                if name == "$anonymous" {
                    let mut seen = BTreeSet::new();
                    let mut compiled = Vec::with_capacity(fields.len());
                    for (field, value, field_span) in fields {
                        if !seen.insert(field.clone()) {
                            return Err(Diagnostic::new(
                                "E3007",
                                format!("duplicate record field '{field}'"),
                                *field_span,
                            ));
                        }
                        compiled.push((field.clone(), self.compile_expression(value)?));
                    }
                    compiled.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                    let value_type = ValueType::Record(
                        compiled
                            .iter()
                            .map(|(field, value)| RecordField {
                                name: field.clone(),
                                value_type: value.value_type.clone(),
                            })
                            .collect(),
                    );
                    let register = self.allocate(value_type.clone())?;
                    self.code.push(Instruction::RecordNew {
                        destination: register,
                        fields: compiled
                            .iter()
                            .enumerate()
                            .map(|(index, (_, value))| {
                                (
                                    u32::try_from(index).expect("record field index fits"),
                                    value.register,
                                )
                            })
                            .collect(),
                    });
                    self.mir.push(MirOperation::Record {
                        destination: u32::from(register),
                    });
                    let effects =
                        self.union_effects(compiled.iter().map(|(_, value)| value.effects));
                    return Ok(CompiledExpr {
                        register,
                        value_type: value_type.clone(),
                        effects,
                        hir: self.hir(
                            HirExprKind::Record(
                                compiled.into_iter().map(|(_, value)| value.hir).collect(),
                            ),
                            None,
                            &value_type,
                            effects,
                            expression.span,
                        ),
                    });
                }
                if self.global.bundle.transcript_part.is_some()
                    && matches!(name.as_str(), "TranscriptSnapshot" | "TranscriptMessage")
                {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("{name} is a read-only standard type"),
                        expression.span,
                    ));
                }
                let value_type = resolve_named_type(
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                    &self.info.module,
                    name,
                    expression.span,
                )?;
                let ValueType::Record(layout) = &value_type else {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("'{name}' is not a record type"),
                        expression.span,
                    ));
                };
                if fields.len() != layout.len() {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("record '{name}' requires every field exactly once"),
                        expression.span,
                    ));
                }
                let mut seen = BTreeSet::new();
                let mut compiled = Vec::new();
                for (field, value, field_span) in fields {
                    if !seen.insert(field.clone()) {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!("duplicate record field '{field}'"),
                            *field_span,
                        ));
                    }
                    let index = layout
                        .iter()
                        .position(|candidate| candidate.name == *field)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                "E3007",
                                format!("record '{name}' has no field '{field}'"),
                                *field_span,
                            )
                        })?;
                    let value =
                        self.compile_expected(value, &layout[index].value_type, "record field")?;
                    if value.value_type != layout[index].value_type {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!(
                                "record field '{field}' expects {}, found {}",
                                layout[index].value_type, value.value_type
                            ),
                            *field_span,
                        ));
                    }
                    compiled.push((index, value));
                }
                compiled.sort_by_key(|(index, _)| *index);
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::RecordNew {
                    destination: register,
                    fields: compiled
                        .iter()
                        .map(|(index, value)| {
                            (
                                u32::try_from(*index).expect("field index fits"),
                                value.register,
                            )
                        })
                        .collect(),
                });
                self.mir.push(MirOperation::Record {
                    destination: u32::from(register),
                });
                let effects = self.union_effects(compiled.iter().map(|(_, value)| value.effects));
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Record(
                            compiled.into_iter().map(|(_, value)| value.hir).collect(),
                        ),
                        None,
                        &value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Prompt {
                system,
                context,
                data,
                output,
                max_attempts,
            } => self.compile_prompt(
                system,
                context.as_deref(),
                data.as_deref(),
                output,
                *max_attempts,
                expression.span,
            ),
            LoweredExprKind::Enum {
                name,
                variant,
                payload,
            } => self.compile_user_enum(name, variant, payload, expression.span),
            LoweredExprKind::FieldGet {
                record,
                field,
                field_span,
            } => {
                if let LoweredExprKind::Variable(name) = &record.kind {
                    if name == "ExternalFsAccess"
                        || resolve_named_type(
                            &self.global.bundle.modules,
                            &self.global.bundle.types,
                            &self.info.module,
                            name,
                            record.span,
                        )
                        .is_ok_and(|value_type| matches!(value_type, ValueType::Enum(_)))
                    {
                        return self.compile_user_enum(
                            name,
                            field,
                            &LoweredEnumValuePayload::Unit,
                            expression.span,
                        );
                    }
                }
                self.compile_field_get(record, field, *field_span, expression.span)
            }
            LoweredExprKind::Try(value) => self.compile_try(value, expression.span),
            LoweredExprKind::Match { source, arms } => {
                self.compile_match(source, arms, expression.span, None)
            }
            LoweredExprKind::If {
                condition,
                then_body,
                else_branch,
            } => self.compile_if(
                condition,
                then_body,
                else_branch.as_ref(),
                expression.span,
                None,
                None,
            ),
            LoweredExprKind::List(elements) => self.compile_list(expression, elements, None),
            LoweredExprKind::Map(entries) => self.compile_map(expression, entries, None),
            LoweredExprKind::ListWithSpread(items) => {
                self.compile_list_with_spread(expression, items, None)
            }
            LoweredExprKind::MapWithSpread(items) => {
                self.compile_map_with_spread(expression, items, None)
            }
            LoweredExprKind::RecordUpdate {
                name,
                base,
                spread_span,
                fields,
            } => self.compile_record_update(expression, name, base, *spread_span, fields),
            LoweredExprKind::OptionalFieldGet {
                receiver,
                field,
                operator_span,
                field_span,
            } => self.compile_optional_field_get(
                receiver,
                field,
                *operator_span,
                *field_span,
                expression.span,
            ),
            LoweredExprKind::Tuple(elements) => {
                let values = elements
                    .iter()
                    .map(|element| self.compile_expression(element))
                    .collect::<Result<Vec<_>, _>>()?;
                if values.iter().any(|value| is_affine(&value.value_type)) {
                    return Err(Diagnostic::new(
                        "E3011",
                        "future or task values cannot be stored in a tuple",
                        expression.span,
                    ));
                }
                if values
                    .iter()
                    .any(|value| contains_workspace(&value.value_type))
                {
                    return Err(Diagnostic::new(
                        "E3011",
                        "Workspace cannot be stored in a tuple",
                        expression.span,
                    ));
                }
                let value_type = if values.is_empty() {
                    ValueType::Unit
                } else {
                    ValueType::Tuple(
                        values
                            .iter()
                            .map(|value| value.value_type.clone())
                            .collect(),
                    )
                };
                let register = self.allocate(value_type.clone())?;
                if values.is_empty() {
                    let constant = self.global.constant(Constant::Unit)?;
                    self.code.push(Instruction::Const {
                        destination: register,
                        constant,
                    });
                    self.mir.push(MirOperation::Constant {
                        destination: u32::from(register),
                    });
                } else {
                    self.code.push(Instruction::TupleNew {
                        destination: register,
                        elements: values.iter().map(|value| value.register).collect(),
                    });
                    self.mir.push(MirOperation::Tuple {
                        destination: u32::from(register),
                    });
                }
                let effects = self.union_effects(values.iter().map(|value| value.effects));
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Tuple(values.into_iter().map(|value| value.hir).collect()),
                        None,
                        &value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Unary { operation, operand } => {
                let operand = self.compile_expression(operand)?;
                let value_type = match operation {
                    Unary::Not if operand.value_type == ValueType::Bool => ValueType::Bool,
                    Unary::Negate
                        if matches!(operand.value_type, ValueType::Int | ValueType::Float) =>
                    {
                        operand.value_type.clone()
                    }
                    Unary::Not => {
                        return Err(Diagnostic::new(
                            "E2003",
                            format!("operand of '!' must be Bool, found {}", operand.value_type),
                            expression.span,
                        ));
                    }
                    Unary::Negate => {
                        return Err(Diagnostic::new(
                            "E2003",
                            format!(
                                "operand of '-' must be Int or Float, found {}",
                                operand.value_type
                            ),
                            expression.span,
                        ));
                    }
                };
                let register = self.allocate(value_type.clone())?;
                self.code.push(match (operation, &value_type) {
                    (Unary::Not, _) => Instruction::BoolNot {
                        destination: register,
                        source: operand.register,
                    },
                    (Unary::Negate, ValueType::Int) => Instruction::IntNegate {
                        destination: register,
                        source: operand.register,
                    },
                    (Unary::Negate, ValueType::Float) => Instruction::FloatNegate {
                        destination: register,
                        source: operand.register,
                    },
                    _ => unreachable!("unary type was validated"),
                });
                self.mir.push(MirOperation::Binary {
                    destination: u32::from(register),
                });
                let effects = operand.effects;
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Unary(Box::new(operand.hir)),
                        None,
                        &value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Range {
                start,
                end,
                inclusive,
                operator_span,
            } => {
                let start = self.compile_expected(start, &ValueType::Int, "range start")?;
                let end = if start.value_type == ValueType::Never {
                    self.compile_without_runtime(|lowering| {
                        lowering.compile_expected(end, &ValueType::Int, "range end")
                    })?
                } else {
                    self.compile_expected(end, &ValueType::Int, "range end")?
                };
                let effects = self.union_effects([start.effects, end.effects]);
                if start.value_type == ValueType::Never || end.value_type == ValueType::Never {
                    let register = if start.value_type == ValueType::Never {
                        start.register
                    } else {
                        end.register
                    };
                    return Ok(CompiledExpr {
                        register,
                        value_type: ValueType::Never,
                        effects,
                        hir: self.hir(
                            HirExprKind::Range {
                                start: Box::new(start.hir),
                                end: Box::new(end.hir),
                                inclusive: *inclusive,
                            },
                            None,
                            &ValueType::Never,
                            effects,
                            expression.span,
                        ),
                    });
                }
                let register = self.allocate(ValueType::Range)?;
                self.code.push(Instruction::RangeNew {
                    destination: register,
                    start: start.register,
                    end: end.register,
                    inclusive: *inclusive,
                });
                self.mark_last_instruction(*operator_span);
                self.mir.push(MirOperation::Range {
                    destination: u32::from(register),
                    start: u32::from(start.register),
                    end: u32::from(end.register),
                    inclusive: *inclusive,
                });
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Range,
                    effects,
                    hir: self.hir(
                        HirExprKind::Range {
                            start: Box::new(start.hir),
                            end: Box::new(end.hir),
                            inclusive: *inclusive,
                        },
                        None,
                        &ValueType::Range,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Slice {
                collection,
                range,
                bracket_span,
            } => {
                let collection = self.compile_expression(collection)?;
                let result_type = match &collection.value_type {
                    ValueType::List(element) => {
                        ValueType::Option(Box::new(ValueType::List(element.clone())))
                    }
                    ValueType::Bytes => ValueType::Option(Box::new(ValueType::Bytes)),
                    ValueType::String => ValueType::Option(Box::new(ValueType::String)),
                    found => {
                        return Err(Diagnostic::new(
                            "E2009",
                            format!("cannot slice a value of type {found}"),
                            *bracket_span,
                        ));
                    }
                };
                let LoweredExprKind::Range {
                    start,
                    end,
                    inclusive,
                    ..
                } = &range.kind
                else {
                    unreachable!("slice syntax lowering always retains a literal range")
                };
                if *inclusive {
                    return Err(Diagnostic::new(
                        "E3010",
                        "slice ranges must be half-open; `..=` is not allowed",
                        range.span,
                    ));
                }
                let start = self.compile_expected(start, &ValueType::Int, "slice start")?;
                let end = self.compile_expected(end, &ValueType::Int, "slice end")?;
                let register = self.allocate(result_type.clone())?;
                self.code.push(Instruction::SliceGet {
                    destination: register,
                    collection: collection.register,
                    start: start.register,
                    end: end.register,
                });
                self.mark_last_instruction(*bracket_span);
                self.mir.push(MirOperation::Slice {
                    destination: u32::from(register),
                    collection: u32::from(collection.register),
                    start: u32::from(start.register),
                    end: u32::from(end.register),
                });
                let bounds_effects = self.union_effects([start.effects, end.effects]);
                let range_hir = self.hir(
                    HirExprKind::Range {
                        start: Box::new(start.hir),
                        end: Box::new(end.hir),
                        inclusive: false,
                    },
                    None,
                    &ValueType::Range,
                    bounds_effects,
                    range.span,
                );
                let effects = self.union_effects([collection.effects, bounds_effects]);
                Ok(CompiledExpr {
                    register,
                    value_type: result_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Slice {
                            collection: Box::new(collection.hir),
                            range: Box::new(range_hir),
                        },
                        None,
                        &result_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Index { collection, index } => {
                let collection = self.compile_expression(collection)?;
                let (index, value_type, tuple_index) = match &collection.value_type {
                    ValueType::List(element) => (
                        self.compile_expected(index, &ValueType::Int, "list index")?,
                        element.as_ref().clone(),
                        None,
                    ),
                    ValueType::Bytes => (
                        self.compile_expected(index, &ValueType::Int, "bytes index")?,
                        ValueType::Int,
                        None,
                    ),
                    ValueType::Map(key, value) => (
                        self.compile_expected(index, key, "map index")?,
                        value.as_ref().clone(),
                        None,
                    ),
                    ValueType::Tuple(elements) => {
                        let LoweredExprKind::Int(index_value) = index.kind else {
                            return Err(Diagnostic::new(
                                "E2009",
                                "tuple index must be a nonnegative integer literal",
                                index.span,
                            ));
                        };
                        let tuple_index = usize::try_from(index_value)
                            .ok()
                            .filter(|value| *value < elements.len())
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    "E2009",
                                    format!(
                                        "tuple index {index_value} is out of range for {} elements",
                                        elements.len()
                                    ),
                                    index.span,
                                )
                            })?;
                        (
                            self.compile_expression(index)?,
                            elements[tuple_index].clone(),
                            Some(u32::try_from(tuple_index).expect("tuple index fits")),
                        )
                    }
                    found => {
                        return Err(Diagnostic::new(
                            "E2009",
                            format!("cannot index a value of type {found}"),
                            expression.span,
                        ));
                    }
                };
                let register = self.allocate(value_type.clone())?;
                self.code.push(if let Some(tuple_index) = tuple_index {
                    Instruction::TupleGet {
                        destination: register,
                        tuple: collection.register,
                        index: tuple_index,
                    }
                } else {
                    Instruction::IndexGet {
                        destination: register,
                        collection: collection.register,
                        index: index.register,
                    }
                });
                self.mir.push(MirOperation::Binary {
                    destination: u32::from(register),
                });
                let effects = self.union_effects([collection.effects, index.effects]);
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Index {
                            collection: Box::new(collection.hir),
                            index: Box::new(index.hir),
                        },
                        None,
                        &value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Binary {
                operation: operation @ (Binary::And | Binary::Or),
                left,
                right,
            } => self.compile_short_circuit_binary(*operation, left, right, expression.span),
            LoweredExprKind::Binary {
                operation,
                left,
                right,
            } => {
                let left_span = left.span;
                let right_span = right.span;
                let left = self.compile_expression(left)?;
                if left.value_type == ValueType::Unknown {
                    return Err(Diagnostic::new(
                        "E2018",
                        "unknown must be narrowed before use in an operator",
                        left_span,
                    ));
                }
                let right_needs_expected_type = matches!(
                    &right.kind,
                    LoweredExprKind::Variable(name) if name == "None"
                ) || matches!(
                    &right.kind,
                    LoweredExprKind::Call { callee, .. }
                        if matches!(&callee.kind, LoweredExprKind::Variable(name)
                            if matches!(name.as_str(), "Some" | "Ok" | "Err"))
                );
                let right = if right_needs_expected_type {
                    self.compile_expected(right, &left.value_type, "binary operand")?
                } else {
                    self.compile_expression(right)?
                };
                if right.value_type == ValueType::Unknown {
                    return Err(Diagnostic::new(
                        "E2018",
                        "unknown must be narrowed before use in an operator",
                        right_span,
                    ));
                }
                if matches!(operation, Binary::Remainder) {
                    if left.value_type != ValueType::Int {
                        return Err(Diagnostic::new(
                            "E2003",
                            "remainder requires Int operands",
                            left_span,
                        ));
                    }
                    if right.value_type != ValueType::Int {
                        return Err(Diagnostic::new(
                            "E2003",
                            "remainder requires Int operands",
                            right_span,
                        ));
                    }
                }
                if left.value_type != right.value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "binary operands must have one exact type",
                        right_span,
                    ));
                }
                let value_type = match operation {
                    Binary::Add | Binary::Subtract | Binary::Multiply | Binary::Divide => {
                        if !matches!(left.value_type, ValueType::Int | ValueType::Float) {
                            return Err(Diagnostic::new(
                                "E2003",
                                "arithmetic requires Int or Float",
                                expression.span,
                            ));
                        }
                        left.value_type.clone()
                    }
                    Binary::Remainder => ValueType::Int,
                    Binary::Equal | Binary::NotEqual => {
                        if !left.value_type.is_equatable() {
                            return Err(Diagnostic::new(
                                "E3008",
                                format!("type {} does not satisfy Eq", left.value_type),
                                expression.span,
                            ));
                        }
                        ValueType::Bool
                    }
                    Binary::Less | Binary::LessEqual | Binary::Greater | Binary::GreaterEqual => {
                        if !left.value_type.is_ordered() {
                            return Err(Diagnostic::new(
                                "E2003",
                                "ordered comparison requires an ordered type",
                                expression.span,
                            ));
                        }
                        ValueType::Bool
                    }
                    Binary::And | Binary::Or => {
                        unreachable!("Boolean binary operations lower through branch control flow")
                    }
                };
                let register = self.allocate(value_type.clone())?;
                let instruction = match operation {
                    Binary::Add | Binary::Subtract | Binary::Multiply | Binary::Divide => {
                        let operation = match operation {
                            Binary::Add => NumericBinaryOp::Add,
                            Binary::Subtract => NumericBinaryOp::Subtract,
                            Binary::Multiply => NumericBinaryOp::Multiply,
                            Binary::Divide => NumericBinaryOp::Divide,
                            _ => unreachable!(),
                        };
                        if left.value_type == ValueType::Int {
                            Instruction::IntBinary {
                                destination: register,
                                left: left.register,
                                right: right.register,
                                operation,
                            }
                        } else {
                            Instruction::FloatBinary {
                                destination: register,
                                left: left.register,
                                right: right.register,
                                operation,
                            }
                        }
                    }
                    Binary::Remainder => Instruction::IntRemainder {
                        destination: register,
                        left: left.register,
                        right: right.register,
                    },
                    _ => Instruction::Compare {
                        destination: register,
                        left: left.register,
                        right: right.register,
                        operation: match operation {
                            Binary::Equal => CompareOp::Equal,
                            Binary::NotEqual => CompareOp::NotEqual,
                            Binary::Less => CompareOp::Less,
                            Binary::LessEqual => CompareOp::LessEqual,
                            Binary::Greater => CompareOp::Greater,
                            Binary::GreaterEqual => CompareOp::GreaterEqual,
                            _ => unreachable!(),
                        },
                    },
                };
                self.code.push(instruction);
                if *operation == Binary::Remainder {
                    self.mark_last_instruction(expression.span);
                }
                self.mir.push(MirOperation::Binary {
                    destination: u32::from(register),
                });
                let effects = self.union_effects([left.effects, right.effects]);
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Binary(vec![left.hir, right.hir]),
                        None,
                        &value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Compose {
                left,
                right,
                operator_span,
            } => {
                let _ = operator_span;
                let left = self.compile_expression(left)?;
                let right = self.compile_expression(right)?;
                let ValueType::Function {
                    parameters: left_parameters,
                    return_type: left_return,
                    effects: left_effects,
                } = &left.value_type
                else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "left composition operand must be an exact unary function",
                        expression.span,
                    ));
                };
                let ValueType::Function {
                    parameters: right_parameters,
                    return_type: right_return,
                    effects: right_effects,
                } = &right.value_type
                else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "right composition operand must be an exact unary function",
                        expression.span,
                    ));
                };
                if left_parameters.len() != 1 || right_parameters.len() != 1 {
                    return Err(Diagnostic::new(
                        "E3007",
                        "composition requires two unary function values",
                        expression.span,
                    ));
                }
                if left_return.as_ref() != &right_parameters[0] {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!(
                            "composition intermediate type {} does not match {}",
                            left_return, right_parameters[0]
                        ),
                        expression.span,
                    ));
                }
                if is_affine(&left.value_type) || is_affine(&right.value_type) {
                    return Err(Diagnostic::new(
                        "E3011",
                        "composition cannot capture an affine function value",
                        expression.span,
                    ));
                }
                let left_name = format!("__allen_compose_left_{}", self.global.allocate_symbol());
                let right_name = format!("__allen_compose_right_{}", self.global.allocate_symbol());
                let scope = self.current_scope();
                for (name, value) in [(&left_name, &left), (&right_name, &right)] {
                    self.bindings.insert(
                        name.clone(),
                        LocalBinding {
                            register: value.register,
                            symbol: self.global.allocate_symbol(),
                            value_type: value.value_type.clone(),
                            scope,
                            value_scope: scope,
                            mutable: false,
                            moved: false,
                        },
                    );
                }
                let parameter_name =
                    format!("__allen_compose_value_{}", self.global.allocate_symbol());
                let parameter_type =
                    self.lowered_type_for_short_closure(&left_parameters[0], expression.span)?;
                let return_type =
                    self.lowered_type_for_short_closure(right_return, expression.span)?;
                let variable = |name: String| LoweredExpr {
                    kind: LoweredExprKind::Variable(name),
                    span: expression.span,
                };
                let argument = |value: LoweredExpr| LoweredCallArgument {
                    label: None,
                    value,
                    placeholder: false,
                    trailing: false,
                    preceding_call_span: None,
                    span: expression.span,
                };
                let first_call = LoweredExpr {
                    kind: LoweredExprKind::Call {
                        callee: Box::new(variable(left_name)),
                        type_arguments: Vec::new(),
                        arguments: vec![argument(variable(parameter_name.clone()))],
                    },
                    span: expression.span,
                };
                let body_expression = LoweredExpr {
                    kind: LoweredExprKind::Call {
                        callee: Box::new(variable(right_name)),
                        type_arguments: Vec::new(),
                        arguments: vec![argument(first_call)],
                    },
                    span: expression.span,
                };
                let body = LoweredBody {
                    statements: Vec::new(),
                    tail: Some(body_expression),
                    span: expression.span,
                };
                let composed_effects = self.union_effects([*left_effects, *right_effects]);
                let declared_effects = self.global.effect_sets[composed_effects as usize].clone();
                let mut compiled = self.compile_closure(
                    &[(parameter_name, parameter_type, expression.span)],
                    &return_type,
                    Some(&declared_effects),
                    &body,
                    expression.span,
                )?;
                compiled.effects =
                    self.union_effects([left.effects, right.effects, compiled.effects]);
                compiled.hir.effects = compiled.effects;
                Ok(compiled)
            }
            LoweredExprKind::Pipe {
                left,
                stage,
                operator_span,
            } => {
                let _ = operator_span;
                let left_span = left.span;
                let left = self.compile_expression(left)?;
                let LoweredExprKind::Call {
                    callee,
                    type_arguments,
                    arguments,
                } = &stage.kind
                else {
                    return Err(Diagnostic::new(
                        "E3010",
                        "a pipeline stage must be a direct call",
                        stage.span,
                    ));
                };
                let placeholder_count = arguments
                    .iter()
                    .filter(|argument| argument.placeholder)
                    .count();
                if placeholder_count > 1 {
                    return Err(Diagnostic::new(
                        "E3010",
                        "a pipeline stage can contain at most one placeholder",
                        stage.span,
                    ));
                }
                let temporary = format!("__allen_pipe_value_{}", self.global.allocate_symbol());
                let scope = self.current_scope();
                self.bindings.insert(
                    temporary.clone(),
                    LocalBinding {
                        register: left.register,
                        symbol: self.global.allocate_symbol(),
                        value_type: left.value_type,
                        scope,
                        value_scope: scope,
                        mutable: false,
                        moved: false,
                    },
                );
                let inserted = LoweredExpr {
                    kind: LoweredExprKind::Variable(temporary),
                    span: left_span,
                };
                let mut expanded = arguments.clone();
                if placeholder_count == 1 {
                    let placeholder = expanded
                        .iter_mut()
                        .find(|argument| argument.placeholder)
                        .expect("one pipeline placeholder");
                    placeholder.placeholder = false;
                    placeholder.value = inserted;
                } else {
                    expanded.insert(
                        0,
                        LoweredCallArgument {
                            label: None,
                            value: inserted,
                            placeholder: false,
                            trailing: false,
                            preceding_call_span: None,
                            span: left_span,
                        },
                    );
                }
                let mut compiled =
                    self.compile_call(callee, type_arguments, &expanded, stage.span)?;
                compiled.effects = self.union_effects([left.effects, compiled.effects]);
                compiled.hir.effects = compiled.effects;
                Ok(compiled)
            }
            LoweredExprKind::Call {
                callee,
                type_arguments,
                arguments,
            } => self.compile_call(callee, type_arguments, arguments, expression.span),
            LoweredExprKind::Spawn(value) => self.compile_spawn(value, expression.span),
            LoweredExprKind::Await(value) => self.compile_await(value, expression.span),
            LoweredExprKind::AwaitBlock(body) => self.compile_await_block(body, expression.span),
            LoweredExprKind::Closure {
                parameters,
                return_type,
                declared_effects,
                body,
            } => self.compile_closure(
                parameters,
                return_type,
                declared_effects.as_deref(),
                body,
                expression.span,
            ),
            LoweredExprKind::ShortClosure { .. } => Err(Diagnostic::new(
                "E3011",
                "concise lambda requires one exact expected function type",
                expression.span,
            )),
        }
    }

    pub(super) fn compile_short_circuit_binary(
        &mut self,
        operation: Binary,
        left: &LoweredExpr,
        right: &LoweredExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let compiled_left = self.compile_expected(left, &ValueType::Bool, "Boolean operand")?;
        if compiled_left.value_type == ValueType::Never {
            let right = self.compile_without_runtime(|lowering| {
                lowering.compile_expected(right, &ValueType::Bool, "Boolean operand")
            })?;
            let effects = self.union_effects([compiled_left.effects, right.effects]);
            return Ok(CompiledExpr {
                register: compiled_left.register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::Binary(vec![compiled_left.hir, right.hir]),
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        let right_body = LoweredBody {
            statements: Vec::new(),
            tail: Some(right.clone()),
            span: right.span,
        };
        let literal_body = |value| LoweredBody {
            statements: Vec::new(),
            tail: Some(LoweredExpr {
                kind: LoweredExprKind::Bool(value),
                span,
            }),
            span,
        };
        let mut lowered = match operation {
            Binary::And => {
                let false_branch = LoweredElse::Body(Box::new(literal_body(false)));
                self.compile_if(
                    left,
                    &right_body,
                    Some(&false_branch),
                    span,
                    None,
                    Some(compiled_left),
                )?
            }
            Binary::Or => {
                let true_branch = literal_body(true);
                let false_branch = LoweredElse::Body(Box::new(right_body));
                self.compile_if(
                    left,
                    &true_branch,
                    Some(&false_branch),
                    span,
                    None,
                    Some(compiled_left),
                )?
            }
            _ => unreachable!("only Boolean short-circuit operations are lowered here"),
        };
        let HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = lowered.hir.kind
        else {
            unreachable!("short-circuit lowering produces conditional HIR")
        };
        let right = match operation {
            Binary::And => *then_branch,
            Binary::Or => *else_branch.expect("short-circuit lowering always has an else branch"),
            _ => unreachable!("only Boolean short-circuit operations are lowered here"),
        };
        lowered.hir = self.hir(
            HirExprKind::Binary(vec![*condition, right]),
            None,
            &lowered.value_type,
            lowered.effects,
            span,
        );
        Ok(lowered)
    }

    pub(super) fn compile_spawn(
        &mut self,
        value: &LoweredExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let future = self.compile_expression(value)?;
        let ValueType::Future(result_type) = &future.value_type else {
            return Err(Diagnostic::new(
                "E3011",
                format!("spawn requires Future<T>, found {}", future.value_type),
                span,
            ));
        };
        self.consume_ownership(future.register, MirOwnershipState::Moved);
        let value_type = ValueType::Task(result_type.clone());
        let register = self.allocate(value_type.clone())?;
        let scope = self.active_scopes.last().copied().unwrap_or(0);
        self.code.push(Instruction::Spawn {
            destination: register,
            future: future.register,
            scope,
        });
        self.mir.push(MirOperation::Spawn {
            destination: u32::from(register),
            future: u32::from(future.register),
            scope,
        });
        self.record_ownership(register, scope, MirOwnershipState::Live, true);
        let spawn_effects = effect_id(&self.global.effect_sets, &["task.spawn".to_owned()]);
        let effects = self.union_effects([future.effects, spawn_effects]);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Spawn {
                    future: Box::new(future.hir),
                    scope,
                },
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_observed_task(
        &mut self,
        expression: &LoweredExpr,
    ) -> Result<CompiledExpr, Diagnostic> {
        let LoweredExprKind::Variable(name) = &expression.kind else {
            return self.compile_expression(expression);
        };
        let binding = self.bindings.get(name).cloned().ok_or_else(|| {
            Diagnostic::new(
                "E3005",
                format!("unknown local value '{name}'"),
                expression.span,
            )
        })?;
        if is_affine(&binding.value_type) && binding.moved {
            return Err(Diagnostic::new(
                "E3011",
                format!("use of moved {} value '{name}'", binding.value_type),
                expression.span,
            ));
        }
        let effects = self.empty_effects();
        Ok(CompiledExpr {
            register: binding.register,
            value_type: binding.value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Variable,
                Some(binding.symbol),
                &binding.value_type,
                effects,
                expression.span,
            ),
        })
    }

    pub(super) fn compile_task_snapshot(
        &mut self,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if arguments.len() != 1 {
            return Err(Diagnostic::new(
                "E3011",
                "allen.internal.task_snapshot requires exactly one Task<T> argument",
                span,
            ));
        }
        let source = self.compile_observed_task(&arguments[0])?;
        if !matches!(&source.value_type, ValueType::Task(_)) {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "allen.internal.task_snapshot requires Task<T>, found {}",
                    source.value_type
                ),
                arguments[0].span,
            ));
        }
        let value_type = task_snapshot_type();
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::TaskSnapshot {
            destination: register,
            source: source.register,
        });
        self.mir.push(MirOperation::TaskSnapshot {
            destination: u32::from(register),
            source: u32::from(source.register),
        });
        let inspect_effects = effect_id(&self.global.effect_sets, &["debug.inspect".to_owned()]);
        let effects = self.union_effects([source.effects, inspect_effects]);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::TaskSnapshot(Box::new(source.hir)),
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_workspace_get(
        &mut self,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if !arguments.is_empty() {
            return Err(Diagnostic::new(
                "E3011",
                "fs.workspace requires no arguments",
                span,
            ));
        }
        let value_type = ValueType::Workspace;
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::WorkspaceGet {
            destination: register,
        });
        self.mir.push(MirOperation::WorkspaceGet {
            destination: u32::from(register),
        });
        let effects = self.empty_effects();
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(HirExprKind::WorkspaceGet, None, &value_type, effects, span),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_effect_call(
        &mut self,
        operation: EffectOperation,
        type_arguments: &[LoweredType],
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if matches!(
            operation,
            EffectOperation::AgentAsk
                | EffectOperation::ModelRequest
                | EffectOperation::UserAsk
                | EffectOperation::SubAgentRun
                | EffectOperation::SubAgentAsk
        ) {
            let request_index = usize::from(operation == EffectOperation::SubAgentAsk);
            let expected_arguments = if operation == EffectOperation::SubAgentRun {
                2
            } else {
                request_index + 1
            };
            if arguments.len() != expected_arguments {
                return Err(Diagnostic::new(
                    "E3011",
                    "typed request has the wrong argument count",
                    span,
                ));
            }
            let mut values = Vec::with_capacity(arguments.len());
            for argument in arguments {
                values.push(self.compile_expression(argument)?);
            }
            if operation == EffectOperation::SubAgentAsk
                && values[0].value_type != ValueType::SubAgent
            {
                return Err(Diagnostic::new(
                    "E3011",
                    format!(
                        "sub_agent.ask expected SubAgent, found {}",
                        values[0].value_type
                    ),
                    arguments[0].span,
                ));
            }
            if operation == EffectOperation::SubAgentRun
                && values[1].value_type != sub_agent_projection_type()
            {
                return Err(Diagnostic::new(
                    "E3011",
                    "sub_agent.run requires the exact authority projection record",
                    arguments[1].span,
                ));
            }
            let value = &values[request_index];
            let result = if let Some(output) = prompt_output_type(&value.value_type) {
                output.clone()
            } else {
                return Err(Diagnostic::new(
                    "E3011",
                    "typed request requires Prompt<T>",
                    arguments[request_index].span,
                ));
            };
            if let Some(type_argument) = type_arguments.first() {
                let SemanticType::Value(expected) = semantic_type(
                    type_argument,
                    &BTreeSet::new(),
                    &self.info.module,
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                )?
                else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "typed request response type must be concrete",
                        type_argument.span(),
                    ));
                };
                if expected != result {
                    return Err(Diagnostic::new(
                        "E3010",
                        format!(
                            "typed request declares response {expected}, but its prompt produces {result}"
                        ),
                        type_argument.span(),
                    ));
                }
            }
            let error = match operation {
                EffectOperation::AgentAsk => agent_error_type(),
                EffectOperation::ModelRequest => model_error_type(),
                EffectOperation::UserAsk => user_error_type(),
                EffectOperation::SubAgentRun | EffectOperation::SubAgentAsk => {
                    sub_agent_error_type()
                }
                _ => unreachable!("guarded typed request"),
            };
            let result = ValueType::Result(Box::new(result), Box::new(error));
            let value_type = ValueType::Future(Box::new(result));
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::EffectCall {
                destination: register,
                operation,
                arguments: values.iter().map(|value| value.register).collect(),
            });
            self.mir.push(MirOperation::EffectCall {
                destination: u32::from(register),
                operation,
                arguments: values
                    .iter()
                    .map(|value| u32::from(value.register))
                    .collect(),
            });
            self.record_ownership(register, 0, MirOwnershipState::Live, true);
            let call_effect = effect_id(
                &self.global.effect_sets,
                &[operation.required_effect().to_owned()],
            );
            let effects = self.union_effects(
                values
                    .iter()
                    .map(|value| value.effects)
                    .chain(std::iter::once(call_effect)),
            );
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::EffectCall {
                        operation,
                        arguments: values.into_iter().map(|value| value.hir).collect(),
                    },
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }
        if matches!(
            operation,
            EffectOperation::SubAgentCreate | EffectOperation::SubAgentMessage
        ) {
            if !type_arguments.is_empty() {
                return Err(Diagnostic::new(
                    "E3005",
                    "this sub_agent operation does not take type arguments",
                    span,
                ));
            }
            if arguments.len() != 2 {
                return Err(Diagnostic::new(
                    "E3011",
                    "sub_agent operation requires exactly two arguments",
                    span,
                ));
            }
            let values = arguments
                .iter()
                .map(|argument| self.compile_expression(argument))
                .collect::<Result<Vec<_>, _>>()?;
            let valid = match operation {
                EffectOperation::SubAgentCreate => {
                    prompt_output_type(&values[0].value_type) == Some(&ValueType::Unit)
                        && values[1].value_type == sub_agent_projection_type()
                }
                EffectOperation::SubAgentMessage => {
                    values[0].value_type == ValueType::SubAgent
                        && values[1].value_type == ValueType::String
                }
                _ => unreachable!(),
            };
            if !valid {
                return Err(Diagnostic::new(
                    "E3011",
                    "sub_agent operation arguments do not match its exact signature",
                    span,
                ));
            }
            let result = if operation == EffectOperation::SubAgentCreate {
                ValueType::SubAgent
            } else {
                ValueType::Unit
            };
            let result = ValueType::Result(Box::new(result), Box::new(sub_agent_error_type()));
            let value_type = ValueType::Future(Box::new(result));
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::EffectCall {
                destination: register,
                operation,
                arguments: values.iter().map(|value| value.register).collect(),
            });
            self.mir.push(MirOperation::EffectCall {
                destination: u32::from(register),
                operation,
                arguments: values
                    .iter()
                    .map(|value| u32::from(value.register))
                    .collect(),
            });
            self.record_ownership(register, 0, MirOwnershipState::Live, true);
            let call_effect = effect_id(
                &self.global.effect_sets,
                &[operation.required_effect().to_owned()],
            );
            let effects = self.union_effects(
                values
                    .iter()
                    .map(|value| value.effects)
                    .chain(std::iter::once(call_effect)),
            );
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::EffectCall {
                        operation,
                        arguments: values.into_iter().map(|value| value.hir).collect(),
                    },
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }
        let (parameters, result, effect, label) =
            effect_operation_signature(operation, self.global.bundle.transcript_part)
                .expect("transcript operation has a synthetic transcript part enum");
        if arguments.len() != parameters.len() {
            return Err(Diagnostic::new(
                "E3011",
                format!("{label} has the wrong argument count"),
                span,
            ));
        }
        if operation == EffectOperation::AgentTranscript {
            if let LoweredExprKind::Record { fields, .. } = &arguments[0].kind {
                if let Some((
                    _,
                    LoweredExpr {
                        kind: LoweredExprKind::Int(limit),
                        ..
                    },
                    _,
                )) = fields.iter().find(|(name, _, _)| name == "limit")
                {
                    if !(1..=100).contains(limit) {
                        return Err(Diagnostic::new(
                            "E3011",
                            "agent.transcript limit must be from 1 through 100",
                            arguments[0].span,
                        ));
                    }
                }
            }
        }
        let values = if operation == EffectOperation::AgentTranscript {
            arguments
                .iter()
                .zip(&parameters)
                .map(|(argument, parameter)| {
                    self.compile_expected(argument, parameter, "agent.transcript query")
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            arguments
                .iter()
                .map(|argument| self.compile_expression(argument))
                .collect::<Result<Vec<_>, _>>()?
        };
        for (value, parameter) in values.iter().zip(&parameters) {
            if value.value_type != *parameter {
                return Err(Diagnostic::new(
                    "E3011",
                    format!("{label} expected {parameter}, found {}", value.value_type),
                    span,
                ));
            }
        }
        let value_type = ValueType::Future(Box::new(result));
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::EffectCall {
            destination: register,
            operation,
            arguments: values.iter().map(|value| value.register).collect(),
        });
        self.mir.push(MirOperation::EffectCall {
            destination: u32::from(register),
            operation,
            arguments: values
                .iter()
                .map(|value| u32::from(value.register))
                .collect(),
        });
        self.record_ownership(register, 0, MirOwnershipState::Live, true);
        let call_effect = effect_id(&self.global.effect_sets, &[effect.to_owned()]);
        let effects = self.union_effects(
            values
                .iter()
                .map(|value| value.effects)
                .chain(std::iter::once(call_effect)),
        );
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::EffectCall {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_await(
        &mut self,
        value: &LoweredExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if !self.info.lowered.is_async {
            return Err(Diagnostic::new(
                "E3011",
                "await requires an async function",
                span,
            ));
        }
        let source = self.compile_expression(value)?;
        let value_type = match &source.value_type {
            ValueType::Future(value) | ValueType::Task(value) => value.as_ref().clone(),
            _ => {
                return Err(Diagnostic::new(
                    "E3011",
                    format!(
                        "await requires Future<T> or Task<T>, found {}",
                        source.value_type
                    ),
                    span,
                ));
            }
        };
        self.consume_ownership(source.register, MirOwnershipState::Awaited);
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::Await {
            destination: register,
            source: source.register,
        });
        if is_affine(&value_type) {
            let scope = if matches!(value_type, ValueType::Task(_)) {
                self.active_scopes.last().copied().unwrap_or(0)
            } else {
                0
            };
            self.record_ownership(register, scope, MirOwnershipState::Live, true);
        }
        if contains_stored_sub_agent(&value_type) {
            self.sub_agent_value_scopes
                .insert(register, self.current_scope());
        }
        let suspension = self.next_mir_block();
        let resume = suspension + 1;
        let exceptional_cancel = suspension + 2;
        let timeout_cancel = suspension + 3;
        let external_cancel = suspension + 4;
        let permanent_stop = suspension + 5;
        self.mir_suspensions.push(MirSuspension {
            destination: u32::from(register),
            source: u32::from(source.register),
            resume,
            exceptional_cancel,
            timeout_cancel,
            external_cancel,
            permanent_stop,
        });
        self.mir_blocks.push(MirBlock {
            operations: vec![MirOperation::Await {
                destination: u32::from(register),
                source: u32::from(source.register),
            }],
            terminator: MirTerminator::Suspend {
                destination: u32::from(register),
                source: u32::from(source.register),
                resume,
                exceptional_cancel,
                timeout_cancel,
                external_cancel,
                permanent_stop,
            },
        });
        self.mir_blocks.push(MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        });
        for kind in [
            MirCleanupKind::ExceptionalCancel,
            MirCleanupKind::TimeoutCancel,
            MirCleanupKind::ExternalCancel,
            MirCleanupKind::PermanentStop,
        ] {
            let operations = self.cleanup_operations(kind);
            self.mir_blocks.push(MirBlock {
                operations,
                terminator: MirTerminator::Unreachable,
            });
        }
        self.register_mir_region(suspension, resume);
        let effects = source.effects;
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Await(Box::new(source.hir)),
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_await_block(
        &mut self,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if !self.info.lowered.is_async {
            return Err(Diagnostic::new(
                "E3011",
                "await block requires an async function",
                span,
            ));
        }
        let scope = self.next_scope;
        self.next_scope = self
            .next_scope
            .checked_add(1)
            .ok_or_else(|| Diagnostic::new("E3011", "too many task scopes", span))?;
        self.code.push(Instruction::TaskScopeEnter { scope });
        self.mir.push(MirOperation::TaskScopeEnter { scope });
        self.active_scopes.push(scope);
        let outer_bindings = self.bindings.clone();
        let body_mir_block_start = self.mir_blocks.len();
        let body_mir_entry_start = self.mir_entries.len();
        let (body_hir, result, mut returns_from_function) = self.compile_block_value(body)?;
        let result_value_scope = self.sub_agent_value_scope(&result);
        let outer_scope = self.active_scopes.iter().rev().nth(1).copied().unwrap_or(0);
        if contains_stored_sub_agent(&result.value_type)
            && !self.scope_outlives(result_value_scope, outer_scope)
        {
            return Err(Diagnostic::new(
                "E3011",
                "SubAgent-containing value cannot escape an await block",
                body.tail.as_ref().map_or(body.span, |tail| tail.span),
            ));
        }
        returns_from_function |= result.value_type == ValueType::Never
            && matches!(self.code.last(), Some(Instruction::Return { .. }));
        self.active_scopes.pop();
        let exits_through_scope = result.value_type != ValueType::Never
            || returns_from_function
            || matches!(self.code.last(), Some(Instruction::Jump { .. }));
        if matches!(result.value_type, ValueType::Task(_))
            && self
                .ownership_states
                .get(&result.register)
                .is_some_and(|ownership| ownership.scope == scope)
        {
            return Err(Diagnostic::new(
                "E3011",
                "task ownership cannot escape an await block",
                span,
            ));
        }
        let joined = self
            .ownership_states
            .iter()
            .filter_map(|(register, ownership)| {
                (ownership.scope == scope && ownership.state == MirOwnershipState::Live)
                    .then_some(*register)
            })
            .collect::<Vec<_>>();
        for register in joined {
            let nested_task_result = matches!(
                &self.registers[register as usize],
                ValueType::Task(result) if is_affine(result)
            );
            if nested_task_result
                || (matches!(self.registers[register as usize], ValueType::Future(_))
                    && self.must_consume(register))
            {
                return Err(Diagnostic::new(
                    "E3011",
                    "nested affine result must be awaited before scope exit",
                    span,
                ));
            }
            if matches!(self.registers[register as usize], ValueType::Task(_)) {
                self.record_ownership(register, scope, MirOwnershipState::ScopeJoined, true);
            }
        }
        if result.value_type != ValueType::Never {
            self.code.push(Instruction::TaskScopeExit { scope });
            self.mark_last_instruction(span);
        }
        let normal_join = self.next_mir_block();
        let exceptional_cancel = normal_join + 1;
        let timeout_cancel = normal_join + 2;
        let external_cancel = normal_join + 3;
        let permanent_stop = normal_join + 4;
        self.mir_task_scopes.push(MirTaskScope {
            scope,
            normal_join,
            exceptional_cancel,
            timeout_cancel,
            external_cancel,
            permanent_stop,
        });
        let exit_dispatch = normal_join;
        let normal_cleanup = exit_dispatch + 1;
        let exceptional_cancel = exit_dispatch + 2;
        let timeout_cancel = exit_dispatch + 3;
        let external_cancel = exit_dispatch + 4;
        let permanent_stop = exit_dispatch + 5;
        let continuation = exit_dispatch + 6;
        if let Some(metadata) = self.mir_task_scopes.last_mut() {
            metadata.normal_join = normal_cleanup;
            metadata.exceptional_cancel = exceptional_cancel;
            metadata.timeout_cancel = timeout_cancel;
            metadata.external_cancel = external_cancel;
            metadata.permanent_stop = permanent_stop;
        }
        self.mir_blocks.push(MirBlock {
            operations: exits_through_scope
                .then_some(MirOperation::TaskScopeExit { scope })
                .into_iter()
                .collect(),
            terminator: MirTerminator::TaskScopeExit {
                scope,
                normal_join: normal_cleanup,
                exceptional_cancel,
                timeout_cancel,
                external_cancel,
                permanent_stop,
            },
        });
        self.mir_blocks.push(MirBlock {
            operations: vec![MirOperation::TaskScopeCleanup {
                scope,
                kind: MirCleanupKind::NormalJoin,
            }],
            terminator: MirTerminator::Goto {
                target: continuation,
            },
        });
        for kind in [
            MirCleanupKind::ExceptionalCancel,
            MirCleanupKind::TimeoutCancel,
            MirCleanupKind::ExternalCancel,
            MirCleanupKind::PermanentStop,
        ] {
            self.mir_blocks.push(MirBlock {
                operations: vec![MirOperation::TaskScopeCleanup { scope, kind }],
                terminator: if kind == MirCleanupKind::PermanentStop
                    && result.value_type == ValueType::Never
                    && !returns_from_function
                {
                    match self.code.last() {
                        Some(Instruction::Stop { reason }) => MirTerminator::Stop {
                            reason: u32::from(*reason),
                        },
                        _ => MirTerminator::Unreachable,
                    }
                } else {
                    MirTerminator::Unreachable
                },
            });
        }
        self.mir_blocks.push(MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        });
        let terminal_terminator = if result.value_type == ValueType::Never {
            if returns_from_function {
                Some(MirTerminator::Return {
                    source: u32::from(result.register),
                })
            } else if let Some(Instruction::Stop { reason }) = self.code.last() {
                Some(MirTerminator::Stop {
                    reason: u32::from(*reason),
                })
            } else {
                None
            }
        } else {
            None
        };
        let terminal_child_region = self.mir_tail.is_none()
            && self.mir_entries.len() > body_mir_entry_start
            && result.value_type == ValueType::Never;
        let mut emitted_scope_region = true;
        if terminal_child_region {
            let mut rewired = false;
            for block in &mut self.mir_blocks[body_mir_block_start..exit_dispatch as usize - 1] {
                if matches!(
                    (&block.terminator, &terminal_terminator),
                    (
                        MirTerminator::Return { .. },
                        Some(MirTerminator::Return { .. })
                    ) | (MirTerminator::Stop { .. }, Some(MirTerminator::Stop { .. }))
                ) {
                    block.terminator = MirTerminator::Goto {
                        target: exit_dispatch,
                    };
                    rewired = true;
                }
            }
            if !rewired {
                self.mir_blocks.truncate(exit_dispatch as usize - 1);
                self.mir_task_scopes.pop();
                emitted_scope_region = false;
            }
        } else {
            self.register_mir_region(exit_dispatch, continuation);
        }
        if terminal_child_region && emitted_scope_region && terminal_terminator.is_some() {
            self.mir_blocks[continuation as usize - 1].terminator =
                terminal_terminator.expect("terminal await block has a terminator");
        } else if terminal_terminator.is_some() && !terminal_child_region {
            self.set_mir_handoff(
                continuation,
                terminal_terminator.expect("terminal await block has a terminator"),
            );
            self.mir_tail = None;
        }
        let mut restored = outer_bindings;
        for (name, binding) in &mut restored {
            if let Some(inner) = self.bindings.get(name) {
                binding.moved |= inner.moved;
                binding.value_scope = inner.value_scope;
            }
        }
        self.bindings = restored;
        let effects = result.effects;
        if returns_from_function {
            if result.value_type != ValueType::Never && result.value_type != self.return_type {
                return Err(Diagnostic::new(
                    "E3007",
                    format!(
                        "function returns {}, expected {}",
                        result.value_type, self.return_type
                    ),
                    body.span,
                ));
            }
            if result.value_type != ValueType::Never {
                self.prepare_return(&result, body.span)?;
                self.code.push(Instruction::Return {
                    source: result.register,
                });
            }
            return Ok(CompiledExpr {
                register: result.register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::AwaitBlock {
                        scope,
                        body: Box::new(body_hir),
                    },
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        Ok(CompiledExpr {
            register: result.register,
            value_type: result.value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::AwaitBlock {
                    scope,
                    body: Box::new(body_hir),
                },
                None,
                &result.value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_block_value(
        &mut self,
        body: &LoweredBody,
    ) -> Result<(HirExpr, CompiledExpr, bool), Diagnostic> {
        self.compile_block_value_expected(body, None)
    }

    #[allow(clippy::too_many_lines)]
    fn compile_block_value_expected(
        &mut self,
        body: &LoweredBody,
        expected: Option<(&ValueType, &str)>,
    ) -> Result<(HirExpr, CompiledExpr, bool), Diagnostic> {
        let outer_local_functions = self.local_functions.clone();
        let mut expressions = Vec::new();
        let mut result = None;
        let mut returns_from_function = false;
        let mut runtime_falls_through = true;
        for (index, statement) in body.statements.iter().enumerate() {
            match statement {
                LoweredStatement::Let {
                    name,
                    name_span,
                    mutable,
                    annotation,
                    value,
                } => {
                    if self.bindings.contains_key(name) || self.local_name_conflicts(name) {
                        return Err(Diagnostic::new(
                            "E3005",
                            format!("duplicate local binding '{name}'"),
                            *name_span,
                        ));
                    }
                    let value = if let Some(annotation) = annotation {
                        let expected = self.annotation_type(annotation)?;
                        self.compile_expected(value, &expected, "binding")?
                    } else {
                        self.compile_expression(value)?
                    };
                    let value_scope = self.sub_agent_value_scope(&value);
                    if *mutable && is_affine(&value.value_type) {
                        return Err(Diagnostic::new(
                            "E3011",
                            "future or task values cannot use mutable bindings",
                            *name_span,
                        ));
                    }
                    let symbol = self.global.allocate_symbol();
                    let scope = self.active_scopes.last().copied().unwrap_or(0);
                    self.bindings.insert(
                        name.clone(),
                        LocalBinding {
                            register: value.register,
                            symbol,
                            value_type: value.value_type.clone(),
                            scope,
                            value_scope,
                            mutable: *mutable,
                            moved: false,
                        },
                    );
                    let terminates = value.value_type == ValueType::Never;
                    runtime_falls_through &= self.runtime_falls_through(&value);
                    expressions.push(value.hir.clone());
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                *name_span,
                            ));
                        }
                        result = Some(value);
                        returns_from_function = true;
                    }
                }
                LoweredStatement::Assignment {
                    name,
                    name_span,
                    operation,
                    value,
                } => {
                    let assignment =
                        self.compile_assignment(name, *name_span, *operation, value)?;
                    let terminates = assignment.value_type == ValueType::Never;
                    runtime_falls_through &= self.runtime_falls_through(&assignment);
                    expressions.push(assignment.hir.clone());
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                value.span,
                            ));
                        }
                        result = Some(assignment);
                    }
                }
                LoweredStatement::ControlFlow(expression) => {
                    let value = self.compile_expression(expression)?;
                    if !matches!(value.value_type, ValueType::Unit | ValueType::Never) {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!(
                                "control-flow statement must have type Void, found {}",
                                value.value_type
                            ),
                            expression.span,
                        ));
                    }
                    let terminates = value.value_type == ValueType::Never;
                    runtime_falls_through &= self.runtime_falls_through(&value);
                    expressions.push(value.hir.clone());
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                expression.span,
                            ));
                        }
                        result = Some(value);
                        returns_from_function =
                            matches!(self.code.last(), Some(Instruction::Return { .. }));
                    }
                }
                LoweredStatement::Return(value, statement_span) => {
                    if index + 1 != body.statements.len() || body.tail.is_some() {
                        return Err(Diagnostic::new(
                            "E3005",
                            "code after return is unreachable",
                            *statement_span,
                        ));
                    }
                    let value = self.compile_return(value.as_ref(), *statement_span)?;
                    runtime_falls_through = false;
                    expressions.push(value.hir.clone());
                    result = Some(value);
                    returns_from_function = true;
                }
                LoweredStatement::While {
                    condition,
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_while(condition, loop_body, *span)?;
                    runtime_falls_through &= value.falls_through;
                    let effects = value.hir.effects;
                    expressions.push(value.hir.clone());
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after non-terminating loop is unreachable",
                                *span,
                            ));
                        }
                        result = Some(CompiledExpr {
                            register: value.register,
                            value_type: ValueType::Never,
                            effects,
                            hir: value.hir,
                        });
                    }
                }
                LoweredStatement::Loop {
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_infinite_loop(loop_body, *span)?;
                    runtime_falls_through &= value.falls_through;
                    let effects = value.hir.effects;
                    expressions.push(value.hir.clone());
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after non-terminating loop is unreachable",
                                *span,
                            ));
                        }
                        result = Some(CompiledExpr {
                            register: value.register,
                            value_type: ValueType::Never,
                            effects,
                            hir: value.hir,
                        });
                    }
                }
                LoweredStatement::For {
                    binding,
                    source,
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_for(binding, source, loop_body, *span)?;
                    runtime_falls_through &= value.falls_through;
                    let effects = value.hir.effects;
                    expressions.push(value.hir.clone());
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating loop header is unreachable",
                                *span,
                            ));
                        }
                        result = Some(CompiledExpr {
                            register: value.register,
                            value_type: ValueType::Never,
                            effects,
                            hir: value.hir,
                        });
                    }
                }
                LoweredStatement::LocalFunction(function) => {
                    self.compile_local_function_declaration(function)?;
                }
                LoweredStatement::Break(span) | LoweredStatement::Continue(span) => {
                    let value = self.compile_loop_control(
                        matches!(statement, LoweredStatement::Break(_)),
                        *span,
                    )?;
                    runtime_falls_through = false;
                    if index + 1 != body.statements.len() || body.tail.is_some() {
                        return Err(Diagnostic::new(
                            "E3005",
                            "code after loop control is unreachable",
                            *span,
                        ));
                    }
                    expressions.push(value.hir.clone());
                    result = Some(value);
                }
            }
        }
        let result = if let Some(result) = result {
            result
        } else if let Some(tail) = &body.tail {
            match expected {
                Some((expected, label)) => {
                    self.compile_contextually_expected(tail, expected, label)?
                }
                None => self.compile_expression(tail)?,
            }
        } else {
            let unit = LoweredExpr {
                kind: LoweredExprKind::Tuple(Vec::new()),
                span: body.span,
            };
            self.compile_expression(&unit)?
        };
        runtime_falls_through &= self.runtime_falls_through(&result);
        if !runtime_falls_through {
            self.runtime_terminal_values.insert(result.register);
        }
        if !returns_from_function {
            expressions.push(result.hir.clone());
        }
        let effects = self.union_effects(
            expressions
                .iter()
                .map(|expression| expression.effects)
                .chain(std::iter::once(result.effects)),
        );
        let value_type = result.value_type.clone();
        let hir = self.hir(
            HirExprKind::Block(expressions),
            None,
            &value_type,
            effects,
            body.span,
        );
        self.local_functions = outer_local_functions;
        Ok((hir, result, returns_from_function))
    }

    pub(super) fn compile_return(
        &mut self,
        expression: Option<&LoweredExpr>,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let value = if let Some(expression) = expression {
            self.compile_expected(expression, &self.return_type.clone(), "return")?
        } else {
            if self.return_type != ValueType::Unit {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("bare return requires Void, found {}", self.return_type),
                    span,
                ));
            }
            self.compile_expression(&LoweredExpr {
                kind: LoweredExprKind::Unit,
                span,
            })?
        };
        if value.value_type == ValueType::Never {
            return Ok(value);
        }

        for scope in self.active_scopes.iter().rev().copied().collect::<Vec<_>>() {
            let live = self
                .ownership_states
                .iter()
                .filter_map(|(register, ownership)| {
                    (ownership.scope == scope && ownership.state == MirOwnershipState::Live)
                        .then_some(*register)
                })
                .collect::<Vec<_>>();
            for register in live {
                let nested_task_result = matches!(
                    &self.registers[register as usize],
                    ValueType::Task(result) if is_affine(result)
                );
                let hidden_future_obligation =
                    matches!(self.registers[register as usize], ValueType::Future(_))
                        && self.must_consume(register);
                if nested_task_result || hidden_future_obligation {
                    return Err(Diagnostic::new(
                        "E3011",
                        "nested affine result must be awaited before scope exit",
                        span,
                    ));
                }
                self.record_ownership(register, scope, MirOwnershipState::ScopeJoined, true);
            }
            self.code.push(Instruction::TaskScopeExit { scope });
            self.mir.push(MirOperation::TaskScopeExit { scope });
            self.mir.push(MirOperation::TaskScopeCleanup {
                scope,
                kind: MirCleanupKind::NormalJoin,
            });
            self.invalidate_scope_local_sub_agents(scope);
        }
        self.prepare_return(&value, span)?;
        self.code.push(Instruction::Return {
            source: value.register,
        });
        self.mark_last_instruction(span);
        let effects = value.effects;
        Ok(CompiledExpr {
            register: value.register,
            value_type: ValueType::Never,
            effects,
            hir: self.hir(
                HirExprKind::Return(Box::new(value.hir)),
                None,
                &ValueType::Never,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_field_get(
        &mut self,
        record: &LoweredExpr,
        field: &str,
        field_span: Span,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let record = self.compile_expression(record)?;
        self.compile_field_get_compiled(record, field, field_span, span)
    }

    fn compile_field_get_compiled(
        &mut self,
        record: CompiledExpr,
        field: &str,
        field_span: Span,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if let ValueType::Newtype { underlying, .. } = &record.value_type {
            if field != "value" {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("newtype has no field '{field}'"),
                    field_span,
                ));
            }
            let value_type = underlying.as_ref().clone();
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::NewtypeUnwrap {
                destination: register,
                source: record.register,
            });
            self.mir.push(MirOperation::NewtypeUnwrap {
                destination: u32::from(register),
                source: u32::from(record.register),
            });
            let effects = record.effects;
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::NewtypeUnwrap(Box::new(record.hir)),
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }
        let ValueType::Record(layout) = &record.value_type else {
            return Err(Diagnostic::new(
                "E3007",
                "field access requires a record value",
                field_span,
            ));
        };
        let index = layout
            .iter()
            .position(|candidate| candidate.name == field)
            .ok_or_else(|| {
                Diagnostic::new(
                    "E3007",
                    format!("record has no field '{field}'"),
                    field_span,
                )
            })?;
        let value_type = layout[index].value_type.clone();
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::FieldGet {
            destination: register,
            record: record.register,
            field: u32::try_from(index).expect("field index fits"),
        });
        self.mir.push(MirOperation::FieldGet {
            destination: u32::from(register),
            record: u32::from(record.register),
        });
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects: record.effects,
            hir: self.hir(
                HirExprKind::FieldGet(Box::new(record.hir)),
                None,
                &value_type,
                record.effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn compile_record_update(
        &mut self,
        expression: &LoweredExpr,
        name: &str,
        base: &LoweredExpr,
        spread_span: Span,
        fields: &[(String, LoweredExpr, Span)],
    ) -> Result<CompiledExpr, Diagnostic> {
        let base = self.compile_expression(base)?;
        let ValueType::Record(_) = &base.value_type else {
            return Err(Diagnostic::new(
                "E3007",
                "record update base must be a record value",
                spread_span,
            ));
        };
        let expected_type = if name == "$anonymous" {
            base.value_type.clone()
        } else {
            resolve_named_type(
                &self.global.bundle.modules,
                &self.global.bundle.types,
                &self.info.module,
                name,
                expression.span,
            )?
        };
        if expected_type != base.value_type {
            return Err(Diagnostic::new(
                "E3007",
                "record update base has a different record type",
                spread_span,
            ));
        }
        let ValueType::Record(layout) = &expected_type else {
            return Err(Diagnostic::new(
                "E3007",
                "record update target must be a record type",
                expression.span,
            ));
        };
        let mut replacements = BTreeMap::new();
        for (field, value, field_span) in fields {
            if replacements.contains_key(field) {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("duplicate record field '{field}'"),
                    *field_span,
                ));
            }
            let index = layout
                .iter()
                .position(|candidate| candidate.name == *field)
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E3007",
                        format!("record has no field '{field}'"),
                        *field_span,
                    )
                })?;
            let compiled =
                self.compile_expected(value, &layout[index].value_type, "record field")?;
            replacements.insert(field.clone(), (index, compiled));
        }
        let mut compiled_fields = Vec::with_capacity(layout.len());
        for (index, field) in layout.iter().enumerate() {
            let value = if let Some((_, value)) = replacements.remove(&field.name) {
                value
            } else {
                self.compile_field_get_compiled(
                    CompiledExpr {
                        register: base.register,
                        value_type: base.value_type.clone(),
                        effects: base.effects,
                        hir: base.hir.clone(),
                    },
                    &field.name,
                    spread_span,
                    expression.span,
                )?
            };
            compiled_fields.push((index, value));
        }
        let register = self.allocate(expected_type.clone())?;
        self.code.push(Instruction::RecordNew {
            destination: register,
            fields: compiled_fields
                .iter()
                .map(|(index, value)| {
                    (
                        u32::try_from(*index).expect("field index fits"),
                        value.register,
                    )
                })
                .collect(),
        });
        self.mir.push(MirOperation::Record {
            destination: u32::from(register),
        });
        let effects = self.union_effects(
            std::iter::once(base.effects)
                .chain(compiled_fields.iter().map(|(_, value)| value.effects)),
        );
        Ok(CompiledExpr {
            register,
            value_type: expected_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Record(
                    compiled_fields
                        .into_iter()
                        .map(|(_, value)| value.hir)
                        .collect(),
                ),
                None,
                &expected_type,
                effects,
                expression.span,
            ),
        })
    }

    fn wrap_optional_value(
        &mut self,
        value: CompiledExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if matches!(value.value_type, ValueType::Option(_)) {
            return Ok(value);
        }
        let value_type = ValueType::Option(Box::new(value.value_type.clone()));
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::EnumNew {
            destination: register,
            variant: 1,
            payload: vec![value.register],
        });
        self.mir.push(MirOperation::Enum {
            destination: u32::from(register),
        });
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects: value.effects,
            hir: self.hir(HirExprKind::Enum, None, &value_type, value.effects, span),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn compile_optional_branch<F>(
        &mut self,
        receiver: &LoweredExpr,
        span: Span,
        operation: F,
    ) -> Result<CompiledExpr, Diagnostic>
    where
        F: FnOnce(&mut Self, CompiledExpr) -> Result<CompiledExpr, Diagnostic>,
    {
        let source = self.compile_expression(receiver)?;
        let ValueType::Option(payload_type) = source.value_type.clone() else {
            return Err(Diagnostic::new(
                "E3007",
                "optional chain requires an Option value",
                span,
            ));
        };
        let branch_index = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        let base = self.next_mir_block();
        self.mir_blocks.extend((0..=2).map(|_| MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        }));
        let outer_bindings = self.bindings.clone();
        let outer_ownership = self.ownership_states.clone();
        let mut joined_bindings = None;
        let mut joined_ownership = None;
        let mut arm_operations = Vec::new();
        let mut arm_regions = Vec::new();
        let mut arm_terminals = Vec::new();
        let mut hir_arms = Vec::new();

        // Compile the Some arm first so its exact result type determines the
        // local Option type used by the None arm.
        let some_target = u32::try_from(self.code.len()).expect("instruction index fits");
        self.bindings = outer_bindings.clone();
        self.ownership_states = outer_ownership.clone();
        let payload_register = self.allocate(payload_type.as_ref().clone())?;
        let payload_name = format!("__allen_optional_payload_{}", self.global.allocate_symbol());
        let payload_symbol = self.global.allocate_symbol();
        self.bindings.insert(
            payload_name.clone(),
            LocalBinding {
                register: payload_register,
                symbol: payload_symbol,
                value_type: payload_type.as_ref().clone(),
                scope: self.current_scope(),
                value_scope: self.current_scope(),
                mutable: false,
                moved: false,
            },
        );
        let payload = CompiledExpr {
            register: payload_register,
            value_type: payload_type.as_ref().clone(),
            effects: self.empty_effects(),
            hir: self.hir(
                HirExprKind::Variable,
                Some(payload_symbol),
                payload_type.as_ref(),
                self.empty_effects(),
                span,
            ),
        };
        let region_capture = self.begin_nested_mir_region();
        let operation_start = self.mir.len();
        let value = operation(self, payload)?;
        let value = self.wrap_optional_value(value, span)?;
        let region = self.finish_nested_mir_region(region_capture);
        arm_operations.push(self.mir.split_off(operation_start));
        arm_regions.push(region);
        arm_terminals.push(None);
        let operation_effects = value.effects;
        Self::validate_conditional_branch_state(
            self,
            &outer_bindings,
            &outer_ownership,
            Some(value.register),
            span,
            &mut joined_bindings,
            &mut joined_ownership,
        )?;
        let result_type = value.value_type.clone();
        let result_register = self.allocate(result_type.clone())?;
        self.code.push(Instruction::Move {
            destination: result_register,
            source: value.register,
        });
        self.mir.push(MirOperation::Move {
            destination: u32::from(result_register),
            source: u32::from(value.register),
        });
        hir_arms.push(value.hir);
        let some_jump = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });

        // None constructs a local result and never evaluates anything after
        // the chain receiver.
        self.bindings = outer_bindings.clone();
        self.ownership_states = outer_ownership.clone();
        let none_target = u32::try_from(self.code.len()).expect("instruction index fits");
        let none = self.allocate(result_type.clone())?;
        self.code.push(Instruction::EnumNew {
            destination: none,
            variant: 0,
            payload: Vec::new(),
        });
        self.mir.push(MirOperation::Enum {
            destination: u32::from(none),
        });
        self.code.push(Instruction::Move {
            destination: result_register,
            source: none,
        });
        self.mir.push(MirOperation::Move {
            destination: u32::from(result_register),
            source: u32::from(none),
        });
        hir_arms.push(self.hir(
            HirExprKind::Enum,
            None,
            &result_type,
            self.empty_effects(),
            span,
        ));
        arm_operations.push(self.mir.split_off(self.mir.len().saturating_sub(2)));
        arm_regions.push(CapturedMirRegion::default());
        arm_terminals.push(None);
        Self::validate_conditional_branch_state(
            self,
            &outer_bindings,
            &outer_ownership,
            Some(result_register),
            span,
            &mut joined_bindings,
            &mut joined_ownership,
        )?;
        let join = u32::try_from(self.code.len()).expect("instruction index fits");
        self.code[some_jump] = Instruction::Jump { target: join };
        self.code[branch_index] = Instruction::SwitchEnum {
            source: source.register,
            arms: vec![
                allen_bytecode::EnumSwitchArm {
                    variant: 0,
                    target: none_target,
                    bindings: Vec::new(),
                },
                allen_bytecode::EnumSwitchArm {
                    variant: 1,
                    target: some_target,
                    bindings: vec![payload_register],
                },
            ],
        };
        let join_block = self.next_mir_block();
        self.mir_blocks.push(MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        });
        self.mir_blocks[base as usize - 1] = MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::SwitchEnum {
                targets: vec![base + 2, base + 1],
            },
        };
        for (arm, ((operations, terminal), region)) in arm_operations
            .into_iter()
            .zip(arm_terminals)
            .zip(arm_regions)
            .enumerate()
        {
            let arm_block = base + 1 + u32::try_from(arm).expect("arm ID fits");
            self.mir_blocks[arm_block as usize - 1] = MirBlock {
                operations,
                terminator: region.entry.map_or(
                    terminal
                        .clone()
                        .unwrap_or(MirTerminator::Goto { target: join_block }),
                    |target| MirTerminator::Goto { target },
                ),
            };
            if let Some(tail) = region.tail {
                self.set_mir_handoff(
                    tail,
                    terminal.unwrap_or(MirTerminator::Goto { target: join_block }),
                );
            }
        }
        self.register_mir_region(base, join_block);
        self.bindings = outer_bindings;
        if let Some(joined) = joined_bindings {
            for (name, state) in joined {
                if let Some(binding) = self.bindings.get_mut(&name) {
                    binding.moved = state.moved;
                    binding.value_scope = state.value_scope;
                }
            }
        }
        if let Some(joined) = joined_ownership {
            for (register, state) in joined {
                self.ownership_states.insert(register, state);
            }
        }
        let effects = self.union_effects([source.effects, operation_effects]);
        Ok(CompiledExpr {
            register: result_register,
            value_type: result_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Match {
                    source: Box::new(source.hir),
                    arms: hir_arms,
                },
                None,
                &result_type,
                effects,
                span,
            ),
        })
    }

    fn compile_optional_field_get(
        &mut self,
        receiver: &LoweredExpr,
        field: &str,
        operator_span: Span,
        field_span: Span,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        self.compile_optional_branch(receiver, operator_span, |this, value| {
            this.compile_field_get_compiled(value, field, field_span, span)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_optional_call(
        &mut self,
        receiver: &LoweredExpr,
        field: &str,
        operator_span: Span,
        field_span: Span,
        type_arguments: &[LoweredType],
        arguments: &[LoweredCallArgument],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        self.compile_optional_branch(receiver, operator_span, |this, value| {
            let receiver_name = format!(
                "__allen_optional_receiver_{}",
                this.global.allocate_symbol()
            );
            this.bindings.insert(
                receiver_name.clone(),
                LocalBinding {
                    register: value.register,
                    symbol: this.global.allocate_symbol(),
                    value_type: value.value_type.clone(),
                    scope: this.current_scope(),
                    value_scope: this.sub_agent_value_scope(&value),
                    mutable: false,
                    moved: false,
                },
            );
            let callee = LoweredExpr {
                kind: LoweredExprKind::FieldGet {
                    record: Box::new(LoweredExpr {
                        kind: LoweredExprKind::Variable(receiver_name),
                        span: field_span,
                    }),
                    field: field.to_owned(),
                    field_span,
                },
                span,
            };
            this.compile_call(&callee, type_arguments, arguments, span)
        })
    }

    pub(super) fn compile_try(
        &mut self,
        value: &LoweredExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let value = self.compile_expression(value)?;
        match value.value_type {
            ValueType::Result(..) => self.compile_try_result(value, span),
            ValueType::Option(..) => self.compile_try_option(value, span),
            _ => Err(Diagnostic::new(
                "E2017",
                "'?' requires a Result or Option value",
                span,
            )),
        }
    }

    fn compile_try_result(
        &mut self,
        value: CompiledExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let ValueType::Result(ok, error) = &value.value_type else {
            unreachable!("result try dispatch checks the operand type")
        };
        let ValueType::Result(_, return_error) = &self.return_type else {
            return Err(Diagnostic::new(
                "E2017",
                "a function that uses '?' must return Result",
                span,
            ));
        };
        if error != return_error {
            return Err(Diagnostic::new(
                "E2017",
                "'?' error type must match the function return error type",
                span,
            ));
        }
        if self.ownership_states.iter().any(|(register, ownership)| {
            ownership.state == MirOwnershipState::Live
                && ownership.must_consume
                && (matches!(self.registers[*register as usize], ValueType::Future(_))
                    || ownership.scope == 0)
        }) {
            return Err(Diagnostic::new(
                "E3011",
                "try error path would discard a live affine obligation",
                span,
            ));
        }
        let value_type = ok.as_ref().clone();
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::TryResult {
            destination: register,
            source: value.register,
        });
        let base = self.next_mir_block();
        let cleanup_operations = self.cleanup_operations(MirCleanupKind::NormalJoin);
        let success = base + 1;
        let error = base + 2;
        let continuation = base + 3;
        self.mir_blocks.extend([
            MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::TryResult { success, error },
            },
            MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Goto {
                    target: continuation,
                },
            },
            MirBlock {
                operations: cleanup_operations,
                terminator: MirTerminator::Return {
                    source: u32::from(value.register),
                },
            },
            MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            },
        ]);
        self.register_mir_region(base, continuation);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects: value.effects,
            hir: self.hir(
                HirExprKind::Try(Box::new(value.hir)),
                None,
                &value_type,
                value.effects,
                span,
            ),
        })
    }

    fn compile_try_option(
        &mut self,
        value: CompiledExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let ValueType::Option(value_type) = &value.value_type else {
            unreachable!("option try dispatch checks the operand type")
        };
        if !matches!(self.return_type, ValueType::Option(_)) {
            return Err(Diagnostic::new(
                "E2017",
                "a function that uses '?' with Option must return Option",
                span,
            ));
        }
        if self.ownership_states.iter().any(|(register, ownership)| {
            ownership.state == MirOwnershipState::Live
                && ownership.must_consume
                && (matches!(self.registers[*register as usize], ValueType::Future(_))
                    || ownership.scope == 0)
        }) {
            return Err(Diagnostic::new(
                "E3011",
                "try none path would discard a live affine obligation",
                span,
            ));
        }
        let value_type = value_type.as_ref().clone();
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::TryOption {
            destination: register,
            source: value.register,
        });
        let base = self.next_mir_block();
        let cleanup_operations = self.cleanup_operations(MirCleanupKind::NormalJoin);
        let some = base + 1;
        let none = base + 2;
        let continuation = base + 3;
        self.mir_blocks.extend([
            MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::TryOption { some, none },
            },
            MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Goto {
                    target: continuation,
                },
            },
            MirBlock {
                operations: cleanup_operations,
                terminator: MirTerminator::Return {
                    source: u32::from(value.register),
                },
            },
            MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            },
        ]);
        self.register_mir_region(base, continuation);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects: value.effects,
            hir: self.hir(
                HirExprKind::Try(Box::new(value.hir)),
                None,
                &value_type,
                value.effects,
                span,
            ),
        })
    }

    fn simple_pattern_binding(
        pattern: &LoweredPattern,
        span: Span,
    ) -> Result<Option<String>, Diagnostic> {
        match pattern {
            LoweredPattern::Binding {
                name,
                span: binding_span,
            } => {
                let _ = binding_span;
                Ok(Some(name.clone()))
            }
            LoweredPattern::Wildcard => Ok(None),
            LoweredPattern::Range {
                start,
                end,
                inclusive,
                operator_span,
            } => {
                let _ = (start, end, inclusive);
                Err(Diagnostic::new(
                    "E3011",
                    "range patterns are not implemented in this compiler pass",
                    *operator_span,
                ))
            }
            LoweredPattern::Or { operator_spans, .. } => Err(Diagnostic::new(
                "E3011",
                "OR patterns are not implemented in this compiler pass",
                operator_spans.first().copied().unwrap_or(span),
            )),
            _ => Err(Diagnostic::new(
                "E3011",
                "nested patterns are not implemented in this compiler pass",
                span,
            )),
        }
    }

    fn pattern_literal(
        expression: &LoweredExpr,
        expected: &ValueType,
    ) -> Result<PatternLiteralValue, Diagnostic> {
        let value = match &expression.kind {
            LoweredExprKind::Int(value) => PatternLiteralValue::Int(*value),
            LoweredExprKind::String(value) => PatternLiteralValue::String(value.clone()),
            LoweredExprKind::Bytes(value) => PatternLiteralValue::Bytes(value.clone()),
            LoweredExprKind::Float(_) => {
                return Err(Diagnostic::new(
                    "E3007",
                    "range-pattern endpoints cannot be Float",
                    expression.span,
                ));
            }
            _ => {
                return Err(Diagnostic::new(
                    "E3007",
                    "range-pattern endpoints must be compile-time Int, String, or Bytes literals",
                    expression.span,
                ));
            }
        };
        let actual = match value {
            PatternLiteralValue::Int(_) => ValueType::Int,
            PatternLiteralValue::String(_) => ValueType::String,
            PatternLiteralValue::Bytes(_) => ValueType::Bytes,
        };
        if &actual != expected {
            return Err(Diagnostic::new(
                "E3007",
                format!(
                    "range-pattern endpoint has type {actual}, but the scrutinee has type {expected}"
                ),
                expression.span,
            ));
        }
        Ok(value)
    }

    fn pattern_interval(
        pattern: &LoweredPattern,
        expected: &ValueType,
    ) -> Result<Option<PatternInterval>, Diagnostic> {
        let LoweredPattern::Range {
            start,
            end,
            inclusive,
            operator_span,
        } = pattern
        else {
            return Ok(None);
        };
        if !matches!(
            expected,
            ValueType::Int | ValueType::String | ValueType::Bytes
        ) {
            return Err(Diagnostic::new(
                "E3007",
                format!("range pattern cannot match {expected}"),
                *operator_span,
            ));
        }
        let start = Self::pattern_literal(start, expected)?;
        let end = Self::pattern_literal(end, expected)?;
        let ordering = start
            .compare(&end)
            .expect("validated endpoints have one exact type");
        if ordering.is_gt() || ordering.is_eq() && !inclusive {
            return Err(Diagnostic::new(
                "E3007",
                "range pattern is empty",
                *operator_span,
            ));
        }
        Ok(Some(PatternInterval {
            start,
            end,
            inclusive: *inclusive,
        }))
    }

    fn insert_pattern_binding(
        bindings: &mut BTreeMap<String, ValueType>,
        name: &str,
        value_type: &ValueType,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if bindings
            .insert(name.to_owned(), value_type.clone())
            .is_some()
        {
            return Err(Diagnostic::new(
                "E3005",
                format!("duplicate pattern binding '{name}'"),
                span,
            ));
        }
        Ok(())
    }

    fn collect_pattern_bindings(
        &self,
        pattern: &LoweredPattern,
        value_type: &ValueType,
        span: Span,
    ) -> Result<BTreeMap<String, ValueType>, Diagnostic> {
        let mut bindings = BTreeMap::new();
        self.collect_pattern_bindings_into(pattern, value_type, span, &mut bindings)?;
        Ok(bindings)
    }

    #[allow(clippy::too_many_lines)]
    fn collect_pattern_bindings_into(
        &self,
        pattern: &LoweredPattern,
        value_type: &ValueType,
        span: Span,
        bindings: &mut BTreeMap<String, ValueType>,
    ) -> Result<(), Diagnostic> {
        match pattern {
            LoweredPattern::Binding {
                name,
                span: binding_span,
            } => Self::insert_pattern_binding(bindings, name, value_type, *binding_span),
            LoweredPattern::Wildcard => Ok(()),
            LoweredPattern::Bool(_) if *value_type == ValueType::Bool => Ok(()),
            LoweredPattern::Range { .. } => {
                Self::pattern_interval(pattern, value_type)?;
                Ok(())
            }
            LoweredPattern::Or {
                alternatives,
                operator_spans,
            } => {
                let Some(first) = alternatives.first() else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "OR pattern has no alternatives",
                        span,
                    ));
                };
                let expected = self.collect_pattern_bindings(first, value_type, span)?;
                let mut earlier = vec![first];
                for (index, alternative) in alternatives.iter().enumerate().skip(1) {
                    let actual = self.collect_pattern_bindings(alternative, value_type, span)?;
                    if actual != expected {
                        let mismatch = expected
                            .keys()
                            .chain(actual.keys())
                            .find(|name| expected.get(*name) != actual.get(*name))
                            .cloned()
                            .unwrap_or_else(|| "<binding>".to_owned());
                        let detail = match (expected.get(&mismatch), actual.get(&mismatch)) {
                            (Some(expected), Some(actual)) => format!(
                                "binding '{mismatch}' has type {actual}, expected {expected}; OR alternatives must bind the same names, types, and ownership"
                            ),
                            (Some(_), None) => format!(
                                "OR alternative does not bind '{mismatch}'; every alternative must bind the same names, types, and ownership"
                            ),
                            (None, Some(_)) => format!(
                                "OR alternative unexpectedly binds '{mismatch}'; every alternative must bind the same names, types, and ownership"
                            ),
                            (None, None) => unreachable!(),
                        };
                        return Err(Diagnostic::new(
                            "E3007",
                            detail,
                            operator_spans.get(index - 1).copied().unwrap_or(span),
                        ));
                    }
                    if self.pattern_fully_covered(&earlier, alternative, value_type)? {
                        return Err(Diagnostic::new(
                            "E2016",
                            "OR pattern alternative is unreachable",
                            operator_spans.get(index - 1).copied().unwrap_or(span),
                        ));
                    }
                    earlier.push(alternative);
                }
                for (name, value_type) in expected {
                    Self::insert_pattern_binding(bindings, &name, &value_type, span)?;
                }
                Ok(())
            }
            LoweredPattern::Option { some: false, .. }
                if matches!(value_type, ValueType::Option(_)) =>
            {
                Ok(())
            }
            LoweredPattern::Option {
                some: true,
                payload: Some(payload),
            } => {
                let ValueType::Option(payload_type) = value_type else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "option pattern does not match the source type",
                        span,
                    ));
                };
                self.collect_pattern_bindings_into(payload, payload_type, span, bindings)
            }
            LoweredPattern::Result { ok, payload } => {
                let ValueType::Result(ok_type, error_type) = value_type else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "result pattern does not match the source type",
                        span,
                    ));
                };
                self.collect_pattern_bindings_into(
                    payload,
                    if *ok { ok_type } else { error_type },
                    span,
                    bindings,
                )
            }
            LoweredPattern::Enum {
                name,
                variant,
                patterns,
                fields,
            } => {
                let expected = resolve_named_type(
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                    &self.info.module,
                    name,
                    span,
                )?;
                if expected != *value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "match pattern uses a different nominal enum",
                        span,
                    ));
                }
                let ValueType::Enum(id) = value_type else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "enum pattern does not match the source type",
                        span,
                    ));
                };
                let metadata = self.global.bundle.enum_types[*id as usize]
                    .variants
                    .iter()
                    .find(|candidate| candidate.name == *variant)
                    .ok_or_else(|| {
                        Diagnostic::new("E3007", format!("unknown enum variant '{variant}'"), span)
                    })?;
                match (&metadata.payload, fields) {
                    (EnumPayloadType::Unit, None) if patterns.is_empty() => Ok(()),
                    (EnumPayloadType::Tuple(types), None) if types.len() == patterns.len() => {
                        for (pattern, value_type) in patterns.iter().zip(types) {
                            self.collect_pattern_bindings_into(
                                pattern, value_type, span, bindings,
                            )?;
                        }
                        Ok(())
                    }
                    (EnumPayloadType::Record(types), Some(fields))
                        if types.len() == fields.len() =>
                    {
                        let supplied = fields
                            .iter()
                            .map(|(name, _, _)| name)
                            .collect::<BTreeSet<_>>();
                        if supplied.len() != fields.len()
                            || types.iter().any(|field| !supplied.contains(&field.name))
                        {
                            return Err(Diagnostic::new(
                                "E3007",
                                "enum record pattern must contain every field exactly once",
                                span,
                            ));
                        }
                        for field in types {
                            let (_, field_span, pattern) = fields
                                .iter()
                                .find(|(name, _, _)| *name == field.name)
                                .expect("validated enum record field");
                            self.collect_pattern_bindings_into(
                                pattern,
                                &field.value_type,
                                *field_span,
                                bindings,
                            )?;
                        }
                        Ok(())
                    }
                    _ => Err(Diagnostic::new(
                        "E3007",
                        "enum pattern uses the wrong payload form or count",
                        span,
                    )),
                }
            }
            LoweredPattern::Record { name, fields } => {
                let expected = resolve_named_type(
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                    &self.info.module,
                    name,
                    span,
                )?;
                if expected != *value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "record pattern has a different structural type",
                        span,
                    ));
                }
                let ValueType::Record(layout) = value_type else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "record pattern does not match the source type",
                        span,
                    ));
                };
                if fields.len() != layout.len() {
                    return Err(Diagnostic::new(
                        "E3007",
                        "record pattern must contain exactly the declared fields",
                        span,
                    ));
                }
                let mut seen = BTreeSet::new();
                for (field, field_span, pattern) in fields {
                    if !seen.insert(field) {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!("duplicate record pattern field '{field}'"),
                            *field_span,
                        ));
                    }
                    let field_type = layout
                        .iter()
                        .find(|candidate| candidate.name == *field)
                        .map(|field| &field.value_type)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                "E3007",
                                format!("record pattern has no field '{field}'"),
                                *field_span,
                            )
                        })?;
                    self.collect_pattern_bindings_into(pattern, field_type, *field_span, bindings)?;
                }
                Ok(())
            }
            _ => Err(Diagnostic::new(
                "E3007",
                "match pattern does not match the source type",
                span,
            )),
        }
    }

    fn pattern_uses_decision_lowering(pattern: &LoweredPattern) -> bool {
        match pattern {
            LoweredPattern::Range { .. } | LoweredPattern::Or { .. } => true,
            LoweredPattern::Record { fields, .. } => fields
                .iter()
                .any(|(_, _, pattern)| Self::pattern_uses_decision_lowering(pattern)),
            LoweredPattern::Enum {
                patterns, fields, ..
            } => {
                patterns.iter().any(Self::pattern_uses_decision_lowering)
                    || fields.as_ref().is_some_and(|fields| {
                        fields
                            .iter()
                            .any(|(_, _, pattern)| Self::pattern_uses_decision_lowering(pattern))
                    })
            }
            LoweredPattern::Option { payload, .. } => payload
                .as_deref()
                .is_some_and(Self::pattern_uses_decision_lowering),
            LoweredPattern::Result { payload, .. } => Self::pattern_uses_decision_lowering(payload),
            LoweredPattern::Binding { .. } | LoweredPattern::Wildcard | LoweredPattern::Bool(_) => {
                false
            }
        }
    }

    fn flattened_alternatives<'a>(
        pattern: &'a LoweredPattern,
        output: &mut Vec<&'a LoweredPattern>,
    ) {
        if let LoweredPattern::Or { alternatives, .. } = pattern {
            for alternative in alternatives {
                Self::flattened_alternatives(alternative, output);
            }
        } else {
            output.push(pattern);
        }
    }

    fn pattern_is_irrefutable(pattern: &LoweredPattern) -> bool {
        match pattern {
            LoweredPattern::Binding { .. } | LoweredPattern::Wildcard => true,
            LoweredPattern::Or { alternatives, .. } => {
                alternatives.iter().any(Self::pattern_is_irrefutable)
            }
            LoweredPattern::Record { fields, .. } => fields
                .iter()
                .all(|(_, _, pattern)| Self::pattern_is_irrefutable(pattern)),
            _ => false,
        }
    }

    fn interval_covers(intervals: &[PatternInterval], target: &PatternInterval) -> bool {
        let mut intervals = intervals.to_vec();
        intervals.sort_by(|left, right| {
            left.start
                .compare(&right.start)
                .expect("range intervals have one exact type")
        });
        let mut cursor = target.start.clone();
        for interval in intervals {
            let end_to_cursor = interval
                .end
                .compare(&cursor)
                .expect("range intervals have one exact type");
            if end_to_cursor.is_lt() || end_to_cursor.is_eq() && !interval.inclusive {
                continue;
            }
            if interval
                .start
                .compare(&cursor)
                .expect("range intervals have one exact type")
                .is_gt()
            {
                return false;
            }
            let end_to_target = interval
                .end
                .compare(&target.end)
                .expect("range intervals have one exact type");
            if end_to_target.is_gt()
                || end_to_target.is_eq() && (interval.inclusive || !target.inclusive)
            {
                return true;
            }
            cursor = if interval.inclusive {
                match interval.end {
                    PatternLiteralValue::Int(value) => {
                        let Some(value) = value.checked_add(1) else {
                            return true;
                        };
                        PatternLiteralValue::Int(value)
                    }
                    PatternLiteralValue::String(mut value) => {
                        value.push('\0');
                        PatternLiteralValue::String(value)
                    }
                    PatternLiteralValue::Bytes(mut value) => {
                        value.push(0);
                        PatternLiteralValue::Bytes(value)
                    }
                }
            } else {
                interval.end
            };
            if !target.inclusive
                && cursor
                    .compare(&target.end)
                    .expect("range intervals have one exact type")
                    .is_eq()
            {
                return true;
            }
        }
        false
    }

    #[allow(clippy::too_many_lines)]
    fn patterns_equivalent(left: &LoweredPattern, right: &LoweredPattern) -> bool {
        match (left, right) {
            (LoweredPattern::Wildcard, LoweredPattern::Wildcard)
            | (LoweredPattern::Binding { .. }, LoweredPattern::Binding { .. }) => true,
            (LoweredPattern::Bool(left), LoweredPattern::Bool(right)) => left == right,
            (
                LoweredPattern::Range {
                    start: left_start,
                    end: left_end,
                    inclusive: left_inclusive,
                    ..
                },
                LoweredPattern::Range {
                    start: right_start,
                    end: right_end,
                    inclusive: right_inclusive,
                    ..
                },
            ) => {
                left_inclusive == right_inclusive
                    && Self::literal_expressions_equal(left_start, right_start)
                    && Self::literal_expressions_equal(left_end, right_end)
            }
            (
                LoweredPattern::Option {
                    some: left_some,
                    payload: left_payload,
                },
                LoweredPattern::Option {
                    some: right_some,
                    payload: right_payload,
                },
            ) => {
                left_some == right_some
                    && match (left_payload, right_payload) {
                        (None, None) => true,
                        (Some(left), Some(right)) => Self::patterns_equivalent(left, right),
                        _ => false,
                    }
            }
            (
                LoweredPattern::Result {
                    ok: left_ok,
                    payload: left_payload,
                },
                LoweredPattern::Result {
                    ok: right_ok,
                    payload: right_payload,
                },
            ) => left_ok == right_ok && Self::patterns_equivalent(left_payload, right_payload),
            (
                LoweredPattern::Enum {
                    name: left_name,
                    variant: left_variant,
                    patterns: left_patterns,
                    fields: left_fields,
                },
                LoweredPattern::Enum {
                    name: right_name,
                    variant: right_variant,
                    patterns: right_patterns,
                    fields: right_fields,
                },
            ) => {
                left_name == right_name
                    && left_variant == right_variant
                    && left_patterns.len() == right_patterns.len()
                    && left_patterns
                        .iter()
                        .zip(right_patterns)
                        .all(|(left, right)| Self::patterns_equivalent(left, right))
                    && match (left_fields, right_fields) {
                        (None, None) => true,
                        (Some(left), Some(right)) if left.len() == right.len() => left
                            .iter()
                            .zip(right)
                            .all(|((left_name, _, left), (right_name, _, right))| {
                                left_name == right_name && Self::patterns_equivalent(left, right)
                            }),
                        _ => false,
                    }
            }
            (
                LoweredPattern::Record {
                    name: left_name,
                    fields: left_fields,
                },
                LoweredPattern::Record {
                    name: right_name,
                    fields: right_fields,
                },
            ) => {
                left_name == right_name
                    && left_fields.len() == right_fields.len()
                    && left_fields.iter().all(|(left_name, _, left)| {
                        right_fields.iter().any(|(right_name, _, right)| {
                            left_name == right_name && Self::patterns_equivalent(left, right)
                        })
                    })
            }
            (
                LoweredPattern::Or {
                    alternatives: left, ..
                },
                right,
            ) => left
                .iter()
                .any(|left| Self::patterns_equivalent(left, right)),
            (
                left,
                LoweredPattern::Or {
                    alternatives: right,
                    ..
                },
            ) => right
                .iter()
                .all(|right| Self::patterns_equivalent(left, right)),
            _ => false,
        }
    }

    fn literal_expressions_equal(left: &LoweredExpr, right: &LoweredExpr) -> bool {
        match (&left.kind, &right.kind) {
            (LoweredExprKind::Int(left), LoweredExprKind::Int(right)) => left == right,
            (LoweredExprKind::String(left), LoweredExprKind::String(right)) => left == right,
            (LoweredExprKind::Bytes(left), LoweredExprKind::Bytes(right)) => left == right,
            (LoweredExprKind::Float(left), LoweredExprKind::Float(right)) => left == right,
            _ => false,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn pattern_fully_covered(
        &self,
        earlier: &[&LoweredPattern],
        target: &LoweredPattern,
        value_type: &ValueType,
    ) -> Result<bool, Diagnostic> {
        let mut expanded_earlier = Vec::new();
        for pattern in earlier {
            Self::flattened_alternatives(pattern, &mut expanded_earlier);
        }
        let earlier = expanded_earlier.as_slice();
        if earlier
            .iter()
            .any(|pattern| Self::pattern_is_irrefutable(pattern))
        {
            return Ok(true);
        }
        if let LoweredPattern::Or { alternatives, .. } = target {
            return alternatives.iter().try_fold(true, |covered, alternative| {
                Ok(covered && self.pattern_fully_covered(earlier, alternative, value_type)?)
            });
        }
        if let Some(target) = Self::pattern_interval(target, value_type)? {
            let intervals = earlier
                .iter()
                .filter_map(|pattern| Self::pattern_interval(pattern, value_type).transpose())
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Self::interval_covers(&intervals, &target));
        }
        if let (ValueType::Option(payload_type), LoweredPattern::Option { some, payload }) =
            (value_type, target)
        {
            if !some {
                return Ok(earlier
                    .iter()
                    .any(|pattern| matches!(pattern, LoweredPattern::Option { some: false, .. })));
            }
            let Some(payload) = payload.as_deref() else {
                return Ok(false);
            };
            let payloads = earlier
                .iter()
                .filter_map(|pattern| match pattern {
                    LoweredPattern::Option {
                        some: true,
                        payload: Some(payload),
                    } => Some(payload.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            return self.pattern_fully_covered(&payloads, payload, payload_type);
        }
        if let (ValueType::Result(ok_type, error_type), LoweredPattern::Result { ok, payload }) =
            (value_type, target)
        {
            let payloads = earlier
                .iter()
                .filter_map(|pattern| match pattern {
                    LoweredPattern::Result {
                        ok: earlier_ok,
                        payload,
                    } if earlier_ok == ok => Some(payload.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            return self.pattern_fully_covered(
                &payloads,
                payload,
                if *ok { ok_type } else { error_type },
            );
        }
        if let (
            ValueType::Enum(id),
            LoweredPattern::Enum {
                variant,
                patterns,
                fields,
                ..
            },
        ) = (value_type, target)
        {
            let metadata = self.global.bundle.enum_types[*id as usize]
                .variants
                .iter()
                .find(|candidate| candidate.name == *variant)
                .expect("enum pattern was validated before usefulness");
            let matching = earlier
                .iter()
                .filter(|pattern| {
                    matches!(pattern, LoweredPattern::Enum { variant: earlier, .. } if earlier == variant)
                })
                .copied()
                .collect::<Vec<_>>();
            return match (&metadata.payload, fields) {
                (EnumPayloadType::Unit, None) => Ok(!matching.is_empty()),
                (EnumPayloadType::Tuple(types), None) if types.len() == patterns.len() => {
                    Ok(matching.iter().any(|earlier| {
                        let LoweredPattern::Enum {
                            patterns: earlier, ..
                        } = earlier
                        else {
                            unreachable!()
                        };
                        earlier.len() == patterns.len()
                            && earlier.iter().zip(patterns).zip(types).all(
                                |((earlier, target), value_type)| {
                                    self.pattern_fully_covered(&[earlier], target, value_type)
                                        .unwrap_or(false)
                                },
                            )
                    }))
                }
                (EnumPayloadType::Record(types), Some(fields)) => {
                    Ok(matching.iter().any(|earlier| {
                        let LoweredPattern::Enum {
                            fields: Some(earlier),
                            ..
                        } = earlier
                        else {
                            return false;
                        };
                        types.iter().all(|field| {
                            let target = fields
                                .iter()
                                .find(|(name, _, _)| *name == field.name)
                                .map(|(_, _, pattern)| pattern.as_ref());
                            let earlier = earlier
                                .iter()
                                .find(|(name, _, _)| *name == field.name)
                                .map(|(_, _, pattern)| pattern.as_ref());
                            match (earlier, target) {
                                (Some(earlier), Some(target)) => self
                                    .pattern_fully_covered(&[earlier], target, &field.value_type)
                                    .unwrap_or(false),
                                _ => false,
                            }
                        })
                    }))
                }
                _ => Ok(false),
            };
        }
        if let (
            ValueType::Record(layout),
            LoweredPattern::Record {
                name: target_name,
                fields: target_fields,
            },
        ) = (value_type, target)
        {
            let matching = earlier
                .iter()
                .filter_map(|pattern| match pattern {
                    LoweredPattern::Record { name, fields } if name == target_name => Some(fields),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if layout.len() == 1 {
                let field = &layout[0];
                let target = target_fields
                    .iter()
                    .find(|(name, _, _)| *name == field.name)
                    .map(|(_, _, pattern)| pattern.as_ref())
                    .expect("record pattern was validated before usefulness");
                let earlier = matching
                    .iter()
                    .filter_map(|fields| {
                        fields
                            .iter()
                            .find(|(name, _, _)| *name == field.name)
                            .map(|(_, _, pattern)| pattern.as_ref())
                    })
                    .collect::<Vec<_>>();
                return self.pattern_fully_covered(&earlier, target, &field.value_type);
            }
        }
        Ok(earlier.iter().any(|pattern| match (*pattern, target) {
            (LoweredPattern::Bool(left), LoweredPattern::Bool(right)) => left == right,
            (
                LoweredPattern::Option {
                    some: left_some,
                    payload: left_payload,
                },
                LoweredPattern::Option {
                    some: right_some,
                    payload: right_payload,
                },
            ) if left_some == right_some => match (left_payload, right_payload) {
                (None, None) => true,
                (Some(left), Some(right)) => {
                    Self::pattern_is_irrefutable(left) || Self::patterns_equivalent(left, right)
                }
                _ => false,
            },
            (
                LoweredPattern::Result {
                    ok: left_ok,
                    payload: left_payload,
                },
                LoweredPattern::Result {
                    ok: right_ok,
                    payload: right_payload,
                },
            ) => {
                left_ok == right_ok
                    && (Self::pattern_is_irrefutable(left_payload)
                        || Self::patterns_equivalent(left_payload, right_payload))
            }
            (
                LoweredPattern::Enum {
                    name: left_name,
                    variant: left_variant,
                    patterns: left_patterns,
                    fields: left_fields,
                },
                LoweredPattern::Enum {
                    name: right_name,
                    variant: right_variant,
                    patterns: _,
                    fields: _,
                },
            ) => {
                left_name == right_name
                    && left_variant == right_variant
                    && (left_patterns.iter().all(Self::pattern_is_irrefutable)
                        && left_fields.as_ref().is_none_or(|fields| {
                            fields
                                .iter()
                                .all(|(_, _, pattern)| Self::pattern_is_irrefutable(pattern))
                        })
                        || Self::patterns_equivalent(pattern, target))
            }
            _ => Self::patterns_equivalent(pattern, target),
        }))
    }

    #[allow(clippy::too_many_lines)]
    fn advanced_patterns_are_exhaustive(
        &self,
        alternatives: &[&LoweredPattern],
        value_type: &ValueType,
    ) -> Result<bool, Diagnostic> {
        let mut expanded_alternatives = Vec::new();
        for pattern in alternatives {
            Self::flattened_alternatives(pattern, &mut expanded_alternatives);
        }
        let alternatives = expanded_alternatives.as_slice();
        if alternatives
            .iter()
            .any(|pattern| Self::pattern_is_irrefutable(pattern))
        {
            return Ok(true);
        }
        match value_type {
            ValueType::Int => Ok(Self::pattern_interval(
                &LoweredPattern::Range {
                    start: LoweredExpr {
                        kind: LoweredExprKind::Int(i64::MIN),
                        span: Span { start: 0, end: 0 },
                    },
                    end: LoweredExpr {
                        kind: LoweredExprKind::Int(i64::MAX),
                        span: Span { start: 0, end: 0 },
                    },
                    inclusive: true,
                    operator_span: Span { start: 0, end: 0 },
                },
                value_type,
            )?
            .is_some_and(|target| {
                let intervals = alternatives
                    .iter()
                    .filter_map(|pattern| {
                        Self::pattern_interval(pattern, value_type).ok().flatten()
                    })
                    .collect::<Vec<_>>();
                Self::interval_covers(&intervals, &target)
            })),
            ValueType::Bool => Ok([false, true].into_iter().all(|value| {
                alternatives.iter().any(
                    |pattern| matches!(pattern, LoweredPattern::Bool(actual) if *actual == value),
                )
            })),
            ValueType::Option(payload_type) => {
                let none = alternatives
                    .iter()
                    .any(|pattern| matches!(pattern, LoweredPattern::Option { some: false, .. }));
                let some = alternatives
                    .iter()
                    .filter_map(|pattern| match pattern {
                        LoweredPattern::Option {
                            some: true,
                            payload: Some(payload),
                        } => Some(payload.as_ref()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                Ok(none && self.advanced_patterns_are_exhaustive(&some, payload_type)?)
            }
            ValueType::Result(ok_type, error_type) => {
                let ok = alternatives
                    .iter()
                    .filter_map(|pattern| match pattern {
                        LoweredPattern::Result { ok: true, payload } => Some(payload.as_ref()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let error = alternatives
                    .iter()
                    .filter_map(|pattern| match pattern {
                        LoweredPattern::Result { ok: false, payload } => Some(payload.as_ref()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                Ok(self.advanced_patterns_are_exhaustive(&ok, ok_type)?
                    && self.advanced_patterns_are_exhaustive(&error, error_type)?)
            }
            ValueType::Enum(id) => {
                for variant in &self.global.bundle.enum_types[*id as usize].variants {
                    let matching = alternatives
                        .iter()
                        .filter_map(|pattern| match pattern {
                            LoweredPattern::Enum {
                                variant: actual,
                                patterns,
                                fields,
                                ..
                            } if *actual == variant.name => Some((patterns, fields)),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    let covered = match &variant.payload {
                        EnumPayloadType::Unit => !matching.is_empty(),
                        EnumPayloadType::Tuple(types) if types.len() == 1 => {
                            let payloads = matching
                                .iter()
                                .filter_map(|(patterns, _)| patterns.first())
                                .collect::<Vec<_>>();
                            self.advanced_patterns_are_exhaustive(&payloads, &types[0])?
                        }
                        EnumPayloadType::Record(fields) if fields.len() == 1 => {
                            let field = &fields[0];
                            let payloads = matching
                                .iter()
                                .filter_map(|(_, pattern_fields)| {
                                    pattern_fields.as_ref()?.iter().find_map(
                                        |(name, _, pattern)| {
                                            (name == &field.name).then_some(pattern.as_ref())
                                        },
                                    )
                                })
                                .collect::<Vec<_>>();
                            self.advanced_patterns_are_exhaustive(&payloads, &field.value_type)?
                        }
                        _ => matching.iter().any(|(patterns, fields)| {
                            patterns.iter().all(Self::pattern_is_irrefutable)
                                && fields.as_ref().is_none_or(|fields| {
                                    fields.iter().all(|(_, _, pattern)| {
                                        Self::pattern_is_irrefutable(pattern)
                                    })
                                })
                        }),
                    };
                    if !covered {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            ValueType::Record(layout) if layout.len() == 1 => {
                let field = &layout[0];
                let payloads = alternatives
                    .iter()
                    .filter_map(|pattern| match pattern {
                        LoweredPattern::Record { fields, .. } => {
                            fields.iter().find_map(|(name, _, pattern)| {
                                (name == &field.name).then_some(pattern.as_ref())
                            })
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                self.advanced_patterns_are_exhaustive(&payloads, &field.value_type)
            }
            ValueType::Record(_) => Ok(alternatives.iter().any(|pattern| match pattern {
                LoweredPattern::Record { fields, .. } => fields
                    .iter()
                    .all(|(_, _, pattern)| Self::pattern_is_irrefutable(pattern)),
                _ => false,
            })),
            _ => Ok(false),
        }
    }

    fn pattern_body(expression: LoweredExpr, span: Span) -> LoweredBody {
        LoweredBody {
            statements: Vec::new(),
            tail: Some(expression),
            span,
        }
    }

    fn pattern_if(
        condition: LoweredExpr,
        success: LoweredExpr,
        failure: LoweredExpr,
        span: Span,
    ) -> LoweredExpr {
        LoweredExpr {
            kind: LoweredExprKind::If {
                condition: Box::new(condition),
                then_body: Box::new(Self::pattern_body(success, span)),
                else_branch: Some(LoweredElse::Body(Box::new(Self::pattern_body(
                    failure, span,
                )))),
            },
            span,
        }
    }

    fn internal_pattern_failure(span: Span) -> LoweredExpr {
        let reason = LoweredExpr {
            kind: LoweredExprKind::String("internal non-exhaustive pattern decision".to_owned()),
            span,
        };
        LoweredExpr {
            kind: LoweredExprKind::Call {
                callee: Box::new(LoweredExpr {
                    kind: LoweredExprKind::Variable("fail".to_owned()),
                    span,
                }),
                type_arguments: Vec::new(),
                arguments: vec![LoweredCallArgument {
                    label: None,
                    value: reason,
                    placeholder: false,
                    trailing: false,
                    preceding_call_span: None,
                    span,
                }],
            },
            span,
        }
    }

    fn generated_pattern_name(counter: &mut usize) -> String {
        let name = format!("$pattern_payload_{}", *counter);
        *counter += 1;
        name
    }

    #[allow(clippy::too_many_lines)]
    fn pattern_decision_expression(
        pattern: &LoweredPattern,
        source: LoweredExpr,
        success: LoweredExpr,
        failure: LoweredExpr,
        span: Span,
        counter: &mut usize,
    ) -> LoweredExpr {
        match pattern {
            LoweredPattern::Binding {
                name,
                span: name_span,
            } => LoweredExpr {
                kind: LoweredExprKind::If {
                    condition: Box::new(LoweredExpr {
                        kind: LoweredExprKind::Bool(true),
                        span,
                    }),
                    then_body: Box::new(LoweredBody {
                        statements: vec![LoweredStatement::Let {
                            name: name.clone(),
                            name_span: *name_span,
                            mutable: false,
                            annotation: None,
                            value: source,
                        }],
                        tail: Some(success),
                        span,
                    }),
                    else_branch: Some(LoweredElse::Body(Box::new(Self::pattern_body(
                        failure, span,
                    )))),
                },
                span,
            },
            LoweredPattern::Wildcard => success,
            LoweredPattern::Bool(value) => {
                let condition = LoweredExpr {
                    kind: LoweredExprKind::Binary {
                        operation: Binary::Equal,
                        left: Box::new(source),
                        right: Box::new(LoweredExpr {
                            kind: LoweredExprKind::Bool(*value),
                            span,
                        }),
                    },
                    span,
                };
                Self::pattern_if(condition, success, failure, span)
            }
            LoweredPattern::Range {
                start,
                end,
                inclusive,
                ..
            } => {
                let lower = LoweredExpr {
                    kind: LoweredExprKind::Binary {
                        operation: Binary::LessEqual,
                        left: Box::new(start.clone()),
                        right: Box::new(source.clone()),
                    },
                    span,
                };
                let upper = LoweredExpr {
                    kind: LoweredExprKind::Binary {
                        operation: if *inclusive {
                            Binary::LessEqual
                        } else {
                            Binary::Less
                        },
                        left: Box::new(source),
                        right: Box::new(end.clone()),
                    },
                    span,
                };
                let condition = LoweredExpr {
                    kind: LoweredExprKind::Binary {
                        operation: Binary::And,
                        left: Box::new(lower),
                        right: Box::new(upper),
                    },
                    span,
                };
                Self::pattern_if(condition, success, failure, span)
            }
            LoweredPattern::Or { alternatives, .. } => {
                alternatives
                    .iter()
                    .rev()
                    .fold(failure, |failure, alternative| {
                        Self::pattern_decision_expression(
                            alternative,
                            source.clone(),
                            success.clone(),
                            failure,
                            span,
                            counter,
                        )
                    })
            }
            LoweredPattern::Option { some, payload } => {
                let (outer_pattern, nested_success) = if *some {
                    let name = Self::generated_pattern_name(counter);
                    let nested_success = payload.as_deref().map_or_else(
                        || success.clone(),
                        |payload| {
                            Self::pattern_decision_expression(
                                payload,
                                LoweredExpr {
                                    kind: LoweredExprKind::Variable(name.clone()),
                                    span,
                                },
                                success.clone(),
                                failure.clone(),
                                span,
                                counter,
                            )
                        },
                    );
                    (
                        LoweredPattern::Option {
                            some: true,
                            payload: Some(Box::new(LoweredPattern::Binding { name, span })),
                        },
                        nested_success,
                    )
                } else {
                    (
                        LoweredPattern::Option {
                            some: false,
                            payload: None,
                        },
                        success,
                    )
                };
                LoweredExpr {
                    kind: LoweredExprKind::Match {
                        source: Box::new(source),
                        arms: vec![
                            (outer_pattern, nested_success, span),
                            (LoweredPattern::Wildcard, failure, span),
                        ],
                    },
                    span,
                }
            }
            LoweredPattern::Result { ok, payload } => {
                let name = Self::generated_pattern_name(counter);
                let nested_success = Self::pattern_decision_expression(
                    payload,
                    LoweredExpr {
                        kind: LoweredExprKind::Variable(name.clone()),
                        span,
                    },
                    success,
                    failure.clone(),
                    span,
                    counter,
                );
                LoweredExpr {
                    kind: LoweredExprKind::Match {
                        source: Box::new(source),
                        arms: vec![
                            (
                                LoweredPattern::Result {
                                    ok: *ok,
                                    payload: Box::new(LoweredPattern::Binding { name, span }),
                                },
                                nested_success,
                                span,
                            ),
                            (LoweredPattern::Wildcard, failure, span),
                        ],
                    },
                    span,
                }
            }
            LoweredPattern::Enum {
                name,
                variant,
                patterns,
                fields,
            } => {
                let mut nested = success;
                let outer = if let Some(fields) = fields {
                    let mut generated = Vec::new();
                    for (field, field_span, pattern) in fields.iter().rev() {
                        let binding = Self::generated_pattern_name(counter);
                        nested = Self::pattern_decision_expression(
                            pattern,
                            LoweredExpr {
                                kind: LoweredExprKind::Variable(binding.clone()),
                                span: *field_span,
                            },
                            nested,
                            failure.clone(),
                            *field_span,
                            counter,
                        );
                        generated.push((
                            field.clone(),
                            *field_span,
                            Box::new(LoweredPattern::Binding {
                                name: binding,
                                span: *field_span,
                            }),
                        ));
                    }
                    generated.reverse();
                    LoweredPattern::Enum {
                        name: name.clone(),
                        variant: variant.clone(),
                        patterns: Vec::new(),
                        fields: Some(generated),
                    }
                } else {
                    let mut generated = Vec::new();
                    for pattern in patterns.iter().rev() {
                        let binding = Self::generated_pattern_name(counter);
                        nested = Self::pattern_decision_expression(
                            pattern,
                            LoweredExpr {
                                kind: LoweredExprKind::Variable(binding.clone()),
                                span,
                            },
                            nested,
                            failure.clone(),
                            span,
                            counter,
                        );
                        generated.push(LoweredPattern::Binding {
                            name: binding,
                            span,
                        });
                    }
                    generated.reverse();
                    LoweredPattern::Enum {
                        name: name.clone(),
                        variant: variant.clone(),
                        patterns: generated,
                        fields: None,
                    }
                };
                LoweredExpr {
                    kind: LoweredExprKind::Match {
                        source: Box::new(source),
                        arms: vec![
                            (outer, nested, span),
                            (LoweredPattern::Wildcard, failure, span),
                        ],
                    },
                    span,
                }
            }
            LoweredPattern::Record { name: _, fields } => {
                fields
                    .iter()
                    .rev()
                    .fold(success, |nested, (field, field_span, pattern)| {
                        Self::pattern_decision_expression(
                            pattern,
                            LoweredExpr {
                                kind: LoweredExprKind::FieldGet {
                                    record: Box::new(source.clone()),
                                    field: field.clone(),
                                    field_span: *field_span,
                                },
                                span: *field_span,
                            },
                            nested,
                            failure.clone(),
                            *field_span,
                            counter,
                        )
                    })
            }
        }
    }

    fn compile_advanced_match(
        &mut self,
        source: CompiledExpr,
        arms: &[(LoweredPattern, LoweredExpr, Span)],
        span: Span,
        expected: Option<(&ValueType, &str)>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let name = format!("$pattern_source_{}_{}", span.start, span.end);
        let source_expression = LoweredExpr {
            kind: LoweredExprKind::Variable(name.clone()),
            span,
        };
        let mut counter = 0;
        let mut decision = Self::internal_pattern_failure(span);
        for (pattern, body, pattern_span) in arms.iter().rev() {
            decision = Self::pattern_decision_expression(
                pattern,
                source_expression.clone(),
                body.clone(),
                decision,
                *pattern_span,
                &mut counter,
            );
        }
        let previous = self.bindings.insert(
            name.clone(),
            LocalBinding {
                register: source.register,
                symbol: self.global.allocate_symbol(),
                value_type: source.value_type.clone(),
                scope: self.active_scopes.last().copied().unwrap_or(0),
                value_scope: self.active_scopes.last().copied().unwrap_or(0),
                mutable: false,
                moved: false,
            },
        );
        let value = match expected {
            Some((expected, label)) => {
                self.compile_contextually_expected(&decision, expected, label)
            }
            None => self.compile_expression(&decision),
        };
        if let Some(previous) = previous {
            self.bindings.insert(name, previous);
        } else {
            self.bindings.remove(&name);
        }
        let value = value?;
        let effects = self.union_effects([source.effects, value.effects]);
        Ok(CompiledExpr {
            register: value.register,
            value_type: value.value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Match {
                    source: Box::new(source.hir),
                    arms: vec![value.hir],
                },
                None,
                &value.value_type,
                effects,
                span,
            ),
        })
    }

    fn compile_expanded_or_match(
        &mut self,
        source: CompiledExpr,
        arms: Vec<(LoweredPattern, LoweredExpr, Span)>,
        span: Span,
        expected: Option<(&ValueType, &str)>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let name = format!("$or_pattern_source_{}_{}", span.start, span.end);
        let previous = self.bindings.insert(
            name.clone(),
            LocalBinding {
                register: source.register,
                symbol: self.global.allocate_symbol(),
                value_type: source.value_type.clone(),
                scope: self.active_scopes.last().copied().unwrap_or(0),
                value_scope: self.active_scopes.last().copied().unwrap_or(0),
                mutable: false,
                moved: false,
            },
        );
        let expanded = LoweredExpr {
            kind: LoweredExprKind::Match {
                source: Box::new(LoweredExpr {
                    kind: LoweredExprKind::Variable(name.clone()),
                    span,
                }),
                arms,
            },
            span,
        };
        let value = match expected {
            Some((expected, label)) => {
                self.compile_contextually_expected(&expanded, expected, label)
            }
            None => self.compile_expression(&expanded),
        };
        if let Some(previous) = previous {
            self.bindings.insert(name, previous);
        } else {
            self.bindings.remove(&name);
        }
        let value = value?;
        let effects = self.union_effects([source.effects, value.effects]);
        Ok(CompiledExpr {
            register: value.register,
            value_type: value.value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Match {
                    source: Box::new(source.hir),
                    arms: vec![value.hir],
                },
                None,
                &value.value_type,
                effects,
                span,
            ),
        })
    }

    fn validate_local_function_effects(
        &self,
        function: &FunctionInfo,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let current = self
            .global
            .bundle
            .functions
            .iter()
            .map(|function| function.effects.clone())
            .collect::<Vec<_>>();
        let required = required_body_effects(self.global.bundle, function, &current)?;
        if required
            .iter()
            .any(|effect| function.effects.binary_search(effect).is_err())
        {
            return Err(Diagnostic::new(
                "E2403",
                format!(
                    "local function '{}' requires undeclared effects [{}]",
                    function.lowered.name,
                    required.join(", ")
                ),
                span,
            ));
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn compile_local_function_declaration(
        &mut self,
        function: &super::LoweredLocalFunction,
    ) -> Result<(), Diagnostic> {
        if self.bindings.contains_key(&function.name)
            || self.local_name_conflicts(&function.name)
            || resolve_function_name(self.global.bundle, &self.info.module, &function.name)?
                .is_some()
            || resolve_named_type(
                &self.global.bundle.modules,
                &self.global.bundle.types,
                &self.info.module,
                &function.name,
                function.name_span,
            )
            .is_ok()
        {
            return Err(Diagnostic::new(
                "E3005",
                format!(
                    "local function '{}' conflicts with an existing name",
                    function.name
                ),
                function.name_span,
            ));
        }

        let parameter_names = function
            .parameters
            .iter()
            .map(|(name, _, _)| name.clone())
            .collect::<BTreeSet<_>>();
        if parameter_names.len() != function.parameters.len() {
            return Err(Diagnostic::new(
                "E3005",
                format!(
                    "local function '{}' has duplicate parameters",
                    function.name
                ),
                function.name_span,
            ));
        }
        let mut free = free_variables(&function.body, &function.parameters);
        for (parameter_index, default) in function.parameter_defaults.iter().enumerate() {
            let Some(default) = default else {
                continue;
            };
            let default_body = LoweredBody {
                statements: Vec::new(),
                tail: Some(default.value.clone()),
                span: default.span,
            };
            free.extend(free_variables(
                &default_body,
                &function.parameters[..parameter_index],
            ));
        }
        let mut captures = free
            .into_iter()
            .filter(|name| self.bindings.contains_key(name))
            .collect::<Vec<_>>();
        captures.sort();
        if let Some(binding) = captures.first() {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "local function '{}' cannot capture enclosing binding '{binding}'",
                    function.name
                ),
                function.span,
            ));
        }

        if function.parameter_defaults.len() != function.parameters.len() {
            return Err(Diagnostic::new(
                "E3010",
                format!(
                    "local function '{}' has inconsistent parameter default metadata",
                    function.name
                ),
                function.name_span,
            ));
        }
        let ordinal = self.local_function_ordinal;
        self.local_function_ordinal = self
            .local_function_ordinal
            .checked_add(1)
            .expect("local function ordinal fits");
        let private_name = format!("$local@{}@{ordinal}@{}", self.info.symbol, function.name);
        let generics = BTreeSet::new();
        let parameters = function
            .parameters
            .iter()
            .map(|(_, value_type, _)| {
                semantic_type(
                    value_type,
                    &generics,
                    &self.info.module,
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let return_type = semantic_type(
            &function.return_type,
            &generics,
            &self.info.module,
            &self.global.bundle.modules,
            &self.global.bundle.types,
        )?;
        let effects = function.declared_effects.clone().unwrap_or_default();
        let mut unavailable_local_functions = self.reserved_local_names();
        unavailable_local_functions.insert(function.name.clone());
        let mut saw_default = false;
        for (parameter_index, default) in function.parameter_defaults.iter().enumerate() {
            let Some(default) = default else {
                if saw_default {
                    let (parameter, _, parameter_span) = &function.parameters[parameter_index];
                    return Err(Diagnostic::new(
                        "E3010",
                        format!(
                            "required parameter '{parameter}' cannot follow a default parameter"
                        ),
                        *parameter_span,
                    ));
                }
                continue;
            };
            saw_default = true;
            let helper_name = default_helper_name(&private_name, parameter_index);
            let helper_symbol = self.global.allocate_symbol();
            let helper_id = u32::try_from(self.global.functions.len()).map_err(|_| {
                Diagnostic::new("E3005", "too many local default helpers", default.span)
            })?;
            let helper = FunctionInfo {
                module: self.info.module.clone(),
                symbol: helper_symbol,
                bytecode: Some(helper_id),
                lowered: LoweredFunction {
                    exported: false,
                    is_async: false,
                    name: helper_name.clone(),
                    name_span: default.span,
                    generics: Vec::new(),
                    parameters: function.parameters[..parameter_index].to_vec(),
                    parameter_defaults: vec![None; parameter_index],
                    return_type: function.parameters[parameter_index].1.clone(),
                    declared_effects: Some(Vec::new()),
                    effects_span: Some(default.span),
                    body: LoweredBody {
                        statements: Vec::new(),
                        tail: Some(default.value.clone()),
                        span: default.span,
                    },
                },
                parameters: parameters[..parameter_index].to_vec(),
                return_type: parameters[parameter_index].clone(),
                effects: Vec::new(),
                is_const: false,
            };
            self.validate_local_function_effects(&helper, default.span)?;
            self.global.functions.push(None);
            let (compiled, hir, mir) = lower_one_function(
                self.global,
                helper.clone(),
                helper_id,
                Vec::new(),
                BTreeMap::new(),
                unavailable_local_functions.clone(),
                &BTreeMap::new(),
            )?;
            self.global.functions[helper_id as usize] = Some(compiled);
            self.global
                .hir_modules
                .entry(helper.module.clone())
                .or_default()
                .push(hir);
            self.global.mir_functions.push(mir);
            self.local_functions.insert(helper_name, helper);
        }

        let symbol = self.global.allocate_symbol();
        let function_id = u32::try_from(self.global.functions.len()).map_err(|_| {
            Diagnostic::new("E3005", "too many local functions", function.name_span)
        })?;
        let target = FunctionInfo {
            module: self.info.module.clone(),
            symbol,
            bytecode: Some(function_id),
            lowered: LoweredFunction {
                exported: false,
                is_async: false,
                name: private_name,
                name_span: function.name_span,
                generics: Vec::new(),
                parameters: function.parameters.clone(),
                parameter_defaults: function.parameter_defaults.clone(),
                return_type: function.return_type.clone(),
                declared_effects: Some(effects.clone()),
                effects_span: function.effects_span,
                body: function.body.clone(),
            },
            parameters,
            return_type,
            effects,
            is_const: false,
        };
        self.validate_local_function_effects(
            &target,
            function.effects_span.unwrap_or(function.span),
        )?;
        self.global.functions.push(None);
        let (compiled, hir, mir) = lower_one_function(
            self.global,
            target.clone(),
            function_id,
            Vec::new(),
            BTreeMap::new(),
            unavailable_local_functions,
            &BTreeMap::new(),
        )?;
        if !compiled.captures.is_empty() {
            return Err(Diagnostic::new(
                "E3011",
                format!("local function '{}' cannot capture values", function.name),
                function.name_span,
            ));
        }
        self.global.functions[function_id as usize] = Some(compiled);
        self.global
            .hir_modules
            .entry(target.module.clone())
            .or_default()
            .push(hir);
        self.global.mir_functions.push(mir);
        self.local_functions.insert(function.name.clone(), target);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_record_match(
        &mut self,
        source: CompiledExpr,
        layout: &[RecordField],
        arms: &[(LoweredPattern, LoweredExpr, Span)],
        span: Span,
        expected: Option<(&ValueType, &str)>,
    ) -> Result<CompiledExpr, Diagnostic> {
        if arms.len() != 1 {
            return Err(Diagnostic::new(
                "E3007",
                "a structural record match has exactly one reachable arm",
                span,
            ));
        }
        let (pattern, arm, pattern_span) = &arms[0];
        let fields = match pattern {
            LoweredPattern::Wildcard => Vec::new(),
            LoweredPattern::Record { name, fields } => {
                let expected = resolve_named_type(
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                    &self.info.module,
                    name,
                    *pattern_span,
                )?;
                if expected != source.value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "record pattern has a different structural type",
                        *pattern_span,
                    ));
                }
                if fields.len() != layout.len() {
                    return Err(Diagnostic::new(
                        "E3007",
                        "record pattern must contain exactly the declared fields",
                        *pattern_span,
                    ));
                }
                fields.clone()
            }
            _ => {
                return Err(Diagnostic::new(
                    "E3007",
                    "record match requires a record pattern or '_'",
                    *pattern_span,
                ));
            }
        };
        let outer = self.bindings.clone();
        let mut seen_fields = BTreeSet::new();
        let mut seen_bindings = BTreeSet::new();
        for (field, field_span, binding) in fields {
            if !seen_fields.insert(field.clone()) {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("duplicate record pattern field '{field}'"),
                    field_span,
                ));
            }
            let index = layout
                .iter()
                .position(|candidate| candidate.name == field)
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E3007",
                        format!("record pattern has no field '{field}'"),
                        field_span,
                    )
                })?;
            if let Some(binding) = Self::simple_pattern_binding(&binding, field_span)? {
                if !seen_bindings.insert(binding.clone())
                    || self.bindings.contains_key(&binding)
                    || self.local_name_conflicts(&binding)
                {
                    return Err(Diagnostic::new(
                        "E3005",
                        format!("duplicate pattern binding '{binding}'"),
                        field_span,
                    ));
                }
                let value_type = layout[index].value_type.clone();
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::FieldGet {
                    destination: register,
                    record: source.register,
                    field: u32::try_from(index).expect("field index fits"),
                });
                self.mir.push(MirOperation::FieldGet {
                    destination: u32::from(register),
                    record: u32::from(source.register),
                });
                let symbol = self.global.allocate_symbol();
                let scope = self.active_scopes.last().copied().unwrap_or(0);
                self.bindings.insert(
                    binding,
                    LocalBinding {
                        register,
                        symbol,
                        value_type,
                        scope,
                        value_scope: scope,
                        mutable: false,
                        moved: false,
                    },
                );
            }
        }
        let value = match expected {
            Some((expected, label)) => self.compile_contextually_expected(arm, expected, label)?,
            None => self.compile_expression(arm)?,
        };
        self.bindings = outer;
        let effects = self.union_effects([source.effects, value.effects]);
        Ok(CompiledExpr {
            register: value.register,
            value_type: value.value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Match {
                    source: Box::new(source.hir),
                    arms: vec![value.hir],
                },
                None,
                &value.value_type,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_if(
        &mut self,
        condition: &LoweredExpr,
        then_body: &LoweredBody,
        else_branch: Option<&LoweredElse>,
        span: Span,
        expected: Option<(&ValueType, &str)>,
        compiled_condition: Option<CompiledExpr>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let constant_condition = match &condition.kind {
            LoweredExprKind::Bool(value) => Some(*value),
            _ => None,
        };
        let false_branch_continues = constant_condition != Some(true);
        let true_branch_continues = constant_condition != Some(false);
        let outer_control_reachable = self.control_reachable;
        let condition_span = condition.span;
        let condition = match compiled_condition {
            Some(condition) => condition,
            None => self.compile_expression(condition)?,
        };
        if condition.value_type != ValueType::Bool {
            return Err(Diagnostic::new(
                "E3007",
                format!("if condition must be Bool, found {}", condition.value_type),
                condition_span,
            ));
        }

        let branch_index = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        let base = self.next_mir_block();
        let false_block = base + 1;
        let true_block = base + 2;
        self.mir_blocks.extend((0..3).map(|_| MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        }));
        let outer_bindings = self.bindings.clone();
        let outer_ownership = self.ownership_states.clone();
        let mut continuing_bindings: Option<BTreeMap<String, BindingState>> = None;
        let mut continuing_ownership: Option<BTreeMap<Register, OwnershipRecord>> = None;
        let mut static_bindings: Option<BTreeMap<String, BindingState>> = None;
        let mut static_ownership: Option<BTreeMap<Register, OwnershipRecord>> = None;
        let mut result: Option<(Register, ValueType)> = None;
        let mut never_register = None;
        let mut jumps = Vec::new();

        let false_target = u32::try_from(self.code.len()).expect("instruction index fits");
        self.bindings = outer_bindings.clone();
        self.ownership_states = outer_ownership.clone();
        self.control_reachable = outer_control_reachable && constant_condition != Some(true);
        let false_region_capture = self.begin_nested_mir_region();
        let false_operation_start = self.mir.len();
        let false_span = match else_branch {
            Some(LoweredElse::Body(body)) => body.span,
            Some(LoweredElse::If(expression)) => expression.span,
            None => span,
        };
        let (false_hir, false_value) = match else_branch {
            Some(LoweredElse::Body(body)) => {
                let (hir, value, _) = self.compile_block_value_expected(body, expected)?;
                (hir, value)
            }
            Some(LoweredElse::If(expression)) => {
                let value = match expected {
                    Some((expected, label)) => {
                        self.compile_contextually_expected(expression, expected, label)?
                    }
                    None => self.compile_expression(expression)?,
                };
                (value.hir.clone(), value)
            }
            None => {
                let unit = LoweredExpr {
                    kind: LoweredExprKind::Unit,
                    span,
                };
                let value = self.compile_expression(&unit)?;
                (value.hir.clone(), value)
            }
        };
        let false_region = self.finish_nested_mir_region(false_region_capture);
        let false_value_scope = self.sub_agent_value_scope(&false_value);
        let false_runtime_falls_through =
            false_branch_continues && self.runtime_falls_through(&false_value);
        let mut false_operations = self.mir.split_off(false_operation_start);
        let false_terminal = if false_value.value_type == ValueType::Never {
            never_register.get_or_insert(false_value.register);
            if !false_branch_continues
                && !matches!(
                    self.code.last(),
                    Some(
                        Instruction::Return { .. }
                            | Instruction::Stop { .. }
                            | Instruction::Fail { .. }
                    )
                )
            {
                let ownership_at_entry = outer_ownership.keys().copied().collect();
                let reason = self.terminate_source_dead_path(&ownership_at_entry, false_span)?;
                false_operations.push(MirOperation::Constant {
                    destination: u32::from(reason),
                });
                Some(MirTerminator::Stop {
                    reason: u32::from(reason),
                })
            } else {
                match self.code.last() {
                    Some(Instruction::Return { source }) => Some(MirTerminator::Return {
                        source: u32::from(*source),
                    }),
                    Some(Instruction::Stop { reason }) => Some(MirTerminator::Stop {
                        reason: u32::from(*reason),
                    }),
                    Some(Instruction::Fail { reason }) => Some(MirTerminator::Fail {
                        reason: u32::from(*reason),
                    }),
                    Some(Instruction::Jump { .. }) if false_branch_continues => None,
                    _ => Some(MirTerminator::Unreachable),
                }
            }
        } else {
            let (result_register, result_type) = if let Some((register, value_type)) = &result {
                if *value_type != false_value.value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "if branches must have one exact result type",
                        span,
                    ));
                }
                (*register, value_type.clone())
            } else {
                let register = self.allocate(false_value.value_type.clone())?;
                result = Some((register, false_value.value_type.clone()));
                (register, false_value.value_type.clone())
            };
            self.code.push(Instruction::Move {
                destination: result_register,
                source: false_value.register,
            });
            self.mark_last_instruction(false_span);
            false_operations.push(MirOperation::Move {
                destination: u32::from(result_register),
                source: u32::from(false_value.register),
            });
            if is_affine(&result_type) {
                let ownership = self
                    .ownership_states
                    .get(&false_value.register)
                    .copied()
                    .unwrap_or(OwnershipRecord {
                        scope: 0,
                        state: MirOwnershipState::Live,
                        must_consume: matches!(result_type, ValueType::Task(_)),
                    });
                self.consume_ownership(false_value.register, MirOwnershipState::Moved);
                self.record_ownership(
                    result_register,
                    ownership.scope,
                    MirOwnershipState::Live,
                    ownership.must_consume,
                );
            }
            self.validate_conditional_branch_state(
                &outer_bindings,
                &outer_ownership,
                Some(result_register),
                span,
                &mut static_bindings,
                &mut static_ownership,
            )?;
            if false_runtime_falls_through {
                self.validate_conditional_branch_state(
                    &outer_bindings,
                    &outer_ownership,
                    Some(result_register),
                    span,
                    &mut continuing_bindings,
                    &mut continuing_ownership,
                )?;
            }
            jumps.push(self.code.len());
            self.code.push(Instruction::Jump { target: 0 });
            self.mark_last_instruction(false_span);
            None
        };

        let true_target = u32::try_from(self.code.len()).expect("instruction index fits");
        self.bindings = outer_bindings.clone();
        self.ownership_states = outer_ownership.clone();
        self.control_reachable = outer_control_reachable && constant_condition != Some(false);
        let true_region_capture = self.begin_nested_mir_region();
        let true_operation_start = self.mir.len();
        let (true_hir, true_value, _) = self.compile_block_value_expected(then_body, expected)?;
        let true_region = self.finish_nested_mir_region(true_region_capture);
        let true_value_scope = self.sub_agent_value_scope(&true_value);
        let true_runtime_falls_through =
            true_branch_continues && self.runtime_falls_through(&true_value);
        let mut true_operations = self.mir.split_off(true_operation_start);
        if else_branch.is_none()
            && !matches!(true_value.value_type, ValueType::Unit | ValueType::Never)
        {
            return Err(Diagnostic::new(
                "E3007",
                format!(
                    "if without else requires a Void true branch, found {}",
                    true_value.value_type
                ),
                then_body.span,
            ));
        }
        let true_terminal = if true_value.value_type == ValueType::Never {
            never_register.get_or_insert(true_value.register);
            if !true_branch_continues
                && !matches!(
                    self.code.last(),
                    Some(
                        Instruction::Return { .. }
                            | Instruction::Stop { .. }
                            | Instruction::Fail { .. }
                    )
                )
            {
                let ownership_at_entry = outer_ownership.keys().copied().collect();
                let reason =
                    self.terminate_source_dead_path(&ownership_at_entry, then_body.span)?;
                true_operations.push(MirOperation::Constant {
                    destination: u32::from(reason),
                });
                Some(MirTerminator::Stop {
                    reason: u32::from(reason),
                })
            } else {
                match self.code.last() {
                    Some(Instruction::Return { source }) => Some(MirTerminator::Return {
                        source: u32::from(*source),
                    }),
                    Some(Instruction::Stop { reason }) => Some(MirTerminator::Stop {
                        reason: u32::from(*reason),
                    }),
                    Some(Instruction::Fail { reason }) => Some(MirTerminator::Fail {
                        reason: u32::from(*reason),
                    }),
                    Some(Instruction::Jump { .. }) if true_branch_continues => None,
                    _ => Some(MirTerminator::Unreachable),
                }
            }
        } else {
            let (result_register, result_type) = if let Some((register, value_type)) = &result {
                if *value_type != true_value.value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "if branches must have one exact result type",
                        then_body.span,
                    ));
                }
                (*register, value_type.clone())
            } else {
                let register = self.allocate(true_value.value_type.clone())?;
                result = Some((register, true_value.value_type.clone()));
                (register, true_value.value_type.clone())
            };
            self.code.push(Instruction::Move {
                destination: result_register,
                source: true_value.register,
            });
            self.mark_last_instruction(then_body.span);
            true_operations.push(MirOperation::Move {
                destination: u32::from(result_register),
                source: u32::from(true_value.register),
            });
            if is_affine(&result_type) {
                let ownership = self
                    .ownership_states
                    .get(&true_value.register)
                    .copied()
                    .unwrap_or(OwnershipRecord {
                        scope: 0,
                        state: MirOwnershipState::Live,
                        must_consume: matches!(result_type, ValueType::Task(_)),
                    });
                self.consume_ownership(true_value.register, MirOwnershipState::Moved);
                self.record_ownership(
                    result_register,
                    ownership.scope,
                    MirOwnershipState::Live,
                    ownership.must_consume,
                );
            }
            self.validate_conditional_branch_state(
                &outer_bindings,
                &outer_ownership,
                Some(result_register),
                span,
                &mut static_bindings,
                &mut static_ownership,
            )?;
            if true_runtime_falls_through {
                self.validate_conditional_branch_state(
                    &outer_bindings,
                    &outer_ownership,
                    Some(result_register),
                    span,
                    &mut continuing_bindings,
                    &mut continuing_ownership,
                )?;
            }
            jumps.push(self.code.len());
            self.code.push(Instruction::Jump { target: 0 });
            self.mark_last_instruction(then_body.span);
            None
        };

        let join = u32::try_from(self.code.len()).expect("instruction index fits");
        for jump in jumps {
            self.code[jump] = Instruction::Jump { target: join };
        }
        self.code[branch_index] = Instruction::BranchBool {
            condition: condition.register,
            false_target,
            true_target,
        };
        self.mark_instruction(branch_index, condition_span);

        let has_continuation = false_terminal.is_none() || true_terminal.is_none();
        let join_block = self.next_mir_block();
        self.mir_blocks[base as usize - 1] = MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::SwitchBool {
                false_target: false_block,
                true_target: true_block,
            },
        };
        self.mir_blocks[false_block as usize - 1] = MirBlock {
            operations: false_operations,
            terminator: false_region.entry.map_or_else(
                || {
                    false_terminal
                        .clone()
                        .unwrap_or(MirTerminator::Goto { target: join_block })
                },
                |target| MirTerminator::Goto { target },
            ),
        };
        self.mir_blocks[true_block as usize - 1] = MirBlock {
            operations: true_operations,
            terminator: true_region.entry.map_or_else(
                || {
                    true_terminal
                        .clone()
                        .unwrap_or(MirTerminator::Goto { target: join_block })
                },
                |target| MirTerminator::Goto { target },
            ),
        };
        if let Some(tail) = false_region.tail {
            self.set_mir_handoff(
                tail,
                false_terminal
                    .clone()
                    .unwrap_or(MirTerminator::Goto { target: join_block }),
            );
        }
        if let Some(tail) = true_region.tail {
            self.set_mir_handoff(
                tail,
                true_terminal
                    .clone()
                    .unwrap_or(MirTerminator::Goto { target: join_block }),
            );
        }
        if has_continuation {
            self.mir_blocks.push(MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            });
            self.register_mir_region(base, join_block);
        } else {
            if let Some(previous) = self.mir_tail {
                self.mir_blocks[previous as usize - 1].terminator =
                    MirTerminator::Goto { target: base };
            }
            self.mir_entries.push(base);
            self.mir_tail = None;
        }

        self.bindings = outer_bindings;
        self.control_reachable = outer_control_reachable;
        if let Some(joined) = continuing_bindings {
            for (name, state) in joined {
                if let Some(binding) = self.bindings.get_mut(&name) {
                    binding.moved = state.moved;
                    binding.value_scope = state.value_scope;
                }
            }
        }
        self.ownership_states = outer_ownership;
        if let Some(joined) = continuing_ownership {
            self.ownership_states.extend(joined);
        }

        let runtime_falls_through = match constant_condition {
            Some(true) => true_runtime_falls_through,
            Some(false) => false_runtime_falls_through,
            None => false_runtime_falls_through || true_runtime_falls_through,
        };
        let (register, value_type) = result
            .or_else(|| never_register.map(|register| (register, ValueType::Never)))
            .expect("an if always has a true and false path");
        if contains_stored_sub_agent(&value_type) {
            let value_scope = match (
                false_value.value_type != ValueType::Never,
                true_value.value_type != ValueType::Never,
            ) {
                (true, true) => self.deeper_scope(false_value_scope, true_value_scope),
                (true, false) => false_value_scope,
                (false, true) => true_value_scope,
                (false, false) => self.current_scope(),
            };
            self.sub_agent_value_scopes.insert(register, value_scope);
        }
        if !runtime_falls_through {
            self.runtime_terminal_values.insert(register);
        }
        let effects =
            self.union_effects([condition.effects, false_value.effects, true_value.effects]);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::If {
                    condition: Box::new(condition.hir),
                    then_branch: Box::new(true_hir),
                    else_branch: else_branch.map(|_| Box::new(false_hir)),
                },
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn validate_conditional_branch_state(
        &self,
        outer_bindings: &BTreeMap<String, LocalBinding>,
        outer_ownership: &BTreeMap<Register, OwnershipRecord>,
        result: Option<Register>,
        span: Span,
        joined_bindings: &mut Option<BTreeMap<String, BindingState>>,
        joined_ownership: &mut Option<BTreeMap<Register, OwnershipRecord>>,
    ) -> Result<(), Diagnostic> {
        let binding_state = outer_bindings
            .keys()
            .filter_map(|name| {
                self.bindings
                    .get(name)
                    .map(|binding| (name.clone(), BindingState::from(binding)))
            })
            .collect::<BTreeMap<_, _>>();
        let ownership_state = outer_ownership
            .keys()
            .filter_map(|register| {
                self.ownership_states
                    .get(register)
                    .map(|ownership| (*register, *ownership))
            })
            .chain(result.and_then(|register| {
                self.ownership_states
                    .get(&register)
                    .map(|ownership| (register, *ownership))
            }))
            .collect::<BTreeMap<_, _>>();
        if self.ownership_states.iter().any(|(register, ownership)| {
            !outer_ownership.contains_key(register)
                && Some(*register) != result
                && ownership.state == MirOwnershipState::Live
                && ownership.must_consume
        }) {
            return Err(Diagnostic::new(
                "E3011",
                "conditional branch leaves a live affine obligation",
                span,
            ));
        }
        if joined_bindings
            .as_ref()
            .is_some_and(|joined| joined != &binding_state)
            || joined_ownership
                .as_ref()
                .is_some_and(|joined| joined != &ownership_state)
        {
            return Err(Diagnostic::new(
                "E3011",
                "conditional paths must leave the same affine ownership state",
                span,
            ));
        }
        joined_bindings.get_or_insert(binding_state);
        joined_ownership.get_or_insert(ownership_state);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_match(
        &mut self,
        source: &LoweredExpr,
        arms: &[(LoweredPattern, LoweredExpr, Span)],
        span: Span,
        expected: Option<(&ValueType, &str)>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let source = self.compile_expression(source)?;
        if arms
            .iter()
            .any(|(pattern, _, _)| Self::pattern_uses_decision_lowering(pattern))
        {
            let mut earlier = Vec::new();
            let mut alternatives = Vec::new();
            for (pattern, _, pattern_span) in arms {
                self.collect_pattern_bindings(pattern, &source.value_type, *pattern_span)?;
                let mut current = Vec::new();
                Self::flattened_alternatives(pattern, &mut current);
                for (index, alternative) in current.into_iter().enumerate() {
                    if self.pattern_fully_covered(&earlier, alternative, &source.value_type)? {
                        let unreachable_span = match pattern {
                            LoweredPattern::Or { operator_spans, .. } if index > 0 => {
                                operator_spans
                                    .get(index - 1)
                                    .copied()
                                    .unwrap_or(*pattern_span)
                            }
                            _ => *pattern_span,
                        };
                        return Err(Diagnostic::new(
                            "E2016",
                            "match pattern alternative is unreachable",
                            unreachable_span,
                        ));
                    }
                    earlier.push(alternative);
                    alternatives.push(alternative);
                }
            }
            if !self.advanced_patterns_are_exhaustive(&alternatives, &source.value_type)? {
                return Err(Diagnostic::new(
                    "E2015",
                    "non-exhaustive match; add a finite complete set of patterns or a catch-all",
                    span,
                ));
            }
            let pure_top_level_or = arms.iter().all(|(pattern, _, _)| match pattern {
                LoweredPattern::Or { alternatives, .. } => alternatives
                    .iter()
                    .all(|pattern| !Self::pattern_uses_decision_lowering(pattern)),
                _ => !Self::pattern_uses_decision_lowering(pattern),
            });
            if pure_top_level_or {
                let mut expanded = Vec::new();
                for (pattern, body, pattern_span) in arms {
                    if let LoweredPattern::Or {
                        alternatives,
                        operator_spans,
                    } = pattern
                    {
                        for (index, alternative) in alternatives.iter().enumerate() {
                            expanded.push((
                                alternative.clone(),
                                body.clone(),
                                if index == 0 {
                                    *pattern_span
                                } else {
                                    operator_spans
                                        .get(index - 1)
                                        .copied()
                                        .unwrap_or(*pattern_span)
                                },
                            ));
                        }
                    } else {
                        expanded.push((pattern.clone(), body.clone(), *pattern_span));
                    }
                }
                return self.compile_expanded_or_match(source, expanded, span, expected);
            }
            return self.compile_advanced_match(source, arms, span, expected);
        }
        if let ValueType::Record(layout) = source.value_type.clone() {
            return self.compile_record_match(source, &layout, arms, span, expected);
        }
        let variant_count = match &source.value_type {
            ValueType::Bool | ValueType::Option(_) | ValueType::Result(_, _) => 2,
            ValueType::Enum(id) => self.global.bundle.enum_types[*id as usize].variants.len(),
            _ => {
                return Err(Diagnostic::new(
                    "E3007",
                    "match requires Bool, Option, Result, or a nominal enum",
                    span,
                ));
            }
        };
        let mut planned = vec![None; variant_count];
        let mut wildcard = None;
        for (arm_index, (pattern, _, pattern_span)) in arms.iter().enumerate() {
            if matches!(pattern, LoweredPattern::Range { .. }) {
                return Err(Diagnostic::new(
                    "E3011",
                    "range patterns are not implemented in this compiler pass",
                    *pattern_span,
                ));
            }
            if matches!(pattern, LoweredPattern::Or { .. }) {
                return Err(Diagnostic::new(
                    "E3011",
                    "OR patterns are not implemented in this compiler pass",
                    *pattern_span,
                ));
            }
            if wildcard.is_some() || planned.iter().all(Option::is_some) {
                return Err(Diagnostic::new(
                    "E2016",
                    "match case is unreachable",
                    *pattern_span,
                ));
            }
            let variant = match (&source.value_type, pattern) {
                (ValueType::Bool, LoweredPattern::Bool(false)) => Some(0),
                (ValueType::Bool, LoweredPattern::Bool(true)) => Some(1),
                (
                    ValueType::Enum(id),
                    LoweredPattern::Enum {
                        name,
                        variant,
                        patterns,
                        fields,
                    },
                ) => {
                    let expected = resolve_named_type(
                        &self.global.bundle.modules,
                        &self.global.bundle.types,
                        &self.info.module,
                        name,
                        *pattern_span,
                    )?;
                    if expected != ValueType::Enum(*id) {
                        return Err(Diagnostic::new(
                            "E3007",
                            "match pattern uses a different nominal enum",
                            *pattern_span,
                        ));
                    }
                    let variant_index = self.global.bundle.enum_types[*id as usize]
                        .variants
                        .iter()
                        .position(|candidate| candidate.name == *variant)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                "E3007",
                                format!("unknown enum variant '{variant}'"),
                                *pattern_span,
                            )
                        })?;
                    match (
                        &self.global.bundle.enum_types[*id as usize].variants[variant_index]
                            .payload,
                        fields,
                    ) {
                        (EnumPayloadType::Unit, None) if patterns.is_empty() => {}
                        (EnumPayloadType::Tuple(expected), None)
                            if expected.len() == patterns.len() =>
                        {
                            for pattern in patterns {
                                Self::simple_pattern_binding(pattern, *pattern_span)?;
                            }
                        }
                        (EnumPayloadType::Record(expected), Some(fields)) => {
                            let supplied = fields
                                .iter()
                                .map(|(name, _, _)| name)
                                .collect::<BTreeSet<_>>();
                            if supplied.len() != fields.len()
                                || supplied.len() != expected.len()
                                || expected.iter().any(|field| !supplied.contains(&field.name))
                            {
                                return Err(Diagnostic::new(
                                    "E3007",
                                    "enum record pattern must contain every field exactly once",
                                    *pattern_span,
                                ));
                            }
                            for (_, field_span, pattern) in fields {
                                Self::simple_pattern_binding(pattern, *field_span)?;
                            }
                        }
                        _ => {
                            return Err(Diagnostic::new(
                                "E3007",
                                "enum pattern uses the wrong payload form or count",
                                *pattern_span,
                            ));
                        }
                    }
                    Some(variant_index)
                }
                (ValueType::Option(_), LoweredPattern::Option { some, .. }) => {
                    Some(usize::from(*some))
                }
                (ValueType::Result(_, _), LoweredPattern::Result { ok, .. }) => {
                    Some(usize::from(!*ok))
                }
                (_, LoweredPattern::Wildcard) => None,
                _ => {
                    return Err(Diagnostic::new(
                        "E3007",
                        "match pattern does not match the source type",
                        *pattern_span,
                    ));
                }
            };
            if let Some(variant) = variant {
                if planned[variant].replace(arm_index).is_some() {
                    return Err(Diagnostic::new(
                        "E2016",
                        "duplicate match case is unreachable",
                        *pattern_span,
                    ));
                }
            } else if wildcard.replace(arm_index).is_some() {
                return Err(Diagnostic::new(
                    "E2016",
                    "duplicate match case is unreachable",
                    *pattern_span,
                ));
            }
        }
        let missing = if wildcard.is_some() {
            Vec::new()
        } else {
            let cases = match &source.value_type {
                ValueType::Bool => vec!["false".to_owned(), "true".to_owned()],
                ValueType::Option(_) => vec!["None".to_owned(), "Some".to_owned()],
                ValueType::Result(_, _) => vec!["Ok".to_owned(), "Err".to_owned()],
                ValueType::Enum(id) => self.global.bundle.enum_types[*id as usize]
                    .variants
                    .iter()
                    .map(|variant| variant.name.clone())
                    .collect(),
                _ => unreachable!("match source type was validated"),
            };
            cases
                .into_iter()
                .enumerate()
                .filter_map(|(index, name)| planned[index].is_none().then_some(name))
                .collect::<Vec<_>>()
        };
        if !missing.is_empty() {
            return Err(Diagnostic::new(
                "E2015",
                format!(
                    "non-exhaustive match; missing cases: {}",
                    missing.join(", ")
                ),
                span,
            ));
        }
        for plan in &mut planned {
            if plan.is_none() {
                *plan = wildcard;
            }
        }
        let branch_index = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        let mir_arm_count = planned.len();
        let base = self.next_mir_block();
        self.mir_blocks
            .extend((0..=mir_arm_count).map(|_| MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            }));
        let mut targets = Vec::new();
        let mut target_bindings = Vec::new();
        let mut jumps = Vec::new();
        let mut result = None;
        let mut never_register = None;
        let mut hir_arms = Vec::new();
        let mut arm_effects = Vec::new();
        let mut arm_operations = Vec::new();
        let mut arm_terminals = Vec::new();
        let mut arm_regions = Vec::new();
        let mut arm_value_scopes = Vec::new();
        let branch_bindings = self.bindings.clone();
        let branch_ownership = self.ownership_states.clone();
        let mut joined_bindings: Option<BTreeMap<String, BindingState>> = None;
        let mut joined_ownership: Option<BTreeMap<Register, OwnershipRecord>> = None;
        for (variant_index, arm_index) in planned.into_iter().map(Option::unwrap).enumerate() {
            self.bindings = branch_bindings.clone();
            for (register, state) in &branch_ownership {
                self.ownership_states.insert(*register, *state);
            }
            targets.push(u32::try_from(self.code.len()).expect("instruction index fits"));
            let binding_names = match (&source.value_type, &arms[arm_index].0) {
                (
                    ValueType::Option(_),
                    LoweredPattern::Option {
                        some: true,
                        payload,
                    },
                ) => {
                    vec![
                        payload
                            .as_deref()
                            .map(|pattern| Self::simple_pattern_binding(pattern, arms[arm_index].2))
                            .transpose()?
                            .flatten(),
                    ]
                }
                (ValueType::Result(_, _), LoweredPattern::Result { payload, .. }) => {
                    vec![Self::simple_pattern_binding(payload, arms[arm_index].2)?]
                }
                (
                    ValueType::Enum(id),
                    LoweredPattern::Enum {
                        variant,
                        patterns,
                        fields,
                        ..
                    },
                ) => {
                    let metadata = self.global.bundle.enum_types[*id as usize]
                        .variants
                        .iter()
                        .find(|candidate| candidate.name == *variant)
                        .expect("validated enum match variant");
                    match (&metadata.payload, fields) {
                        (EnumPayloadType::Tuple(_), None) => patterns
                            .iter()
                            .map(|pattern| Self::simple_pattern_binding(pattern, arms[arm_index].2))
                            .collect::<Result<Vec<_>, _>>()?,
                        (EnumPayloadType::Record(expected), Some(fields)) => {
                            let supplied = fields
                                .iter()
                                .map(|(name, field_span, pattern)| {
                                    Ok((name, Self::simple_pattern_binding(pattern, *field_span)?))
                                })
                                .collect::<Result<Vec<_>, Diagnostic>>()?
                                .into_iter()
                                .collect::<BTreeMap<_, _>>();
                            expected
                                .iter()
                                .map(|field| supplied[&field.name].clone())
                                .collect()
                        }
                        _ => Vec::new(),
                    }
                }
                _ => Vec::new(),
            };
            let mut payload_registers = Vec::new();
            let mut previous_bindings = Vec::new();
            let payload_types = match (&source.value_type, &arms[arm_index].0) {
                (ValueType::Option(payload), LoweredPattern::Option { some: true, .. }) => {
                    vec![payload.as_ref().clone()]
                }
                (ValueType::Result(ok, _), LoweredPattern::Result { ok: true, .. }) => {
                    vec![ok.as_ref().clone()]
                }
                (ValueType::Result(_, error), LoweredPattern::Result { ok: false, .. }) => {
                    vec![error.as_ref().clone()]
                }
                (ValueType::Enum(id), LoweredPattern::Enum { variant, .. }) => {
                    let metadata = &self.global.bundle.enum_types[*id as usize];
                    let payload = &metadata
                        .variants
                        .iter()
                        .find(|candidate| candidate.name == *variant)
                        .expect("validated enum match variant")
                        .payload;
                    match payload {
                        EnumPayloadType::Unit => vec![],
                        EnumPayloadType::Tuple(values) => values.clone(),
                        EnumPayloadType::Record(fields) => fields
                            .iter()
                            .map(|field| field.value_type.clone())
                            .collect(),
                    }
                }
                (ValueType::Option(payload), LoweredPattern::Wildcard) => {
                    if variant_index == 1 {
                        vec![payload.as_ref().clone()]
                    } else {
                        Vec::new()
                    }
                }
                (ValueType::Result(ok, error), LoweredPattern::Wildcard) => {
                    vec![if variant_index == 0 {
                        ok.as_ref().clone()
                    } else {
                        error.as_ref().clone()
                    }]
                }
                (ValueType::Enum(id), LoweredPattern::Wildcard) => {
                    match &self.global.bundle.enum_types[*id as usize].variants[variant_index]
                        .payload
                    {
                        EnumPayloadType::Unit => Vec::new(),
                        EnumPayloadType::Tuple(values) => values.clone(),
                        EnumPayloadType::Record(fields) => fields
                            .iter()
                            .map(|field| field.value_type.clone())
                            .collect(),
                    }
                }
                _ => vec![],
            };
            for (index, value_type) in payload_types.into_iter().enumerate() {
                let register = self.allocate(value_type.clone())?;
                payload_registers.push(register);
                if let Some(Some(name)) = binding_names.get(index) {
                    if binding_names[..index]
                        .iter()
                        .flatten()
                        .any(|previous| previous == name)
                    {
                        return Err(Diagnostic::new(
                            "E3005",
                            format!("duplicate pattern binding '{name}'"),
                            arms[arm_index].2,
                        ));
                    }
                    if self.local_name_conflicts(name) {
                        return Err(Diagnostic::new(
                            "E3005",
                            format!("pattern binding '{name}' conflicts with a local function"),
                            arms[arm_index].2,
                        ));
                    }
                    let symbol = self.global.allocate_symbol();
                    let scope = self.active_scopes.last().copied().unwrap_or(0);
                    let previous = self.bindings.insert(
                        name.clone(),
                        LocalBinding {
                            register,
                            symbol,
                            value_type,
                            scope,
                            value_scope: scope,
                            mutable: false,
                            moved: false,
                        },
                    );
                    previous_bindings.push((name.clone(), previous));
                }
            }
            let region_capture = self.begin_nested_mir_region();
            let operation_start = self.mir.len();
            let value = match expected {
                Some((expected, label)) => {
                    self.compile_contextually_expected(&arms[arm_index].1, expected, label)?
                }
                None => self.compile_expression(&arms[arm_index].1)?,
            };
            let region = self.finish_nested_mir_region(region_capture);
            arm_operations.push(self.mir.split_off(operation_start));
            arm_regions.push(region);
            arm_value_scopes.push((
                value.value_type != ValueType::Never,
                self.sub_agent_value_scope(&value),
            ));
            arm_terminals.push((value.value_type == ValueType::Never).then(
                || match self.code.last() {
                    Some(Instruction::Stop { reason }) => MirTerminator::Stop {
                        reason: u32::from(*reason),
                    },
                    Some(Instruction::Fail { reason }) => MirTerminator::Fail {
                        reason: u32::from(*reason),
                    },
                    _ => MirTerminator::Unreachable,
                },
            ));
            if value.value_type != ValueType::Never {
                let affine_bindings = self
                    .bindings
                    .iter()
                    .filter(|(_, binding)| {
                        is_affine(&binding.value_type)
                            || contains_stored_sub_agent(&binding.value_type)
                    })
                    .map(|(name, binding)| (name.clone(), BindingState::from(binding)))
                    .collect::<BTreeMap<_, _>>();
                let affine_ownership = branch_ownership
                    .keys()
                    .filter_map(|register| {
                        self.ownership_states
                            .get(register)
                            .map(|state| (*register, *state))
                    })
                    .collect::<BTreeMap<_, _>>();
                if joined_bindings
                    .as_ref()
                    .is_some_and(|joined| joined != &affine_bindings)
                    || joined_ownership
                        .as_ref()
                        .is_some_and(|joined| joined != &affine_ownership)
                {
                    return Err(Diagnostic::new(
                        "E3011",
                        "match paths must leave the same affine ownership state",
                        span,
                    ));
                }
                joined_bindings.get_or_insert(affine_bindings);
                joined_ownership.get_or_insert(affine_ownership);
            }
            for (name, previous) in previous_bindings {
                if let Some(previous) = previous {
                    self.bindings.insert(name, previous);
                } else {
                    self.bindings.remove(&name);
                }
            }
            target_bindings.push(payload_registers);
            if value.value_type == ValueType::Never {
                never_register.get_or_insert(value.register);
            } else if let Some((result_register, result_type)) = &result {
                if *result_type != value.value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "every match arm must have one exact result type",
                        arms[arm_index].2,
                    ));
                }
                self.code.push(Instruction::Move {
                    destination: *result_register,
                    source: value.register,
                });
            } else {
                let result_register = self.allocate(value.value_type.clone())?;
                self.code.push(Instruction::Move {
                    destination: result_register,
                    source: value.register,
                });
                result = Some((result_register, value.value_type.clone()));
            }
            hir_arms.push(value.hir);
            arm_effects.push(value.effects);
            if value.value_type != ValueType::Never {
                jumps.push(self.code.len());
                self.code.push(Instruction::Jump { target: 0 });
            }
        }
        let join = u32::try_from(self.code.len()).expect("instruction index fits");
        for jump in jumps {
            self.code[jump] = Instruction::Jump { target: join };
        }
        self.code[branch_index] = if source.value_type == ValueType::Bool {
            Instruction::BranchBool {
                condition: source.register,
                false_target: targets[0],
                true_target: targets[1],
            }
        } else {
            Instruction::SwitchEnum {
                source: source.register,
                arms: targets
                    .iter()
                    .zip(&target_bindings)
                    .enumerate()
                    .map(
                        |(variant, (target, bindings))| allen_bytecode::EnumSwitchArm {
                            variant: u32::try_from(variant).expect("variant index fits"),
                            target: *target,
                            bindings: bindings.clone(),
                        },
                    )
                    .collect(),
            }
        };
        let join_block = self.next_mir_block();
        self.mir_blocks[base as usize - 1] = MirBlock {
            operations: Vec::new(),
            terminator: if source.value_type == ValueType::Bool {
                MirTerminator::SwitchBool {
                    false_target: base + 1,
                    true_target: base + 2,
                }
            } else {
                MirTerminator::SwitchEnum {
                    targets: (0..targets.len())
                        .map(|index| base + 1 + u32::try_from(index).expect("arm ID fits"))
                        .collect(),
                }
            },
        };
        let has_continuation = arm_terminals.iter().any(Option::is_none);
        for (arm, ((operations, terminal), region)) in arm_operations
            .into_iter()
            .zip(arm_terminals)
            .zip(arm_regions)
            .enumerate()
        {
            let arm_block = base + 1 + u32::try_from(arm).expect("arm ID fits");
            self.mir_blocks[arm_block as usize - 1] = MirBlock {
                operations,
                terminator: region.entry.map_or_else(
                    || {
                        terminal
                            .clone()
                            .unwrap_or(MirTerminator::Goto { target: join_block })
                    },
                    |target| MirTerminator::Goto { target },
                ),
            };
            if let Some(tail) = region.tail {
                self.set_mir_handoff(
                    tail,
                    terminal.unwrap_or(MirTerminator::Goto { target: join_block }),
                );
            }
        }
        if has_continuation {
            self.mir_blocks.push(MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            });
            self.register_mir_region(base, join_block);
        } else {
            if let Some(previous) = self.mir_tail {
                self.set_mir_handoff(previous, MirTerminator::Goto { target: base });
            }
            self.mir_entries.push(base);
            self.mir_tail = None;
        }
        let (register, value_type) = result
            .or_else(|| never_register.map(|register| (register, ValueType::Never)))
            .ok_or_else(|| Diagnostic::new("E3007", "match must contain at least one arm", span))?;
        if is_affine(&value_type) {
            return Err(Diagnostic::new(
                "E3011",
                "match cannot produce a future or task value",
                span,
            ));
        }
        if contains_stored_sub_agent(&value_type) {
            let value_scope = arm_value_scopes
                .into_iter()
                .filter_map(|(continues, scope)| continues.then_some(scope))
                .reduce(|left, right| self.deeper_scope(left, right))
                .unwrap_or_else(|| self.current_scope());
            self.sub_agent_value_scopes.insert(register, value_scope);
        }
        self.bindings = branch_bindings;
        if let Some(joined) = joined_bindings {
            for (name, state) in joined {
                if let Some(binding) = self.bindings.get_mut(&name) {
                    binding.moved = state.moved;
                    binding.value_scope = state.value_scope;
                }
            }
        }
        if let Some(joined) = joined_ownership {
            for (register, state) in joined {
                self.ownership_states.insert(register, state);
            }
        }
        let effects = self.union_effects(
            arm_effects
                .into_iter()
                .chain(std::iter::once(source.effects)),
        );
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Match {
                    source: Box::new(source.hir),
                    arms: hir_arms,
                },
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_prompt(
        &mut self,
        system: &LoweredExpr,
        context: Option<&LoweredExpr>,
        data: Option<&LoweredExpr>,
        output: &LoweredType,
        max_attempts: u32,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let system = self.compile_expression(system)?;
        if system.value_type != ValueType::String {
            return Err(Diagnostic::new(
                "E3010",
                "prompt system must be String",
                span,
            ));
        }
        let SemanticType::Value(output_type) = semantic_type(
            output,
            &BTreeSet::new(),
            &self.info.module,
            &self.global.bundle.modules,
            &self.global.bundle.types,
        )?
        else {
            return Err(Diagnostic::new(
                "E3007",
                "prompt output type must be concrete",
                output.span(),
            ));
        };
        if !is_strict_schema_type(&output_type) {
            return Err(Diagnostic::new(
                "E3011",
                "prompt output is not supported by the strict schema profile",
                output.span(),
            ));
        }
        let context = self.compile_prompt_segment(context, "context")?;
        let data = self.compile_prompt_segment(data, "data")?;
        let output_option = ValueType::Option(Box::new(output_type.clone()));
        let output_register = self.allocate(output_option.clone())?;
        self.code.push(Instruction::EnumNew {
            destination: output_register,
            variant: 0,
            payload: Vec::new(),
        });
        self.mir.push(MirOperation::Enum {
            destination: u32::from(output_register),
        });
        let attempts_register = self.allocate(ValueType::Int)?;
        let attempts_constant = self
            .global
            .constant(Constant::Int(i64::from(max_attempts)))?;
        self.code.push(Instruction::Const {
            destination: attempts_register,
            constant: attempts_constant,
        });
        self.mir.push(MirOperation::Constant {
            destination: u32::from(attempts_register),
        });
        let value_type = prompt_type(output_type);
        let register = self.allocate(value_type.clone())?;
        let registers = [
            system.register,
            context.register,
            data.register,
            output_register,
            attempts_register,
        ];
        self.code.push(Instruction::RecordNew {
            destination: register,
            fields: registers
                .iter()
                .enumerate()
                .map(|(index, register)| {
                    (u32::try_from(index).expect("prompt field index"), *register)
                })
                .collect(),
        });
        self.mir.push(MirOperation::Record {
            destination: u32::from(register),
        });
        let effects = self.union_effects([system.effects, context.effects, data.effects]);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Prompt(vec![system.hir, context.hir, data.hir]),
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_prompt_segment(
        &mut self,
        expression: Option<&LoweredExpr>,
        label: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let option_type = ValueType::Option(Box::new(ValueType::Unknown));
        let Some(expression) = expression else {
            let register = self.allocate(option_type.clone())?;
            self.code.push(Instruction::EnumNew {
                destination: register,
                variant: 0,
                payload: Vec::new(),
            });
            self.mir.push(MirOperation::Enum {
                destination: u32::from(register),
            });
            let effects = self.empty_effects();
            return Ok(CompiledExpr {
                register,
                value_type: option_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::Enum,
                    None,
                    &option_type,
                    effects,
                    Span { start: 0, end: 0 },
                ),
            });
        };
        let value = self.compile_prompt_data_value(expression, label)?;
        if !is_strict_schema_type(&value.value_type) {
            return Err(Diagnostic::new(
                "E3011",
                format!("prompt {label} is not supported by the strict schema profile"),
                expression.span,
            ));
        }
        let unknown = self.allocate(ValueType::Unknown)?;
        self.code.push(Instruction::ToUnknown {
            destination: unknown,
            source: value.register,
        });
        let register = self.allocate(option_type.clone())?;
        self.code.push(Instruction::EnumNew {
            destination: register,
            variant: 1,
            payload: vec![unknown],
        });
        self.mir.push(MirOperation::Enum {
            destination: u32::from(register),
        });
        Ok(CompiledExpr {
            register,
            value_type: option_type.clone(),
            effects: value.effects,
            hir: self.hir(
                HirExprKind::Enum,
                None,
                &option_type,
                value.effects,
                expression.span,
            ),
        })
    }

    pub(super) fn compile_prompt_data_value(
        &mut self,
        expression: &LoweredExpr,
        label: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let LoweredExprKind::Record { name, fields } = &expression.kind else {
            return self.compile_expression(expression);
        };
        if name != "$anonymous" {
            return self.compile_expression(expression);
        }
        let mut compiled = Vec::new();
        let mut seen = BTreeSet::new();
        for (name, value, field_span) in fields {
            if !seen.insert(name.clone()) {
                return Err(Diagnostic::new(
                    "E3010",
                    format!("duplicate prompt {label} field '{name}'"),
                    *field_span,
                ));
            }
            compiled.push((name.clone(), self.compile_expression(value)?));
        }
        compiled.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let value_type = ValueType::Record(
            compiled
                .iter()
                .map(|(name, value)| RecordField {
                    name: name.clone(),
                    value_type: value.value_type.clone(),
                })
                .collect(),
        );
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::RecordNew {
            destination: register,
            fields: compiled
                .iter()
                .enumerate()
                .map(|(index, (_, value))| {
                    (
                        u32::try_from(index).expect("prompt data field index"),
                        value.register,
                    )
                })
                .collect(),
        });
        self.mir.push(MirOperation::Record {
            destination: u32::from(register),
        });
        let effects = self.union_effects(compiled.iter().map(|(_, value)| value.effects));
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Record(compiled.into_iter().map(|(_, value)| value.hir).collect()),
                None,
                &value_type,
                effects,
                expression.span,
            ),
        })
    }

    fn needs_expected_context(expression: &LoweredExpr) -> bool {
        match &expression.kind {
            LoweredExprKind::ShortClosure { .. }
            | LoweredExprKind::List(_)
            | LoweredExprKind::ListWithSpread(_)
            | LoweredExprKind::Map(_)
            | LoweredExprKind::MapWithSpread(_)
            | LoweredExprKind::Tuple(_)
            | LoweredExprKind::Match { .. }
            | LoweredExprKind::If { .. } => true,
            LoweredExprKind::Record { name, .. } => name == "$anonymous",
            LoweredExprKind::Variable(name) => name == "None",
            LoweredExprKind::Call { callee, .. } => matches!(
                &callee.kind,
                LoweredExprKind::Variable(name)
                    if matches!(name.as_str(), "Some" | "Ok" | "Err")
            ),
            _ => false,
        }
    }

    fn compile_contextually_expected(
        &mut self,
        expression: &LoweredExpr,
        expected: &ValueType,
        label: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        if Self::needs_expected_context(expression) {
            self.compile_expected(expression, expected, label)
        } else {
            self.compile_expression(expression)
        }
    }

    fn require_expected_result(
        value: CompiledExpr,
        expected: &ValueType,
        label: &str,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if value.value_type != ValueType::Never && &value.value_type != expected {
            return Err(Diagnostic::new(
                expected_type_diagnostic_code(label),
                format!("expected {expected}, found {}", value.value_type),
                span,
            ));
        }
        Ok(value)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_expected(
        &mut self,
        expression: &LoweredExpr,
        expected: &ValueType,
        label: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        if let LoweredExprKind::ShortClosure { parameters, body } = &expression.kind {
            return self.compile_short_closure(parameters, body, expected, expression.span);
        }
        if let LoweredExprKind::Match { source, arms } = &expression.kind {
            let value =
                self.compile_match(source, arms, expression.span, Some((expected, label)))?;
            return Self::require_expected_result(value, expected, label, expression.span);
        }
        if let LoweredExprKind::If {
            condition,
            then_body,
            else_branch,
        } = &expression.kind
        {
            let value = self.compile_if(
                condition,
                then_body,
                else_branch.as_ref(),
                expression.span,
                Some((expected, label)),
                None,
            )?;
            return Self::require_expected_result(value, expected, label, expression.span);
        }
        let constructor = match &expression.kind {
            LoweredExprKind::Variable(name) if name == "None" => Some((name.as_str(), Vec::new())),
            LoweredExprKind::Call {
                callee, arguments, ..
            } => match &callee.kind {
                LoweredExprKind::Variable(name)
                    if matches!(name.as_str(), "Some" | "Ok" | "Err") =>
                {
                    Some((
                        name.as_str(),
                        arguments.iter().map(|argument| &argument.value).collect(),
                    ))
                }
                _ => None,
            },
            _ => None,
        };
        if let Some((name, arguments)) = constructor {
            let (variant, payload_type) = match (name, expected) {
                ("None", ValueType::Option(_)) => (0, None),
                ("Some", ValueType::Option(value)) | ("Err", ValueType::Result(_, value)) => {
                    (1, Some(value.as_ref()))
                }
                ("Ok", ValueType::Result(value, _)) => (0, Some(value.as_ref())),
                _ => {
                    return Err(Diagnostic::new(
                        "E2019",
                        format!("{name} is not valid for expected type {expected}"),
                        expression.span,
                    ));
                }
            };
            let values = match payload_type {
                None if arguments.is_empty() => Vec::new(),
                Some(payload_type) if arguments.len() == 1 => vec![self.compile_expected(
                    arguments[0],
                    payload_type,
                    "constructor payload",
                )?],
                _ => {
                    return Err(Diagnostic::new(
                        "E2019",
                        format!("{name} has the wrong payload count"),
                        expression.span,
                    ));
                }
            };
            let register = self.allocate(expected.clone())?;
            self.code.push(Instruction::EnumNew {
                destination: register,
                variant,
                payload: values.iter().map(|value| value.register).collect(),
            });
            self.mir.push(MirOperation::Enum {
                destination: u32::from(register),
            });
            let effects = self.union_effects(values.iter().map(|value| value.effects));
            return Ok(CompiledExpr {
                register,
                value_type: expected.clone(),
                effects,
                hir: self.hir(HirExprKind::Enum, None, expected, effects, expression.span),
            });
        }
        if let LoweredExprKind::List(elements) = &expression.kind {
            return self.compile_list(expression, elements, Some(expected));
        }
        if let LoweredExprKind::ListWithSpread(items) = &expression.kind {
            return self.compile_list_with_spread(expression, items, Some(expected));
        }
        if let LoweredExprKind::Map(entries) = &expression.kind {
            return self.compile_map(expression, entries, Some(expected));
        }
        if let LoweredExprKind::MapWithSpread(items) = &expression.kind {
            return self.compile_map_with_spread(expression, items, Some(expected));
        }
        if let LoweredExprKind::RecordUpdate {
            name,
            base,
            spread_span,
            fields,
        } = &expression.kind
        {
            let value = self.compile_record_update(expression, name, base, *spread_span, fields)?;
            if value.value_type != ValueType::Never && &value.value_type != expected {
                return Err(Diagnostic::new(
                    expected_type_diagnostic_code(label),
                    format!("expected {expected}, found {}", value.value_type),
                    expression.span,
                ));
            }
            return Ok(value);
        }
        if let (LoweredExprKind::Tuple(elements), ValueType::Tuple(element_types)) =
            (&expression.kind, expected)
        {
            if elements.len() != element_types.len() {
                return Err(Diagnostic::new(
                    "E3010",
                    "tuple value has the wrong element count",
                    expression.span,
                ));
            }
            let values = elements
                .iter()
                .zip(element_types)
                .map(|(element, element_type)| {
                    self.compile_expected(element, element_type, "tuple element")
                })
                .collect::<Result<Vec<_>, _>>()?;
            if values.iter().any(|value| is_affine(&value.value_type)) {
                return Err(Diagnostic::new(
                    "E3011",
                    "future or task values cannot be stored in a tuple",
                    expression.span,
                ));
            }
            if values
                .iter()
                .any(|value| contains_workspace(&value.value_type))
            {
                return Err(Diagnostic::new(
                    "E3011",
                    "Workspace cannot be stored in a tuple",
                    expression.span,
                ));
            }
            let register = self.allocate(expected.clone())?;
            self.code.push(Instruction::TupleNew {
                destination: register,
                elements: values.iter().map(|value| value.register).collect(),
            });
            self.mir.push(MirOperation::Tuple {
                destination: u32::from(register),
            });
            let effects = self.union_effects(values.iter().map(|value| value.effects));
            return Ok(CompiledExpr {
                register,
                value_type: expected.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::Tuple(values.into_iter().map(|value| value.hir).collect()),
                    None,
                    expected,
                    effects,
                    expression.span,
                ),
            });
        }
        let LoweredExprKind::Record { name, fields } = &expression.kind else {
            let value = self.compile_expression(expression)?;
            if value.value_type != ValueType::Never && &value.value_type != expected {
                return Err(Diagnostic::new(
                    expected_type_diagnostic_code(label),
                    format!("expected {expected}, found {}", value.value_type),
                    expression.span,
                ));
            }
            return Ok(value);
        };
        if name != "$anonymous" {
            let value = self.compile_expression(expression)?;
            if value.value_type != ValueType::Never && &value.value_type != expected {
                return Err(Diagnostic::new(
                    expected_type_diagnostic_code(label),
                    format!("expected {expected}, found {}", value.value_type),
                    expression.span,
                ));
            }
            return Ok(value);
        }
        let ValueType::Record(layout) = expected else {
            return Err(Diagnostic::new(
                "E3010",
                format!("anonymous record is not valid for this {label}"),
                expression.span,
            ));
        };
        if fields.len() != layout.len() {
            return Err(Diagnostic::new(
                "E3010",
                format!("{label} record requires every field exactly once"),
                expression.span,
            ));
        }
        let mut seen = BTreeSet::new();
        let mut compiled = Vec::new();
        for (field, value, field_span) in fields {
            if !seen.insert(field.clone()) {
                return Err(Diagnostic::new(
                    "E3010",
                    format!("duplicate {label} field '{field}'"),
                    *field_span,
                ));
            }
            let index = layout
                .iter()
                .position(|candidate| candidate.name == *field)
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E3010",
                        format!("{label} has no field '{field}'"),
                        *field_span,
                    )
                })?;
            let value = self.compile_expected(value, &layout[index].value_type, label)?;
            compiled.push((index, value));
        }
        compiled.sort_by_key(|(index, _)| *index);
        let register = self.allocate(expected.clone())?;
        self.code.push(Instruction::RecordNew {
            destination: register,
            fields: compiled
                .iter()
                .map(|(index, value)| {
                    (
                        u32::try_from(*index).expect("field index fits"),
                        value.register,
                    )
                })
                .collect(),
        });
        self.mir.push(MirOperation::Record {
            destination: u32::from(register),
        });
        let effects = self.union_effects(compiled.iter().map(|(_, value)| value.effects));
        Ok(CompiledExpr {
            register,
            value_type: expected.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Record(compiled.into_iter().map(|(_, value)| value.hir).collect()),
                None,
                expected,
                effects,
                expression.span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn compile_template_render(
        &mut self,
        name: &str,
        type_arguments: &[LoweredType],
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if !type_arguments.is_empty() {
            return Err(Diagnostic::new(
                "E3012",
                "template render does not take type arguments",
                span,
            ));
        }
        let binding = template_binding(self.global.bundle, &self.info.module, name)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(
                    "E3012",
                    format!("template '{name}' is not declared in this package"),
                    span,
                )
            })?;
        let [argument] = arguments else {
            return Err(Diagnostic::new(
                "E3012",
                "template render requires exactly one record argument",
                span,
            ));
        };
        let LoweredExprKind::Record {
            name: record_name,
            fields,
        } = &argument.kind
        else {
            return Err(Diagnostic::new(
                "E3012",
                "template render argument must be a record literal",
                argument.span,
            ));
        };
        if record_name != "$anonymous" {
            return Err(Diagnostic::new(
                "E3012",
                "template render argument must be an anonymous record literal",
                argument.span,
            ));
        }
        let mut seen = BTreeSet::new();
        for (field, _, field_span) in fields {
            if !seen.insert(field.as_str()) {
                return Err(Diagnostic::new(
                    "E3012",
                    format!("duplicate template field '{field}'"),
                    *field_span,
                ));
            }
            if !binding.holes.iter().any(|(hole, _)| hole == field) {
                return Err(Diagnostic::new(
                    "E3012",
                    format!("template '{name}' has no hole '{field}'"),
                    *field_span,
                ));
            }
        }
        if let Some((missing, _)) = binding
            .holes
            .iter()
            .find(|(hole, _)| !seen.contains(hole.as_str()))
        {
            return Err(Diagnostic::new(
                "E3012",
                format!("template '{name}' requires hole '{missing}'"),
                argument.span,
            ));
        }

        let mut compiled = Vec::with_capacity(fields.len());
        for (field, value, field_span) in fields {
            let index = binding
                .holes
                .iter()
                .position(|(hole, _)| hole == field)
                .expect("template field shape was checked");
            let value = self.compile_expression(value)?;
            if value.value_type == ValueType::Never {
                return Ok(value);
            }
            let expected = &binding.holes[index].1;
            if template_scalar_type(&value.value_type) != expected {
                return Err(Diagnostic::new(
                    "E3012",
                    format!(
                        "template hole '{field}' expects {expected}, found {}",
                        value.value_type
                    ),
                    *field_span,
                ));
            }
            compiled.push((index, value));
        }
        compiled.sort_by_key(|(index, _)| *index);
        let register = self.allocate(ValueType::String)?;
        let operand_registers = compiled
            .iter()
            .map(|(_, value)| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::TemplateRender {
            destination: register,
            template: binding.template,
            arguments: operand_registers.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::TemplateRender {
            destination: u32::from(register),
            template: binding.template,
            arguments: operand_registers
                .iter()
                .map(|register| u32::from(*register))
                .collect(),
        });
        let effects = self.union_effects(compiled.iter().map(|(_, value)| value.effects));
        Ok(CompiledExpr {
            register,
            value_type: ValueType::String,
            effects,
            hir: self.hir(
                HirExprKind::TemplateRender {
                    template: binding.template,
                    arguments: compiled.into_iter().map(|(_, value)| value.hir).collect(),
                },
                None,
                &ValueType::String,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn direct_builtin_partial_contract(
        &self,
        callee: &LoweredExpr,
        known: &[Option<ValueType>],
        span: Span,
    ) -> Result<Option<DirectPartialContract>, Diagnostic> {
        let pure = |parameters, result| DirectPartialContract {
            parameters,
            result,
            effects: Vec::new(),
        };
        if let Some(operation) = string_builtin_callee(callee) {
            let (_, parameters, result) = string_operation_signature(operation);
            return Ok(Some(pure(parameters, result)));
        }
        if let Some(operation) = standard_operation_callee(callee) {
            let (_, parameters, result) = standard_operation_signature(operation);
            return Ok(Some(pure(parameters, result)));
        }
        if let Some(operation) = capability_builtin_callee(callee) {
            let (_, parameters, result) = capability_operation_signature(operation);
            return Ok(Some(DirectPartialContract {
                parameters,
                result,
                effects: vec!["capability.inspect".to_owned()],
            }));
        }
        if let Some(StandardBuiltin::Operation(operation)) = standard_builtin_callee(callee) {
            let Some((parameters, result, effect, _)) =
                effect_operation_signature(operation, self.global.bundle.transcript_part)
            else {
                return Err(Diagnostic::new(
                    "E3010",
                    "placeholder partial application requires one exact builtin signature",
                    span,
                ));
            };
            return Ok(Some(DirectPartialContract {
                parameters,
                result: ValueType::Future(Box::new(result)),
                effects: vec![effect.to_owned()],
            }));
        }
        let Some(builtin) = collection_builtin_callee(callee) else {
            return Ok(None);
        };
        let known_at = |index: usize| known.get(index).and_then(Option::as_ref);
        let list_item = |list: Option<&ValueType>| match list {
            Some(ValueType::List(item)) => Some(item.as_ref().clone()),
            _ => None,
        };
        let map_parts = |map: Option<&ValueType>| match map {
            Some(ValueType::Map(key, value)) => {
                Some((key.as_ref().clone(), value.as_ref().clone()))
            }
            _ => None,
        };
        let contract = match builtin {
            CollectionBuiltin::Length => {
                let Some(value_type) = known_at(0).cloned() else {
                    return Err(Diagnostic::new(
                        "E3010",
                        "cannot infer the collection type for this call placeholder",
                        span,
                    ));
                };
                if !matches!(
                    value_type,
                    ValueType::String
                        | ValueType::Bytes
                        | ValueType::List(_)
                        | ValueType::Map(_, _)
                ) {
                    return Err(Diagnostic::new(
                        "E3011",
                        "length requires String, Bytes, List<T>, or Map<K, V>",
                        span,
                    ));
                }
                pure(vec![value_type], ValueType::Int)
            }
            CollectionBuiltin::ListAppend => {
                let item = list_item(known_at(0))
                    .or_else(|| known_at(1).cloned())
                    .ok_or_else(|| {
                        Diagnostic::new(
                            "E3010",
                            "cannot infer the list element type for this call placeholder",
                            span,
                        )
                    })?;
                let list = ValueType::List(Box::new(item.clone()));
                pure(vec![list.clone(), item], list)
            }
            CollectionBuiltin::ListSet => {
                let item = list_item(known_at(0))
                    .or_else(|| known_at(2).cloned())
                    .ok_or_else(|| {
                        Diagnostic::new(
                            "E3010",
                            "cannot infer the list element type for this call placeholder",
                            span,
                        )
                    })?;
                let list = ValueType::List(Box::new(item.clone()));
                pure(vec![list.clone(), ValueType::Int, item], list)
            }
            CollectionBuiltin::Safe(SafeCollectionOperation::BytesGet) => pure(
                vec![ValueType::Bytes, ValueType::Int],
                ValueType::Option(Box::new(ValueType::Int)),
            ),
            CollectionBuiltin::Safe(SafeCollectionOperation::ListGet) => {
                let item = list_item(known_at(0)).ok_or_else(|| {
                    Diagnostic::new(
                        "E3010",
                        "cannot infer the list element type for this call placeholder",
                        span,
                    )
                })?;
                pure(
                    vec![ValueType::List(Box::new(item.clone())), ValueType::Int],
                    ValueType::Option(Box::new(item)),
                )
            }
            CollectionBuiltin::Safe(SafeCollectionOperation::ListTrySet) => {
                let item = list_item(known_at(0))
                    .or_else(|| known_at(2).cloned())
                    .ok_or_else(|| {
                        Diagnostic::new(
                            "E3010",
                            "cannot infer the list element type for this call placeholder",
                            span,
                        )
                    })?;
                let list = ValueType::List(Box::new(item.clone()));
                pure(
                    vec![list.clone(), ValueType::Int, item],
                    ValueType::Option(Box::new(list)),
                )
            }
            CollectionBuiltin::Safe(SafeCollectionOperation::MapGet) => {
                let (key, value) = map_parts(known_at(0)).ok_or_else(|| {
                    Diagnostic::new(
                        "E3010",
                        "cannot infer the Map types for this call placeholder",
                        span,
                    )
                })?;
                pure(
                    vec![
                        ValueType::Map(Box::new(key.clone()), Box::new(value.clone())),
                        key,
                    ],
                    ValueType::Option(Box::new(value)),
                )
            }
            CollectionBuiltin::Safe(SafeCollectionOperation::MapInsert) => {
                let (key, value) = map_parts(known_at(0))
                    .or_else(|| Some((known_at(1)?.clone(), known_at(2)?.clone())))
                    .ok_or_else(|| {
                        Diagnostic::new(
                            "E3010",
                            "cannot infer the Map types for this call placeholder",
                            span,
                        )
                    })?;
                let map = ValueType::Map(Box::new(key.clone()), Box::new(value.clone()));
                pure(
                    vec![map.clone(), key, value.clone()],
                    ValueType::Record(vec![
                        RecordField {
                            name: "previous".to_owned(),
                            value_type: ValueType::Option(Box::new(value)),
                        },
                        RecordField {
                            name: "values".to_owned(),
                            value_type: map,
                        },
                    ]),
                )
            }
            CollectionBuiltin::Safe(SafeCollectionOperation::MapRemove) => {
                let (key, value) = map_parts(known_at(0)).ok_or_else(|| {
                    Diagnostic::new(
                        "E3010",
                        "cannot infer the Map types for this call placeholder",
                        span,
                    )
                })?;
                let map = ValueType::Map(Box::new(key.clone()), Box::new(value.clone()));
                pure(
                    vec![map.clone(), key],
                    ValueType::Record(vec![
                        RecordField {
                            name: "removed".to_owned(),
                            value_type: ValueType::Option(Box::new(value)),
                        },
                        RecordField {
                            name: "values".to_owned(),
                            value_type: map,
                        },
                    ]),
                )
            }
            CollectionBuiltin::Safe(SafeCollectionOperation::MapKeys) => {
                let (key, value) = map_parts(known_at(0)).ok_or_else(|| {
                    Diagnostic::new(
                        "E3010",
                        "cannot infer the Map types for this call placeholder",
                        span,
                    )
                })?;
                pure(
                    vec![ValueType::Map(Box::new(key.clone()), Box::new(value))],
                    ValueType::List(Box::new(key)),
                )
            }
            CollectionBuiltin::CheckedInt(operation) => pure(
                vec![
                    ValueType::Int;
                    if operation == CheckedIntOperation::Negate {
                        1
                    } else {
                        2
                    }
                ],
                ValueType::Option(Box::new(ValueType::Int)),
            ),
            CollectionBuiltin::Operation(
                operation @ (CollectionOperation::ListMin
                | CollectionOperation::ListMax
                | CollectionOperation::ListSumInt
                | CollectionOperation::ListSumFloat),
            ) => {
                let item = list_item(known_at(0)).ok_or_else(|| {
                    Diagnostic::new(
                        "E3010",
                        "cannot infer the list element type for this call placeholder",
                        span,
                    )
                })?;
                let result = match (operation, &item) {
                    (
                        CollectionOperation::ListMin | CollectionOperation::ListMax,
                        ValueType::Int | ValueType::Float,
                    ) => ValueType::Option(Box::new(item.clone())),
                    (CollectionOperation::ListSumInt, ValueType::Int) => {
                        ValueType::Option(Box::new(ValueType::Int))
                    }
                    (
                        CollectionOperation::ListSumInt | CollectionOperation::ListSumFloat,
                        ValueType::Float,
                    ) => ValueType::Float,
                    _ => {
                        return Err(Diagnostic::new(
                            "E3011",
                            "list aggregate requires List<Int> or List<Float>",
                            span,
                        ));
                    }
                };
                pure(vec![ValueType::List(Box::new(item))], result)
            }
            CollectionBuiltin::Operation(CollectionOperation::Zip)
            | CollectionBuiltin::ListFold
            | CollectionBuiltin::ListCombinator(_) => {
                return Err(Diagnostic::new(
                    "E3010",
                    "placeholder partial application requires one non-variadic exact builtin signature",
                    span,
                ));
            }
            CollectionBuiltin::Sequence(_) => {
                return Err(Diagnostic::new(
                    "E3010",
                    "cannot infer a Sequence placeholder contract from this call",
                    span,
                ));
            }
        };
        Ok(Some(contract))
    }

    #[allow(clippy::too_many_lines)]
    fn compile_builtin_partial_call(
        &mut self,
        callee: &LoweredExpr,
        arguments: &[LoweredCallArgument],
        span: Span,
    ) -> Result<Option<CompiledExpr>, Diagnostic> {
        let Some(labels) = builtin_argument_labels(callee) else {
            return Ok(None);
        };
        if arguments.iter().any(|argument| argument.trailing) {
            return Err(Diagnostic::new(
                "E3010",
                "placeholder partial application does not accept a trailing callback",
                span,
            ));
        }
        if labels.is_empty() || arguments.len() != labels.len() {
            return Err(Diagnostic::new(
                "E3010",
                "placeholder partial application requires every builtin argument or a placeholder",
                span,
            ));
        }
        let has_labels = arguments.iter().any(|argument| argument.label.is_some());
        if has_labels != arguments.iter().all(|argument| argument.label.is_some()) {
            return Err(Diagnostic::new(
                "E3010",
                "placeholder partial application cannot mix labeled and positional arguments",
                span,
            ));
        }
        let mut seen = BTreeSet::new();
        let parameter_indexes = if has_labels {
            arguments
                .iter()
                .map(|argument| {
                    let (label, label_span) = argument.label.as_ref().expect("labeled partial");
                    let index = labels
                        .iter()
                        .position(|candidate| *candidate == label)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                "E3010",
                                format!("direct builtin has no parameter labeled '{label}'"),
                                *label_span,
                            )
                        })?;
                    if !seen.insert(index) {
                        return Err(Diagnostic::new(
                            "E3010",
                            format!("direct builtin received duplicate label '{label}'"),
                            *label_span,
                        ));
                    }
                    Ok(index)
                })
                .collect::<Result<Vec<_>, Diagnostic>>()?
        } else {
            (0..arguments.len()).collect()
        };
        let mut known = vec![None; labels.len()];
        let mut rewritten = arguments.to_vec();
        let mut creation_effects = Vec::new();
        for (source_index, (argument, parameter_index)) in
            arguments.iter().zip(&parameter_indexes).enumerate()
        {
            if argument.placeholder {
                continue;
            }
            let expected = self
                .direct_builtin_partial_contract(callee, &known, argument.span)
                .ok()
                .flatten()
                .and_then(|contract| contract.parameters.get(*parameter_index).cloned());
            let value = match expected {
                Some(expected) => {
                    self.compile_expected(&argument.value, &expected, "partial argument")?
                }
                None => self.compile_expression(&argument.value)?,
            };
            known[*parameter_index] = Some(value.value_type.clone());
            creation_effects.push(value.effects);
            let temporary = format!("__allen_partial_capture_{}", self.global.allocate_symbol());
            let scope = self.current_scope();
            self.bindings.insert(
                temporary.clone(),
                LocalBinding {
                    register: value.register,
                    symbol: self.global.allocate_symbol(),
                    value_type: value.value_type,
                    scope,
                    value_scope: scope,
                    mutable: false,
                    moved: false,
                },
            );
            rewritten[source_index].value = LoweredExpr {
                kind: LoweredExprKind::Variable(temporary),
                span: argument.value.span,
            };
        }
        let Some(contract) = self.direct_builtin_partial_contract(callee, &known, span)? else {
            return Ok(None);
        };
        for (source_index, parameter_index) in parameter_indexes.iter().enumerate() {
            if let Some(found) = &known[*parameter_index] {
                let expected = &contract.parameters[*parameter_index];
                if found != expected {
                    return Err(Diagnostic::new(
                        "E3011",
                        format!("partial argument expected {expected}, found {found}"),
                        arguments[source_index].span,
                    ));
                }
            }
        }
        let mut closure_parameters = Vec::new();
        for (source_index, parameter_index) in parameter_indexes.iter().enumerate() {
            if !arguments[source_index].placeholder {
                continue;
            }
            let exact = &contract.parameters[*parameter_index];
            let parameter_name = format!(
                "__allen_partial_parameter_{}",
                self.global.allocate_symbol()
            );
            closure_parameters.push((
                parameter_name.clone(),
                self.lowered_type_for_short_closure(exact, arguments[source_index].span)?,
                arguments[source_index].span,
            ));
            rewritten[source_index].placeholder = false;
            rewritten[source_index].value = LoweredExpr {
                kind: LoweredExprKind::Variable(parameter_name),
                span: arguments[source_index].span,
            };
        }
        let body = LoweredBody {
            statements: Vec::new(),
            tail: Some(LoweredExpr {
                kind: LoweredExprKind::Call {
                    callee: Box::new(callee.clone()),
                    type_arguments: Vec::new(),
                    arguments: rewritten,
                },
                span,
            }),
            span,
        };
        let lowered_return = self.lowered_type_for_short_closure(&contract.result, span)?;
        let mut compiled = self.compile_closure(
            &closure_parameters,
            &lowered_return,
            Some(&contract.effects),
            &body,
            span,
        )?;
        let creation_effects = self.union_effects(creation_effects);
        compiled.effects = self.union_effects([compiled.effects, creation_effects]);
        compiled.hir.effects = compiled.effects;
        Ok(Some(compiled))
    }

    #[allow(clippy::too_many_lines)]
    fn compile_partial_call(
        &mut self,
        callee: &LoweredExpr,
        type_arguments: &[LoweredType],
        arguments: &[LoweredCallArgument],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if !type_arguments.is_empty() {
            return Err(Diagnostic::new(
                "E3010",
                "placeholder partial application requires inferred type arguments",
                span,
            ));
        }
        if let Some(compiled) = self.compile_builtin_partial_call(callee, arguments, span)? {
            return Ok(compiled);
        }
        let LoweredExprKind::Variable(name) = &callee.kind else {
            return Err(Diagnostic::new(
                "E3010",
                "placeholder partial application requires one resolved function name",
                callee.span,
            ));
        };
        let (target, _) = self.resolve_callable(name)?.ok_or_else(|| {
            Diagnostic::new("E3005", format!("unknown function '{name}'"), callee.span)
        })?;
        if target.is_const || arguments.iter().any(|argument| argument.trailing) {
            return Err(Diagnostic::new(
                "E3010",
                "placeholder partial application requires an ordinary direct function call",
                span,
            ));
        }
        let has_labels = arguments.iter().any(|argument| argument.label.is_some());
        if has_labels != arguments.iter().all(|argument| argument.label.is_some()) {
            return Err(Diagnostic::new(
                "E3010",
                "placeholder partial application cannot mix labeled and positional arguments",
                span,
            ));
        }
        let mut seen = BTreeSet::new();
        let parameter_indexes = if has_labels {
            arguments
                .iter()
                .map(|argument| {
                    let (label, label_span) = argument.label.as_ref().expect("labeled partial");
                    let index = target
                        .lowered
                        .parameters
                        .iter()
                        .position(|(parameter, _, _)| parameter == label)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                "E3010",
                                format!("function '{name}' has no parameter labeled '{label}'"),
                                *label_span,
                            )
                        })?;
                    if !seen.insert(index) {
                        return Err(Diagnostic::new(
                            "E3010",
                            format!("function '{name}' received duplicate label '{label}'"),
                            *label_span,
                        ));
                    }
                    Ok(index)
                })
                .collect::<Result<Vec<_>, Diagnostic>>()?
        } else {
            if arguments.len() > target.parameters.len() {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("function '{name}' has the wrong argument count"),
                    span,
                ));
            }
            (0..arguments.len()).collect()
        };
        let mut substitutions = BTreeMap::new();
        let mut rewritten = arguments.to_vec();
        let mut creation_effects = Vec::new();
        for (source_index, (argument, parameter_index)) in
            arguments.iter().zip(&parameter_indexes).enumerate()
        {
            if argument.placeholder {
                continue;
            }
            let parameter = &target.parameters[*parameter_index];
            let value = if matches!(parameter, SemanticType::Generic(_)) {
                self.compile_expression(&argument.value)?
            } else {
                let expected = concrete_type(parameter, &substitutions, &self.global.effect_sets)?;
                self.compile_expected(&argument.value, &expected, "partial argument")?
            };
            if let SemanticType::Generic(generic) = parameter {
                if let Some(previous) =
                    substitutions.insert(generic.clone(), value.value_type.clone())
                {
                    if previous != value.value_type {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!("generic '{generic}' inferred as two different types"),
                            argument.span,
                        ));
                    }
                }
            }
            creation_effects.push(value.effects);
            let temporary = format!("__allen_partial_capture_{}", self.global.allocate_symbol());
            let scope = self.current_scope();
            self.bindings.insert(
                temporary.clone(),
                LocalBinding {
                    register: value.register,
                    symbol: self.global.allocate_symbol(),
                    value_type: value.value_type,
                    scope,
                    value_scope: scope,
                    mutable: false,
                    moved: false,
                },
            );
            rewritten[source_index].value = LoweredExpr {
                kind: LoweredExprKind::Variable(temporary),
                span: argument.value.span,
            };
        }
        let mut closure_parameters = Vec::new();
        for (source_index, parameter_index) in parameter_indexes.iter().enumerate() {
            if !arguments[source_index].placeholder {
                continue;
            }
            let exact = concrete_type(
                &target.parameters[*parameter_index],
                &substitutions,
                &self.global.effect_sets,
            )
            .map_err(|_| {
                Diagnostic::new(
                    "E3007",
                    "cannot infer an exact type for this call placeholder",
                    arguments[source_index].span,
                )
            })?;
            let parameter_name = format!(
                "__allen_partial_parameter_{}",
                self.global.allocate_symbol()
            );
            closure_parameters.push((
                parameter_name.clone(),
                self.lowered_type_for_short_closure(&exact, arguments[source_index].span)?,
                arguments[source_index].span,
            ));
            rewritten[source_index].placeholder = false;
            rewritten[source_index].value = LoweredExpr {
                kind: LoweredExprKind::Variable(parameter_name),
                span: arguments[source_index].span,
            };
        }
        let return_type = concrete_type(
            &target.return_type,
            &substitutions,
            &self.global.effect_sets,
        )?;
        let return_type = if target.lowered.is_async {
            ValueType::Future(Box::new(return_type))
        } else {
            return_type
        };
        let body_expression = LoweredExpr {
            kind: LoweredExprKind::Call {
                callee: Box::new(callee.clone()),
                type_arguments: Vec::new(),
                arguments: rewritten,
            },
            span,
        };
        let body = LoweredBody {
            statements: Vec::new(),
            tail: Some(body_expression),
            span,
        };
        let declared_effects = target.effects.clone();
        let lowered_return = self.lowered_type_for_short_closure(&return_type, span)?;
        let mut compiled = self.compile_closure(
            &closure_parameters,
            &lowered_return,
            Some(&declared_effects),
            &body,
            span,
        )?;
        let creation_effects = self.union_effects(creation_effects);
        compiled.effects = self.union_effects([compiled.effects, creation_effects]);
        compiled.hir.effects = compiled.effects;
        Ok(compiled)
    }

    #[allow(clippy::too_many_lines)]
    fn compile_extension_call(
        &mut self,
        callee: &LoweredExpr,
        type_arguments: &[LoweredType],
        arguments: &[LoweredCallArgument],
        span: Span,
    ) -> Option<Result<CompiledExpr, Diagnostic>> {
        if is_task_snapshot_callee(callee)
            || template_callee(callee).is_some()
            || tool_callee(callee).is_some()
            || standard_builtin_callee(callee).is_some()
            || collection_builtin_callee(callee).is_some()
            || string_builtin_callee(callee).is_some()
            || standard_operation_callee(callee).is_some()
            || capability_builtin_callee(callee).is_some()
        {
            return None;
        }
        let LoweredExprKind::FieldGet {
            record,
            field,
            field_span,
        } = &callee.kind
        else {
            return None;
        };
        if let LoweredExprKind::Variable(name) = &record.kind {
            let namespace = matches!(
                name.as_str(),
                "allen"
                    | "agent"
                    | "bytes"
                    | "capability"
                    | "float"
                    | "fs"
                    | "int"
                    | "list"
                    | "map"
                    | "model"
                    | "string"
                    | "sub_agent"
                    | "task"
                    | "time"
                    | "user"
            );
            let type_namespace = !self.bindings.contains_key(name)
                && resolve_named_type(
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                    &self.info.module,
                    name,
                    record.span,
                )
                .is_ok();
            if namespace || type_namespace {
                return None;
            }
        }
        Some((|| {
            let receiver = self.compile_expression(record)?;
            let receiver_effects = receiver.effects;
            let receiver_type = receiver.value_type.clone();
            let receiver_scope = self.sub_agent_value_scope(&receiver);
            let receiver_name = format!(
                "__allen_extension_receiver_{}",
                self.global.allocate_symbol()
            );
            self.bindings.insert(
                receiver_name.clone(),
                LocalBinding {
                    register: receiver.register,
                    symbol: self.global.allocate_symbol(),
                    value_type: receiver_type.clone(),
                    scope: self.current_scope(),
                    value_scope: receiver_scope,
                    mutable: false,
                    moved: false,
                },
            );
            let receiver_expression = LoweredExpr {
                kind: LoweredExprKind::Variable(receiver_name),
                span: record.span,
            };

            if matches!(&receiver_type, ValueType::Record(fields) if fields.iter().any(|candidate| candidate.name == *field))
            {
                let field_value = self.compile_expression(&LoweredExpr {
                    kind: LoweredExprKind::FieldGet {
                        record: Box::new(receiver_expression),
                        field: field.clone(),
                        field_span: *field_span,
                    },
                    span: callee.span,
                })?;
                let field_name =
                    format!("__allen_function_field_{}", self.global.allocate_symbol());
                self.bindings.insert(
                    field_name.clone(),
                    LocalBinding {
                        register: field_value.register,
                        symbol: self.global.allocate_symbol(),
                        value_type: field_value.value_type,
                        scope: self.current_scope(),
                        value_scope: self.current_scope(),
                        mutable: false,
                        moved: false,
                    },
                );
                let mut compiled = self.compile_call(
                    &LoweredExpr {
                        kind: LoweredExprKind::Variable(field_name),
                        span: callee.span,
                    },
                    type_arguments,
                    arguments,
                    span,
                )?;
                compiled.effects = self.union_effects([receiver_effects, compiled.effects]);
                compiled.hir.effects = compiled.effects;
                return Ok(compiled);
            }

            let namespace = match receiver_type {
                ValueType::List(_) => Some("list"),
                ValueType::Map(_, _) => Some("map"),
                ValueType::String => Some("string"),
                ValueType::Bytes => Some("bytes"),
                ValueType::Sequence(_) => Some("seq"),
                _ => None,
            };
            let namespace_callee = namespace.map(|namespace| LoweredExpr {
                kind: LoweredExprKind::FieldGet {
                    record: Box::new(LoweredExpr {
                        kind: LoweredExprKind::Variable(namespace.to_owned()),
                        span: record.span,
                    }),
                    field: field.clone(),
                    field_span: *field_span,
                },
                span: callee.span,
            });
            let builtin = namespace_callee.as_ref().is_some_and(|candidate| {
                collection_builtin_callee(candidate).is_some()
                    || string_builtin_callee(candidate).is_some()
            });
            let candidates = resolve_extension_functions(
                self.global.bundle,
                &self.info.module,
                field,
                &receiver_type,
            )?;
            if builtin && !candidates.is_empty() || candidates.len() > 1 {
                return Err(Diagnostic::new(
                    "E3010",
                    format!("extension call '.{field}' is ambiguous for {receiver_type}"),
                    *field_span,
                ));
            }
            let mut expanded = Vec::with_capacity(arguments.len() + 1);
            expanded.push(LoweredCallArgument {
                label: None,
                value: receiver_expression,
                placeholder: false,
                trailing: false,
                preceding_call_span: None,
                span: record.span,
            });
            expanded.extend_from_slice(arguments);
            let labeled = arguments
                .iter()
                .any(|argument| !argument.trailing && argument.label.is_some());
            let mut compiled = if builtin {
                if labeled {
                    let receiver_label = builtin_argument_labels(
                        namespace_callee.as_ref().expect("builtin namespace exists"),
                    )
                    .and_then(|labels| labels.first())
                    .expect("extension builtin has a receiver parameter");
                    expanded[0].label = Some(((*receiver_label).to_owned(), record.span));
                }
                self.compile_call(
                    namespace_callee.as_ref().expect("builtin namespace exists"),
                    type_arguments,
                    &expanded,
                    span,
                )?
            } else if let [symbol] = candidates.as_slice() {
                let target = self.global.bundle.functions[*symbol as usize].clone();
                if labeled {
                    expanded[0].label = Some((target.lowered.parameters[0].0.clone(), record.span));
                }
                let direct_callee = LoweredExpr {
                    kind: LoweredExprKind::Variable(target.lowered.name),
                    span: callee.span,
                };
                let caller_module = std::mem::replace(&mut self.info.module, target.module.clone());
                let result = self.compile_call(&direct_callee, type_arguments, &expanded, span);
                self.info.module = caller_module;
                result?
            } else {
                return Err(Diagnostic::new(
                    "E3005",
                    format!("type {receiver_type} has no field or extension '{field}'"),
                    *field_span,
                ));
            };
            compiled.effects = self.union_effects([receiver_effects, compiled.effects]);
            compiled.hir.effects = compiled.effects;
            Ok(compiled)
        })())
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_call(
        &mut self,
        callee: &LoweredExpr,
        type_arguments: &[LoweredType],
        call_arguments: &[LoweredCallArgument],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if let LoweredExprKind::OptionalFieldGet {
            receiver,
            field,
            field_span,
            operator_span,
        } = &callee.kind
        {
            return self.compile_optional_call(
                receiver,
                field,
                *operator_span,
                *field_span,
                type_arguments,
                call_arguments,
                span,
            );
        }
        if let Some(extension) =
            self.compile_extension_call(callee, type_arguments, call_arguments, span)
        {
            return extension;
        }
        if call_arguments.iter().any(|argument| argument.placeholder) {
            return self.compile_partial_call(callee, type_arguments, call_arguments, span);
        }
        let source_arguments = call_arguments
            .iter()
            .map(|argument| {
                let _ = argument.preceding_call_span;
                argument.value.clone()
            })
            .collect::<Vec<_>>();
        let reordered_arguments = builtin_argument_labels(callee)
            .map(|labels| reorder_labeled_builtin_arguments(call_arguments, labels, span))
            .transpose()?
            .flatten();
        let mut argument_effects = None;
        let arguments = if let Some(order) = reordered_arguments {
            if order.iter().copied().eq(0..call_arguments.len()) {
                source_arguments
            } else {
                let mut temporary_names = Vec::with_capacity(call_arguments.len());
                let mut effects = Vec::with_capacity(call_arguments.len());
                let mut terminal = false;
                for argument in call_arguments {
                    let value = if terminal {
                        self.compile_without_runtime(|lowering| {
                            lowering.compile_expression(&argument.value)
                        })?
                    } else {
                        self.compile_expression(&argument.value)?
                    };
                    terminal |= value.value_type == ValueType::Never;
                    effects.push(value.effects);
                    let name =
                        format!("__allen_labeled_argument_{}", self.global.allocate_symbol());
                    let scope = self.active_scopes.last().copied().unwrap_or(0);
                    self.bindings.insert(
                        name.clone(),
                        LocalBinding {
                            register: value.register,
                            symbol: self.global.allocate_symbol(),
                            value_type: value.value_type,
                            scope,
                            value_scope: scope,
                            mutable: false,
                            moved: false,
                        },
                    );
                    temporary_names.push(name);
                }
                argument_effects = Some(self.union_effects(effects));
                order
                    .into_iter()
                    .map(|source_index| LoweredExpr {
                        kind: LoweredExprKind::Variable(temporary_names[source_index].clone()),
                        span: call_arguments[source_index].value.span,
                    })
                    .collect()
            }
        } else {
            source_arguments
        };
        macro_rules! finish_builtin_call {
            ($value:expr) => {{
                let mut value = $value?;
                if let Some(argument_effects) = argument_effects {
                    value.effects = self.union_effects([value.effects, argument_effects]);
                    value.hir.effects = value.effects;
                }
                return Ok(value);
            }};
        }
        if is_task_snapshot_callee(callee) {
            finish_builtin_call!(self.compile_task_snapshot(&arguments, span));
        }
        if let Some(name) = template_callee(callee) {
            finish_builtin_call!(self.compile_template_render(
                name,
                type_arguments,
                &arguments,
                span
            ));
        }
        if let Some(path) = tool_callee(callee) {
            let binding = self
                .global
                .bundle
                .tools
                .get(&path)
                .cloned()
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E3005",
                        "tool call is not in the frozen catalog",
                        callee.span,
                    )
                })?;
            if arguments.len() != 1 {
                return Err(Diagnostic::new(
                    "E3010",
                    "tool call requires exactly one input",
                    span,
                ));
            }
            let input = self.compile_expected(&arguments[0], &binding.input, "tool input")?;
            let value_type = ValueType::Future(Box::new(ValueType::Result(
                Box::new(binding.output.clone()),
                Box::new(binding.error.clone()),
            )));
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::ToolInvoke {
                destination: register,
                tool: binding.contract,
                input: input.register,
            });
            self.mir.push(MirOperation::ToolCall {
                destination: u32::from(register),
                tool: binding.contract,
                input: u32::from(input.register),
            });
            self.record_ownership(register, 0, MirOwnershipState::Live, true);
            let call_effect = effect_id(&self.global.effect_sets, &[binding.effect]);
            let effects = self.union_effects([input.effects, call_effect]);
            finish_builtin_call!(Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::ToolCall {
                        tool: binding.contract,
                        input: Box::new(input.hir),
                    },
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            }));
        }
        if let Some(builtin) = standard_builtin_callee(callee) {
            let result = match builtin {
                StandardBuiltin::Workspace => {
                    if !type_arguments.is_empty() {
                        return Err(Diagnostic::new(
                            "E3005",
                            "fs.workspace does not take type arguments",
                            span,
                        ));
                    }
                    self.compile_workspace_get(&arguments, span)
                }
                StandardBuiltin::Operation(operation) => {
                    self.compile_effect_call(operation, type_arguments, &arguments, span)
                }
            };
            finish_builtin_call!(result);
        }
        if let Some(builtin) = collection_builtin_callee(callee) {
            if !type_arguments.is_empty() {
                return Err(Diagnostic::new(
                    "E3005",
                    "collection builtins do not take type arguments",
                    span,
                ));
            }
            finish_builtin_call!(self.compile_collection_builtin(builtin, &arguments, span));
        }
        if let Some(operation) = string_builtin_callee(callee) {
            if !type_arguments.is_empty() {
                return Err(Diagnostic::new(
                    "E3005",
                    "String builtins do not take type arguments",
                    span,
                ));
            }
            finish_builtin_call!(self.compile_string_builtin(operation, &arguments, span));
        }
        if let Some(operation) = standard_operation_callee(callee) {
            if !type_arguments.is_empty() {
                return Err(Diagnostic::new(
                    "E3005",
                    "standard operations do not take type arguments",
                    span,
                ));
            }
            finish_builtin_call!(self.compile_standard_builtin(operation, &arguments, span));
        }
        if let Some(operation) = capability_builtin_callee(callee) {
            if !type_arguments.is_empty() {
                return Err(Diagnostic::new(
                    "E3005",
                    "capability inspection does not take type arguments",
                    span,
                ));
            }
            finish_builtin_call!(self.compile_capability_builtin(operation, &arguments, span));
        }
        if matches!(&callee.kind, LoweredExprKind::Variable(name) if name == "narrow") {
            if type_arguments.len() != 1 || arguments.len() != 1 {
                return Err(Diagnostic::new(
                    "E2018",
                    "narrow<T> requires one concrete target type and one argument",
                    span,
                ));
            }
            let target = self.annotation_type(&type_arguments[0])?;
            if matches!(
                target,
                ValueType::Never
                    | ValueType::Unknown
                    | ValueType::Function { .. }
                    | ValueType::Future(_)
                    | ValueType::Task(_)
                    | ValueType::Workspace
                    | ValueType::SubAgent
            ) || contains_workspace(&target)
                || contains_affine(&target)
                || contains_sub_agent(&target)
            {
                return Err(Diagnostic::new(
                    "E2018",
                    "narrow target must be a complete concrete value type",
                    type_arguments[0].span(),
                ));
            }
            let value =
                self.compile_expected(&arguments[0], &ValueType::Unknown, "narrow input")?;
            let value_type = ValueType::Option(Box::new(target.clone()));
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::Narrow {
                destination: register,
                source: value.register,
                target,
            });
            self.mir.push(MirOperation::Enum {
                destination: u32::from(register),
            });
            let effects = value.effects;
            finish_builtin_call!(Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::Narrow(Box::new(value.hir)),
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            }));
        }
        if !type_arguments.is_empty()
            && matches!(&callee.kind, LoweredExprKind::Variable(name) if name == "decode")
        {
            if type_arguments.len() != 1 || arguments.len() != 1 {
                return Err(Diagnostic::new(
                    "E2018",
                    "decode<T> requires one concrete target type and one Bytes argument",
                    span,
                ));
            }
            let target = self.annotation_type(&type_arguments[0])?;
            if !is_strict_schema_type(&target)
                || contains_workspace(&target)
                || contains_affine(&target)
                || contains_sub_agent(&target)
            {
                return Err(Diagnostic::new(
                    "E2018",
                    "decode target must be a complete concrete entry-boundary value type",
                    type_arguments[0].span(),
                ));
            }
            let value = self.compile_expected(&arguments[0], &ValueType::Bytes, "decode input")?;
            let value_type =
                ValueType::Result(Box::new(target.clone()), Box::new(decode_error_type()));
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::Decode {
                destination: register,
                source: value.register,
                target,
            });
            self.mir.push(MirOperation::Enum {
                destination: u32::from(register),
            });
            let effects = value.effects;
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::Decode(Box::new(value.hir)),
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }
        if let LoweredExprKind::Variable(name) = &callee.kind {
            if let Ok(value_type @ ValueType::Newtype { .. }) = resolve_named_type(
                &self.global.bundle.modules,
                &self.global.bundle.types,
                &self.info.module,
                name,
                callee.span,
            ) {
                if !type_arguments.is_empty() || arguments.len() != 1 {
                    return Err(Diagnostic::new(
                        "E3010",
                        format!("newtype constructor '{name}' requires exactly one argument"),
                        span,
                    ));
                }
                let ValueType::Newtype { underlying, .. } = &value_type else {
                    unreachable!("matched newtype")
                };
                let value = self.compile_expected(
                    &arguments[0],
                    underlying,
                    "newtype constructor argument",
                )?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::NewtypeWrap {
                    destination: register,
                    source: value.register,
                });
                self.mir.push(MirOperation::NewtypeWrap {
                    destination: u32::from(register),
                    source: u32::from(value.register),
                });
                let effects = value.effects;
                return Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::NewtypeWrap(Box::new(value.hir)),
                        None,
                        &value_type,
                        effects,
                        span,
                    ),
                });
            }
        }
        if !type_arguments.is_empty() {
            return Err(Diagnostic::new(
                "E3005",
                "only typed response operations take explicit type arguments",
                span,
            ));
        }
        let LoweredExprKind::Variable(name) = &callee.kind else {
            if let LoweredExprKind::FieldGet { record, field, .. } = &callee.kind {
                if let LoweredExprKind::Variable(enum_name) = &record.kind {
                    if resolve_named_type(
                        &self.global.bundle.modules,
                        &self.global.bundle.types,
                        &self.info.module,
                        enum_name,
                        record.span,
                    )
                    .is_ok_and(|value_type| matches!(value_type, ValueType::Enum(_)))
                    {
                        return self.compile_user_enum(
                            enum_name,
                            field,
                            &LoweredEnumValuePayload::Tuple(arguments.clone()),
                            span,
                        );
                    }
                }
            }
            return Err(Diagnostic::new(
                "E3005",
                "call target must be a resolved name",
                callee.span,
            ));
        };
        if name == "to_int" {
            finish_builtin_call!(self.compile_standard_builtin(
                StandardOperation::ToInt,
                &arguments,
                span
            ));
        }
        if matches!(
            name.as_str(),
            "to_float" | "to_string" | "to_bytes" | "to_unknown"
        ) {
            if arguments.len() != 1 {
                return Err(Diagnostic::new(
                    "E2011",
                    format!("'{name}' expects exactly one argument"),
                    span,
                ));
            }
            let value = self.compile_expression(&arguments[0])?;
            if name == "to_unknown" {
                if matches!(
                    value.value_type,
                    ValueType::Never
                        | ValueType::Unknown
                        | ValueType::Function { .. }
                        | ValueType::Future(_)
                        | ValueType::Task(_)
                        | ValueType::Workspace
                        | ValueType::SubAgent
                ) || contains_workspace(&value.value_type)
                    || contains_affine(&value.value_type)
                    || contains_sub_agent(&value.value_type)
                {
                    return Err(Diagnostic::new(
                        "E2018",
                        "to_unknown requires a concrete encodable value",
                        arguments[0].span,
                    ));
                }
                let register = self.allocate(ValueType::Unknown)?;
                self.code.push(Instruction::ToUnknown {
                    destination: register,
                    source: value.register,
                });
                self.mir.push(MirOperation::Move {
                    destination: u32::from(register),
                    source: u32::from(value.register),
                });
                let effects = value.effects;
                return Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Unknown,
                    effects,
                    hir: self.hir(
                        HirExprKind::ToUnknown(Box::new(value.hir)),
                        None,
                        &ValueType::Unknown,
                        effects,
                        span,
                    ),
                });
            }
            let (conversion, value_type) = match (name.as_str(), &value.value_type) {
                ("to_float", ValueType::Int) => (Conversion::IntToFloat, ValueType::Float),
                ("to_bytes", ValueType::String) => (Conversion::StringToBytes, ValueType::Bytes),
                (
                    "to_string",
                    ValueType::Bool | ValueType::Int | ValueType::Float | ValueType::String,
                ) => (Conversion::ToString, ValueType::String),
                _ => {
                    return Err(Diagnostic::new(
                        "E2011",
                        format!("'{name}' does not accept {}", value.value_type),
                        arguments[0].span,
                    ));
                }
            };
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::Convert {
                destination: register,
                source: value.register,
                conversion,
            });
            self.mir.push(MirOperation::Move {
                destination: u32::from(register),
                source: u32::from(value.register),
            });
            let effects = value.effects;
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::Convert(Box::new(value.hir)),
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }
        if matches!(name.as_str(), "Some" | "Ok" | "Err") {
            let values = arguments
                .iter()
                .map(|argument| self.compile_expression(argument))
                .collect::<Result<Vec<_>, _>>()?;
            return self.compile_builtin_enum(name, &values, span);
        }
        if name == "stop" {
            let values = arguments
                .iter()
                .map(|argument| self.compile_expression(argument))
                .collect::<Result<Vec<_>, _>>()?;
            if values.len() != 1 || values[0].value_type != ValueType::String {
                return Err(Diagnostic::new(
                    "E3011",
                    "stop requires one String reason",
                    span,
                ));
            }
            self.code.push(Instruction::Stop {
                reason: values[0].register,
            });
            let effects = values[0].effects;
            return Ok(CompiledExpr {
                register: values[0].register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::Stop(Box::new(values.into_iter().next().expect("one value").hir)),
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        if name == "fail" && arguments.len() == 1 {
            let values = arguments
                .iter()
                .map(|argument| self.compile_expression(argument))
                .collect::<Result<Vec<_>, _>>()?;
            if values.len() != 1 || values[0].value_type != ValueType::String {
                return Err(Diagnostic::new(
                    "E3011",
                    "fail requires one String reason",
                    span,
                ));
            }
            self.code.push(Instruction::Fail {
                reason: values[0].register,
            });
            let effects = values[0].effects;
            return Ok(CompiledExpr {
                register: values[0].register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::Fail(Box::new(values.into_iter().next().expect("one value").hir)),
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        if let Some(binding) = self.bindings.get(name).cloned() {
            if let Some(labeled_argument) = call_arguments
                .iter()
                .find(|argument| argument.label.is_some())
            {
                return Err(Diagnostic::new(
                    "E3010",
                    "calls through function values use positional arguments",
                    labeled_argument.span,
                ));
            }
            let ValueType::Function {
                parameters,
                return_type,
                effects,
            } = &binding.value_type
            else {
                return Err(Diagnostic::new(
                    "E3005",
                    format!("local value '{name}' is not callable"),
                    callee.span,
                ));
            };
            if arguments.len() != parameters.len() {
                return Err(Diagnostic::new(
                    "E3010",
                    "callback arguments do not match its exact type",
                    span,
                ));
            }
            let values = arguments
                .iter()
                .zip(parameters)
                .map(|(argument, parameter)| {
                    self.compile_expected(argument, parameter, "callback argument")
                })
                .collect::<Result<Vec<_>, _>>()?;
            for value in &values {
                if is_affine(&value.value_type) {
                    let scope = self
                        .ownership_states
                        .get(&value.register)
                        .map_or(0, |ownership| ownership.scope);
                    if matches!(value.value_type, ValueType::Task(_)) && scope != 0 {
                        return Err(Diagnostic::new(
                            "E3011",
                            "task ownership cannot escape an await block",
                            span,
                        ));
                    }
                    self.consume_ownership(value.register, MirOwnershipState::Moved);
                }
            }
            let value_type = return_type.as_ref().clone();
            let result_scope = if contains_stored_sub_agent(&value_type) {
                Some(
                    values
                        .iter()
                        .filter(|value| contains_stored_sub_agent(&value.value_type))
                        .map(|value| self.sub_agent_value_scope(value))
                        .reduce(|left, right| self.deeper_scope(left, right))
                        .ok_or_else(|| {
                            Diagnostic::new(
                                "E3011",
                                "closure call returning SubAgent requires a SubAgent-containing argument",
                                span,
                            )
                        })?,
                )
            } else {
                None
            };
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::ClosureCall {
                destination: register,
                closure: binding.register,
                arguments: values.iter().map(|value| value.register).collect(),
            });
            self.mir.push(MirOperation::ClosureCall {
                destination: u32::from(register),
                closure: u32::from(binding.register),
                arguments: values
                    .iter()
                    .map(|value| u32::from(value.register))
                    .collect(),
            });
            if is_affine(&value_type) {
                self.record_ownership(register, 0, MirOwnershipState::Live, true);
            }
            if let Some(result_scope) = result_scope {
                self.sub_agent_value_scopes.insert(register, result_scope);
            }
            let effects = self.union_effects(
                values
                    .iter()
                    .map(|value| value.effects)
                    .chain(std::iter::once(*effects)),
            );
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::ClosureCall(values.into_iter().map(|value| value.hir).collect()),
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }

        let (target, _) = self.resolve_callable(name)?.ok_or_else(|| {
            Diagnostic::new("E3005", format!("unknown function '{name}'"), callee.span)
        })?;
        let symbol = target.symbol;
        if target.is_const {
            return Err(Diagnostic::new(
                "E3005",
                format!("constant '{name}' is a value and cannot be called"),
                callee.span,
            ));
        }
        let trailing_arguments = call_arguments
            .iter()
            .enumerate()
            .filter(|(_, argument)| argument.trailing)
            .collect::<Vec<_>>();
        if trailing_arguments.len() > 1
            || trailing_arguments
                .first()
                .is_some_and(|(index, _)| *index + 1 != call_arguments.len())
        {
            return Err(Diagnostic::new(
                "E3010",
                "a call can have only one final trailing callback",
                trailing_arguments
                    .first()
                    .map_or(span, |(_, argument)| argument.span),
            ));
        }
        let supplied_labels = call_arguments
            .iter()
            .filter(|argument| !argument.trailing)
            .filter_map(|argument| argument.label.as_ref());
        let has_labeled_arguments = supplied_labels.clone().next().is_some();
        let has_positional_arguments = call_arguments
            .iter()
            .any(|argument| !argument.trailing && argument.label.is_none());
        if has_labeled_arguments && has_positional_arguments {
            return Err(Diagnostic::new(
                "E3010",
                format!("function '{name}' call cannot mix labeled and positional arguments"),
                call_arguments
                    .iter()
                    .find(|argument| !argument.trailing && argument.label.is_none())
                    .map_or(span, |argument| argument.span),
            ));
        }
        let parameter_indexes = if has_labeled_arguments {
            let mut indexes = Vec::with_capacity(call_arguments.len());
            let mut seen = BTreeSet::new();
            for argument in call_arguments {
                let index = if argument.trailing {
                    target.parameters.len().checked_sub(1).ok_or_else(|| {
                        Diagnostic::new(
                            "E3010",
                            format!("function '{name}' has no final callback parameter"),
                            argument.span,
                        )
                    })?
                } else {
                    let (label, label_span) = argument.label.as_ref().expect("labeled call");
                    target
                        .lowered
                        .parameters
                        .iter()
                        .position(|(parameter, _, _)| parameter == label)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                "E3010",
                                format!("function '{name}' has no parameter labeled '{label}'"),
                                *label_span,
                            )
                        })?
                };
                if !seen.insert(index) {
                    return Err(Diagnostic::new(
                        "E3010",
                        format!("function '{name}' received the same parameter twice"),
                        argument.span,
                    ));
                }
                indexes.push(index);
            }
            let missing = target
                .lowered
                .parameters
                .iter()
                .enumerate()
                .filter_map(|(index, (parameter, _, _))| {
                    (!seen.contains(&index) && target.lowered.parameter_defaults[index].is_none())
                        .then_some(parameter.as_str())
                })
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                return Err(Diagnostic::new(
                    "E3010",
                    format!(
                        "function '{name}' is missing labeled argument{} {}",
                        if missing.len() == 1 { "" } else { "s" },
                        missing.join(", ")
                    ),
                    span,
                ));
            }
            indexes
        } else {
            let ordinary_count = call_arguments
                .iter()
                .filter(|argument| !argument.trailing)
                .count();
            let trailing = usize::from(!trailing_arguments.is_empty());
            if ordinary_count + trailing > target.parameters.len() {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("function '{name}' has the wrong argument count"),
                    span,
                ));
            }
            let mut indexes = (0..ordinary_count).collect::<Vec<_>>();
            if trailing == 1 {
                let final_index = target.parameters.len() - 1;
                if indexes.contains(&final_index) {
                    return Err(Diagnostic::new(
                        "E3010",
                        format!("function '{name}' received its final parameter twice"),
                        trailing_arguments[0].1.span,
                    ));
                }
                indexes.push(final_index);
            }
            if let Some((missing_index, (missing, _, _))) = target
                .lowered
                .parameters
                .iter()
                .enumerate()
                .find(|(index, _)| {
                    !indexes.contains(index) && target.lowered.parameter_defaults[*index].is_none()
                })
            {
                let _ = missing_index;
                return Err(Diagnostic::new(
                    "E3007",
                    format!("function '{name}' is missing required parameter '{missing}'"),
                    span,
                ));
            }
            indexes
        };
        let mut substitutions = BTreeMap::new();
        let mut source_values = Vec::with_capacity(target.parameters.len());
        for (argument, parameter_index) in arguments.iter().zip(parameter_indexes) {
            if call_arguments[source_values.len()].placeholder {
                return Err(Diagnostic::new(
                    "E3010",
                    "call placeholders require partial-application lowering",
                    call_arguments[source_values.len()].span,
                ));
            }
            let parameter = &target.parameters[parameter_index];
            let value = if matches!(parameter, SemanticType::Generic(_)) {
                self.compile_expression(argument)?
            } else {
                match concrete_type(parameter, &substitutions, &self.global.effect_sets) {
                    Ok(expected) => {
                        self.compile_expected(argument, &expected, "function argument")?
                    }
                    Err(_) if has_labeled_arguments => self.compile_expression(argument)?,
                    Err(error) => return Err(error),
                }
            };
            if let SemanticType::Generic(generic) = parameter {
                if let Some(previous) =
                    substitutions.insert(generic.clone(), value.value_type.clone())
                {
                    if previous != value.value_type {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!("generic '{generic}' inferred as two different types"),
                            span,
                        ));
                    }
                }
            }
            source_values.push((parameter_index, value));
        }
        let mut values = (0..target.parameters.len())
            .map(|_| None)
            .collect::<Vec<Option<CompiledExpr>>>();
        let mut value_names = vec![None; target.parameters.len()];
        for (parameter_index, value) in source_values {
            let temporary = format!("__allen_call_argument_{}", self.global.allocate_symbol());
            let scope = self.active_scopes.last().copied().unwrap_or(0);
            self.bindings.insert(
                temporary.clone(),
                LocalBinding {
                    register: value.register,
                    symbol: self.global.allocate_symbol(),
                    value_type: value.value_type.clone(),
                    scope,
                    value_scope: scope,
                    mutable: false,
                    moved: false,
                },
            );
            value_names[parameter_index] = Some(temporary);
            values[parameter_index] = Some(value);
        }
        for parameter_index in 0..target.parameters.len() {
            if values[parameter_index].is_some() {
                continue;
            }
            let default = target.lowered.parameter_defaults[parameter_index]
                .as_ref()
                .expect("required parameter omissions were rejected");
            let helper_name = default_helper_name(&target.lowered.name, parameter_index);
            let helper_arguments = value_names[..parameter_index]
                .iter()
                .map(|name| LoweredCallArgument {
                    label: None,
                    value: LoweredExpr {
                        kind: LoweredExprKind::Variable(
                            name.clone()
                                .expect("earlier default argument was populated"),
                        ),
                        span: default.span,
                    },
                    placeholder: false,
                    trailing: false,
                    preceding_call_span: None,
                    span: default.span,
                })
                .collect::<Vec<_>>();
            let helper_callee = LoweredExpr {
                kind: LoweredExprKind::Variable(helper_name),
                span: default.span,
            };
            let caller_module = std::mem::replace(&mut self.info.module, target.module.clone());
            let value = self.compile_call(&helper_callee, &[], &helper_arguments, default.span);
            self.info.module = caller_module;
            let value = value?;
            let parameter = &target.parameters[parameter_index];
            if let SemanticType::Generic(generic) = parameter {
                if let Some(previous) =
                    substitutions.insert(generic.clone(), value.value_type.clone())
                {
                    if previous != value.value_type {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!("generic '{generic}' inferred as two different types"),
                            default.span,
                        ));
                    }
                }
            }
            let temporary = format!("__allen_default_argument_{}", self.global.allocate_symbol());
            let scope = self.active_scopes.last().copied().unwrap_or(0);
            self.bindings.insert(
                temporary.clone(),
                LocalBinding {
                    register: value.register,
                    symbol: self.global.allocate_symbol(),
                    value_type: value.value_type.clone(),
                    scope,
                    value_scope: scope,
                    mutable: false,
                    moved: false,
                },
            );
            value_names[parameter_index] = Some(temporary);
            values[parameter_index] = Some(value);
        }
        let values = values
            .into_iter()
            .map(|value| value.expect("every call parameter was populated"))
            .collect::<Vec<_>>();
        for (parameter, value) in target.parameters.iter().zip(&values) {
            let expected = concrete_type(parameter, &substitutions, &self.global.effect_sets)?;
            if value.value_type != ValueType::Never && value.value_type != expected {
                return Err(Diagnostic::new(
                    "E3007",
                    format!(
                        "function argument expects {expected}, found {}",
                        value.value_type
                    ),
                    span,
                ));
            }
        }
        let captured_obligation = values.iter().any(|value| {
            self.must_consume(value.register) || matches!(value.value_type, ValueType::Task(_))
        });
        let captured_scope = values
            .iter()
            .filter_map(|value| self.ownership_states.get(&value.register))
            .map(|ownership| ownership.scope)
            .find(|scope| *scope != 0)
            .unwrap_or(0);
        for value in &values {
            if is_affine(&value.value_type) {
                self.consume_ownership(value.register, MirOwnershipState::Moved);
            }
        }
        for (generic, declaration_span) in &target.lowered.generics {
            let inferred = substitutions.get(generic).ok_or_else(|| {
                Diagnostic::new(
                    "E3007",
                    format!("cannot infer generic '{generic}'"),
                    *declaration_span,
                )
            })?;
            if !inferred.is_equatable() {
                return Err(Diagnostic::new(
                    "E3008",
                    format!("type {inferred} does not satisfy Eq for '{generic}'"),
                    span,
                )
                .with_label(*declaration_span, "Eq constraint declared here"));
            }
        }
        let declared_return_type = concrete_type(
            &target.return_type,
            &substitutions,
            &self.global.effect_sets,
        )?;
        let callee_effects = effect_id(&self.global.effect_sets, &target.effects);
        let function = if let Some(function) = target.bytecode {
            function
        } else {
            let arguments = target
                .lowered
                .generics
                .iter()
                .map(|(generic, _)| substitutions[generic].clone())
                .collect::<Vec<_>>();
            if let Some((_, _, function)) = self
                .global
                .monomorphs
                .iter()
                .find(|(callee, types, _)| *callee == symbol && *types == arguments)
            {
                *function
            } else {
                let function = u32::try_from(self.global.functions.len()).map_err(|_| {
                    Diagnostic::new(
                        "E3005",
                        "too many generic instances",
                        target.lowered.name_span,
                    )
                })?;
                self.global.functions.push(None);
                self.global.monomorphs.push((symbol, arguments, function));
                let mut instance = target.clone();
                instance.bytecode = Some(function);
                let (compiled, hir, mir) = lower_one_function(
                    self.global,
                    instance.clone(),
                    function,
                    Vec::new(),
                    BTreeMap::new(),
                    BTreeSet::new(),
                    &substitutions,
                )
                .map_err(|diagnostic| diagnostic.with_source(&target.module))?;
                self.global.functions[function as usize] = Some(compiled);
                self.global
                    .hir_modules
                    .entry(instance.module)
                    .or_default()
                    .push(hir);
                self.global.mir_functions.push(mir);
                function
            }
        };
        let value_type = if target.lowered.is_async {
            ValueType::Future(Box::new(declared_return_type))
        } else {
            declared_return_type
        };
        let register = self.allocate(value_type.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        if target.lowered.is_async {
            self.code.push(Instruction::AsyncCall {
                destination: register,
                function,
                arguments: argument_registers,
            });
            self.mir.push(MirOperation::AsyncCall {
                destination: u32::from(register),
                function: symbol,
                arguments: values
                    .iter()
                    .map(|value| u32::from(value.register))
                    .collect(),
            });
            self.record_ownership(
                register,
                captured_scope,
                MirOwnershipState::Live,
                captured_obligation,
            );
        } else {
            self.code.push(Instruction::DirectCall {
                destination: register,
                function,
                arguments: argument_registers,
            });
            self.mir.push(MirOperation::DirectCall {
                destination: u32::from(register),
                function: symbol,
                arguments: values
                    .iter()
                    .map(|value| u32::from(value.register))
                    .collect(),
            });
            if is_affine(&value_type) {
                let result_scope = if matches!(value_type, ValueType::Task(_)) {
                    self.active_scopes.last().copied().unwrap_or(captured_scope)
                } else {
                    captured_scope
                };
                self.record_ownership(
                    register,
                    result_scope,
                    MirOwnershipState::Live,
                    captured_obligation || matches!(value_type, ValueType::Task(_)),
                );
            }
            if contains_stored_sub_agent(&value_type) {
                let result_scope = values
                    .iter()
                    .filter(|value| contains_stored_sub_agent(&value.value_type))
                    .map(|value| self.sub_agent_value_scope(value))
                    .reduce(|left, right| self.deeper_scope(left, right))
                    .ok_or_else(|| {
                        Diagnostic::new(
                            "E3011",
                            "direct call returning SubAgent requires a SubAgent-containing argument",
                            span,
                        )
                    })?;
                self.sub_agent_value_scopes.insert(register, result_scope);
            }
        }
        let effects = self.union_effects(
            values
                .iter()
                .map(|value| value.effects)
                .chain(std::iter::once(callee_effects)),
        );
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                if target.lowered.is_async {
                    HirExprKind::AsyncCall(values.into_iter().map(|value| value.hir).collect())
                } else {
                    HirExprKind::DirectCall(values.into_iter().map(|value| value.hir).collect())
                },
                Some(symbol),
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_builtin_enum(
        &mut self,
        name: &str,
        values: &[CompiledExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if values.len() != 1 {
            return Err(Diagnostic::new(
                "E3007",
                format!("{name} requires one payload"),
                span,
            ));
        }
        let (value_type, variant, expected) = match (&self.return_type, name) {
            (_, "Some") => (
                ValueType::Option(Box::new(values[0].value_type.clone())),
                1,
                values[0].value_type.clone(),
            ),
            (ValueType::Result(ok, _), "Ok") => (self.return_type.clone(), 0, ok.as_ref().clone()),
            (ValueType::Result(_, error), "Err") => {
                (self.return_type.clone(), 1, error.as_ref().clone())
            }
            _ => {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("cannot infer {name} from this function return type"),
                    span,
                ));
            }
        };
        if values[0].value_type != expected {
            return Err(Diagnostic::new(
                "E3007",
                format!("{name} payload has the wrong type"),
                span,
            ));
        }
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::EnumNew {
            destination: register,
            variant,
            payload: vec![values[0].register],
        });
        self.mir.push(MirOperation::Enum {
            destination: u32::from(register),
        });
        let effects = values[0].effects;
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(HirExprKind::Enum, None, &value_type, effects, span),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_assignment(
        &mut self,
        name: &str,
        name_span: Span,
        operation: Option<Binary>,
        expression: &LoweredExpr,
    ) -> Result<CompiledExpr, Diagnostic> {
        let binding = self.bindings.get(name).cloned().ok_or_else(|| {
            Diagnostic::new("E3005", format!("unknown local value '{name}'"), name_span)
        })?;
        if !binding.mutable {
            return Err(Diagnostic::new(
                "E3010",
                format!("cannot assign to immutable binding '{name}'"),
                name_span,
            ));
        }
        if is_affine(&binding.value_type) {
            return Err(Diagnostic::new(
                "E3011",
                "future or task values cannot use mutable bindings",
                name_span,
            ));
        }
        if let Some(operation) = operation {
            if !matches!(binding.value_type, ValueType::Int | ValueType::Float) {
                return Err(Diagnostic::new(
                    "E2003",
                    "compound assignment requires an Int or Float mutable local",
                    name_span,
                ));
            }
            if operation == Binary::Remainder && binding.value_type != ValueType::Int {
                return Err(Diagnostic::new(
                    "E2003",
                    "remainder compound assignment requires Int",
                    name_span,
                ));
            }

            let old = self.allocate(binding.value_type.clone())?;
            self.code.push(Instruction::Move {
                destination: old,
                source: binding.register,
            });
            self.mark_last_instruction(name_span);
            self.mir.push(MirOperation::Move {
                destination: u32::from(old),
                source: u32::from(binding.register),
            });

            let working = self.allocate(binding.value_type.clone())?;
            self.code.push(Instruction::Move {
                destination: working,
                source: old,
            });
            self.mark_last_instruction(name_span);
            self.mir.push(MirOperation::Move {
                destination: u32::from(working),
                source: u32::from(old),
            });
            self.bindings
                .get_mut(name)
                .expect("compound assignment binding remains available")
                .register = working;
            let value = self.compile_expected(
                expression,
                &binding.value_type,
                "compound assignment operand",
            );
            self.bindings
                .get_mut(name)
                .expect("compound assignment binding remains available")
                .register = binding.register;
            let value = value?;
            if value.value_type == ValueType::Never {
                let effects = value.effects;
                return Ok(CompiledExpr {
                    register: value.register,
                    value_type: ValueType::Never,
                    effects,
                    hir: self.hir(
                        HirExprKind::Assignment(Box::new(value.hir)),
                        Some(binding.symbol),
                        &ValueType::Never,
                        effects,
                        Span {
                            start: name_span.start,
                            end: expression.span.end,
                        },
                    ),
                });
            }

            let result = self.allocate(binding.value_type.clone())?;
            let instruction = match operation {
                Binary::Add | Binary::Subtract | Binary::Multiply | Binary::Divide => {
                    let numeric = match operation {
                        Binary::Add => NumericBinaryOp::Add,
                        Binary::Subtract => NumericBinaryOp::Subtract,
                        Binary::Multiply => NumericBinaryOp::Multiply,
                        Binary::Divide => NumericBinaryOp::Divide,
                        _ => unreachable!("compound numeric operation was matched"),
                    };
                    if binding.value_type == ValueType::Int {
                        Instruction::IntBinary {
                            destination: result,
                            left: old,
                            right: value.register,
                            operation: numeric,
                        }
                    } else {
                        Instruction::FloatBinary {
                            destination: result,
                            left: old,
                            right: value.register,
                            operation: numeric,
                        }
                    }
                }
                Binary::Remainder => Instruction::IntRemainder {
                    destination: result,
                    left: old,
                    right: value.register,
                },
                _ => unreachable!("parser only creates arithmetic compound assignments"),
            };
            self.code.push(instruction);
            self.mark_last_instruction(Span {
                start: name_span.start,
                end: expression.span.end,
            });
            self.mir.push(MirOperation::Binary {
                destination: u32::from(result),
            });
            self.code.push(Instruction::Move {
                destination: binding.register,
                source: result,
            });
            self.mark_last_instruction(Span {
                start: name_span.start,
                end: expression.span.end,
            });
            self.mir.push(MirOperation::Move {
                destination: u32::from(binding.register),
                source: u32::from(result),
            });

            let effects = value.effects;
            let old_hir = self.hir(
                HirExprKind::Variable,
                Some(binding.symbol),
                &binding.value_type,
                self.empty_effects(),
                name_span,
            );
            let binary_hir = self.hir(
                HirExprKind::Binary(vec![old_hir, value.hir]),
                None,
                &binding.value_type,
                effects,
                Span {
                    start: name_span.start,
                    end: expression.span.end,
                },
            );
            return Ok(CompiledExpr {
                register: binding.register,
                value_type: ValueType::Unit,
                effects,
                hir: self.hir(
                    HirExprKind::Assignment(Box::new(binary_hir)),
                    Some(binding.symbol),
                    &ValueType::Unit,
                    effects,
                    Span {
                        start: name_span.start,
                        end: expression.span.end,
                    },
                ),
            });
        }
        let value = self.compile_expected(expression, &binding.value_type, "assignment")?;
        let value_scope = self.sub_agent_value_scope(&value);
        if value.value_type == ValueType::Never {
            let effects = value.effects;
            return Ok(CompiledExpr {
                register: value.register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::Assignment(Box::new(value.hir)),
                    Some(binding.symbol),
                    &ValueType::Never,
                    effects,
                    Span {
                        start: name_span.start,
                        end: expression.span.end,
                    },
                ),
            });
        }
        if contains_stored_sub_agent(&binding.value_type)
            && !self.scope_outlives(value_scope, binding.scope)
        {
            return Err(Diagnostic::new(
                "E3011",
                "SubAgent-containing value cannot escape its lexical scope through assignment",
                expression.span,
            ));
        }
        self.code.push(Instruction::Move {
            destination: binding.register,
            source: value.register,
        });
        self.mir.push(MirOperation::Move {
            destination: u32::from(binding.register),
            source: u32::from(value.register),
        });
        if contains_stored_sub_agent(&binding.value_type) {
            self.bindings
                .get_mut(name)
                .expect("assignment binding remains available")
                .value_scope = value_scope;
        }
        let effects = value.effects;
        Ok(CompiledExpr {
            register: binding.register,
            value_type: ValueType::Unit,
            effects,
            hir: self.hir(
                HirExprKind::Assignment(Box::new(value.hir)),
                Some(binding.symbol),
                &ValueType::Unit,
                effects,
                Span {
                    start: name_span.start,
                    end: expression.span.end,
                },
            ),
        })
    }

    fn lowered_type_for_short_closure(
        &self,
        value_type: &ValueType,
        span: Span,
    ) -> Result<LoweredType, Diagnostic> {
        let named = |name: &str| Ok(LoweredType::Named(name.to_owned(), span));
        match value_type {
            ValueType::Int => named("Int"),
            ValueType::Bool => named("Bool"),
            ValueType::Float => named("Float"),
            ValueType::String => named("String"),
            ValueType::Bytes => named("Bytes"),
            ValueType::Unit => named("Void"),
            ValueType::List(value) => Ok(LoweredType::List(
                Box::new(self.lowered_type_for_short_closure(value, span)?),
                span,
            )),
            ValueType::Option(value) => Ok(LoweredType::Option(
                Box::new(self.lowered_type_for_short_closure(value, span)?),
                span,
            )),
            ValueType::Map(key, value) => Ok(LoweredType::Map(
                Box::new(self.lowered_type_for_short_closure(key, span)?),
                Box::new(self.lowered_type_for_short_closure(value, span)?),
                span,
            )),
            ValueType::Result(ok, error) => Ok(LoweredType::Result(
                Box::new(self.lowered_type_for_short_closure(ok, span)?),
                Box::new(self.lowered_type_for_short_closure(error, span)?),
                span,
            )),
            ValueType::Future(value) => Ok(LoweredType::Future(
                Box::new(self.lowered_type_for_short_closure(value, span)?),
                span,
            )),
            ValueType::Task(value) => Ok(LoweredType::Task(
                Box::new(self.lowered_type_for_short_closure(value, span)?),
                span,
            )),
            ValueType::Workspace => named("Workspace"),
            ValueType::ExternalFsAccess => named("ExternalFsAccess"),
            ValueType::SubAgent => named("SubAgent"),
            ValueType::Tuple(values) => Ok(LoweredType::Tuple(
                values
                    .iter()
                    .map(|value| self.lowered_type_for_short_closure(value, span))
                    .collect::<Result<_, _>>()?,
                span,
            )),
            ValueType::Record(fields) => Ok(LoweredType::Record(
                fields
                    .iter()
                    .map(|field| {
                        Ok((
                            field.name.clone(),
                            self.lowered_type_for_short_closure(&field.value_type, span)?,
                            span,
                        ))
                    })
                    .collect::<Result<_, Diagnostic>>()?,
                span,
            )),
            ValueType::Function {
                parameters,
                return_type,
                effects,
            } => Ok(LoweredType::Function {
                parameters: parameters
                    .iter()
                    .map(|value| self.lowered_type_for_short_closure(value, span))
                    .collect::<Result<_, _>>()?,
                return_type: Box::new(self.lowered_type_for_short_closure(return_type, span)?),
                effects: self.global.effect_sets[*effects as usize].clone(),
                span,
            }),
            ValueType::Enum(id) => self.global.bundle.enum_types.get(*id as usize).map_or_else(
                || {
                    Err(Diagnostic::new(
                        "E3011",
                        "concise lambda expected type is invalid",
                        span,
                    ))
                },
                |value| named(&value.name),
            ),
            ValueType::Newtype { name, .. } => named(name.rsplit("::").next().unwrap_or(name)),
            _ => Err(Diagnostic::new(
                "E3011",
                "concise lambda requires one exact concrete function type",
                span,
            )),
        }
    }

    fn compile_short_closure(
        &mut self,
        parameters: &[(String, Span)],
        body: &LoweredExpr,
        expected: &ValueType,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let ValueType::Function {
            parameters: expected_parameters,
            return_type,
            effects,
        } = expected
        else {
            return Err(Diagnostic::new(
                "E3011",
                "concise lambda requires one exact expected function type",
                span,
            ));
        };
        if parameters.len() != expected_parameters.len() {
            return Err(Diagnostic::new(
                "E3011",
                "concise lambda parameter count does not match its expected function type",
                span,
            ));
        }
        let parameters = parameters
            .iter()
            .zip(expected_parameters)
            .map(|((name, parameter_span), value_type)| {
                Ok((
                    name.clone(),
                    self.lowered_type_for_short_closure(value_type, *parameter_span)?,
                    *parameter_span,
                ))
            })
            .collect::<Result<Vec<_>, Diagnostic>>()?;
        let return_type = self.lowered_type_for_short_closure(return_type, body.span)?;
        let body = LoweredBody {
            statements: Vec::new(),
            tail: Some(body.clone()),
            span: body.span,
        };
        let declared_effects = self.global.effect_sets[*effects as usize].clone();
        let compiled = self.compile_closure(
            &parameters,
            &return_type,
            Some(&declared_effects),
            &body,
            span,
        )?;
        if compiled.value_type != *expected {
            return Err(Diagnostic::new(
                "E3011",
                "concise lambda does not match its expected function type",
                span,
            ));
        }
        Ok(compiled)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_closure(
        &mut self,
        parameters: &[(String, LoweredType, Span)],
        return_type: &LoweredType,
        declared_effects: Option<&[String]>,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let free_names = free_variables(body, parameters);
        let mut capture_names = free_names
            .into_iter()
            .filter(|name| self.bindings.contains_key(name))
            .collect::<Vec<_>>();
        capture_names.sort();
        let mut outer_captures = Vec::new();
        let mut capture_bindings = Vec::new();
        for name in capture_names {
            let binding = self.bindings[&name].clone();
            if binding.mutable {
                return Err(Diagnostic::new(
                    "E3010",
                    format!("closure cannot capture mutable binding '{name}'"),
                    span,
                ));
            }
            if is_affine(&binding.value_type) {
                return Err(Diagnostic::new(
                    "E3011",
                    format!("closure cannot capture affine binding '{name}'"),
                    span,
                ));
            }
            if contains_sub_agent(&binding.value_type) {
                return Err(Diagnostic::new(
                    "E3011",
                    format!("closure cannot capture SubAgent-containing binding '{name}'"),
                    span,
                ));
            }
            outer_captures.push(binding.register);
            capture_bindings.push((name, binding.value_type, binding.symbol));
        }
        let generics = BTreeSet::new();
        let semantic_parameters = parameters
            .iter()
            .map(|(_, value_type, _)| {
                semantic_type(
                    value_type,
                    &generics,
                    &self.info.module,
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let semantic_return = semantic_type(
            return_type,
            &generics,
            &self.info.module,
            &self.global.bundle.modules,
            &self.global.bundle.types,
        )?;
        let current = self
            .global
            .bundle
            .functions
            .iter()
            .map(|function| function.effects.clone())
            .collect::<Vec<_>>();
        let fake_lowered = LoweredFunction {
            exported: false,
            is_async: false,
            name: format!("$closure@{}", span.start),
            name_span: span,
            generics: Vec::new(),
            parameters: parameters.to_vec(),
            parameter_defaults: vec![None; parameters.len()],
            return_type: return_type.clone(),
            declared_effects: declared_effects.map(<[String]>::to_vec),
            effects_span: None,
            body: body.clone(),
        };
        let closure_symbol = self.global.allocate_symbol();
        let mut fake = FunctionInfo {
            is_const: false,
            module: self.info.module.clone(),
            symbol: closure_symbol,
            bytecode: None,
            lowered: fake_lowered,
            parameters: semantic_parameters,
            return_type: semantic_return,
            effects: Vec::new(),
        };
        if direct_capability_inspection_body_span(body).is_some()
            && !declared_effects.is_some_and(|effects| {
                effects
                    .binary_search_by(|effect| effect.as_str().cmp("capability.inspect"))
                    .is_ok()
            })
        {
            return Err(Diagnostic::new(
                "E2403",
                "closure directly inspects capabilities and must explicitly declare effect 'capability.inspect'",
                span,
            ));
        }
        let required = required_body_effects(self.global.bundle, &fake, &current)?;
        if let Some(declared) = declared_effects {
            if required
                .iter()
                .any(|effect| declared.binary_search(effect).is_err())
            {
                return Err(Diagnostic::new(
                    "E2403",
                    format!(
                        "closure requires undeclared effects [{}]",
                        required.join(", ")
                    ),
                    span,
                ));
            }
            fake.effects = declared.to_vec();
        } else {
            fake.effects = required;
        }
        let function_id = u32::try_from(self.global.functions.len())
            .map_err(|_| Diagnostic::new("E3005", "too many closure functions", span))?;
        fake.bytecode = Some(function_id);
        self.global.functions.push(None);
        let capture_symbols = capture_bindings
            .iter()
            .map(|(_, _, symbol)| *symbol)
            .collect::<Vec<_>>();
        let (function, hir_function, mir_function) = lower_one_function(
            self.global,
            fake.clone(),
            function_id,
            capture_bindings,
            self.local_functions.clone(),
            self.reserved_local_names(),
            &BTreeMap::new(),
        )
        .map_err(|diagnostic| diagnostic.with_source(&fake.module))?;
        self.global.functions[function_id as usize] = Some(function);
        let closure_hir_body = hir_function.body.clone();
        self.global
            .hir_modules
            .entry(fake.module.clone())
            .or_default()
            .push(hir_function);
        self.global.mir_functions.push(mir_function);

        let value_type = ValueType::Function {
            parameters: fake
                .parameters
                .iter()
                .map(|value_type| {
                    concrete_type(value_type, &BTreeMap::new(), &self.global.effect_sets)
                })
                .collect::<Result<_, _>>()?,
            return_type: Box::new(concrete_type(
                &fake.return_type,
                &BTreeMap::new(),
                &self.global.effect_sets,
            )?),
            effects: effect_id(&self.global.effect_sets, &fake.effects),
        };
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::ClosureNew {
            destination: register,
            function: function_id,
            captures: outer_captures.clone(),
        });
        self.mir.push(MirOperation::ClosureEnvironment {
            destination: u32::from(register),
            function: closure_symbol,
            captures: outer_captures
                .iter()
                .map(|value| u32::from(*value))
                .collect(),
        });
        let effects = effect_id(&self.global.effect_sets, &fake.effects);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Closure {
                    captures: capture_symbols,
                    body: Box::new(closure_hir_body),
                },
                Some(closure_symbol),
                &value_type,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_body(
        &mut self,
        body: &LoweredBody,
        return_type: &ValueType,
    ) -> Result<(HirExpr, Register), Diagnostic> {
        let mut expressions = Vec::new();
        let mut returned = None;
        for (index, statement) in body.statements.iter().enumerate() {
            match statement {
                LoweredStatement::Let {
                    name,
                    name_span,
                    mutable,
                    annotation,
                    value,
                } => {
                    if self.bindings.contains_key(name) || self.local_name_conflicts(name) {
                        return Err(Diagnostic::new(
                            "E3005",
                            format!("duplicate local binding '{name}'"),
                            *name_span,
                        ));
                    }
                    if let LoweredExprKind::Closure {
                        parameters, body, ..
                    } = &value.kind
                    {
                        if free_variables(body, parameters).contains(name) {
                            return Err(Diagnostic::new(
                                "E3010",
                                format!("closure '{name}' cannot capture itself"),
                                *name_span,
                            ));
                        }
                    }
                    let value = if let Some(annotation) = annotation {
                        let expected = self.annotation_type(annotation)?;
                        self.compile_expected(value, &expected, "binding")?
                    } else {
                        self.compile_expression(value)?
                    };
                    let value_scope = self.sub_agent_value_scope(&value);
                    if *mutable && is_affine(&value.value_type) {
                        return Err(Diagnostic::new(
                            "E3011",
                            "future or task values cannot use mutable bindings",
                            *name_span,
                        ));
                    }
                    let symbol = self.global.allocate_symbol();
                    let scope = self.active_scopes.last().copied().unwrap_or(0);
                    self.bindings.insert(
                        name.clone(),
                        LocalBinding {
                            register: value.register,
                            symbol,
                            value_type: value.value_type.clone(),
                            scope,
                            value_scope,
                            mutable: *mutable,
                            moved: false,
                        },
                    );
                    let terminates = value.value_type == ValueType::Never;
                    expressions.push(value.hir);
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                *name_span,
                            ));
                        }
                        returned = Some(value.register);
                    }
                }
                LoweredStatement::Assignment {
                    name,
                    name_span,
                    operation,
                    value,
                } => {
                    let assignment =
                        self.compile_assignment(name, *name_span, *operation, value)?;
                    let terminates = assignment.value_type == ValueType::Never;
                    let register = assignment.register;
                    expressions.push(assignment.hir);
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                value.span,
                            ));
                        }
                        returned = Some(register);
                    }
                }
                LoweredStatement::ControlFlow(expression) => {
                    let value = self.compile_expression(expression)?;
                    if !matches!(value.value_type, ValueType::Unit | ValueType::Never) {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!(
                                "control-flow statement must have type Void, found {}",
                                value.value_type
                            ),
                            expression.span,
                        ));
                    }
                    let terminates = value.value_type == ValueType::Never;
                    let register = value.register;
                    expressions.push(value.hir);
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                expression.span,
                            ));
                        }
                        returned = Some(register);
                    }
                }
                LoweredStatement::Return(value, statement_span) => {
                    if index + 1 != body.statements.len() || body.tail.is_some() {
                        return Err(Diagnostic::new(
                            "E3005",
                            "code after return is unreachable",
                            *statement_span,
                        ));
                    }
                    let value = self.compile_return(value.as_ref(), *statement_span)?;
                    expressions.push(value.hir);
                    returned = Some(value.register);
                }
                LoweredStatement::While {
                    condition,
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_while(condition, loop_body, *span)?;
                    expressions.push(value.hir);
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after non-terminating loop is unreachable",
                                *span,
                            ));
                        }
                        returned = Some(value.register);
                    }
                }
                LoweredStatement::Loop {
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_infinite_loop(loop_body, *span)?;
                    expressions.push(value.hir);
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after non-terminating loop is unreachable",
                                *span,
                            ));
                        }
                        returned = Some(value.register);
                    }
                }
                LoweredStatement::For {
                    binding,
                    source,
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_for(binding, source, loop_body, *span)?;
                    expressions.push(value.hir);
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating loop header is unreachable",
                                *span,
                            ));
                        }
                        returned = Some(value.register);
                    }
                }
                LoweredStatement::LocalFunction(function) => {
                    self.compile_local_function_declaration(function)?;
                }
                LoweredStatement::Break(span) | LoweredStatement::Continue(span) => {
                    let value = self.compile_loop_control(
                        matches!(statement, LoweredStatement::Break(_)),
                        *span,
                    )?;
                    if index + 1 != body.statements.len() || body.tail.is_some() {
                        return Err(Diagnostic::new(
                            "E3005",
                            "code after loop control is unreachable",
                            *span,
                        ));
                    }
                    expressions.push(value.hir);
                    returned = Some(value.register);
                }
            }
        }
        if returned.is_none() {
            let value = if let Some(tail) = &body.tail {
                self.compile_expected(tail, return_type, "function result")?
            } else if return_type == &ValueType::Unit {
                self.compile_expression(&LoweredExpr {
                    kind: LoweredExprKind::Unit,
                    span: body.span,
                })?
            } else {
                return Err(Diagnostic::new(
                    "E3005",
                    "function can end without returning a value",
                    body.span,
                ));
            };
            if value.value_type != ValueType::Never && &value.value_type != return_type {
                return Err(Diagnostic::new(
                    "E3007",
                    format!(
                        "function body has {}, expected {return_type}",
                        value.value_type
                    ),
                    body.span,
                ));
            }
            if value.value_type != ValueType::Never {
                self.prepare_return(&value, body.span)?;
                self.code.push(Instruction::Return {
                    source: value.register,
                });
            }
            returned = Some(value.register);
            expressions.push(value.hir);
        }
        let effects = effect_id(&self.global.effect_sets, &self.info.effects);
        let hir = self.hir(
            HirExprKind::Block(expressions),
            Some(self.info.symbol),
            return_type,
            effects,
            body.span,
        );
        Ok((hir, returned.expect("body always returns")))
    }

    pub(super) fn prepare_return(
        &mut self,
        value: &CompiledExpr,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if matches!(value.value_type, ValueType::Task(_)) {
            let scope = self
                .ownership_states
                .get(&value.register)
                .map_or(0, |ownership| ownership.scope);
            if scope != 0 {
                return Err(Diagnostic::new(
                    "E3011",
                    "task ownership cannot escape an await block",
                    span,
                ));
            }
            self.record_ownership(value.register, scope, MirOwnershipState::Returned, true);
        } else if matches!(
            value.value_type,
            ValueType::Future(_) | ValueType::Sequence(_)
        ) {
            self.consume_ownership(value.register, MirOwnershipState::Returned);
        }
        self.reject_live_tasks(Some(value.register), span)
    }

    pub(super) fn reject_live_tasks(
        &self,
        returned: Option<Register>,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if let Some((register, _)) = self.ownership_states.iter().find(|(register, ownership)| {
            Some(**register) != returned
                && ownership.state == MirOwnershipState::Live
                && ownership.must_consume
        }) {
            return Err(Diagnostic::new(
                "E3011",
                format!("live affine obligation in register {register} is discarded"),
                span,
            ));
        }
        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn free_variables(
    body: &LoweredBody,
    parameters: &[(String, LoweredType, Span)],
) -> BTreeSet<String> {
    fn bind_pattern(pattern: &LoweredPattern, bound: &mut BTreeSet<String>) {
        match pattern {
            LoweredPattern::Binding { name, .. } => {
                bound.insert(name.clone());
            }
            LoweredPattern::Record { fields, .. } => {
                for (_, _, pattern) in fields {
                    bind_pattern(pattern, bound);
                }
            }
            LoweredPattern::Enum {
                patterns, fields, ..
            } => {
                for pattern in patterns {
                    bind_pattern(pattern, bound);
                }
                if let Some(fields) = fields {
                    for (_, _, pattern) in fields {
                        bind_pattern(pattern, bound);
                    }
                }
            }
            LoweredPattern::Option { payload, .. } => {
                if let Some(pattern) = payload {
                    bind_pattern(pattern, bound);
                }
            }
            LoweredPattern::Result { payload, .. } => bind_pattern(payload, bound),
            LoweredPattern::Or { alternatives, .. } => {
                for pattern in alternatives {
                    bind_pattern(pattern, bound);
                }
            }
            LoweredPattern::Wildcard | LoweredPattern::Bool(_) | LoweredPattern::Range { .. } => {}
        }
    }

    pub(super) fn expression(
        value: &LoweredExpr,
        bound: &BTreeSet<String>,
        free: &mut BTreeSet<String>,
    ) {
        match &value.kind {
            LoweredExprKind::Template(parts) => {
                for interpolation in template_interpolations(parts) {
                    expression(interpolation, bound, free);
                }
            }
            LoweredExprKind::Variable(name) => {
                if !bound.contains(name) {
                    free.insert(name.clone());
                }
            }
            LoweredExprKind::List(values)
            | LoweredExprKind::Tuple(values)
            | LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Tuple(values),
                ..
            } => {
                for value in values {
                    expression(value, bound, free);
                }
            }
            LoweredExprKind::ListWithSpread(items) => {
                for item in items {
                    expression(&item.value, bound, free);
                }
            }
            LoweredExprKind::Map(entries) => {
                for (key, value) in entries {
                    expression(key, bound, free);
                    expression(value, bound, free);
                }
            }
            LoweredExprKind::MapWithSpread(items) => {
                for item in items {
                    match item {
                        super::LoweredMapItem::Entry { key, value, .. } => {
                            expression(key, bound, free);
                            expression(value, bound, free);
                        }
                        super::LoweredMapItem::Spread { value, .. } => {
                            expression(value, bound, free);
                        }
                    }
                }
            }
            LoweredExprKind::RecordUpdate { base, fields, .. } => {
                expression(base, bound, free);
                for (_, value, _) in fields {
                    expression(value, bound, free);
                }
            }
            LoweredExprKind::Record { fields, .. }
            | LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Record(fields),
                ..
            } => {
                for (_, value, _) in fields {
                    expression(value, bound, free);
                }
            }
            LoweredExprKind::Prompt {
                system,
                context,
                data,
                ..
            } => {
                expression(system, bound, free);
                if let Some(context) = context {
                    expression(context, bound, free);
                }
                if let Some(data) = data {
                    expression(data, bound, free);
                }
            }
            LoweredExprKind::Binary { left, right, .. }
            | LoweredExprKind::Compose { left, right, .. }
            | LoweredExprKind::Range {
                start: left,
                end: right,
                ..
            } => {
                expression(left, bound, free);
                expression(right, bound, free);
            }
            LoweredExprKind::Pipe { left, stage, .. } => {
                expression(left, bound, free);
                expression(stage, bound, free);
            }
            LoweredExprKind::Index { collection, index }
            | LoweredExprKind::Slice {
                collection,
                range: index,
                ..
            } => {
                expression(collection, bound, free);
                expression(index, bound, free);
            }
            LoweredExprKind::FieldGet { record, .. }
            | LoweredExprKind::OptionalFieldGet {
                receiver: record, ..
            }
            | LoweredExprKind::Try(record)
            | LoweredExprKind::Unary {
                operand: record, ..
            } => {
                expression(record, bound, free);
            }
            LoweredExprKind::Spawn(value) | LoweredExprKind::Await(value) => {
                expression(value, bound, free);
            }
            LoweredExprKind::AwaitBlock(body) => {
                let mut nested = bound.clone();
                body_free_variables(body, &mut nested, free);
            }
            LoweredExprKind::Match { source, arms } => {
                expression(source, bound, free);
                for (pattern, value, _) in arms {
                    let mut arm_bound = bound.clone();
                    bind_pattern(pattern, &mut arm_bound);
                    expression(value, &arm_bound, free);
                }
            }
            LoweredExprKind::If {
                condition,
                then_body,
                else_branch,
            } => {
                expression(condition, bound, free);
                let mut then_bound = bound.clone();
                body_free_variables(then_body, &mut then_bound, free);
                match else_branch {
                    Some(LoweredElse::Body(body)) => {
                        let mut else_bound = bound.clone();
                        body_free_variables(body, &mut else_bound, free);
                    }
                    Some(LoweredElse::If(value)) => expression(value, bound, free),
                    None => {}
                }
            }
            LoweredExprKind::Call {
                callee, arguments, ..
            } => {
                if !is_task_snapshot_callee(callee)
                    && standard_builtin_callee(callee).is_none()
                    && collection_builtin_callee(callee).is_none()
                    && string_builtin_callee(callee).is_none()
                    && standard_operation_callee(callee).is_none()
                    && capability_builtin_callee(callee).is_none()
                    && template_callee(callee).is_none()
                    && tool_callee(callee).is_none()
                {
                    expression(callee, bound, free);
                }
                for argument in arguments {
                    expression(&argument.value, bound, free);
                }
            }
            LoweredExprKind::Closure {
                parameters, body, ..
            } => {
                let mut nested = bound.clone();
                nested.extend(parameters.iter().map(|(name, _, _)| name.clone()));
                body_free_variables(body, &mut nested, free);
            }
            LoweredExprKind::ShortClosure { parameters, body } => {
                let mut nested = bound.clone();
                nested.extend(parameters.iter().map(|(name, _)| name.clone()));
                expression(body, &nested, free);
            }
            LoweredExprKind::Unit
            | LoweredExprKind::Int(_)
            | LoweredExprKind::Float(_)
            | LoweredExprKind::Bool(_)
            | LoweredExprKind::String(_)
            | LoweredExprKind::Bytes(_)
            | LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Unit,
                ..
            } => {}
        }
    }

    pub(super) fn body_free_variables(
        body: &LoweredBody,
        bound: &mut BTreeSet<String>,
        free: &mut BTreeSet<String>,
    ) {
        for statement in &body.statements {
            match statement {
                LoweredStatement::Let { name, value, .. } => {
                    expression(value, bound, free);
                    bound.insert(name.clone());
                }
                LoweredStatement::Assignment { name, value, .. } => {
                    if !bound.contains(name) {
                        free.insert(name.clone());
                    }
                    expression(value, bound, free);
                }
                LoweredStatement::ControlFlow(value) => expression(value, bound, free),
                LoweredStatement::Return(value, _) => {
                    if let Some(value) = value {
                        expression(value, bound, free);
                    }
                }
                LoweredStatement::While {
                    condition, body, ..
                } => {
                    expression(condition, bound, free);
                    let mut nested = bound.clone();
                    body_free_variables(body, &mut nested, free);
                }
                LoweredStatement::Loop { body, .. } => {
                    let mut nested = bound.clone();
                    body_free_variables(body, &mut nested, free);
                }
                LoweredStatement::For {
                    binding,
                    source,
                    body,
                    ..
                } => {
                    match source {
                        LoweredForSource::Iterable(value) => expression(value, bound, free),
                    }
                    let mut nested = bound.clone();
                    nested.extend(
                        binding
                            .elements
                            .iter()
                            .filter_map(|element| element.name.clone()),
                    );
                    body_free_variables(body, &mut nested, free);
                }
                LoweredStatement::LocalFunction(function) => {
                    bound.insert(function.name.clone());
                    let mut nested = bound.clone();
                    nested.extend(function.parameters.iter().map(|(name, _, _)| name.clone()));
                    body_free_variables(&function.body, &mut nested, free);
                }
                LoweredStatement::Break(_) | LoweredStatement::Continue(_) => {}
            }
        }
        if let Some(tail) = &body.tail {
            expression(tail, bound, free);
        }
    }

    let mut bound = parameters
        .iter()
        .map(|(name, _, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let mut free = BTreeSet::new();
    body_free_variables(body, &mut bound, &mut free);
    free
}

#[allow(clippy::too_many_lines)]
pub(super) fn lower_one_function(
    global: &mut GlobalLowering<'_>,
    info: FunctionInfo,
    function_id: FunctionId,
    capture_values: Vec<(String, ValueType, SymbolId)>,
    local_functions: BTreeMap<String, FunctionInfo>,
    unavailable_local_functions: BTreeSet<String>,
    substitutions: &BTreeMap<String, ValueType>,
) -> Result<(Function, HirFunction, MirFunction), Diagnostic> {
    if info.lowered.is_async {
        global.async_functions.insert(function_id);
    }
    let return_type = concrete_type(&info.return_type, substitutions, &global.effect_sets)?;
    let effects = effect_id(&global.effect_sets, &info.effects);
    let mut lowering = FunctionLowering {
        global,
        info: info.clone(),
        return_type: return_type.clone(),
        registers: Vec::new(),
        parameters: Vec::new(),
        captures: Vec::new(),
        bindings: BTreeMap::new(),
        local_functions,
        unavailable_local_functions,
        local_function_ordinal: 0,
        code: Vec::new(),
        instruction_spans: BTreeMap::new(),
        mir: Vec::new(),
        mir_blocks: Vec::new(),
        mir_suspensions: Vec::new(),
        mir_task_scopes: Vec::new(),
        mir_ownership: Vec::new(),
        ownership_states: BTreeMap::new(),
        active_scopes: Vec::new(),
        next_scope: 1,
        mir_continuations: BTreeSet::new(),
        mir_entries: Vec::new(),
        mir_tail: None,
        loops: Vec::new(),
        control_reachable: true,
        runtime_terminal_values: BTreeSet::new(),
        sub_agent_value_scopes: BTreeMap::new(),
    };
    for ((name, _, span), parameter_type) in info.lowered.parameters.iter().zip(&info.parameters) {
        if lowering.local_name_conflicts(name) {
            return Err(Diagnostic::new(
                "E3005",
                format!("parameter '{name}' conflicts with a local function"),
                *span,
            ));
        }
        let value_type =
            concrete_type(parameter_type, substitutions, &lowering.global.effect_sets)?;
        let register = lowering.allocate(value_type.clone())?;
        let symbol = lowering.global.allocate_symbol();
        lowering.parameters.push(register);
        if lowering
            .bindings
            .insert(
                name.clone(),
                LocalBinding {
                    register,
                    symbol,
                    value_type: value_type.clone(),
                    scope: 0,
                    value_scope: 0,
                    mutable: false,
                    moved: false,
                },
            )
            .is_some()
        {
            return Err(Diagnostic::new(
                "E3005",
                format!("duplicate parameter '{name}'"),
                *span,
            ));
        }
        if is_affine(&value_type) {
            lowering.record_ownership(register, 0, MirOwnershipState::Live, true);
        }
    }
    for (name, value_type, symbol) in capture_values {
        let register = lowering.allocate(value_type.clone())?;
        lowering.captures.push(register);
        lowering.bindings.insert(
            name,
            LocalBinding {
                register,
                symbol,
                value_type: value_type.clone(),
                scope: 0,
                value_scope: 0,
                mutable: false,
                moved: false,
            },
        );
        if is_affine(&value_type) {
            lowering.record_ownership(register, 0, MirOwnershipState::Live, true);
        }
    }
    let (body, return_register) = lowering.compile_body(&info.lowered.body, &return_type)?;
    let parameters = lowering
        .parameters
        .iter()
        .map(|register| {
            lowering
                .global
                .intern_type(lowering.registers[*register as usize].clone())
        })
        .collect();
    let return_type_id = lowering.global.intern_type(return_type.clone());
    let temporaries = lowering
        .registers
        .iter()
        .cloned()
        .map(|value_type| lowering.global.intern_type(value_type))
        .collect();
    // Bytecode function symbols use the verifier's normalized path grammar.
    // Source, import, and entry metadata retain the exact `pkg://` identity.
    let bytecode_module = bytecode_module_path(&info.module);
    let bytecode_name =
        if info.lowered.name.starts_with("$default@") || info.lowered.name.starts_with("$local@") {
            format!("$closure@{}", info.symbol)
        } else {
            info.lowered.name.clone()
        };
    let symbol_name = if substitutions.is_empty() {
        format!("{bytecode_module}::{bytecode_name}")
    } else {
        let arguments = substitutions
            .iter()
            .map(|(name, value_type)| format!("{name}={value_type}"))
            .collect::<Vec<_>>()
            .join(",");
        format!("{bytecode_module}::{bytecode_name}<{arguments}>")
    };
    let terminal = match lowering.code.last() {
        Some(Instruction::Stop { reason }) => Some(MirTerminator::Stop {
            reason: u32::from(*reason),
        }),
        Some(Instruction::Fail { reason }) => Some(MirTerminator::Fail {
            reason: u32::from(*reason),
        }),
        _ => None,
    };
    let instruction_spans = std::mem::take(&mut lowering.instruction_spans);
    let function = Function {
        name: symbol_name,
        parameters: lowering.parameters,
        parameter_names: info
            .lowered
            .parameters
            .iter()
            .map(|(name, _, _)| name.clone())
            .collect(),
        parameter_default_digests: info
            .lowered
            .parameter_defaults
            .iter()
            .map(|default| {
                default
                    .as_ref()
                    .map(|default| parameter_default_digest(&default.source_text))
            })
            .collect(),
        captures: lowering.captures,
        registers: lowering.registers,
        return_type,
        effects,
        code: lowering.code,
    };
    let source = lowering
        .global
        .debug_sources
        .binary_search(&info.module)
        .expect("resolved module has a debug source ID");
    let source = u32::try_from(source).expect("debug source ID fits");
    let mut locations = Vec::with_capacity(function.code.len());
    for instruction in 0..function.code.len() {
        let span = instruction_spans
            .get(&instruction)
            .copied()
            .unwrap_or(info.lowered.body.span);
        let start = u32::try_from(span.start)
            .map_err(|_| Diagnostic::new("E3005", "source span exceeds artifact limits", span))?;
        let end = u32::try_from(span.end)
            .map_err(|_| Diagnostic::new("E3005", "source span exceeds artifact limits", span))?;
        locations.push(DebugLocation {
            function: function_id,
            instruction: u32::try_from(instruction).expect("instruction ID fits"),
            source,
            start,
            end,
        });
    }
    lowering.global.debug_locations.extend(locations);
    let hir = HirFunction {
        symbol: info.symbol,
        name: info.lowered.name.clone(),
        is_async: info.lowered.is_async,
        parameters,
        return_type: return_type_id,
        effects,
        body,
    };
    let final_terminator = if let Some(entry) = lowering.mir_entries.first() {
        MirTerminator::Goto { target: *entry }
    } else {
        match terminal.clone() {
            Some(terminal) => terminal,
            _ => MirTerminator::Return {
                source: u32::from(return_register),
            },
        }
    };
    let mut blocks = vec![MirBlock {
        operations: lowering.mir,
        terminator: final_terminator,
    }];
    blocks.extend(lowering.mir_blocks);
    for continuation in lowering.mir_continuations {
        let block = &mut blocks[continuation as usize];
        if matches!(block.terminator, MirTerminator::Unreachable) {
            block.terminator = match terminal.clone() {
                Some(terminal) => terminal,
                _ => MirTerminator::Return {
                    source: u32::from(return_register),
                },
            };
        }
    }
    let mir = MirFunction {
        symbol: info.symbol,
        name: info.lowered.name,
        is_async: info.lowered.is_async,
        temporaries,
        blocks,
        suspensions: lowering.mir_suspensions,
        task_scopes: lowering.mir_task_scopes,
        ownership: lowering.mir_ownership,
    };
    mir.validate_cfg().map_err(|message| {
        Diagnostic::new(
            "E3011",
            format!("invalid generated MIR: {message}"),
            info.lowered.body.span,
        )
    })?;
    Ok((function, hir, mir))
}

pub(super) fn bytecode_module_path(module: &str) -> String {
    let Some(package_path) = module.strip_prefix("pkg://") else {
        return module.to_owned();
    };
    let Some((package, source_path)) = package_path.split_once('/') else {
        return module.to_owned();
    };
    let Some((name, version)) = package.rsplit_once('@') else {
        return module.to_owned();
    };
    let mut components = vec![
        "pkg".to_owned(),
        escape_package_symbol_component(name),
        escape_package_symbol_component(version),
    ];
    let mut source_components = source_path.split('/').collect::<Vec<_>>();
    let Some(file_name) = source_components.pop() else {
        return module.to_owned();
    };
    let Some(file_stem) = file_name.strip_suffix(".allen") else {
        return module.to_owned();
    };
    components.extend(
        source_components
            .into_iter()
            .map(escape_package_symbol_component),
    );
    // The bytecode verifier requires a normalized ASCII path ending in
    // `.allen`. Every canonical URI component is otherwise hex escaped.
    components.push(format!(
        "{}.allen",
        escape_package_symbol_component(file_stem)
    ));
    components.join("/")
}

pub(super) fn escape_package_symbol_component(component: &str) -> String {
    let mut escaped = String::with_capacity(1 + component.len() * 2);
    escaped.push('x');
    for byte in component.bytes() {
        use std::fmt::Write as _;
        write!(&mut escaped, "{byte:02x}").expect("writing into String cannot fail");
    }
    escaped
}
