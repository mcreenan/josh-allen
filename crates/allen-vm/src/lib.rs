#![forbid(unsafe_code)]

mod canonical;

pub use canonical::{
    CanonicalDecodeError, CanonicalEncodeError, decode_canonical, decode_canonical_with_limit,
    encode_canonical, encode_canonical_with_limit,
};

use allen_bytecode::{
    BoolBinaryOp, CapabilityOperation, CheckedIntOperation, CompareOp, Constant, Conversion,
    DebugInfo, EnumPayloadType, EnumTypeId, ExternalFsAccess, FsOperation, FunctionId, Instruction,
    MAX_VALUE_NESTING, NumericBinaryOp, Register, SafeCollectionOperation, StringOperation,
    ValueType, VerifiedArtifact, VerifiedModule, canonical_float_bits,
};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{self, Write as _};
use std::rc::Rc;
use std::time::{Duration, Instant};

const FRAME_BASE_BYTES: u64 = 32;
const REGISTER_BYTES: u64 = 16;
const COLLECTION_BASE_BYTES: u64 = 8;
const VALUE_SLOT_BYTES: u64 = 16;
const MAP_ENTRY_BYTES: u64 = 32;
const FUTURE_BASE_BYTES: u64 = 16;
const TASK_HANDLE_BYTES: u64 = 16;
const SCHEDULER_TASK_BYTES: u64 = 64;

/// Stable resource name for the instruction budget.
pub const RESOURCE_INSTRUCTIONS: &str = "instructions";
/// Stable resource name for cumulative logical allocation.
pub const RESOURCE_ALLOCATION_BYTES: &str = "allocation_bytes";
/// Stable resource name for the largest allowed logical allocation.
pub const RESOURCE_MAXIMUM_ALLOCATION_BYTES: &str = "maximum_allocation_bytes";
/// Stable resource name for call depth.
pub const RESOURCE_CALL_DEPTH: &str = "call_depth";
/// Stable resource name for the wall deadline.
pub const RESOURCE_WALL_TIME: &str = "wall_time";
/// Stable resource name for live scheduler tasks.
pub const RESOURCE_TASKS: &str = "tasks";
/// Stable resource name for concurrently active effects.
pub const RESOURCE_CONCURRENT_EFFECTS: &str = "concurrent_effects";
/// Stable resource name for cleanup instructions.
pub const RESOURCE_CLEANUP_INSTRUCTIONS: &str = "cleanup_instructions";

#[derive(Clone, Copy, Debug)]
pub struct FloatValue(u64);

impl FloatValue {
    #[must_use]
    pub fn new(value: f64) -> Self {
        Self(canonical_float_bits(value.to_bits()))
    }

    #[must_use]
    pub const fn from_canonical_bits(bits: u64) -> Self {
        Self(canonical_float_bits(bits))
    }

    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn as_f64(self) -> f64 {
        f64::from_bits(self.0)
    }
}

impl PartialEq for FloatValue {
    fn eq(&self, other: &Self) -> bool {
        self.as_f64() == other.as_f64()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Bool(bool),
    Float(FloatValue),
    String(Rc<str>),
    Bytes(Rc<[u8]>),
    ExternalFsAccess(ExternalFsAccess),
    Unit,
    List(Rc<[Value]>),
    Map(Rc<[(Value, Value)]>),
    Tuple(Rc<[Value]>),
    Record(Rc<[(Rc<str>, Value)]>),
    Enum(Rc<EnumValue>),
    Closure(Rc<ClosureValue>),
    Future(Rc<FutureValue>),
    Task(TaskValue),
    Workspace(WorkspaceValue),
    SubAgent(SubAgentValue),
    Unknown(Rc<Value>),
}

type RecordValues = Rc<[(Rc<str>, Value)]>;

/// One opaque reference into the current execution's sub-agent handle table.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct SubAgentValue {
    generation: u64,
    index: u32,
    nonce: u64,
}

impl SubAgentValue {
    #[must_use]
    pub const fn new(generation: u64, index: u32, nonce: u64) -> Self {
        Self {
            generation,
            index,
            nonce,
        }
    }

    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    #[must_use]
    pub const fn nonce(self) -> u64 {
        self.nonce
    }
}

impl fmt::Debug for SubAgentValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SubAgentValue(<opaque>)")
    }
}

/// One opaque reference into the current execution's capability table.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct WorkspaceValue {
    generation: u64,
    index: u32,
    nonce: u64,
}

impl WorkspaceValue {
    /// Create one host-issued execution capability reference.
    #[must_use]
    pub const fn new(generation: u64, index: u32, nonce: u64) -> Self {
        Self {
            generation,
            index,
            nonce,
        }
    }

    /// Return the execution generation used for host validation.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// Return the capability-table index used for host validation.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Return the unguessable execution-local validation nonce.
    #[must_use]
    pub const fn nonce(self) -> u64 {
        self.nonce
    }
}

impl fmt::Debug for WorkspaceValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkspaceValue(<opaque>)")
    }
}

/// A lazy async function or external effect recipe.
#[derive(Clone, Debug, PartialEq)]
pub enum FutureValue {
    Function {
        function: FunctionId,
        arguments: Rc<[Value]>,
    },
    Effect {
        operation: FsOperation,
        arguments: Rc<[Value]>,
    },
    Tool {
        tool: u32,
        input: Value,
        result_type: ValueType,
    },
    Agent {
        operation: FsOperation,
        arguments: Rc<[Value]>,
        result_type: ValueType,
    },
}

impl FutureValue {
    fn arguments(&self) -> &[Value] {
        match self {
            Self::Function { arguments, .. }
            | Self::Effect { arguments, .. }
            | Self::Agent { arguments, .. } => arguments,
            Self::Tool { input, .. } => std::slice::from_ref(input),
        }
    }

    fn failure_family(&self) -> Option<EffectFailureFamily> {
        match self {
            Self::Effect { operation, .. } | Self::Agent { operation, .. } => {
                Some(EffectFailureFamily::Operation(*operation))
            }
            Self::Tool { .. } => Some(EffectFailureFamily::Tool),
            Self::Function { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum EffectFailureFamily {
    Operation(FsOperation),
    Tool,
}

/// The affine language handle for one scheduler task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskValue {
    pub id: u64,
}

/// VM-owned identity for one provider operation that has not completed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PendingEffectId(pub u64);

/// The nonblocking state returned by an effect provider.
#[derive(Clone, Debug, PartialEq)]
pub enum EffectPoll {
    Ready(Value),
    Pending,
}

/// A callable function together with its immutable captured values.
#[derive(Clone, Debug)]
pub struct ClosureValue {
    pub function: FunctionId,
    pub captures: Rc<[Value]>,
}

impl PartialEq for ClosureValue {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

/// The stable identity of a tagged value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnumIdentity {
    User(EnumTypeId),
    Option,
    Result,
}

/// The payload held by an enum variant.
#[derive(Clone, Debug, PartialEq)]
pub enum EnumPayload {
    Unit,
    Tuple(Rc<[Value]>),
    Record(Rc<[(Rc<str>, Value)]>),
}

/// A nominal or built-in enum value.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumValue {
    pub identity: EnumIdentity,
    pub type_name: Rc<str>,
    pub variant: u32,
    pub variant_name: Rc<str>,
    pub payload: EnumPayload,
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(value) => write!(formatter, "{value}"),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::Float(value) => write_float(formatter, *value),
            Self::String(value) => write_quoted_string(formatter, value),
            Self::Bytes(value) => write_bytes(formatter, value),
            Self::ExternalFsAccess(value) => write!(formatter, "ExternalFsAccess.{value:?}"),
            Self::Unit => formatter.write_str("()"),
            Self::List(values) => write_sequence(formatter, "[", "]", values, false),
            Self::Map(entries) => {
                formatter.write_str("map {")?;
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{key}: {value}")?;
                }
                formatter.write_str("}")
            }
            Self::Tuple(values) => write_sequence(formatter, "(", ")", values, true),
            Self::Record(fields) => write_record(formatter, fields),
            Self::Enum(value) => write_enum(formatter, value),
            Self::Closure(_) => formatter.write_str("<function>"),
            Self::Future(_) => formatter.write_str("<future>"),
            Self::Task(task) => write!(formatter, "<task:{}>", task.id),
            Self::Workspace(_) => formatter.write_str("<workspace>"),
            Self::SubAgent(_) => formatter.write_str("<sub-agent>"),
            Self::Unknown(value) => write!(formatter, "unknown({value})"),
        }
    }
}

fn write_record(formatter: &mut fmt::Formatter<'_>, fields: &[(Rc<str>, Value)]) -> fmt::Result {
    formatter.write_char('{')?;
    for (index, (name, value)) in fields.iter().enumerate() {
        if index > 0 {
            formatter.write_str(", ")?;
        }
        write!(formatter, "{name}: {value}")?;
    }
    formatter.write_char('}')
}

fn write_enum(formatter: &mut fmt::Formatter<'_>, value: &EnumValue) -> fmt::Result {
    match value.identity {
        EnumIdentity::Option if value.variant == 0 => return formatter.write_str("None"),
        EnumIdentity::Option if value.variant == 1 => {
            let EnumPayload::Tuple(payload) = &value.payload else {
                return Err(fmt::Error);
            };
            return write!(formatter, "Some({})", payload.first().ok_or(fmt::Error)?);
        }
        EnumIdentity::Result if value.variant == 0 => {
            let EnumPayload::Tuple(payload) = &value.payload else {
                return Err(fmt::Error);
            };
            return write!(formatter, "Ok({})", payload.first().ok_or(fmt::Error)?);
        }
        EnumIdentity::Result if value.variant == 1 => {
            let EnumPayload::Tuple(payload) = &value.payload else {
                return Err(fmt::Error);
            };
            return write!(formatter, "Err({})", payload.first().ok_or(fmt::Error)?);
        }
        _ => {}
    }

    let display_type = value
        .type_name
        .rsplit_once("::")
        .map_or(value.type_name.as_ref(), |(_, name)| name);
    write!(formatter, "{display_type}.{}", value.variant_name)?;
    match &value.payload {
        EnumPayload::Unit => Ok(()),
        EnumPayload::Tuple(payload) => write_sequence(formatter, "(", ")", payload, false),
        EnumPayload::Record(fields) => {
            formatter.write_char(' ')?;
            write_record(formatter, fields)
        }
    }
}

fn write_quoted_string(formatter: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    formatter.write_char('"')?;
    for character in value.chars() {
        for escaped in character.escape_default() {
            formatter.write_char(escaped)?;
        }
    }
    formatter.write_char('"')
}

fn write_bytes(formatter: &mut fmt::Formatter<'_>, value: &[u8]) -> fmt::Result {
    formatter.write_str("b\"")?;
    for byte in value {
        match *byte {
            b'\\' => formatter.write_str("\\\\")?,
            b'"' => formatter.write_str("\\\"")?,
            b'\n' => formatter.write_str("\\n")?,
            b'\r' => formatter.write_str("\\r")?,
            b'\t' => formatter.write_str("\\t")?,
            0x20..=0x7e => formatter.write_char(char::from(*byte))?,
            _ => write!(formatter, "\\x{byte:02X}")?,
        }
    }
    formatter.write_char('"')
}

fn write_sequence(
    formatter: &mut fmt::Formatter<'_>,
    open: &str,
    close: &str,
    values: &[Value],
    tuple: bool,
) -> fmt::Result {
    formatter.write_str(open)?;
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            formatter.write_str(", ")?;
        }
        write!(formatter, "{value}")?;
    }
    if tuple && values.len() == 1 {
        formatter.write_char(',')?;
    }
    formatter.write_str(close)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionLimits {
    pub instructions: u64,
    pub allocation_bytes: u64,
    pub maximum_allocation_bytes: u64,
    pub call_depth: u32,
    pub wall_time: Duration,
    pub tasks: u32,
    pub concurrent_effects: u32,
    pub cleanup_instructions: u64,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            instructions: u64::MAX,
            allocation_bytes: u64::MAX,
            maximum_allocation_bytes: u64::MAX,
            call_depth: u32::MAX,
            wall_time: Duration::MAX,
            tasks: u32::MAX,
            concurrent_effects: u32::MAX,
            cleanup_instructions: u64::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExecutionUsage {
    pub instructions: u64,
    pub allocation_bytes: u64,
    pub maximum_call_depth: u32,
    pub tasks_started: u64,
    pub maximum_live_tasks: u32,
    pub maximum_concurrent_effects: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionResult {
    pub value: Value,
    pub usage: ExecutionUsage,
}

/// Immutable canonical manifest authority names visible to capability inspection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionCapabilities {
    grants: BTreeSet<String>,
}

impl ExecutionCapabilities {
    /// Freeze only standard manifest grantable authority names in canonical order.
    #[must_use]
    pub fn new(grants: impl IntoIterator<Item = String>) -> Self {
        Self {
            grants: grants
                .into_iter()
                .filter(|grant| is_manifest_grantable_capability(grant))
                .collect(),
        }
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.grants.contains(name)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> {
        self.grants.iter().map(String::as_str)
    }
}

fn is_manifest_grantable_capability(name: &str) -> bool {
    matches!(
        name,
        "fs.read" | "fs.write" | "net.http_get" | "permission.request_external_fs"
    )
}

/// A successful terminal language outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionOutcome {
    Completed(ExecutionResult),
    Stopped {
        reason: String,
        usage: ExecutionUsage,
        cleanup_failure: Option<&'static str>,
    },
}

/// Exact terminal channel reported to execution-scoped effect providers.
///
/// Live providers use the default no-op finalizer. Replay providers use this
/// hook to prove that every journal entry was consumed and that the recorded
/// final channel matches the execution that actually occurred.
#[derive(Clone, Copy, Debug)]
pub enum EffectExecutionOutcome<'outcome> {
    Completed,
    Stopped { reason: &'outcome str },
    Terminal { error: &'outcome VmError },
    RuntimePanic,
}

/// Immutable execution identity supplied before a replay provider can run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectExecutionBinding {
    pub bytecode_version: u16,
    pub artifact_digest: [u8; 32],
    pub contract_digest: [u8; 32],
    pub language_digest: [u8; 32],
    pub runtime_digest: [u8; 32],
    pub policy_digest: [u8; 32],
    pub catalog_digest: [u8; 32],
    pub capability_digest: [u8; 32],
    pub error_registry_digest: [u8; 32],
    pub effective_manifest_grants: Vec<String>,
}

/// Supplies cooperative external cancellation at instruction boundaries.
pub trait CancellationSource {
    fn is_cancelled(&mut self) -> bool;
}

/// Supplies execution-scoped capabilities and verified external-effect results.
///
/// Implementations validate opaque handles and return an exact value for the
/// operation's independently verified result type.
pub trait EffectProvider {
    /// Whether every external result for this execution is supplied by a
    /// replay journal rather than a live host provider.
    ///
    /// Live providers retain the conservative default. Hosts use this signal
    /// only for execution provenance; it must not change language behavior.
    fn is_replayed(&self) -> bool {
        false
    }

    /// Check the supervisor-derived execution identity before any VM work.
    ///
    /// # Errors
    ///
    /// Replay providers return [`VmError::ReplayDiverged`] on any mismatch.
    fn bind_execution(&mut self, _binding: &EffectExecutionBinding) -> Result<(), VmError> {
        Ok(())
    }

    /// Return the opaque workspace reference for this execution.
    ///
    /// # Errors
    ///
    /// Returns a stable VM error when no workspace capability is available.
    fn workspace(&mut self) -> Result<WorkspaceValue, VmError>;

    /// Execute one validated host effect.
    ///
    /// # Errors
    ///
    /// Returns a stable VM error when the capability is unavailable or the
    /// provider cannot complete the operation.
    fn call(&mut self, operation: FsOperation, arguments: &[Value]) -> Result<Value, VmError>;

    /// Start one ordinary effect. Synchronous providers use the default-ready
    /// behavior; protocol adapters may return [`EffectPoll::Pending`].
    ///
    /// # Errors
    ///
    /// Returns the provider's stable effect error.
    fn start_call(
        &mut self,
        _pending: PendingEffectId,
        operation: FsOperation,
        arguments: &[Value],
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        self.call(operation, arguments).map(EffectPoll::Ready)
    }

    /// Start one typed tool operation, optionally without blocking the VM.
    ///
    /// # Errors
    ///
    /// Returns the provider's stable tool error.
    fn start_tool(
        &mut self,
        _pending: PendingEffectId,
        _tool: u32,
        _input: &Value,
        result_type: &ValueType,
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        unavailable_result(
            result_type,
            "tool.unavailable",
            "tool provider is unavailable",
        )
        .map(EffectPoll::Ready)
    }

    /// Execute one validated agent, model, or user-response operation.
    ///
    /// A missing provider is detected only when the lazy external future is polled.
    ///
    /// # Errors
    ///
    /// Returns a stable agent, validation, cancellation, or timeout error.
    fn agent(
        &mut self,
        operation: FsOperation,
        _arguments: &[Value],
        _result_type: &ValueType,
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<Value, VmError> {
        Err(match operation {
            FsOperation::ModelRequest => VmError::ModelUnavailable,
            FsOperation::UserAsk => VmError::UserUnavailable,
            FsOperation::SubAgentCreate
            | FsOperation::SubAgentRun
            | FsOperation::SubAgentMessage
            | FsOperation::SubAgentAsk => VmError::SubAgentUnavailable,
            _ => VmError::AgentUnavailable,
        })
    }

    /// Start one agent, model, or user-response operation, optionally without blocking the VM.
    ///
    /// # Errors
    ///
    /// Returns a stable agent, validation, cancellation, or timeout error.
    fn start_agent(
        &mut self,
        _pending: PendingEffectId,
        operation: FsOperation,
        arguments: &[Value],
        result_type: &ValueType,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        self.agent(operation, arguments, result_type, cancellation)
            .map(EffectPoll::Ready)
    }

    /// Start one sub-agent provider operation. This separate hook keeps child
    /// provider routing independent from the invoking-agent provider.
    ///
    /// # Errors
    ///
    /// Returns a stable child-provider or validation error.
    fn start_sub_agent(
        &mut self,
        _pending: PendingEffectId,
        _operation: FsOperation,
        _arguments: &[Value],
        _result_type: &ValueType,
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        Err(VmError::SubAgentUnavailable)
    }

    /// Poll a provider operation that previously returned pending.
    ///
    /// # Errors
    ///
    /// Returns the pending operation's stable provider error.
    fn poll_effect(
        &mut self,
        _pending: PendingEffectId,
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        Err(VmError::Invariant("pending effect is not supported"))
    }

    /// Cancel one provider operation that previously returned pending.
    fn cancel_effect(&mut self, _pending: PendingEffectId) {}

    /// Cancel every issued provider operation that is still pending.
    ///
    /// The VM calls this before it produces a terminal cancellation or stopped
    /// outcome. Providers may detach work; any later value is discarded.
    fn cancel_pending(&mut self) {}

    /// Validate provider-owned end-of-execution state at the supervisor
    /// boundary. Live providers need no final validation and retain this no-op.
    ///
    /// # Errors
    ///
    /// Replay providers return an in-execution replay-divergence trap for
    /// leftovers or a mismatched final channel.
    fn finish_execution(&mut self, _outcome: EffectExecutionOutcome<'_>) -> Result<(), VmError> {
        Ok(())
    }
}

#[derive(Default)]
struct NoEffects;

impl EffectProvider for NoEffects {
    fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
        Err(VmError::CapabilityMissing)
    }

    fn call(&mut self, operation: FsOperation, _arguments: &[Value]) -> Result<Value, VmError> {
        if matches!(
            operation,
            FsOperation::PermissionRequestFile | FsOperation::PermissionRequestDirectory
        ) {
            Err(VmError::AgentUnavailable)
        } else {
            Err(VmError::CapabilityMissing)
        }
    }
}

#[derive(Default)]
struct NeverCancelled;

impl CancellationSource for NeverCancelled {
    fn is_cancelled(&mut self) -> bool {
        false
    }
}

/// Reusable accounting for concurrent host effects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConcurrentEffectCounter {
    limit: u32,
    active: u32,
    maximum: u32,
}

impl ConcurrentEffectCounter {
    #[must_use]
    pub const fn new(limit: u32) -> Self {
        Self {
            limit,
            active: 0,
            maximum: 0,
        }
    }

    /// Charge one effect before it starts.
    ///
    /// # Errors
    ///
    /// Returns a stable resource error without changing the counter when the
    /// new effect would exceed the limit.
    pub fn start(&mut self) -> Result<(), VmError> {
        let next = self.active.checked_add(1).ok_or(VmError::ResourceLimit {
            resource: RESOURCE_CONCURRENT_EFFECTS,
        })?;
        if next > self.limit {
            return Err(VmError::ResourceLimit {
                resource: RESOURCE_CONCURRENT_EFFECTS,
            });
        }
        self.active = next;
        self.maximum = self.maximum.max(next);
        Ok(())
    }

    /// Release one active effect after its result is accepted or discarded.
    ///
    /// # Errors
    ///
    /// Returns an invariant error when no effect is active.
    pub fn complete(&mut self) -> Result<(), VmError> {
        self.active = self
            .active
            .checked_sub(1)
            .ok_or(VmError::Invariant("effect counter is already empty"))?;
        Ok(())
    }

    #[must_use]
    pub const fn active(self) -> u32 {
        self.active
    }

    #[must_use]
    pub const fn maximum(self) -> u32 {
        self.maximum
    }
}

/// A deterministic instruction boundary reported before the instruction runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Checkpoint {
    pub function: FunctionId,
    pub instruction: u32,
}

/// One deterministic warning that a finite execution resource is near its limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetWarning {
    pub resource: &'static str,
    pub used: u64,
    pub limit: u64,
}

/// Receives synchronous instruction-boundary observations.
///
/// An observer cannot suspend execution or create a task through this API.
pub trait CheckpointObserver {
    fn checkpoint(&mut self, checkpoint: Checkpoint);

    /// Receive the immutable provenance of the execution's effect provider.
    ///
    /// This is delivered before the first instruction so hosts can mark every
    /// subsequent execution event consistently. The VM does not permit the
    /// provider to change provenance during an execution.
    fn execution_effect_provenance(&mut self, _replayed: bool) {}

    /// Receive one deterministic task lifecycle transition.
    fn task_event(&mut self, _event: TaskEvent) {}

    /// Receive the first near-limit charge for one finite resource.
    fn budget_warning(&mut self, _warning: BudgetWarning) {}
}

/// One safe lifecycle transition for a scheduler task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskEvent {
    pub sequence: u64,
    pub task_id: u64,
    pub owner_id: u64,
    pub kind: TaskEventKind,
}

/// Stable lifecycle kinds exposed to host diagnostic adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskEventKind {
    Spawned,
    Waiting,
    Ready,
    Completed,
    Failed,
    Cancelled,
    Stopped,
}

impl fmt::Display for TaskEventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Spawned => "spawned",
            Self::Waiting => "waiting",
            Self::Ready => "ready",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Stopped => "stopped",
        })
    }
}

/// Supplies a monotonic duration from an implementation-defined epoch.
pub trait MonotonicClock {
    fn now(&mut self) -> Duration;
}

/// A monotonic clock backed by the process clock.
#[derive(Debug)]
pub struct SystemMonotonicClock {
    epoch: Instant,
}

impl SystemMonotonicClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Default for SystemMonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for SystemMonotonicClock {
    fn now(&mut self) -> Duration {
        self.epoch.elapsed()
    }
}

#[derive(Default)]
struct IgnoreCheckpoints;

impl CheckpointObserver for IgnoreCheckpoints {
    fn checkpoint(&mut self, _checkpoint: Checkpoint) {}
}

/// A stable bytecode location in a runtime stack trace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackFrame {
    pub function: FunctionId,
    pub function_name: String,
    pub instruction: u32,
    pub source: Option<SourceLocation>,
}

/// A normalized source location attached by optional debug data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLocation {
    pub module_path: String,
    pub start: u32,
    pub end: u32,
}

/// A VM failure together with innermost-to-outermost call frames.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionError {
    pub error: VmError,
    pub frames: Vec<StackFrame>,
}

impl ExecutionError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.error.code()
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.error)?;
        for frame in &self.frames {
            write!(
                formatter,
                "\n  at {} [function {}, instruction {}]",
                frame.function_name, frame.function, frame.instruction
            )?;
            if let Some(source) = &frame.source {
                write!(
                    formatter,
                    " ({}:{}..{})",
                    source.module_path, source.start, source.end
                )?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for ExecutionError {}

/// Resolves optional source information for verified bytecode locations.
pub trait DebugSourceMap {
    fn source_location(&self, function: FunctionId, instruction: u32) -> Option<SourceLocation>;
}

impl DebugSourceMap for DebugInfo {
    fn source_location(&self, function: FunctionId, instruction: u32) -> Option<SourceLocation> {
        let index = self
            .locations
            .binary_search_by_key(&(function, instruction), |location| {
                (location.function, location.instruction)
            })
            .ok()?;
        let location = &self.locations[index];
        let module_path = self.sources.get(location.source as usize)?.clone();
        Some(SourceLocation {
            module_path,
            start: location.start,
            end: location.end,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveFrame {
    function: FunctionId,
    instruction: u32,
}

#[derive(Clone, Debug)]
struct MachineFrame {
    function: FunctionId,
    registers: Vec<Option<Value>>,
    program_counter: usize,
    activation: u32,
    active_scopes: Vec<u32>,
    continuation: Option<u16>,
}

impl MachineFrame {
    fn active(&self) -> ActiveFrame {
        ActiveFrame {
            function: self.function,
            instruction: u32::try_from(self.program_counter.saturating_sub(1)).unwrap_or(u32::MAX),
        }
    }
}

#[derive(Clone, Debug)]
enum MachineTaskState {
    Ready,
    Waiting(WaitState),
    Completed(Result<Value, TaskFailure>),
}

#[derive(Clone, Debug)]
enum WaitState {
    Task {
        handle: u64,
        destination: u16,
    },
    Effect {
        pending: PendingEffectId,
        destination: Option<u16>,
        result_type: ValueType,
        failure_family: EffectFailureFamily,
    },
    Scope {
        scope: ScopeKey,
    },
    Return {
        scope: ScopeKey,
        value: Value,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScopeKey {
    activation: u32,
    lexical: u32,
}

#[derive(Clone, Debug)]
struct MachineTask {
    owner: u64,
    scope: Option<ScopeKey>,
    entry_function: FunctionId,
    frames: Vec<MachineFrame>,
    state: MachineTaskState,
}

fn task_handles(value: &Value, handles: &mut Vec<u64>) {
    match value {
        Value::Task(handle) => handles.push(handle.id),
        Value::Future(future) => {
            for argument in future.arguments() {
                task_handles(argument, handles);
            }
        }
        _ => {}
    }
}

#[derive(Default)]
struct TaskMachine {
    next_task: u64,
    next_effect: u64,
    next_event: u64,
    rotation_cursor: Option<u64>,
    terminal_cleanup_failure: Option<&'static str>,
    cleanup_limit: u64,
    cleanup_remaining: u64,
    cleanup_warning_emitted: bool,
    providers_cancelled: bool,
    tasks: BTreeMap<u64, MachineTask>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VmError {
    ArithmeticOverflow,
    DivisionByZero,
    IndexOutOfBounds,
    MapKeyNotFound,
    DuplicateMapKey,
    ResourceLimit { resource: &'static str },
    Timeout { resource: &'static str },
    Cancelled,
    CapabilityMissing,
    AgentUnavailable,
    AgentResponseSchema,
    ModelUnavailable,
    ModelValidationError,
    UserUnavailable,
    SubAgentUnavailable,
    SubAgentResponseSchema,
    ReplayDiverged,
    ReplayRuntimeDiverged,
    ResponseValidationError,
    ToolUnavailable,
    ToolSchemaError,
    ProtocolViolation,
    Stopped { reason: String },
    Invariant(&'static str),
}

impl VmError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ArithmeticOverflow => "arithmetic.overflow",
            Self::DivisionByZero => "arithmetic.division_by_zero",
            Self::IndexOutOfBounds => "index.out_of_bounds",
            Self::MapKeyNotFound => "map.key_not_found",
            Self::DuplicateMapKey => "map.duplicate_key",
            Self::ResourceLimit { .. } => "resource.limit",
            Self::Timeout { .. } => "runtime.timeout",
            Self::Cancelled => "runtime.cancelled",
            Self::CapabilityMissing | Self::ProtocolViolation => "protocol.violation",
            Self::AgentUnavailable => "agent.unavailable",
            Self::AgentResponseSchema => "agent.validation_failed",
            Self::ModelUnavailable => "model.unavailable",
            Self::ModelValidationError => "model.validation_failed",
            Self::UserUnavailable => "user.unavailable",
            Self::SubAgentUnavailable => "sub_agent.unavailable",
            Self::SubAgentResponseSchema => "sub_agent.validation_failed",
            Self::ReplayDiverged => "replay.diverged",
            Self::ReplayRuntimeDiverged => "replay.runtime_diverged",
            Self::ResponseValidationError => "user.validation_failed",
            Self::ToolUnavailable => "tool.unavailable",
            Self::ToolSchemaError => "tool.schema",
            Self::Stopped { .. } => "stopped",
            Self::Invariant(_) => "runtime.panic",
        }
    }
}

impl fmt::Display for VmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArithmeticOverflow => formatter.write_str("integer arithmetic overflowed"),
            Self::DivisionByZero => formatter.write_str("integer division by zero"),
            Self::IndexOutOfBounds => formatter.write_str("index is out of bounds"),
            Self::MapKeyNotFound => formatter.write_str("map key was not found"),
            Self::DuplicateMapKey => formatter.write_str("map contains a duplicate key"),
            Self::ResourceLimit { resource } => {
                write!(formatter, "resource limit exceeded: {resource}")
            }
            Self::Timeout { resource } => write!(formatter, "deadline expired: {resource}"),
            Self::Cancelled => formatter.write_str("execution was cancelled"),
            Self::CapabilityMissing => formatter.write_str("required capability is unavailable"),
            Self::AgentUnavailable => {
                formatter.write_str("an external decision agent is unavailable")
            }
            Self::AgentResponseSchema => {
                formatter.write_str("the invoking agent returned an invalid response")
            }
            Self::ModelUnavailable => formatter.write_str("the model provider is unavailable"),
            Self::ModelValidationError => {
                formatter.write_str("the model response does not match its schema")
            }
            Self::UserUnavailable => formatter.write_str("the user provider is unavailable"),
            Self::SubAgentUnavailable => {
                formatter.write_str("the sub-agent provider is unavailable")
            }
            Self::SubAgentResponseSchema => {
                formatter.write_str("the sub-agent response did not match its schema")
            }
            Self::ReplayDiverged => formatter.write_str("effect replay diverged"),
            Self::ReplayRuntimeDiverged => {
                formatter.write_str("effect replay diverged during execution")
            }
            Self::ResponseValidationError => {
                formatter.write_str("the typed response does not match its schema")
            }
            Self::ToolUnavailable => formatter.write_str("the selected tool is unavailable"),
            Self::ToolSchemaError => {
                formatter.write_str("the tool value does not match its schema")
            }
            Self::ProtocolViolation => {
                formatter.write_str("the external provider violated the execution protocol")
            }
            Self::Stopped { reason } => write!(formatter, "execution stopped: {reason}"),
            Self::Invariant(message) => {
                write!(formatter, "verified bytecode invariant failed: {message}")
            }
        }
    }
}

impl std::error::Error for VmError {}

#[derive(Clone, Debug)]
struct TaskFailure {
    error: VmError,
    stack: Vec<ActiveFrame>,
}

/// Execute the entry function of a verified module with unrestricted local limits.
///
/// # Errors
///
/// Returns a stable language error or an internal invariant error.
pub fn execute(module: &VerifiedModule) -> Result<Value, VmError> {
    execute_with_limits(module, ExecutionLimits::default()).map(|result| result.value)
}

/// Execute a verified artifact and preserve its optional debug locations.
///
/// # Errors
///
/// Returns a stable VM error and an innermost-to-outermost stack trace.
pub fn execute_verified_artifact(
    artifact: &VerifiedArtifact,
) -> Result<ExecutionResult, ExecutionError> {
    let mut clock = SystemMonotonicClock::new();
    let mut observer = IgnoreCheckpoints;
    execute_verified_artifact_with_context(
        artifact,
        ExecutionLimits::default(),
        &mut clock,
        &mut observer,
    )
}

/// Execute a verified artifact with explicit limits and system providers.
///
/// # Errors
///
/// Returns a stable VM error and an innermost-to-outermost stack trace.
pub fn execute_verified_artifact_with_limits(
    artifact: &VerifiedArtifact,
    limits: ExecutionLimits,
) -> Result<ExecutionResult, ExecutionError> {
    let mut clock = SystemMonotonicClock::new();
    let mut observer = IgnoreCheckpoints;
    execute_verified_artifact_with_context(artifact, limits, &mut clock, &mut observer)
}

/// Execute a verified artifact with deterministic time and checkpoint providers.
///
/// # Errors
///
/// Returns a stable VM error and an innermost-to-outermost stack trace.
pub fn execute_verified_artifact_with_context(
    artifact: &VerifiedArtifact,
    limits: ExecutionLimits,
    clock: &mut dyn MonotonicClock,
    observer: &mut dyn CheckpointObserver,
) -> Result<ExecutionResult, ExecutionError> {
    let mut cancellation = NeverCancelled;
    let mut effects = NoEffects;
    match execute_entry_with_capabilities_and_runtime_context(
        artifact.verified_module(),
        artifact.debug().map(|debug| debug as &dyn DebugSourceMap),
        artifact.verified_module().module().entry,
        &[],
        limits,
        clock,
        observer,
        &mut cancellation,
        &mut effects,
        &ExecutionCapabilities::default(),
    )? {
        ExecutionOutcome::Completed(result) => Ok(result),
        ExecutionOutcome::Stopped { reason, .. } => Err(ExecutionError {
            error: VmError::Stopped { reason },
            frames: Vec::new(),
        }),
    }
}

/// Execute a verified artifact and preserve a host-neutral stopped outcome.
///
/// # Errors
///
/// Returns a stable runtime failure with source-aware stack frames.
pub fn execute_verified_artifact_outcome_with_limits(
    artifact: &VerifiedArtifact,
    limits: ExecutionLimits,
) -> Result<ExecutionOutcome, ExecutionError> {
    let mut clock = SystemMonotonicClock::new();
    let mut observer = IgnoreCheckpoints;
    let mut cancellation = NeverCancelled;
    let mut effects = NoEffects;
    execute_entry_with_capabilities_and_runtime_context(
        artifact.verified_module(),
        artifact.debug().map(|debug| debug as &dyn DebugSourceMap),
        artifact.verified_module().module().entry,
        &[],
        limits,
        &mut clock,
        &mut observer,
        &mut cancellation,
        &mut effects,
        &ExecutionCapabilities::default(),
    )
}

/// Execute a verified artifact and report deterministic task lifecycle events.
///
/// # Errors
///
/// Returns a stable runtime failure with source-aware stack frames.
pub fn execute_verified_artifact_outcome_with_observer(
    artifact: &VerifiedArtifact,
    limits: ExecutionLimits,
    observer: &mut dyn CheckpointObserver,
) -> Result<ExecutionOutcome, ExecutionError> {
    let mut clock = SystemMonotonicClock::new();
    let mut cancellation = NeverCancelled;
    let mut effects = NoEffects;
    execute_entry_with_capabilities_and_runtime_context(
        artifact.verified_module(),
        artifact.debug().map(|debug| debug as &dyn DebugSourceMap),
        artifact.verified_module().module().entry,
        &[],
        limits,
        &mut clock,
        observer,
        &mut cancellation,
        &mut effects,
        &ExecutionCapabilities::default(),
    )
}

/// Execute the entry function and enforce deterministic accounting limits.
///
/// Each instruction, logical allocation, and frame depth is charged before the
/// corresponding operation.
///
/// # Errors
///
/// Returns `resource.limit` before an operation whose charge exceeds a limit.
#[allow(clippy::too_many_lines)]
pub fn execute_with_limits(
    module: &VerifiedModule,
    limits: ExecutionLimits,
) -> Result<ExecutionResult, VmError> {
    let mut clock = SystemMonotonicClock::new();
    let mut observer = IgnoreCheckpoints;
    execute_with_context(module, None, limits, &mut clock, &mut observer)
        .map_err(|failure| failure.error)
}

/// Execute verified bytecode with deterministic time and checkpoint providers.
///
/// The optional source map can add source locations, but it cannot change
/// executable meaning. Checkpoints run synchronously before each instruction.
///
/// # Errors
///
/// Returns a stable VM error and an innermost-to-outermost stack trace.
pub fn execute_with_context(
    module: &VerifiedModule,
    debug: Option<&dyn DebugSourceMap>,
    limits: ExecutionLimits,
    clock: &mut dyn MonotonicClock,
    observer: &mut dyn CheckpointObserver,
) -> Result<ExecutionResult, ExecutionError> {
    let mut cancellation = NeverCancelled;
    match execute_with_runtime_context(module, debug, limits, clock, observer, &mut cancellation)? {
        ExecutionOutcome::Completed(result) => Ok(result),
        ExecutionOutcome::Stopped { reason, .. } => Err(ExecutionError {
            error: VmError::Stopped { reason },
            frames: Vec::new(),
        }),
    }
}

/// Execute verified bytecode with deterministic time, checkpoint, and
/// cancellation providers while preserving terminal stopped outcomes.
///
/// # Errors
///
/// Returns a stable VM failure and innermost-to-outermost stack trace.
pub fn execute_with_runtime_context(
    module: &VerifiedModule,
    debug: Option<&dyn DebugSourceMap>,
    limits: ExecutionLimits,
    clock: &mut dyn MonotonicClock,
    observer: &mut dyn CheckpointObserver,
    cancellation: &mut dyn CancellationSource,
) -> Result<ExecutionOutcome, ExecutionError> {
    let mut effects = NoEffects;
    execute_entry_with_runtime_context(
        module,
        debug,
        module.module().entry,
        &[],
        limits,
        clock,
        observer,
        cancellation,
        &mut effects,
    )
}

/// Execute one verified function with explicit entry arguments and one
/// execution-scoped effect provider.
///
/// # Errors
///
/// Returns a stable VM failure and innermost-to-outermost stack trace.
#[allow(clippy::too_many_arguments)]
pub fn execute_entry_with_runtime_context(
    module: &VerifiedModule,
    debug: Option<&dyn DebugSourceMap>,
    entry: FunctionId,
    arguments: &[Value],
    limits: ExecutionLimits,
    clock: &mut dyn MonotonicClock,
    observer: &mut dyn CheckpointObserver,
    cancellation: &mut dyn CancellationSource,
    effects: &mut dyn EffectProvider,
) -> Result<ExecutionOutcome, ExecutionError> {
    execute_entry_with_capabilities_and_runtime_context(
        module,
        debug,
        entry,
        arguments,
        limits,
        clock,
        observer,
        cancellation,
        effects,
        &ExecutionCapabilities::default(),
    )
}

/// Execute one verified function with an immutable manifest-grant snapshot.
///
/// The snapshot is local VM state. It is shared by every child task and is not
/// read from, or replaced with, the external effect provider.
///
/// # Errors
///
/// Returns a stable VM failure and innermost-to-outermost stack trace.
#[allow(clippy::too_many_arguments)]
pub fn execute_entry_with_capabilities_and_runtime_context(
    module: &VerifiedModule,
    debug: Option<&dyn DebugSourceMap>,
    entry: FunctionId,
    arguments: &[Value],
    limits: ExecutionLimits,
    clock: &mut dyn MonotonicClock,
    observer: &mut dyn CheckpointObserver,
    cancellation: &mut dyn CancellationSource,
    effects: &mut dyn EffectProvider,
    capabilities: &ExecutionCapabilities,
) -> Result<ExecutionOutcome, ExecutionError> {
    observer.execution_effect_provenance(effects.is_replayed());
    let raw_module = module.module();
    let mut budget = Budget::new(limits, clock, observer, cancellation);
    execute_task_machine(
        raw_module,
        debug,
        entry,
        arguments,
        limits,
        &mut budget,
        effects,
        capabilities,
    )
}

fn new_machine_frame(
    raw_module: &allen_bytecode::Module,
    function_id: FunctionId,
    arguments: &[Value],
    captures: &[Value],
    continuation: Option<u16>,
    depth: u32,
    budget: &mut Budget<'_>,
) -> Result<MachineFrame, VmError> {
    let function = raw_module
        .functions
        .get(function_id as usize)
        .ok_or(VmError::Invariant("function is missing"))?;
    if arguments.len() != function.parameters.len() || captures.len() != function.captures.len() {
        return Err(VmError::Invariant("function input count is invalid"));
    }
    budget.enter_frame(depth)?;
    budget.charge_allocation(frame_size(function.registers.len())?)?;
    let mut registers = vec![None; function.registers.len()];
    for (register, value) in function.parameters.iter().zip(arguments) {
        write_register(&mut registers, *register, value.clone())?;
    }
    for (register, value) in function.captures.iter().zip(captures) {
        write_register(&mut registers, *register, value.clone())?;
    }
    Ok(MachineFrame {
        function: function_id,
        registers,
        program_counter: 0,
        activation: depth,
        active_scopes: Vec::new(),
        continuation,
    })
}

fn machine_failure(task: &MachineTask, error: VmError) -> TaskFailure {
    TaskFailure {
        error,
        stack: task.frames.iter().rev().map(MachineFrame::active).collect(),
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn execute_task_machine(
    raw_module: &allen_bytecode::Module,
    debug: Option<&dyn DebugSourceMap>,
    entry: FunctionId,
    arguments: &[Value],
    limits: ExecutionLimits,
    budget: &mut Budget<'_>,
    effects: &mut dyn EffectProvider,
    capabilities: &ExecutionCapabilities,
) -> Result<ExecutionOutcome, ExecutionError> {
    let root = match new_machine_frame(raw_module, entry, arguments, &[], None, 1, budget) {
        Ok(frame) => frame,
        Err(error) => {
            return Err(ExecutionError {
                error,
                frames: Vec::new(),
            });
        }
    };
    let mut machine = TaskMachine {
        cleanup_limit: limits.cleanup_instructions,
        cleanup_remaining: limits.cleanup_instructions,
        ..TaskMachine::default()
    };
    machine.tasks.insert(
        0,
        MachineTask {
            owner: 0,
            scope: None,
            entry_function: entry,
            frames: vec![root],
            state: MachineTaskState::Ready,
        },
    );
    machine.emit_event(budget.observer, 0, 0, TaskEventKind::Spawned);

    loop {
        machine.wake_waiters(raw_module, budget, effects);
        if matches!(
            machine.tasks.get(&0).map(|task| &task.state),
            Some(MachineTaskState::Completed(Err(_)))
        ) {
            machine.cancel_pending_effects(budget, effects);
            machine.cancel_effects(effects);
            let cleanup_failure = machine.cancel_all(budget.observer);
            machine.terminal_cleanup_failure = machine.terminal_cleanup_failure.or(cleanup_failure);
        }
        let Some(task_id) = machine.next_ready() else {
            let root_state = machine
                .tasks
                .get(&0)
                .expect("root task exists")
                .state
                .clone();
            match root_state {
                MachineTaskState::Completed(Ok(value)) => {
                    if machine.tasks.len() != 1 {
                        machine.cancel_pending_effects(budget, effects);
                        machine.cancel_effects(effects);
                        let cleanup = machine.cancel_all(budget.observer);
                        return Err(ExecutionError {
                            error: cleanup.map_or(
                                VmError::Invariant("entry returned with live tasks"),
                                |_| VmError::ResourceLimit {
                                    resource: RESOURCE_CLEANUP_INSTRUCTIONS,
                                },
                            ),
                            frames: Vec::new(),
                        });
                    }
                    return Ok(ExecutionOutcome::Completed(ExecutionResult {
                        value,
                        usage: budget.usage,
                    }));
                }
                MachineTaskState::Completed(Err(failure)) => {
                    if let VmError::Stopped { reason } = failure.error {
                        machine.cancel_pending_effects(budget, effects);
                        machine.cancel_effects(effects);
                        let cleanup_failure = machine
                            .terminal_cleanup_failure
                            .or_else(|| machine.cancel_all(budget.observer));
                        return Ok(ExecutionOutcome::Stopped {
                            reason,
                            usage: budget.usage,
                            cleanup_failure,
                        });
                    }
                    let error = failure.error;
                    let frames = failure
                        .stack
                        .iter()
                        .map(|frame| StackFrame {
                            function: frame.function,
                            function_name: raw_module
                                .functions
                                .get(frame.function as usize)
                                .map_or_else(
                                    || "<missing>".to_owned(),
                                    |function| function.name.clone(),
                                ),
                            instruction: frame.instruction,
                            source: debug.and_then(|debug| {
                                debug.source_location(frame.function, frame.instruction)
                            }),
                        })
                        .collect();
                    machine.cancel_pending_effects(budget, effects);
                    machine.cancel_effects(effects);
                    let cleanup = machine
                        .terminal_cleanup_failure
                        .or_else(|| machine.cancel_all(budget.observer));
                    return Err(ExecutionError {
                        error: cleanup.map_or(error, |_| VmError::ResourceLimit {
                            resource: RESOURCE_CLEANUP_INSTRUCTIONS,
                        }),
                        frames,
                    });
                }
                MachineTaskState::Ready | MachineTaskState::Waiting(_) => {
                    // A provider may have accepted an effect but not completed it
                    // during this scheduler turn. Keep polling those effects; a
                    // task-only wait with no runnable work remains a deadlock.
                    if machine.tasks.values().any(|task| {
                        matches!(
                            task.state,
                            MachineTaskState::Waiting(WaitState::Effect { .. })
                        )
                    }) {
                        if let Err(error) = budget.check_interruption() {
                            machine.cancel_pending_effects(budget, effects);
                            machine.cancel_effects(effects);
                            let cleanup = machine.cancel_all(budget.observer);
                            return Err(ExecutionError {
                                error: cleanup.map_or(error, |_| VmError::ResourceLimit {
                                    resource: RESOURCE_CLEANUP_INSTRUCTIONS,
                                }),
                                frames: Vec::new(),
                            });
                        }
                        continue;
                    }
                    machine.cancel_pending_effects(budget, effects);
                    machine.cancel_effects(effects);
                    let cleanup = machine.cancel_all(budget.observer);
                    return Err(ExecutionError {
                        error: cleanup.map_or(
                            VmError::Invariant("scheduler has no ready task"),
                            |_| VmError::ResourceLimit {
                                resource: RESOURCE_CLEANUP_INSTRUCTIONS,
                            },
                        ),
                        frames: Vec::new(),
                    });
                }
            }
        };
        machine.rotation_cursor = Some(task_id);
        let failed_checkpoint = machine.tasks.get(&task_id).and_then(|task| {
            task.frames.last().map(|frame| ActiveFrame {
                function: frame.function,
                instruction: u32::try_from(frame.program_counter).unwrap_or(u32::MAX),
            })
        });
        let result = machine.step(raw_module, debug, budget, effects, capabilities, task_id);
        if let Err(error) = result {
            let task = machine.tasks.get(&task_id).expect("scheduled task exists");
            let owner_id = task.owner;
            let mut failure = machine_failure(task, error);
            if let Some(frame) = failure.stack.first_mut() {
                if let Some(current) = failed_checkpoint {
                    frame.instruction = current.instruction;
                }
            }
            if matches!(
                failure.error,
                VmError::Timeout { .. } | VmError::Cancelled | VmError::Stopped { .. }
            ) {
                machine.cancel_pending_effects(budget, effects);
                machine.cancel_effects(effects);
                let cleanup_failure = machine.cancel_all(budget.observer);
                machine.terminal_cleanup_failure =
                    machine.terminal_cleanup_failure.or(cleanup_failure);
                if cleanup_failure.is_some() && !matches!(failure.error, VmError::Stopped { .. }) {
                    failure.error = VmError::ResourceLimit {
                        resource: RESOURCE_CLEANUP_INSTRUCTIONS,
                    };
                }
                machine.tasks.get_mut(&0).expect("root task exists").state =
                    MachineTaskState::Completed(Err(failure));
                let terminal = match machine.tasks[&0].state {
                    MachineTaskState::Completed(Err(TaskFailure {
                        error: VmError::Stopped { .. },
                        ..
                    })) => TaskEventKind::Stopped,
                    MachineTaskState::Completed(Err(TaskFailure {
                        error: VmError::Cancelled,
                        ..
                    })) => TaskEventKind::Cancelled,
                    _ => TaskEventKind::Failed,
                };
                machine.emit_event(budget.observer, 0, 0, terminal);
            } else {
                if task_id != 0
                    && machine
                        .cancel_descendants(task_id, budget.observer)
                        .is_some()
                {
                    failure.error = VmError::ResourceLimit {
                        resource: RESOURCE_CLEANUP_INSTRUCTIONS,
                    };
                }
                machine
                    .tasks
                    .get_mut(&task_id)
                    .expect("scheduled task exists")
                    .state = MachineTaskState::Completed(Err(failure));
                machine.emit_event(budget.observer, task_id, owner_id, TaskEventKind::Failed);
            }
        }
    }
}

impl TaskMachine {
    fn cancel_pending_effects(
        &mut self,
        budget: &mut Budget<'_>,
        effects: &mut dyn EffectProvider,
    ) {
        let pending = self
            .tasks
            .values()
            .filter_map(|task| match task.state {
                MachineTaskState::Waiting(WaitState::Effect { pending, .. }) => Some(pending),
                _ => None,
            })
            .collect::<Vec<_>>();
        for pending in pending {
            effects.cancel_effect(pending);
            let _ = budget.complete_effect();
        }
    }

    fn next_pending_effect(&mut self) -> Result<PendingEffectId, VmError> {
        self.next_effect = self
            .next_effect
            .checked_add(1)
            .ok_or(VmError::ResourceLimit {
                resource: "pending_effects",
            })?;
        Ok(PendingEffectId(self.next_effect))
    }

    fn start_effect_future(
        &mut self,
        future: &FutureValue,
        budget: &mut Budget<'_>,
        effects: &mut dyn EffectProvider,
    ) -> Result<(PendingEffectId, EffectPoll, ValueType, EffectFailureFamily), VmError> {
        let pending = self.next_pending_effect()?;
        let failure_family = future
            .failure_family()
            .ok_or(VmError::Invariant("function is not an external effect"))?;
        let result_type = match future {
            FutureValue::Effect { operation, .. } => effect_result_type(*operation),
            FutureValue::Tool { result_type, .. } | FutureValue::Agent { result_type, .. } => {
                result_type.clone()
            }
            FutureValue::Function { .. } => {
                return Err(VmError::Invariant("function is not an external effect"));
            }
        };
        budget.start_effect()?;
        let poll = match future {
            FutureValue::Effect {
                operation,
                arguments,
            } => effects.start_call(pending, *operation, arguments, budget.cancellation),
            FutureValue::Tool {
                tool,
                input,
                result_type,
            } => effects.start_tool(pending, *tool, input, result_type, budget.cancellation),
            FutureValue::Agent {
                operation,
                arguments,
                result_type,
            } => effects.start_agent(
                pending,
                *operation,
                arguments,
                result_type,
                budget.cancellation,
            ),
            FutureValue::Function { .. } => unreachable!("function was checked above"),
        };
        let poll = match poll {
            Err(error) => {
                close_provider_error(failure_family, &result_type, error).map(EffectPoll::Ready)
            }
            poll => poll,
        };
        match poll {
            Ok(poll) => Ok((pending, poll, result_type, failure_family)),
            Err(error) => {
                let _ = budget.complete_effect();
                Err(error)
            }
        }
    }

    fn cancel_effects(&mut self, effects: &mut dyn EffectProvider) {
        if !self.providers_cancelled {
            effects.cancel_pending();
            self.providers_cancelled = true;
        }
    }

    fn emit_event(
        &mut self,
        observer: &mut dyn CheckpointObserver,
        task_id: u64,
        owner_id: u64,
        kind: TaskEventKind,
    ) {
        self.next_event = self.next_event.saturating_add(1);
        observer.task_event(TaskEvent {
            sequence: self.next_event,
            task_id,
            owner_id,
            kind,
        });
    }

    fn next_ready(&self) -> Option<u64> {
        let after = self.rotation_cursor.unwrap_or(u64::MAX);
        self.tasks
            .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
            .chain(self.tasks.range(..=after))
            .find_map(|(id, task)| matches!(task.state, MachineTaskState::Ready).then_some(*id))
    }

    fn charge_cleanup(
        &mut self,
        count: u64,
        observer: &mut dyn CheckpointObserver,
    ) -> Option<&'static str> {
        if count > self.cleanup_remaining {
            self.cleanup_remaining = 0;
            Some(RESOURCE_CLEANUP_INSTRUCTIONS)
        } else {
            self.cleanup_remaining -= count;
            let used = self.cleanup_limit - self.cleanup_remaining;
            if self.cleanup_limit != u64::MAX
                && !self.cleanup_warning_emitted
                && used >= warning_threshold(self.cleanup_limit)
            {
                self.cleanup_warning_emitted = true;
                observer.budget_warning(BudgetWarning {
                    resource: RESOURCE_CLEANUP_INSTRUCTIONS,
                    used,
                    limit: self.cleanup_limit,
                });
            }
            None
        }
    }

    fn cancel_all(&mut self, observer: &mut dyn CheckpointObserver) -> Option<&'static str> {
        let cancelled = self
            .tasks
            .iter()
            .filter_map(|(id, task)| {
                (*id != 0).then_some((
                    *id,
                    task.owner,
                    !matches!(task.state, MachineTaskState::Completed(_)),
                ))
            })
            .collect::<Vec<_>>();
        let count = u64::try_from(cancelled.len()).unwrap_or(u64::MAX);
        self.tasks.retain(|id, _| *id == 0);
        for (id, owner, was_live) in cancelled {
            if was_live {
                self.emit_event(observer, id, owner, TaskEventKind::Cancelled);
            }
        }
        self.charge_cleanup(count, observer)
    }

    fn cancel_descendants(
        &mut self,
        owner: u64,
        observer: &mut dyn CheckpointObserver,
    ) -> Option<&'static str> {
        let mut frontier = vec![owner];
        let mut removed = Vec::new();
        while let Some(parent) = frontier.pop() {
            for child in self
                .tasks
                .iter()
                .filter_map(|(id, task)| (task.owner == parent).then_some(*id))
                .collect::<Vec<_>>()
            {
                frontier.push(child);
                removed.push(child);
            }
        }
        removed.sort_unstable();
        removed.dedup();
        let count = u64::try_from(removed.len()).unwrap_or(u64::MAX);
        for id in removed {
            if let Some(task) = self.tasks.remove(&id) {
                if !matches!(task.state, MachineTaskState::Completed(_)) {
                    self.emit_event(observer, id, task.owner, TaskEventKind::Cancelled);
                }
            }
        }
        self.charge_cleanup(count, observer)
    }

    fn cancel_task_tree(
        &mut self,
        owner: u64,
        observer: &mut dyn CheckpointObserver,
    ) -> Option<&'static str> {
        let descendants = self.cancel_descendants(owner, observer);
        let removed_task = self.tasks.remove(&owner);
        let removed = u64::from(removed_task.is_some());
        if let Some(task) = removed_task {
            if !matches!(task.state, MachineTaskState::Completed(_)) {
                self.emit_event(observer, owner, task.owner, TaskEventKind::Cancelled);
            }
        }
        let owner_charge = self.charge_cleanup(removed, observer);
        descendants.or(owner_charge)
    }

    fn scope_children(&self, owner: u64, scope: ScopeKey) -> Vec<u64> {
        self.tasks
            .iter()
            .filter_map(|(id, task)| {
                (task.owner == owner && task.scope == Some(scope)).then_some(*id)
            })
            .collect()
    }

    #[allow(clippy::too_many_lines)]
    fn wake_waiters(
        &mut self,
        raw_module: &allen_bytecode::Module,
        budget: &mut Budget<'_>,
        effects: &mut dyn EffectProvider,
    ) {
        let waiters = self
            .tasks
            .iter()
            .filter_map(|(id, task)| {
                matches!(task.state, MachineTaskState::Waiting(_)).then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in waiters {
            let Some(MachineTaskState::Waiting(waiting)) =
                self.tasks.get(&id).map(|task| task.state.clone())
            else {
                continue;
            };
            match waiting {
                WaitState::Effect {
                    pending,
                    destination,
                    result_type,
                    failure_family,
                } => {
                    let result = effects.poll_effect(pending, budget.cancellation);
                    let result = match result {
                        Ok(EffectPoll::Pending) => continue,
                        Ok(EffectPoll::Ready(value)) => {
                            if let Err(error) = budget.complete_effect() {
                                Err(error)
                            } else if let Err(error) = validate_provider_value(
                                &value,
                                &result_type,
                                raw_module,
                                failure_family,
                            ) {
                                Err(error)
                            } else {
                                charge_external_value(&value, budget, 0).map(|()| value)
                            }
                        }
                        Err(error) => {
                            let _ = budget.complete_effect();
                            close_provider_error(failure_family, &result_type, error)
                        }
                    };
                    let task = self.tasks.get_mut(&id).expect("waiting task exists");
                    match result {
                        Ok(value) => {
                            if let Some(destination) = destination {
                                if let Some(frame) = task.frames.last_mut() {
                                    if let Err(error) =
                                        write_register(&mut frame.registers, destination, value)
                                    {
                                        task.state = MachineTaskState::Completed(Err(
                                            machine_failure(task, error),
                                        ));
                                        let owner = task.owner;
                                        self.emit_event(
                                            budget.observer,
                                            id,
                                            owner,
                                            TaskEventKind::Failed,
                                        );
                                        continue;
                                    }
                                }
                                let owner = task.owner;
                                task.state = MachineTaskState::Ready;
                                self.emit_event(budget.observer, id, owner, TaskEventKind::Ready);
                            } else {
                                let owner = task.owner;
                                task.state = MachineTaskState::Completed(Ok(value));
                                self.emit_event(
                                    budget.observer,
                                    id,
                                    owner,
                                    TaskEventKind::Completed,
                                );
                            }
                        }
                        Err(error) => {
                            task.state =
                                MachineTaskState::Completed(Err(machine_failure(task, error)));
                            let owner = task.owner;
                            self.emit_event(budget.observer, id, owner, TaskEventKind::Failed);
                        }
                    }
                }
                WaitState::Task {
                    handle,
                    destination,
                } => {
                    let completed = self
                        .tasks
                        .get(&handle)
                        .and_then(|child| match &child.state {
                            MachineTaskState::Completed(result) => Some(result.clone()),
                            _ => None,
                        });
                    let Some(result) = completed else { continue };
                    self.tasks.remove(&handle);
                    match result {
                        Ok(value) => {
                            let direct_task = match &value {
                                Value::Task(task) => Some(task.id),
                                _ => None,
                            };
                            let current_scope = self
                                .tasks
                                .get(&id)
                                .and_then(|task| task.frames.last())
                                .and_then(|frame| {
                                    frame.active_scopes.last().map(|lexical| ScopeKey {
                                        activation: frame.activation,
                                        lexical: *lexical,
                                    })
                                });
                            let mut transferred = Vec::new();
                            task_handles(&value, &mut transferred);
                            transferred.sort_unstable();
                            if transferred.windows(2).any(|pair| pair[0] == pair[1])
                                || transferred.iter().any(|task_id| {
                                    self.tasks
                                        .get(task_id)
                                        .is_none_or(|task| task.owner != handle)
                                })
                            {
                                let task = self.tasks.get(&id).expect("waiting task exists");
                                let failure = machine_failure(
                                    task,
                                    VmError::Invariant("returned task cannot transfer ownership"),
                                );
                                self.tasks.get_mut(&id).expect("waiting task exists").state =
                                    MachineTaskState::Completed(Err(failure));
                                let owner = self.tasks[&id].owner;
                                self.emit_event(budget.observer, id, owner, TaskEventKind::Failed);
                                continue;
                            }
                            for transferred in transferred {
                                let task = self
                                    .tasks
                                    .get_mut(&transferred)
                                    .expect("returned task exists");
                                task.owner = id;
                                if direct_task == Some(transferred) {
                                    task.scope = current_scope;
                                }
                            }
                            let task = self.tasks.get_mut(&id).expect("waiting task exists");
                            if let Some(frame) = task.frames.last_mut() {
                                if let Err(error) =
                                    write_register(&mut frame.registers, destination, value)
                                {
                                    task.state = MachineTaskState::Completed(Err(machine_failure(
                                        task, error,
                                    )));
                                    let owner = task.owner;
                                    self.emit_event(
                                        budget.observer,
                                        id,
                                        owner,
                                        TaskEventKind::Failed,
                                    );
                                    continue;
                                }
                            }
                            task.state = MachineTaskState::Ready;
                            let owner = task.owner;
                            self.emit_event(budget.observer, id, owner, TaskEventKind::Ready);
                        }
                        Err(failure) => {
                            let task = self.tasks.get_mut(&id).expect("waiting task exists");
                            let mut stack = failure.stack;
                            stack.extend(task.frames.iter().rev().map(MachineFrame::active));
                            task.state = MachineTaskState::Completed(Err(TaskFailure {
                                error: failure.error,
                                stack,
                            }));
                            let owner = task.owner;
                            self.emit_event(budget.observer, id, owner, TaskEventKind::Failed);
                        }
                    }
                }
                WaitState::Scope { scope } => {
                    let children = self.scope_children(id, scope);
                    let mut failure = None;
                    let mut waiting = false;
                    for (index, child) in children.iter().enumerate() {
                        let completed = self.tasks.get(child).and_then(|task| match &task.state {
                            MachineTaskState::Completed(result) => Some(result.clone()),
                            _ => None,
                        });
                        let Some(result) = completed else {
                            waiting = true;
                            break;
                        };
                        self.tasks.remove(child);
                        if let Ok(value) = &result {
                            let mut leaked = Vec::new();
                            task_handles(value, &mut leaked);
                            if !leaked.is_empty() {
                                failure = Some(TaskFailure {
                                    error: VmError::Invariant(
                                        "implicit scope join produced affine result",
                                    ),
                                    stack: Vec::new(),
                                });
                            }
                        }
                        if let Err(child_failure) = result {
                            let mut cleanup_failure = None;
                            for sibling in &children[index + 1..] {
                                let sibling_failure =
                                    self.cancel_task_tree(*sibling, budget.observer);
                                cleanup_failure = cleanup_failure.or(sibling_failure);
                            }
                            failure = Some(if cleanup_failure.is_some() {
                                TaskFailure {
                                    error: VmError::ResourceLimit {
                                        resource: RESOURCE_CLEANUP_INSTRUCTIONS,
                                    },
                                    stack: child_failure.stack,
                                }
                            } else {
                                child_failure
                            });
                            break;
                        }
                    }
                    if waiting {
                        continue;
                    }
                    let task = self.tasks.get_mut(&id).expect("waiting task exists");
                    if let Some(failure) = failure {
                        let mut stack = failure.stack;
                        stack.extend(task.frames.iter().rev().map(MachineFrame::active));
                        task.state = MachineTaskState::Completed(Err(TaskFailure {
                            error: failure.error,
                            stack,
                        }));
                        let owner = task.owner;
                        self.emit_event(budget.observer, id, owner, TaskEventKind::Failed);
                    } else {
                        task.state = MachineTaskState::Ready;
                        let owner = task.owner;
                        self.emit_event(budget.observer, id, owner, TaskEventKind::Ready);
                    }
                }
                WaitState::Return { scope, value } => {
                    let children = self.scope_children(id, scope);
                    if children.iter().any(|child| {
                        !matches!(
                            self.tasks.get(child).map(|task| &task.state),
                            Some(MachineTaskState::Completed(_))
                        )
                    }) {
                        continue;
                    }
                    let mut failure = None;
                    for child in children {
                        if let MachineTaskState::Completed(Err(child_failure)) =
                            self.tasks.remove(&child).expect("scope child exists").state
                        {
                            failure.get_or_insert(child_failure);
                        }
                    }
                    if let Some(failure) = failure {
                        let task = self.tasks.get_mut(&id).expect("waiting task exists");
                        let mut stack = failure.stack;
                        stack.extend(task.frames.iter().rev().map(MachineFrame::active));
                        task.state = MachineTaskState::Completed(Err(TaskFailure {
                            error: failure.error,
                            stack,
                        }));
                        let owner = task.owner;
                        self.emit_event(budget.observer, id, owner, TaskEventKind::Failed);
                        continue;
                    }
                    let next_scope = self
                        .tasks
                        .get(&id)
                        .and_then(|task| task.frames.last())
                        .and_then(|frame| {
                            frame.active_scopes.last().map(|lexical| ScopeKey {
                                activation: frame.activation,
                                lexical: *lexical,
                            })
                        });
                    if let Some(next_scope) = next_scope {
                        self.tasks.get_mut(&id).expect("waiting task exists").state =
                            MachineTaskState::Waiting(WaitState::Return {
                                scope: next_scope,
                                value,
                            });
                    } else if let Err(error) = self.return_value(id, value, budget.observer) {
                        let task = self.tasks.get(&id).expect("waiting task exists");
                        let failure = machine_failure(task, error);
                        self.tasks.get_mut(&id).expect("waiting task exists").state =
                            MachineTaskState::Completed(Err(failure));
                        let owner = self.tasks[&id].owner;
                        self.emit_event(budget.observer, id, owner, TaskEventKind::Failed);
                    } else if !matches!(
                        self.tasks.get(&id).map(|task| &task.state),
                        Some(MachineTaskState::Completed(_))
                    ) {
                        let task = self.tasks.get_mut(&id).expect("waiting task exists");
                        task.state = MachineTaskState::Ready;
                        let owner = task.owner;
                        self.emit_event(budget.observer, id, owner, TaskEventKind::Ready);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn step(
        &mut self,
        raw_module: &allen_bytecode::Module,
        debug: Option<&dyn DebugSourceMap>,
        budget: &mut Budget<'_>,
        effects: &mut dyn EffectProvider,
        capabilities: &ExecutionCapabilities,
        task_id: u64,
    ) -> Result<(), VmError> {
        let (function_id, instruction_index, instruction) = {
            let task = self
                .tasks
                .get(&task_id)
                .ok_or(VmError::Invariant("task is missing"))?;
            let frame = task
                .frames
                .last()
                .ok_or(VmError::Invariant("task has no frame"))?;
            let function = raw_module
                .functions
                .get(frame.function as usize)
                .ok_or(VmError::Invariant("function is missing"))?;
            let instruction = function
                .code
                .get(frame.program_counter)
                .ok_or(VmError::Invariant("function ended without return"))?
                .clone();
            let index = u32::try_from(frame.program_counter)
                .map_err(|_| VmError::Invariant("instruction index is out of range"))?;
            (frame.function, index, instruction)
        };
        budget.charge_instruction(Checkpoint {
            function: function_id,
            instruction: instruction_index,
        })?;
        let task = self.tasks.get_mut(&task_id).expect("scheduled task exists");
        let frame = task.frames.last_mut().expect("scheduled frame exists");
        frame.program_counter = frame
            .program_counter
            .checked_add(1)
            .ok_or(VmError::Invariant("program counter overflowed"))?;

        match instruction {
            Instruction::Const {
                destination,
                constant,
            } => {
                let constant = raw_module
                    .constants
                    .get(constant as usize)
                    .ok_or(VmError::Invariant("constant is missing"))?;
                let value = materialize_constant(constant, budget)?;
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::Move {
                destination,
                source,
            } => {
                let value = read_register(&frame.registers, source)?.clone();
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::IntBinary {
                destination,
                left,
                right,
                operation,
            } => {
                let value = int_binary(
                    operation,
                    read_int(&frame.registers, left)?,
                    read_int(&frame.registers, right)?,
                )?;
                write_register(&mut frame.registers, destination, Value::Int(value))?;
            }
            Instruction::IntRemainder {
                destination,
                left,
                right,
            } => {
                let value = int_remainder(
                    read_int(&frame.registers, left)?,
                    read_int(&frame.registers, right)?,
                )?;
                write_register(&mut frame.registers, destination, Value::Int(value))?;
            }
            Instruction::FloatBinary {
                destination,
                left,
                right,
                operation,
            } => {
                let left = read_float(&frame.registers, left)?.as_f64();
                let right = read_float(&frame.registers, right)?.as_f64();
                let value = match operation {
                    NumericBinaryOp::Add => left + right,
                    NumericBinaryOp::Subtract => left - right,
                    NumericBinaryOp::Multiply => left * right,
                    NumericBinaryOp::Divide => left / right,
                };
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Float(FloatValue::new(value)),
                )?;
            }
            Instruction::IntNegate {
                destination,
                source,
            } => {
                let value = read_int(&frame.registers, source)?
                    .checked_neg()
                    .ok_or(VmError::ArithmeticOverflow)?;
                write_register(&mut frame.registers, destination, Value::Int(value))?;
            }
            Instruction::FloatNegate {
                destination,
                source,
            } => {
                let value = -read_float(&frame.registers, source)?.as_f64();
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Float(FloatValue::new(value)),
                )?;
            }
            Instruction::Compare {
                destination,
                left,
                right,
                operation,
            } => {
                let value = compare(
                    operation,
                    read_register(&frame.registers, left)?,
                    read_register(&frame.registers, right)?,
                )?;
                write_register(&mut frame.registers, destination, Value::Bool(value))?;
            }
            Instruction::BoolNot {
                destination,
                source,
            } => {
                let value = !read_bool(&frame.registers, source)?;
                write_register(&mut frame.registers, destination, Value::Bool(value))?;
            }
            Instruction::BoolBinary {
                destination,
                left,
                right,
                operation,
            } => {
                let left = read_bool(&frame.registers, left)?;
                let right = read_bool(&frame.registers, right)?;
                let value = match operation {
                    BoolBinaryOp::And => left && right,
                    BoolBinaryOp::Or => left || right,
                };
                write_register(&mut frame.registers, destination, Value::Bool(value))?;
            }
            Instruction::ListNew {
                destination,
                elements,
            } => {
                budget.charge_allocation(collection_size(elements.len(), VALUE_SLOT_BYTES)?)?;
                let values = clone_registers(&frame.registers, &elements)?;
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::List(values.into()),
                )?;
            }
            Instruction::MapNew {
                destination,
                entries,
            } => {
                budget.charge_allocation(collection_size(entries.len(), MAP_ENTRY_BYTES)?)?;
                let mut values = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    values.push((
                        read_register(&frame.registers, key)?.clone(),
                        read_register(&frame.registers, value)?.clone(),
                    ));
                }
                values.sort_by(|(left, _), (right, _)| compare_map_keys(left, right));
                if values
                    .windows(2)
                    .any(|pair| language_equal(&pair[0].0, &pair[1].0))
                {
                    return Err(VmError::DuplicateMapKey);
                }
                write_register(&mut frame.registers, destination, Value::Map(values.into()))?;
            }
            Instruction::TupleNew {
                destination,
                elements,
            } => {
                budget.charge_allocation(collection_size(elements.len(), VALUE_SLOT_BYTES)?)?;
                let values = clone_registers(&frame.registers, &elements)?;
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Tuple(values.into()),
                )?;
            }
            Instruction::IndexGet {
                destination,
                collection,
                index,
            } => {
                let value = index_get(
                    read_register(&frame.registers, collection)?,
                    read_register(&frame.registers, index)?,
                )?;
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::MapEntryAt {
                destination,
                map,
                index,
            } => {
                let value = map_entry_at(&frame.registers, map, index, budget)?;
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::Length {
                destination,
                collection,
            } => {
                let value = collection_length(read_register(&frame.registers, collection)?)?;
                write_register(&mut frame.registers, destination, Value::Int(value))?;
            }
            Instruction::StringCall {
                destination,
                operation,
                arguments,
            } => {
                let value = string_call(&frame.registers, operation, &arguments, budget)?;
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::ListAppend {
                destination,
                values,
                value,
            } => {
                let output = list_append(&frame.registers, values, value, budget)?;
                write_register(&mut frame.registers, destination, output)?;
            }
            Instruction::ListSet {
                destination,
                values,
                index,
                value,
            } => {
                let output = list_set(&frame.registers, values, index, value, budget)?;
                write_register(&mut frame.registers, destination, output)?;
            }
            Instruction::TupleGet {
                destination,
                tuple,
                index,
            } => {
                let Value::Tuple(values) = read_register(&frame.registers, tuple)? else {
                    return Err(VmError::Invariant("tuple operand contains another type"));
                };
                let value = values
                    .get(index as usize)
                    .cloned()
                    .ok_or(VmError::Invariant("verified tuple index is out of bounds"))?;
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::Convert {
                destination,
                source,
                conversion,
            } => {
                let value = convert(read_register(&frame.registers, source)?, conversion, budget)?;
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::RecordNew {
                destination,
                fields,
            } => {
                let function = &raw_module.functions[function_id as usize];
                let ValueType::Record(layout) = register_type(function, destination)? else {
                    return Err(VmError::Invariant(
                        "record destination contains another type",
                    ));
                };
                budget.charge_allocation(record_size(layout.len())?)?;
                let value = construct_record(&frame.registers, layout, &fields)?;
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Record(value.into()),
                )?;
            }
            Instruction::FieldGet {
                destination,
                record,
                field,
            } => {
                let Value::Record(fields) = read_register(&frame.registers, record)? else {
                    return Err(VmError::Invariant("record operand contains another type"));
                };
                let value = fields
                    .get(field as usize)
                    .map(|(_, value)| value.clone())
                    .ok_or(VmError::Invariant("verified record field is out of bounds"))?;
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::EnumNew {
                destination,
                variant,
                payload,
            } => {
                let function = &raw_module.functions[function_id as usize];
                let enum_type = register_type(function, destination)?;
                budget.charge_allocation(enum_size(payload.len())?)?;
                let value =
                    construct_enum(raw_module, enum_type, variant, &frame.registers, &payload)?;
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Enum(Rc::new(value)),
                )?;
            }
            Instruction::BranchBool {
                condition,
                true_target,
                false_target,
            } => {
                frame.program_counter =
                    usize::try_from(if read_bool(&frame.registers, condition)? {
                        true_target
                    } else {
                        false_target
                    })
                    .map_err(|_| VmError::Invariant("branch target is out of range"))?;
            }
            Instruction::SwitchEnum { source, arms } => {
                let Value::Enum(value) = read_register(&frame.registers, source)? else {
                    return Err(VmError::Invariant(
                        "enum switch operand contains another type",
                    ));
                };
                let arm = arms
                    .iter()
                    .find(|arm| arm.variant == value.variant)
                    .ok_or(VmError::Invariant("enum switch has no matching arm"))?;
                let payload = value.payload.clone();
                write_switch_bindings(&mut frame.registers, &payload, &arm.bindings)?;
                frame.program_counter = usize::try_from(arm.target)
                    .map_err(|_| VmError::Invariant("enum switch target is out of range"))?;
            }
            Instruction::Jump { target } => {
                frame.program_counter = usize::try_from(target)
                    .map_err(|_| VmError::Invariant("jump target is out of range"))?;
            }
            Instruction::TryResult {
                destination,
                source,
            } => {
                let source_value = read_register(&frame.registers, source)?.clone();
                let Value::Enum(result) = &source_value else {
                    return Err(VmError::Invariant(
                        "TryResult operand contains another type",
                    ));
                };
                if result.identity != EnumIdentity::Result {
                    return Err(VmError::Invariant("TryResult operand is not Result"));
                }
                match (result.variant, &result.payload) {
                    (0, EnumPayload::Tuple(payload)) => write_register(
                        &mut frame.registers,
                        destination,
                        payload
                            .first()
                            .cloned()
                            .ok_or(VmError::Invariant("Ok has no payload"))?,
                    )?,
                    (1, EnumPayload::Tuple(_)) => {
                        if let Some(scope) = frame.active_scopes.pop() {
                            task.state = MachineTaskState::Waiting(WaitState::Return {
                                scope: ScopeKey {
                                    activation: frame.activation,
                                    lexical: scope,
                                },
                                value: source_value,
                            });
                            let owner = task.owner;
                            self.emit_event(
                                budget.observer,
                                task_id,
                                owner,
                                TaskEventKind::Waiting,
                            );
                        } else {
                            self.return_value(task_id, source_value, budget.observer)?;
                        }
                    }
                    _ => return Err(VmError::Invariant("Result has an invalid payload")),
                }
            }
            Instruction::ToUnknown {
                destination,
                source,
            } => {
                budget.charge_allocation(VALUE_SLOT_BYTES)?;
                let value = read_register(&frame.registers, source)?.clone();
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Unknown(Rc::new(value)),
                )?;
            }
            Instruction::Narrow {
                destination,
                source,
                target,
            } => {
                let Value::Unknown(value) = read_register(&frame.registers, source)? else {
                    return Err(VmError::Invariant("Narrow operand contains another type"));
                };
                let matches =
                    value_matches_type(value, &target, raw_module, 0, &mut HashMap::new())?;
                budget.charge_allocation(enum_size(usize::from(matches))?)?;
                let payload = if matches {
                    EnumPayload::Tuple(vec![Rc::clone(value).as_ref().clone()].into())
                } else {
                    EnumPayload::Unit
                };
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Enum(Rc::new(builtin_enum(
                        EnumIdentity::Option,
                        u32::from(matches),
                        payload,
                    ))),
                )?;
            }
            Instruction::DirectCall {
                destination,
                function,
                arguments,
            } => {
                let values = clone_registers(&frame.registers, &arguments)?;
                let depth =
                    u32::try_from(task.frames.len() + 1).map_err(|_| VmError::ResourceLimit {
                        resource: RESOURCE_CALL_DEPTH,
                    })?;
                let child = new_machine_frame(
                    raw_module,
                    function,
                    &values,
                    &[],
                    Some(destination),
                    depth,
                    budget,
                )?;
                task.frames.push(child);
            }
            Instruction::ClosureNew {
                destination,
                function,
                captures,
            } => {
                budget.charge_allocation(collection_size(captures.len(), VALUE_SLOT_BYTES)?)?;
                let captures = clone_registers(&frame.registers, &captures)?;
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Closure(Rc::new(ClosureValue {
                        function,
                        captures: captures.into(),
                    })),
                )?;
            }
            Instruction::ClosureCall {
                destination,
                closure,
                arguments,
            } => {
                let Value::Closure(closure) = read_register(&frame.registers, closure)? else {
                    return Err(VmError::Invariant("closure operand contains another type"));
                };
                let closure = Rc::clone(closure);
                let values = clone_registers(&frame.registers, &arguments)?;
                let depth =
                    u32::try_from(task.frames.len() + 1).map_err(|_| VmError::ResourceLimit {
                        resource: RESOURCE_CALL_DEPTH,
                    })?;
                let child = new_machine_frame(
                    raw_module,
                    closure.function,
                    &values,
                    &closure.captures,
                    Some(destination),
                    depth,
                    budget,
                )?;
                task.frames.push(child);
            }
            Instruction::AsyncCall {
                destination,
                function,
                arguments,
            } => {
                if raw_module.async_functions.binary_search(&function).is_err() {
                    return Err(VmError::Invariant("async call target is not async"));
                }
                budget.charge_allocation(logical_size(
                    FUTURE_BASE_BYTES,
                    arguments.len(),
                    VALUE_SLOT_BYTES,
                )?)?;
                let arguments = clone_registers(&frame.registers, &arguments)?;
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Future(Rc::new(FutureValue::Function {
                        function,
                        arguments: arguments.into(),
                    })),
                )?;
            }
            Instruction::Spawn {
                destination,
                future,
                scope,
            } => {
                if scope != 0 && frame.active_scopes.last() != Some(&scope) {
                    return Err(VmError::Invariant("spawn scope is not active"));
                }
                let Value::Future(future) = read_register(&frame.registers, future)? else {
                    return Err(VmError::Invariant("spawn operand contains another type"));
                };
                let future = Rc::clone(future);
                let scope = (scope != 0).then_some(ScopeKey {
                    activation: frame.activation,
                    lexical: scope,
                });
                let handle = self.spawn_machine(
                    raw_module,
                    budget,
                    effects,
                    task_id,
                    function_id,
                    scope,
                    &future,
                )?;
                let task = self.tasks.get_mut(&task_id).expect("spawning task exists");
                write_register(
                    &mut task
                        .frames
                        .last_mut()
                        .expect("spawning frame exists")
                        .registers,
                    destination,
                    Value::Task(handle),
                )?;
            }
            Instruction::Await {
                destination,
                source,
            } => {
                let awaitable = read_register(&frame.registers, source)?.clone();
                match awaitable {
                    Value::Future(future) => match future.as_ref() {
                        FutureValue::Function {
                            function,
                            arguments,
                        } => {
                            let depth = u32::try_from(task.frames.len() + 1).map_err(|_| {
                                VmError::ResourceLimit {
                                    resource: RESOURCE_CALL_DEPTH,
                                }
                            })?;
                            let child = new_machine_frame(
                                raw_module,
                                *function,
                                arguments,
                                &[],
                                Some(destination),
                                depth,
                                budget,
                            )?;
                            task.frames.push(child);
                        }
                        external => {
                            let _ = task;
                            let (pending, poll, result_type, failure_family) =
                                self.start_effect_future(external, budget, effects)?;
                            let task = self
                                .tasks
                                .get_mut(&task_id)
                                .expect("awaiting effect task exists");
                            match poll {
                                EffectPoll::Ready(value) => {
                                    budget.complete_effect()?;
                                    validate_provider_value(
                                        &value,
                                        &result_type,
                                        raw_module,
                                        failure_family,
                                    )?;
                                    charge_external_value(&value, budget, 0)?;
                                    write_register(
                                        &mut task
                                            .frames
                                            .last_mut()
                                            .expect("frame exists")
                                            .registers,
                                        destination,
                                        value,
                                    )?;
                                }
                                EffectPoll::Pending => {
                                    task.state = MachineTaskState::Waiting(WaitState::Effect {
                                        pending,
                                        destination: Some(destination),
                                        result_type,
                                        failure_family,
                                    });
                                    let owner = task.owner;
                                    self.emit_event(
                                        budget.observer,
                                        task_id,
                                        owner,
                                        TaskEventKind::Waiting,
                                    );
                                }
                            }
                        }
                    },
                    Value::Task(handle) => {
                        let _ = task;
                        let child = self
                            .tasks
                            .get(&handle.id)
                            .ok_or(VmError::Invariant("task handle is not live"))?;
                        if child.owner != task_id {
                            return Err(VmError::Invariant("task handle has another owner"));
                        }
                        let task = self.tasks.get_mut(&task_id).expect("awaiting task exists");
                        task.state = MachineTaskState::Waiting(WaitState::Task {
                            handle: handle.id,
                            destination,
                        });
                        let owner = task.owner;
                        self.emit_event(budget.observer, task_id, owner, TaskEventKind::Waiting);
                    }
                    _ => return Err(VmError::Invariant("await operand contains another type")),
                }
            }
            Instruction::TaskSnapshot {
                destination,
                source,
            } => {
                let Value::Task(handle) = read_register(&frame.registers, source)? else {
                    return Err(VmError::Invariant(
                        "task snapshot operand contains another type",
                    ));
                };
                let handle = *handle;
                let _ = task;
                let snapshot = self.task_snapshot(raw_module, debug, budget, task_id, handle)?;
                let task = self.tasks.get_mut(&task_id).expect("observing task exists");
                write_register(
                    &mut task
                        .frames
                        .last_mut()
                        .expect("observing frame exists")
                        .registers,
                    destination,
                    snapshot,
                )?;
            }
            Instruction::WorkspaceGet { destination } => {
                let workspace = match effects.workspace() {
                    Ok(workspace) => workspace,
                    Err(VmError::CapabilityMissing) => {
                        // An unavailable filesystem is represented by the
                        // later operation's typed Err. This reserved local
                        // handle conveys no authority and remains unusable by
                        // any provider that did not explicitly issue it.
                        WorkspaceValue::new(0, 0, 0)
                    }
                    Err(_) => return Err(VmError::ProtocolViolation),
                };
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Workspace(workspace),
                )?;
            }
            Instruction::EffectCall {
                destination,
                operation,
                arguments,
            } => {
                budget.charge_allocation(logical_size(
                    FUTURE_BASE_BYTES,
                    arguments.len(),
                    VALUE_SLOT_BYTES,
                )?)?;
                let arguments = clone_registers(&frame.registers, &arguments)?;
                let future_type = raw_module.functions[function_id as usize].registers
                    [destination as usize]
                    .clone();
                let ValueType::Future(result_type) = future_type else {
                    return Err(VmError::Invariant(
                        "effect call destination is not a future",
                    ));
                };
                let future = if is_agent_operation(operation) {
                    FutureValue::Agent {
                        operation,
                        arguments: arguments.into(),
                        result_type: *result_type,
                    }
                } else {
                    FutureValue::Effect {
                        operation,
                        arguments: arguments.into(),
                    }
                };
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Future(Rc::new(future)),
                )?;
            }
            Instruction::CapabilityInspect {
                destination,
                operation,
                arguments,
            } => {
                let value = capability_inspect(
                    &frame.registers,
                    operation,
                    &arguments,
                    capabilities,
                    budget,
                )?;
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::SafeCollectionCall {
                destination,
                operation,
                arguments,
            } => {
                let value = safe_collection_call(&frame.registers, operation, &arguments, budget)?;
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::CheckedIntCall {
                destination,
                operation,
                arguments,
            } => {
                let value = checked_int_call(&frame.registers, operation, &arguments, budget)?;
                write_register(&mut frame.registers, destination, value)?;
            }
            Instruction::ToolInvoke {
                destination,
                tool,
                input,
            } => {
                budget.charge_allocation(FUTURE_BASE_BYTES + VALUE_SLOT_BYTES)?;
                let input = read_register(&frame.registers, input)?.clone();
                let result_type = raw_module.functions[function_id as usize].registers
                    [destination as usize]
                    .clone();
                let ValueType::Future(result_type) = result_type else {
                    return Err(VmError::Invariant(
                        "tool invocation destination is not a future",
                    ));
                };
                write_register(
                    &mut frame.registers,
                    destination,
                    Value::Future(Rc::new(FutureValue::Tool {
                        tool,
                        input,
                        result_type: *result_type,
                    })),
                )?;
            }
            Instruction::TaskScopeEnter { scope } => {
                if scope == 0 || frame.active_scopes.contains(&scope) {
                    return Err(VmError::Invariant("task scope ID is invalid"));
                }
                frame.active_scopes.push(scope);
            }
            Instruction::TaskScopeExit { scope } => {
                if frame.active_scopes.pop() != Some(scope) {
                    return Err(VmError::Invariant("task scope exit is not nested"));
                }
                task.state = MachineTaskState::Waiting(WaitState::Scope {
                    scope: ScopeKey {
                        activation: frame.activation,
                        lexical: scope,
                    },
                });
                let owner = task.owner;
                self.emit_event(budget.observer, task_id, owner, TaskEventKind::Waiting);
            }
            Instruction::Stop { reason } => {
                let Value::String(reason) = read_register(&frame.registers, reason)? else {
                    return Err(VmError::Invariant("stop reason contains another type"));
                };
                return Err(VmError::Stopped {
                    reason: reason.to_string(),
                });
            }
            Instruction::Return { source } => {
                if !frame.active_scopes.is_empty() {
                    return Err(VmError::Invariant("return left an active task scope"));
                }
                let value = read_register(&frame.registers, source)?.clone();
                self.return_value(task_id, value, budget.observer)?;
            }
        }
        Ok(())
    }

    fn return_value(
        &mut self,
        task_id: u64,
        value: Value,
        observer: &mut dyn CheckpointObserver,
    ) -> Result<(), VmError> {
        let (continuation, caller_scope) = {
            let task = self
                .tasks
                .get_mut(&task_id)
                .ok_or(VmError::Invariant("task is missing"))?;
            let frame = task
                .frames
                .pop()
                .ok_or(VmError::Invariant("task has no frame"))?;
            let caller_scope = task.frames.last().and_then(|caller| {
                caller.active_scopes.last().map(|lexical| ScopeKey {
                    activation: caller.activation,
                    lexical: *lexical,
                })
            });
            (frame.continuation, caller_scope)
        };
        if continuation.is_some() {
            if let Value::Task(handle) = &value {
                let returned = self
                    .tasks
                    .get_mut(&handle.id)
                    .ok_or(VmError::Invariant("returned task is not live"))?;
                if returned.owner != task_id {
                    return Err(VmError::Invariant("returned task has another owner"));
                }
                returned.scope = caller_scope;
            }
        }
        let task = self.tasks.get_mut(&task_id).expect("returning task exists");
        let owner_id = task.owner;
        if let Some(destination) = continuation {
            let caller = task
                .frames
                .last_mut()
                .ok_or(VmError::Invariant("call has no caller"))?;
            write_register(&mut caller.registers, destination, value)?;
        } else {
            task.state = MachineTaskState::Completed(Ok(value));
        }
        if continuation.is_none() {
            self.emit_event(observer, task_id, owner_id, TaskEventKind::Completed);
        }
        Ok(())
    }

    fn task_snapshot(
        &self,
        raw_module: &allen_bytecode::Module,
        debug: Option<&dyn DebugSourceMap>,
        budget: &mut Budget<'_>,
        observer_id: u64,
        handle: TaskValue,
    ) -> Result<Value, VmError> {
        let observed = self
            .tasks
            .get(&handle.id)
            .ok_or(VmError::Invariant("task snapshot handle is not live"))?;
        if observed.owner != observer_id {
            return Err(VmError::Invariant("task snapshot handle has another owner"));
        }
        let active = observed.frames.last().map(MachineFrame::active);
        let function_id = active.map_or(observed.entry_function, |frame| frame.function);
        let function = raw_module
            .functions
            .get(function_id as usize)
            .ok_or(VmError::Invariant("task snapshot function is missing"))?
            .name
            .clone();
        let location = active.and_then(|frame| {
            debug.and_then(|debug| debug.source_location(frame.function, frame.instruction))
        });
        let location = location
            .map(|source| format!("{}:{}..{}", source.module_path, source.start, source.end));
        let state = match observed.state {
            MachineTaskState::Ready => "ready",
            MachineTaskState::Waiting(_) => "waiting",
            MachineTaskState::Completed(Ok(_)) => "completed",
            MachineTaskState::Completed(Err(_)) => "failed",
        };
        let id =
            i64::try_from(handle.id).map_err(|_| VmError::Invariant("task ID does not fit Int"))?;
        let owner_id = i64::try_from(observed.owner)
            .map_err(|_| VmError::Invariant("task owner ID does not fit Int"))?;
        let has_location = location.is_some();
        let location_value = if let Some(location) = location {
            let location = allocated_string(location, budget)?;
            Value::Enum(Rc::new(builtin_enum(
                EnumIdentity::Option,
                1,
                EnumPayload::Tuple(vec![location].into()),
            )))
        } else {
            Value::Enum(Rc::new(builtin_enum(
                EnumIdentity::Option,
                0,
                EnumPayload::Unit,
            )))
        };
        budget.charge_allocation(enum_size(usize::from(has_location))?)?;
        budget.charge_allocation(record_size(5)?)?;
        Ok(Value::Record(
            vec![
                (Rc::from("function"), allocated_string(function, budget)?),
                (Rc::from("id"), Value::Int(id)),
                (Rc::from("location"), location_value),
                (Rc::from("owner_id"), Value::Int(owner_id)),
                (
                    Rc::from("state"),
                    allocated_string(state.to_owned(), budget)?,
                ),
            ]
            .into(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    fn spawn_machine(
        &mut self,
        raw_module: &allen_bytecode::Module,
        budget: &mut Budget<'_>,
        effects: &mut dyn EffectProvider,
        owner: u64,
        origin_function: FunctionId,
        scope: Option<ScopeKey>,
        future: &Rc<FutureValue>,
    ) -> Result<TaskValue, VmError> {
        let live = u32::try_from(self.tasks.len().saturating_sub(1)).map_err(|_| {
            VmError::ResourceLimit {
                resource: RESOURCE_TASKS,
            }
        })?;
        if live >= budget.limits.tasks {
            return Err(VmError::ResourceLimit {
                resource: RESOURCE_TASKS,
            });
        }
        budget.charge_allocation(SCHEDULER_TASK_BYTES)?;
        budget.charge_allocation(TASK_HANDLE_BYTES)?;
        let id = self
            .next_task
            .checked_add(1)
            .ok_or(VmError::ResourceLimit {
                resource: RESOURCE_TASKS,
            })?;
        let mut captured = Vec::new();
        for argument in future.arguments() {
            task_handles(argument, &mut captured);
        }
        captured.sort_unstable();
        if captured.windows(2).any(|pair| pair[0] == pair[1])
            || captured.iter().any(|task_id| {
                self.tasks
                    .get(task_id)
                    .is_none_or(|task| task.owner != owner)
            })
        {
            return Err(VmError::Invariant(
                "captured task cannot transfer ownership",
            ));
        }
        self.next_task = id;
        let task = match future.as_ref() {
            FutureValue::Function {
                function,
                arguments,
            } => {
                let frame =
                    new_machine_frame(raw_module, *function, arguments, &[], None, 1, budget)?;
                MachineTask {
                    owner,
                    scope,
                    entry_function: *function,
                    frames: vec![frame],
                    state: MachineTaskState::Ready,
                }
            }
            external => {
                let (pending, poll, result_type, failure_family) =
                    self.start_effect_future(external, budget, effects)?;
                let state = match poll {
                    EffectPoll::Ready(value) => {
                        budget.complete_effect()?;
                        validate_provider_value(&value, &result_type, raw_module, failure_family)?;
                        charge_external_value(&value, budget, 0)?;
                        MachineTaskState::Completed(Ok(value))
                    }
                    EffectPoll::Pending => MachineTaskState::Waiting(WaitState::Effect {
                        pending,
                        destination: None,
                        result_type,
                        failure_family,
                    }),
                };
                MachineTask {
                    owner,
                    scope,
                    entry_function: origin_function,
                    frames: Vec::new(),
                    state,
                }
            }
        };
        let completed = matches!(task.state, MachineTaskState::Completed(_));
        let waiting = matches!(task.state, MachineTaskState::Waiting(_));
        self.tasks.insert(id, task);
        for captured in captured {
            self.tasks
                .get_mut(&captured)
                .expect("captured task exists")
                .owner = id;
        }
        budget.usage.tasks_started = budget.usage.tasks_started.saturating_add(1);
        budget.usage.maximum_live_tasks =
            budget.usage.maximum_live_tasks.max(live.saturating_add(1));
        budget.record_task_started(live.saturating_add(1));
        self.emit_event(budget.observer, id, owner, TaskEventKind::Spawned);
        if completed {
            self.emit_event(budget.observer, id, owner, TaskEventKind::Completed);
        } else if waiting {
            self.emit_event(budget.observer, id, owner, TaskEventKind::Waiting);
        }
        Ok(TaskValue { id })
    }
}

fn frame_size(registers: usize) -> Result<u64, VmError> {
    logical_size(FRAME_BASE_BYTES, registers, REGISTER_BYTES)
}

fn collection_size(length: usize, slot_bytes: u64) -> Result<u64, VmError> {
    logical_size(COLLECTION_BASE_BYTES, length, slot_bytes)
}

fn record_size(length: usize) -> Result<u64, VmError> {
    logical_size(COLLECTION_BASE_BYTES, length, VALUE_SLOT_BYTES)
}

fn allocated_string(value: String, budget: &mut Budget<'_>) -> Result<Value, VmError> {
    budget.charge_allocation(logical_size(8, value.len(), 1)?)?;
    Ok(Value::String(Rc::from(value)))
}

fn charge_external_value(
    value: &Value,
    budget: &mut Budget<'_>,
    depth: usize,
) -> Result<(), VmError> {
    if depth > MAX_VALUE_NESTING {
        return Err(VmError::ResourceLimit {
            resource: RESOURCE_ALLOCATION_BYTES,
        });
    }
    match value {
        Value::String(value) => budget.charge_allocation(logical_size(8, value.len(), 1)?)?,
        Value::Bytes(value) => budget.charge_allocation(logical_size(8, value.len(), 1)?)?,
        Value::List(values) | Value::Tuple(values) => {
            budget.charge_allocation(collection_size(values.len(), VALUE_SLOT_BYTES)?)?;
            for value in values.iter() {
                charge_external_value(value, budget, depth + 1)?;
            }
        }
        Value::Map(entries) => {
            budget.charge_allocation(collection_size(entries.len(), MAP_ENTRY_BYTES)?)?;
            for (key, value) in entries.iter() {
                charge_external_value(key, budget, depth + 1)?;
                charge_external_value(value, budget, depth + 1)?;
            }
        }
        Value::Record(fields) => {
            budget.charge_allocation(record_size(fields.len())?)?;
            for (_, value) in fields.iter() {
                charge_external_value(value, budget, depth + 1)?;
            }
        }
        Value::Enum(value) => match &value.payload {
            EnumPayload::Unit => budget.charge_allocation(enum_size(0)?)?,
            EnumPayload::Tuple(values) => {
                budget.charge_allocation(enum_size(values.len())?)?;
                for value in values.iter() {
                    charge_external_value(value, budget, depth + 1)?;
                }
            }
            EnumPayload::Record(fields) => {
                budget.charge_allocation(enum_size(fields.len())?)?;
                for (_, value) in fields.iter() {
                    charge_external_value(value, budget, depth + 1)?;
                }
            }
        },
        Value::Unknown(value) => {
            budget.charge_allocation(VALUE_SLOT_BYTES)?;
            charge_external_value(value, budget, depth + 1)?;
        }
        Value::Int(_)
        | Value::Bool(_)
        | Value::Float(_)
        | Value::ExternalFsAccess(_)
        | Value::Unit
        | Value::Workspace(_)
        | Value::SubAgent(_) => {}
        Value::Closure(_) | Value::Future(_) | Value::Task(_) => {
            return Err(VmError::Invariant("effect result contains an affine value"));
        }
    }
    Ok(())
}

fn validate_provider_value(
    value: &Value,
    expected: &ValueType,
    module: &allen_bytecode::Module,
    failure_family: EffectFailureFamily,
) -> Result<(), VmError> {
    match value_matches_type(value, expected, module, 0, &mut HashMap::new()) {
        Ok(true) => validate_closed_provider_result(value, failure_family),
        Ok(false) | Err(_) => Err(VmError::ProtocolViolation),
    }
}

fn validate_closed_provider_result(
    value: &Value,
    family: EffectFailureFamily,
) -> Result<(), VmError> {
    let Value::Enum(result) = value else {
        return Err(VmError::ProtocolViolation);
    };
    if result.identity != EnumIdentity::Result {
        return Err(VmError::ProtocolViolation);
    }
    if result.variant == 0 {
        return Ok(());
    }
    if result.variant != 1 {
        return Err(VmError::ProtocolViolation);
    }
    let EnumPayload::Tuple(payload) = &result.payload else {
        return Err(VmError::ProtocolViolation);
    };
    let Some(error) = payload.first() else {
        return Err(VmError::ProtocolViolation);
    };
    match family {
        EffectFailureFamily::Operation(operation) => {
            let Value::Record(fields) = error else {
                return Err(VmError::ProtocolViolation);
            };
            let (code, message) = standard_error_fields(fields)?;
            if message.len() > 1_024 || !operation_allows_error_code(operation, code) {
                return Err(VmError::ProtocolViolation);
            }
        }
        EffectFailureFamily::Tool => {
            let Value::Enum(error) = error else {
                return Err(VmError::ProtocolViolation);
            };
            if error.variant == 0 {
                return Ok(());
            }
            let EnumPayload::Record(fields) = &error.payload else {
                return Err(VmError::ProtocolViolation);
            };
            let (code, message) = standard_error_fields(fields)?;
            let valid = match error.variant {
                1 => matches!(code, "tool.unavailable" | "tool.denied"),
                2 => code == "tool.schema",
                _ => false,
            };
            if message.len() > 1_024 || !valid {
                return Err(VmError::ProtocolViolation);
            }
        }
    }
    Ok(())
}

fn standard_error_fields(fields: &[(Rc<str>, Value)]) -> Result<(&str, &str), VmError> {
    let mut code = None;
    let mut message = None;
    for (name, value) in fields {
        match (name.as_ref(), value) {
            ("code", Value::String(value)) => code = Some(value.as_ref()),
            ("message", Value::String(value)) => message = Some(value.as_ref()),
            _ => {}
        }
    }
    code.zip(message).ok_or(VmError::ProtocolViolation)
}

#[allow(clippy::too_many_lines)]
fn operation_allows_error_code(operation: FsOperation, code: &str) -> bool {
    match operation {
        FsOperation::ReadText => matches!(
            code,
            "fs.hard_link_denied"
                | "fs.invalid_path"
                | "fs.invalid_utf8"
                | "fs.io"
                | "fs.is_directory"
                | "fs.not_directory"
                | "fs.not_found"
                | "fs.permission_denied"
                | "fs.special_file_denied"
                | "fs.symlink_denied"
                | "fs.target_changed"
                | "fs.unavailable"
                | "fs.unsupported_platform"
        ),
        FsOperation::ReadBytes => matches!(
            code,
            "fs.hard_link_denied"
                | "fs.invalid_path"
                | "fs.io"
                | "fs.is_directory"
                | "fs.not_directory"
                | "fs.not_found"
                | "fs.permission_denied"
                | "fs.special_file_denied"
                | "fs.symlink_denied"
                | "fs.target_changed"
                | "fs.unavailable"
                | "fs.unsupported_platform"
        ),
        FsOperation::WriteText | FsOperation::WriteBytes => matches!(
            code,
            "fs.hard_link_denied"
                | "fs.invalid_path"
                | "fs.io"
                | "fs.not_directory"
                | "fs.permission_denied"
                | "fs.special_file_denied"
                | "fs.symlink_denied"
                | "fs.target_changed"
                | "fs.unavailable"
                | "fs.unsupported_platform"
        ),
        FsOperation::List | FsOperation::Search => matches!(
            code,
            "fs.hard_link_denied"
                | "fs.invalid_path"
                | "fs.invalid_utf8"
                | "fs.io"
                | "fs.not_directory"
                | "fs.not_found"
                | "fs.permission_denied"
                | "fs.special_file_denied"
                | "fs.symlink_denied"
                | "fs.target_changed"
                | "fs.unavailable"
                | "fs.unsupported_platform"
        ),
        FsOperation::HttpGet => matches!(
            code,
            "net.connect_timeout"
                | "net.destination_denied"
                | "net.dns"
                | "net.dns_timeout"
                | "net.first_byte_timeout"
                | "net.idle_timeout"
                | "net.invalid_limits"
                | "net.invalid_url"
                | "net.io"
                | "net.origin_denied"
                | "net.peer_mismatch"
                | "net.permission_denied"
                | "net.protocol"
                | "net.redirect_invalid"
                | "net.tls"
                | "net.total_timeout"
                | "net.unsupported_encoding"
                | "network.unavailable"
        ),
        FsOperation::PermissionRequestFile | FsOperation::PermissionRequestDirectory => {
            matches!(code, "permission.denied" | "permission.unavailable")
        }
        FsOperation::AgentMessage | FsOperation::AgentTranscript => {
            matches!(code, "agent.denied" | "agent.unavailable")
        }
        FsOperation::AgentAsk => matches!(
            code,
            "agent.denied" | "agent.unavailable" | "agent.validation_failed"
        ),
        FsOperation::ModelRequest => matches!(
            code,
            "model.denied" | "model.unavailable" | "model.validation_failed"
        ),
        FsOperation::UserAsk => matches!(
            code,
            "user.denied" | "user.unavailable" | "user.validation_failed"
        ),
        FsOperation::SubAgentCreate | FsOperation::SubAgentMessage => {
            matches!(code, "sub_agent.denied" | "sub_agent.unavailable")
        }
        FsOperation::SubAgentRun | FsOperation::SubAgentAsk => matches!(
            code,
            "sub_agent.denied" | "sub_agent.unavailable" | "sub_agent.validation_failed"
        ),
    }
}

fn effect_result_type(operation: FsOperation) -> ValueType {
    allen_bytecode::effect_result_type(operation, None).unwrap_or(ValueType::Never)
}

#[allow(clippy::too_many_lines)]
fn close_provider_error(
    family: EffectFailureFamily,
    result_type: &ValueType,
    error: VmError,
) -> Result<Value, VmError> {
    let (variant, code, message) = match (family, &error) {
        (
            EffectFailureFamily::Operation(
                FsOperation::ReadText
                | FsOperation::ReadBytes
                | FsOperation::WriteText
                | FsOperation::WriteBytes
                | FsOperation::List
                | FsOperation::Search,
            ),
            VmError::CapabilityMissing,
        ) => (1, "fs.unavailable", "filesystem provider is unavailable"),
        (EffectFailureFamily::Operation(FsOperation::HttpGet), VmError::CapabilityMissing) => {
            (1, "network.unavailable", "network provider is unavailable")
        }
        (
            EffectFailureFamily::Operation(
                FsOperation::PermissionRequestFile | FsOperation::PermissionRequestDirectory,
            ),
            VmError::CapabilityMissing | VmError::AgentUnavailable,
        ) => (
            1,
            "permission.unavailable",
            "permission provider is unavailable",
        ),
        (
            EffectFailureFamily::Operation(
                FsOperation::AgentMessage | FsOperation::AgentAsk | FsOperation::AgentTranscript,
            ),
            VmError::AgentUnavailable,
        ) => (1, "agent.unavailable", "the invoking agent is unavailable"),
        (
            EffectFailureFamily::Operation(
                FsOperation::AgentMessage | FsOperation::AgentAsk | FsOperation::AgentTranscript,
            ),
            VmError::CapabilityMissing,
        ) => (1, "agent.denied", "the invoking-agent operation was denied"),
        (EffectFailureFamily::Operation(FsOperation::AgentAsk), VmError::AgentResponseSchema) => (
            1,
            "agent.validation_failed",
            "the agent response failed validation",
        ),
        (EffectFailureFamily::Operation(FsOperation::ModelRequest), VmError::ModelUnavailable) => {
            (1, "model.unavailable", "the model provider is unavailable")
        }
        (EffectFailureFamily::Operation(FsOperation::ModelRequest), VmError::CapabilityMissing) => {
            (1, "model.denied", "the model operation was denied")
        }
        (
            EffectFailureFamily::Operation(FsOperation::ModelRequest),
            VmError::ModelValidationError,
        ) => (
            1,
            "model.validation_failed",
            "the model response failed validation",
        ),
        (EffectFailureFamily::Operation(FsOperation::UserAsk), VmError::UserUnavailable) => {
            (1, "user.unavailable", "the user provider is unavailable")
        }
        (EffectFailureFamily::Operation(FsOperation::UserAsk), VmError::CapabilityMissing) => {
            (1, "user.denied", "the user operation was denied")
        }
        (
            EffectFailureFamily::Operation(FsOperation::UserAsk),
            VmError::ResponseValidationError,
        ) => (
            1,
            "user.validation_failed",
            "the user response failed validation",
        ),
        (
            EffectFailureFamily::Operation(
                FsOperation::SubAgentCreate
                | FsOperation::SubAgentRun
                | FsOperation::SubAgentMessage
                | FsOperation::SubAgentAsk,
            ),
            VmError::SubAgentUnavailable,
        ) => (
            1,
            "sub_agent.unavailable",
            "the sub-agent provider is unavailable",
        ),
        (
            EffectFailureFamily::Operation(
                FsOperation::SubAgentCreate
                | FsOperation::SubAgentRun
                | FsOperation::SubAgentMessage
                | FsOperation::SubAgentAsk,
            ),
            VmError::CapabilityMissing,
        ) => (1, "sub_agent.denied", "the sub-agent operation was denied"),
        (
            EffectFailureFamily::Operation(FsOperation::SubAgentRun | FsOperation::SubAgentAsk),
            VmError::SubAgentResponseSchema,
        ) => (
            1,
            "sub_agent.validation_failed",
            "the sub-agent response failed validation",
        ),
        (EffectFailureFamily::Tool, VmError::ToolUnavailable) => {
            (1, "tool.unavailable", "the tool provider is unavailable")
        }
        (EffectFailureFamily::Tool, VmError::CapabilityMissing) => {
            (1, "tool.denied", "the tool operation was denied")
        }
        (EffectFailureFamily::Tool, VmError::ToolSchemaError) => (
            2,
            "tool.schema",
            "the tool response failed schema validation",
        ),
        _ => {
            return Err(match error {
                VmError::Cancelled
                | VmError::Timeout { .. }
                | VmError::ResourceLimit { .. }
                | VmError::ProtocolViolation
                | VmError::ReplayRuntimeDiverged => error,
                VmError::ReplayDiverged => VmError::ReplayRuntimeDiverged,
                _ => VmError::ProtocolViolation,
            });
        }
    };
    operational_error_result(result_type, variant, code, message)
}

fn operational_error_result(
    result_type: &ValueType,
    variant: u32,
    code: &'static str,
    message: &'static str,
) -> Result<Value, VmError> {
    let ValueType::Result(_, error_type) = result_type else {
        return Err(VmError::Invariant("provider result is not recoverable"));
    };
    let fields = || {
        vec![
            (Rc::from("code"), Value::String(Rc::from(code))),
            (Rc::from("message"), Value::String(Rc::from(message))),
        ]
        .into()
    };
    let error = match error_type.as_ref() {
        ValueType::Record(_) => Value::Record(fields()),
        ValueType::Enum(id) => Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::User(*id),
            type_name: Rc::from("<generated-tool-error>"),
            variant,
            variant_name: Rc::from(if variant == 2 {
                "Schema"
            } else {
                "Unavailable"
            }),
            payload: EnumPayload::Record(fields()),
        })),
        _ => return Err(VmError::Invariant("provider error type is not closed")),
    };
    Ok(Value::Enum(Rc::new(builtin_enum(
        EnumIdentity::Result,
        1,
        EnumPayload::Tuple(vec![error].into()),
    ))))
}

const fn is_agent_operation(operation: FsOperation) -> bool {
    matches!(
        operation,
        FsOperation::AgentMessage
            | FsOperation::AgentAsk
            | FsOperation::AgentTranscript
            | FsOperation::ModelRequest
            | FsOperation::UserAsk
            | FsOperation::SubAgentCreate
            | FsOperation::SubAgentRun
            | FsOperation::SubAgentMessage
            | FsOperation::SubAgentAsk
    )
}

fn enum_size(payload_length: usize) -> Result<u64, VmError> {
    logical_size(COLLECTION_BASE_BYTES, payload_length, VALUE_SLOT_BYTES)
}

fn logical_size(base: u64, count: usize, item_size: u64) -> Result<u64, VmError> {
    let count = u64::try_from(count).map_err(|_| VmError::ResourceLimit {
        resource: RESOURCE_ALLOCATION_BYTES,
    })?;
    count
        .checked_mul(item_size)
        .and_then(|items| base.checked_add(items))
        .ok_or(VmError::ResourceLimit {
            resource: RESOURCE_ALLOCATION_BYTES,
        })
}

struct Budget<'a> {
    limits: ExecutionLimits,
    usage: ExecutionUsage,
    started: Duration,
    clock: &'a mut dyn MonotonicClock,
    observer: &'a mut dyn CheckpointObserver,
    cancellation: &'a mut dyn CancellationSource,
    effects: ConcurrentEffectCounter,
    warned_resources: u8,
}

const WARNING_INSTRUCTIONS: u8 = 1 << 0;
const WARNING_ALLOCATION_BYTES: u8 = 1 << 1;
const WARNING_MAXIMUM_ALLOCATION_BYTES: u8 = 1 << 2;
const WARNING_CALL_DEPTH: u8 = 1 << 3;
const WARNING_WALL_TIME: u8 = 1 << 4;
const WARNING_TASKS: u8 = 1 << 5;
const WARNING_CONCURRENT_EFFECTS: u8 = 1 << 6;

fn warning_threshold(limit: u64) -> u64 {
    limit.saturating_sub((limit / 10).max(1))
}

impl<'a> Budget<'a> {
    fn new(
        limits: ExecutionLimits,
        clock: &'a mut dyn MonotonicClock,
        observer: &'a mut dyn CheckpointObserver,
        cancellation: &'a mut dyn CancellationSource,
    ) -> Self {
        let started = clock.now();
        Self {
            limits,
            usage: ExecutionUsage {
                instructions: 0,
                allocation_bytes: 0,
                maximum_call_depth: 0,
                tasks_started: 0,
                maximum_live_tasks: 0,
                maximum_concurrent_effects: 0,
            },
            started,
            clock,
            observer,
            cancellation,
            effects: ConcurrentEffectCounter::new(limits.concurrent_effects),
            warned_resources: 0,
        }
    }

    fn warn_once(&mut self, bit: u8, resource: &'static str, used: u64, limit: u64) {
        if limit == u64::MAX || self.warned_resources & bit != 0 || used < warning_threshold(limit)
        {
            return;
        }
        self.warned_resources |= bit;
        self.observer.budget_warning(BudgetWarning {
            resource,
            used,
            limit,
        });
    }

    fn enter_frame(&mut self, depth: u32) -> Result<(), VmError> {
        if depth > self.limits.call_depth {
            return Err(VmError::ResourceLimit {
                resource: RESOURCE_CALL_DEPTH,
            });
        }
        self.usage.maximum_call_depth = self.usage.maximum_call_depth.max(depth);
        if self.limits.call_depth != u32::MAX {
            self.warn_once(
                WARNING_CALL_DEPTH,
                RESOURCE_CALL_DEPTH,
                u64::from(depth),
                u64::from(self.limits.call_depth),
            );
        }
        Ok(())
    }

    fn check_interruption(&mut self) -> Result<(), VmError> {
        if self.cancellation.is_cancelled() {
            return Err(VmError::Cancelled);
        }
        let elapsed = self
            .clock
            .now()
            .checked_sub(self.started)
            .ok_or(VmError::Invariant("monotonic clock moved backwards"))?;
        if elapsed >= self.limits.wall_time {
            return Err(VmError::Timeout {
                resource: RESOURCE_WALL_TIME,
            });
        }
        if self.limits.wall_time != Duration::MAX {
            let used = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            let limit = u64::try_from(self.limits.wall_time.as_millis()).unwrap_or(u64::MAX);
            self.warn_once(WARNING_WALL_TIME, RESOURCE_WALL_TIME, used, limit);
        }
        Ok(())
    }

    fn charge_instruction(&mut self, checkpoint: Checkpoint) -> Result<(), VmError> {
        self.observer.checkpoint(checkpoint);
        self.check_interruption()?;
        let next = self
            .usage
            .instructions
            .checked_add(1)
            .ok_or(VmError::ResourceLimit {
                resource: RESOURCE_INSTRUCTIONS,
            })?;
        if next > self.limits.instructions {
            return Err(VmError::ResourceLimit {
                resource: RESOURCE_INSTRUCTIONS,
            });
        }
        self.usage.instructions = next;
        self.warn_once(
            WARNING_INSTRUCTIONS,
            RESOURCE_INSTRUCTIONS,
            next,
            self.limits.instructions,
        );
        Ok(())
    }

    fn charge_allocation(&mut self, bytes: u64) -> Result<(), VmError> {
        if bytes > self.limits.maximum_allocation_bytes {
            return Err(VmError::ResourceLimit {
                resource: RESOURCE_MAXIMUM_ALLOCATION_BYTES,
            });
        }
        self.warn_once(
            WARNING_MAXIMUM_ALLOCATION_BYTES,
            RESOURCE_MAXIMUM_ALLOCATION_BYTES,
            bytes,
            self.limits.maximum_allocation_bytes,
        );
        let next =
            self.usage
                .allocation_bytes
                .checked_add(bytes)
                .ok_or(VmError::ResourceLimit {
                    resource: RESOURCE_ALLOCATION_BYTES,
                })?;
        if next > self.limits.allocation_bytes {
            return Err(VmError::ResourceLimit {
                resource: RESOURCE_ALLOCATION_BYTES,
            });
        }
        self.usage.allocation_bytes = next;
        self.warn_once(
            WARNING_ALLOCATION_BYTES,
            RESOURCE_ALLOCATION_BYTES,
            next,
            self.limits.allocation_bytes,
        );
        Ok(())
    }

    fn start_effect(&mut self) -> Result<(), VmError> {
        self.effects.start()?;
        self.usage.maximum_concurrent_effects = self.effects.maximum();
        if self.limits.concurrent_effects != u32::MAX {
            self.warn_once(
                WARNING_CONCURRENT_EFFECTS,
                RESOURCE_CONCURRENT_EFFECTS,
                u64::from(self.effects.active()),
                u64::from(self.limits.concurrent_effects),
            );
        }
        Ok(())
    }

    fn record_task_started(&mut self, live: u32) {
        if self.limits.tasks != u32::MAX {
            self.warn_once(
                WARNING_TASKS,
                RESOURCE_TASKS,
                u64::from(live),
                u64::from(self.limits.tasks),
            );
        }
    }

    fn complete_effect(&mut self) -> Result<(), VmError> {
        self.effects.complete()
    }
}

fn materialize_constant(constant: &Constant, budget: &mut Budget<'_>) -> Result<Value, VmError> {
    match constant {
        Constant::Int(value) => Ok(Value::Int(*value)),
        Constant::Bool(value) => Ok(Value::Bool(*value)),
        Constant::Float(bits) => Ok(Value::Float(FloatValue::from_canonical_bits(*bits))),
        Constant::String(value) => {
            budget.charge_allocation(logical_size(8, value.len(), 1)?)?;
            Ok(Value::String(Rc::from(value.as_str())))
        }
        Constant::Bytes(value) => {
            budget.charge_allocation(logical_size(8, value.len(), 1)?)?;
            Ok(Value::Bytes(Rc::from(value.as_slice())))
        }
        Constant::ExternalFsAccess(value) => Ok(Value::ExternalFsAccess(*value)),
        Constant::Unit => Ok(Value::Unit),
    }
}

fn clone_registers(registers: &[Option<Value>], sources: &[u16]) -> Result<Vec<Value>, VmError> {
    sources
        .iter()
        .map(|source| read_register(registers, *source).cloned())
        .collect()
}

fn register_type(
    function: &allen_bytecode::Function,
    register: u16,
) -> Result<&ValueType, VmError> {
    function
        .registers
        .get(register as usize)
        .ok_or(VmError::Invariant("register type is missing"))
}

fn construct_record(
    registers: &[Option<Value>],
    layout: &[allen_bytecode::RecordField],
    fields: &[(u32, u16)],
) -> Result<Vec<(Rc<str>, Value)>, VmError> {
    if fields.len() != layout.len() {
        return Err(VmError::Invariant("record has the wrong field count"));
    }
    let mut values = Vec::with_capacity(layout.len());
    for (index, field) in layout.iter().enumerate() {
        let (field_index, source) = fields
            .get(index)
            .ok_or(VmError::Invariant("record field is missing"))?;
        if usize::try_from(*field_index).ok() != Some(index) {
            return Err(VmError::Invariant(
                "record fields are not in canonical order",
            ));
        }
        values.push((
            Rc::from(field.name.as_str()),
            read_register(registers, *source)?.clone(),
        ));
    }
    Ok(values)
}

fn construct_enum(
    module: &allen_bytecode::Module,
    value_type: &ValueType,
    variant: u32,
    registers: &[Option<Value>],
    sources: &[u16],
) -> Result<EnumValue, VmError> {
    let (identity, type_name, variant_name, payload_type) = match value_type {
        ValueType::Enum(type_id) => {
            let enum_type = module
                .enum_types
                .get(*type_id as usize)
                .ok_or(VmError::Invariant("enum type is missing"))?;
            let enum_variant = enum_type
                .variants
                .get(variant as usize)
                .ok_or(VmError::Invariant("enum variant is missing"))?;
            (
                EnumIdentity::User(*type_id),
                Rc::from(enum_type.name.as_str()),
                Rc::from(enum_variant.name.as_str()),
                &enum_variant.payload,
            )
        }
        ValueType::Option(element) => {
            let payload_type = match variant {
                0 => EnumPayloadType::Unit,
                1 => EnumPayloadType::Tuple(vec![(**element).clone()]),
                _ => return Err(VmError::Invariant("Option variant is missing")),
            };
            return construct_builtin_enum(
                EnumIdentity::Option,
                variant,
                &payload_type,
                registers,
                sources,
            );
        }
        ValueType::Result(ok, error) => {
            let payload_type = match variant {
                0 => EnumPayloadType::Tuple(vec![(**ok).clone()]),
                1 => EnumPayloadType::Tuple(vec![(**error).clone()]),
                _ => return Err(VmError::Invariant("Result variant is missing")),
            };
            return construct_builtin_enum(
                EnumIdentity::Result,
                variant,
                &payload_type,
                registers,
                sources,
            );
        }
        _ => return Err(VmError::Invariant("enum destination contains another type")),
    };
    let payload = construct_payload(payload_type, registers, sources)?;
    Ok(EnumValue {
        identity,
        type_name,
        variant,
        variant_name,
        payload,
    })
}

fn construct_builtin_enum(
    identity: EnumIdentity,
    variant: u32,
    payload_type: &EnumPayloadType,
    registers: &[Option<Value>],
    sources: &[u16],
) -> Result<EnumValue, VmError> {
    Ok(builtin_enum(
        identity,
        variant,
        construct_payload(payload_type, registers, sources)?,
    ))
}

fn builtin_enum(identity: EnumIdentity, variant: u32, payload: EnumPayload) -> EnumValue {
    let (type_name, variant_name) = match (identity, variant) {
        (EnumIdentity::Option, 0) => ("Option", "None"),
        (EnumIdentity::Option, 1) => ("Option", "Some"),
        (EnumIdentity::Result, 0) => ("Result", "Ok"),
        (EnumIdentity::Result, 1) => ("Result", "Err"),
        _ => ("", ""),
    };
    EnumValue {
        identity,
        type_name: Rc::from(type_name),
        variant,
        variant_name: Rc::from(variant_name),
        payload,
    }
}

fn construct_payload(
    payload_type: &EnumPayloadType,
    registers: &[Option<Value>],
    sources: &[u16],
) -> Result<EnumPayload, VmError> {
    match payload_type {
        EnumPayloadType::Unit => {
            if !sources.is_empty() {
                return Err(VmError::Invariant("unit variant has a payload"));
            }
            Ok(EnumPayload::Unit)
        }
        EnumPayloadType::Tuple(types) => {
            if sources.len() != types.len() {
                return Err(VmError::Invariant(
                    "tuple variant has the wrong payload count",
                ));
            }
            Ok(EnumPayload::Tuple(
                clone_registers(registers, sources)?.into(),
            ))
        }
        EnumPayloadType::Record(fields) => {
            if sources.len() != fields.len() {
                return Err(VmError::Invariant(
                    "record variant has the wrong payload count",
                ));
            }
            let values = fields
                .iter()
                .zip(sources)
                .map(|(field, source)| {
                    Ok((
                        Rc::from(field.name.as_str()),
                        read_register(registers, *source)?.clone(),
                    ))
                })
                .collect::<Result<Vec<_>, VmError>>()?;
            Ok(EnumPayload::Record(values.into()))
        }
    }
}

fn write_switch_bindings(
    registers: &mut [Option<Value>],
    payload: &EnumPayload,
    bindings: &[u16],
) -> Result<(), VmError> {
    let values: &[Value] = match payload {
        EnumPayload::Unit => &[],
        EnumPayload::Tuple(values) => values,
        EnumPayload::Record(fields) => {
            if bindings.len() != fields.len() {
                return Err(VmError::Invariant("record switch binding count is invalid"));
            }
            for ((_, value), binding) in fields.iter().zip(bindings) {
                write_register(registers, *binding, value.clone())?;
            }
            return Ok(());
        }
    };
    if values.len() != bindings.len() {
        return Err(VmError::Invariant("enum switch binding count is invalid"));
    }
    for (value, binding) in values.iter().zip(bindings) {
        write_register(registers, *binding, value.clone())?;
    }
    Ok(())
}

fn int_binary(operation: NumericBinaryOp, left: i64, right: i64) -> Result<i64, VmError> {
    match operation {
        NumericBinaryOp::Add => left.checked_add(right).ok_or(VmError::ArithmeticOverflow),
        NumericBinaryOp::Subtract => left.checked_sub(right).ok_or(VmError::ArithmeticOverflow),
        NumericBinaryOp::Multiply => left.checked_mul(right).ok_or(VmError::ArithmeticOverflow),
        NumericBinaryOp::Divide => {
            if right == 0 {
                Err(VmError::DivisionByZero)
            } else {
                left.checked_div(right).ok_or(VmError::ArithmeticOverflow)
            }
        }
    }
}

fn int_remainder(left: i64, right: i64) -> Result<i64, VmError> {
    if right == 0 {
        Err(VmError::DivisionByZero)
    } else if left == i64::MIN && right == -1 {
        Ok(0)
    } else {
        left.checked_rem(right).ok_or(VmError::ArithmeticOverflow)
    }
}

fn compare(operation: CompareOp, left: &Value, right: &Value) -> Result<bool, VmError> {
    match operation {
        CompareOp::Equal => Ok(language_equal(left, right)),
        CompareOp::NotEqual => Ok(!language_equal(left, right)),
        CompareOp::Less | CompareOp::LessEqual | CompareOp::Greater | CompareOp::GreaterEqual => {
            let ordering = language_order(left, right)?;
            Ok(match operation {
                CompareOp::Less => ordering.is_some_and(Ordering::is_lt),
                CompareOp::LessEqual => ordering.is_some_and(Ordering::is_le),
                CompareOp::Greater => ordering.is_some_and(Ordering::is_gt),
                CompareOp::GreaterEqual => ordering.is_some_and(Ordering::is_ge),
                CompareOp::Equal | CompareOp::NotEqual => unreachable!(),
            })
        }
    }
}

fn language_equal(left: &Value, right: &Value) -> bool {
    language_equal_with_cache(left, right, &mut HashMap::new())
}

fn language_equal_with_cache(
    left: &Value,
    right: &Value,
    cache: &mut HashMap<(u8, usize, usize), bool>,
) -> bool {
    let key = aggregate_identity(left).and_then(|(kind, left)| {
        aggregate_identity(right)
            .filter(|(right_kind, _)| *right_kind == kind)
            .map(|(_, right)| (kind, left, right))
    });
    if let Some(key) = key {
        if let Some(equal) = cache.get(&key) {
            return *equal;
        }
    }
    let equal = match (left, right) {
        (Value::Int(left), Value::Int(right)) => left == right,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Float(left), Value::Float(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Bytes(left), Value::Bytes(right)) => left == right,
        (Value::ExternalFsAccess(left), Value::ExternalFsAccess(right)) => left == right,
        (Value::Unit, Value::Unit) => true,
        (Value::List(left), Value::List(right)) | (Value::Tuple(left), Value::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(left, right)| language_equal_with_cache(left, right, cache))
        }
        (Value::Map(left), Value::Map(right)) => {
            left.len() == right.len()
                && left.iter().zip(right.iter()).all(
                    |((left_key, left_value), (right_key, right_value))| {
                        language_equal_with_cache(left_key, right_key, cache)
                            && language_equal_with_cache(left_value, right_value, cache)
                    },
                )
        }
        (Value::Record(left), Value::Record(right)) => {
            left.len() == right.len()
                && left.iter().zip(right.iter()).all(
                    |((left_name, left_value), (right_name, right_value))| {
                        left_name == right_name
                            && language_equal_with_cache(left_value, right_value, cache)
                    },
                )
        }
        (Value::Enum(left), Value::Enum(right)) => {
            left.identity == right.identity
                && left.variant == right.variant
                && payload_equal(&left.payload, &right.payload, cache)
        }
        (Value::Unknown(left), Value::Unknown(right)) => {
            language_equal_with_cache(left, right, cache)
        }
        _ => false,
    };
    if let Some(key) = key {
        cache.insert(key, equal);
    }
    equal
}

fn payload_equal(
    left: &EnumPayload,
    right: &EnumPayload,
    cache: &mut HashMap<(u8, usize, usize), bool>,
) -> bool {
    match (left, right) {
        (EnumPayload::Unit, EnumPayload::Unit) => true,
        (EnumPayload::Tuple(left), EnumPayload::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(left, right)| language_equal_with_cache(left, right, cache))
        }
        (EnumPayload::Record(left), EnumPayload::Record(right)) => {
            left.len() == right.len()
                && left.iter().zip(right.iter()).all(
                    |((left_name, left_value), (right_name, right_value))| {
                        left_name == right_name
                            && language_equal_with_cache(left_value, right_value, cache)
                    },
                )
        }
        _ => false,
    }
}

fn aggregate_identity(value: &Value) -> Option<(u8, usize)> {
    match value {
        Value::List(values) => Some((0, Rc::as_ptr(values).cast::<()>() as usize)),
        Value::Map(values) => Some((1, Rc::as_ptr(values).cast::<()>() as usize)),
        Value::Tuple(values) => Some((2, Rc::as_ptr(values).cast::<()>() as usize)),
        Value::Record(values) => Some((3, Rc::as_ptr(values).cast::<()>() as usize)),
        Value::Enum(value) => Some((4, Rc::as_ptr(value) as usize)),
        Value::Unknown(value) => Some((5, Rc::as_ptr(value) as usize)),
        Value::Int(_)
        | Value::Bool(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bytes(_)
        | Value::ExternalFsAccess(_)
        | Value::Unit
        | Value::Closure(_)
        | Value::Future(_)
        | Value::Task(_)
        | Value::Workspace(_)
        | Value::SubAgent(_) => None,
    }
}

fn value_matches_type(
    value: &Value,
    value_type: &ValueType,
    module: &allen_bytecode::Module,
    depth: usize,
    cache: &mut HashMap<(u8, usize, ValueType), bool>,
) -> Result<bool, VmError> {
    if depth > MAX_VALUE_NESTING {
        return Ok(false);
    }
    let key =
        aggregate_identity(value).map(|(kind, identity)| (kind, identity, value_type.clone()));
    if let Some(key) = &key {
        if let Some(matches) = cache.get(key) {
            return Ok(*matches);
        }
    }
    let matches = match (value, value_type) {
        (Value::Int(_), ValueType::Int)
        | (Value::Bool(_), ValueType::Bool)
        | (Value::Float(_), ValueType::Float)
        | (Value::String(_), ValueType::String)
        | (Value::Bytes(_), ValueType::Bytes)
        | (Value::ExternalFsAccess(_), ValueType::ExternalFsAccess)
        | (Value::Unit, ValueType::Unit)
        | (Value::Unknown(_), ValueType::Unknown)
        | (Value::Closure(_), ValueType::Function { .. })
        | (Value::Future(_), ValueType::Future(_))
        | (Value::Task(_), ValueType::Task(_))
        | (Value::Workspace(_), ValueType::Workspace)
        | (Value::SubAgent(_), ValueType::SubAgent) => Ok(true),
        (Value::List(values), ValueType::List(element)) => {
            values.iter().try_fold(true, |matches, value| {
                Ok(matches && value_matches_type(value, element, module, depth + 1, cache)?)
            })
        }
        (Value::Map(entries), ValueType::Map(key_type, value_type)) => {
            map_matches_type(entries, key_type, value_type, module, depth, cache)
        }
        (Value::Tuple(values), ValueType::Tuple(types)) => {
            if values.len() != types.len() {
                return Ok(false);
            }
            values
                .iter()
                .zip(types)
                .try_fold(true, |matches, (value, value_type)| {
                    Ok(matches && value_matches_type(value, value_type, module, depth + 1, cache)?)
                })
        }
        (Value::Record(values), ValueType::Record(fields)) => {
            record_matches_type(values, fields, module, depth, cache)
        }
        (Value::Enum(value), ValueType::Enum(type_id)) => {
            if value.identity != EnumIdentity::User(*type_id) {
                return Ok(false);
            }
            let enum_type = module
                .enum_types
                .get(*type_id as usize)
                .ok_or(VmError::Invariant("enum type is missing"))?;
            let variant = enum_type
                .variants
                .get(value.variant as usize)
                .ok_or(VmError::Invariant("enum variant is missing"))?;
            payload_matches_type(&value.payload, &variant.payload, module, depth, cache)
        }
        (Value::Enum(value), ValueType::Option(element)) => {
            if value.identity != EnumIdentity::Option {
                return Ok(false);
            }
            match (value.variant, &value.payload) {
                (0, EnumPayload::Unit) => Ok(true),
                (1, EnumPayload::Tuple(payload)) if payload.len() == 1 => {
                    value_matches_type(&payload[0], element, module, depth + 1, cache)
                }
                _ => Ok(false),
            }
        }
        (Value::Enum(value), ValueType::Result(ok, error)) => {
            if value.identity != EnumIdentity::Result {
                return Ok(false);
            }
            match (value.variant, &value.payload) {
                (0, EnumPayload::Tuple(payload)) if payload.len() == 1 => {
                    value_matches_type(&payload[0], ok, module, depth + 1, cache)
                }
                (1, EnumPayload::Tuple(payload)) if payload.len() == 1 => {
                    value_matches_type(&payload[0], error, module, depth + 1, cache)
                }
                _ => Ok(false),
            }
        }
        _ => Ok(false),
    }?;
    if let Some(key) = key {
        cache.insert(key, matches);
    }
    Ok(matches)
}

fn map_matches_type(
    entries: &[(Value, Value)],
    key_type: &ValueType,
    value_type: &ValueType,
    module: &allen_bytecode::Module,
    depth: usize,
    cache: &mut HashMap<(u8, usize, ValueType), bool>,
) -> Result<bool, VmError> {
    if entries
        .windows(2)
        .any(|pair| compare_map_keys(&pair[0].0, &pair[1].0) != Ordering::Less)
    {
        return Ok(false);
    }
    entries.iter().try_fold(true, |matches, (key, value)| {
        Ok(matches
            && value_matches_type(key, key_type, module, depth + 1, cache)?
            && value_matches_type(value, value_type, module, depth + 1, cache)?)
    })
}

fn record_matches_type(
    values: &[(Rc<str>, Value)],
    fields: &[allen_bytecode::RecordField],
    module: &allen_bytecode::Module,
    depth: usize,
    cache: &mut HashMap<(u8, usize, ValueType), bool>,
) -> Result<bool, VmError> {
    if values.len() != fields.len() {
        return Ok(false);
    }
    values
        .iter()
        .zip(fields)
        .try_fold(true, |matches, ((name, value), field)| {
            Ok(matches
                && name.as_ref() == field.name
                && value_matches_type(value, &field.value_type, module, depth + 1, cache)?)
        })
}

fn payload_matches_type(
    payload: &EnumPayload,
    payload_type: &EnumPayloadType,
    module: &allen_bytecode::Module,
    depth: usize,
    cache: &mut HashMap<(u8, usize, ValueType), bool>,
) -> Result<bool, VmError> {
    match (payload, payload_type) {
        (EnumPayload::Unit, EnumPayloadType::Unit) => Ok(true),
        (EnumPayload::Tuple(values), EnumPayloadType::Tuple(types)) => {
            if values.len() != types.len() {
                return Ok(false);
            }
            values
                .iter()
                .zip(types)
                .try_fold(true, |matches, (value, value_type)| {
                    Ok(matches && value_matches_type(value, value_type, module, depth + 1, cache)?)
                })
        }
        (EnumPayload::Record(values), EnumPayloadType::Record(fields)) => {
            record_matches_type(values, fields, module, depth, cache)
        }
        _ => Ok(false),
    }
}

fn language_order(left: &Value, right: &Value) -> Result<Option<Ordering>, VmError> {
    match (left, right) {
        (Value::Int(left), Value::Int(right)) => Ok(Some(left.cmp(right))),
        (Value::Float(left), Value::Float(right)) => Ok(left.as_f64().partial_cmp(&right.as_f64())),
        (Value::String(left), Value::String(right)) => {
            Ok(Some(left.as_bytes().cmp(right.as_bytes())))
        }
        (Value::Bytes(left), Value::Bytes(right)) => Ok(Some(left.cmp(right))),
        _ => Err(VmError::Invariant(
            "ordered comparison contains unsupported values",
        )),
    }
}

fn compare_map_keys(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        (Value::Int(left), Value::Int(right)) => left.cmp(right),
        (Value::String(left), Value::String(right)) => left.as_bytes().cmp(right.as_bytes()),
        (Value::Bytes(left), Value::Bytes(right)) => left.cmp(right),
        _ => Ordering::Equal,
    }
}

fn index_get(collection: &Value, index: &Value) -> Result<Value, VmError> {
    match (collection, index) {
        (Value::List(values), Value::Int(index)) => sequence_index(values, *index),
        (Value::Bytes(values), Value::Int(index)) => {
            let index = usize::try_from(*index).map_err(|_| VmError::IndexOutOfBounds)?;
            values
                .get(index)
                .map(|value| Value::Int(i64::from(*value)))
                .ok_or(VmError::IndexOutOfBounds)
        }
        (Value::Map(entries), key) => entries
            .iter()
            .find(|(entry_key, _)| language_equal(entry_key, key))
            .map(|(_, value)| value.clone())
            .ok_or(VmError::MapKeyNotFound),
        _ => Err(VmError::Invariant(
            "index instruction contains unsupported values",
        )),
    }
}

fn collection_length(collection: &Value) -> Result<i64, VmError> {
    let length = match collection {
        Value::Bytes(values) => values.len(),
        Value::String(value) => value.chars().count(),
        Value::List(values) => values.len(),
        Value::Map(entries) => entries.len(),
        _ => {
            return Err(VmError::Invariant(
                "length instruction contains unsupported value",
            ));
        }
    };
    i64::try_from(length).map_err(|_| VmError::Invariant("collection length exceeds Int"))
}

#[allow(clippy::too_many_lines)]
fn string_call(
    registers: &[Option<Value>],
    operation: StringOperation,
    arguments: &[Register],
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    match operation {
        StringOperation::ByteLength => {
            let [value] = arguments else {
                return Err(VmError::Invariant("String byte_length arity is invalid"));
            };
            let length = i64::try_from(read_string(registers, *value)?.len())
                .map_err(|_| VmError::Invariant("String byte length exceeds Int"))?;
            Ok(Value::Int(length))
        }
        StringOperation::Concat => {
            let [_, _] = arguments else {
                return Err(VmError::Invariant("String concat arity is invalid"));
            };
            concatenate_string_registers(registers, arguments, budget)
        }
        StringOperation::Get => {
            let [value, index] = arguments else {
                return Err(VmError::Invariant("String get arity is invalid"));
            };
            string_get(
                read_string(registers, *value)?,
                read_int(registers, *index)?,
                budget,
            )
        }
        StringOperation::Slice => {
            let [value, start, end] = arguments else {
                return Err(VmError::Invariant("String slice arity is invalid"));
            };
            string_slice(
                read_string(registers, *value)?,
                read_int(registers, *start)?,
                read_int(registers, *end)?,
                budget,
            )
        }
        StringOperation::Find => {
            let [value, needle] = arguments else {
                return Err(VmError::Invariant("String find arity is invalid"));
            };
            string_find(
                read_string(registers, *value)?,
                read_string(registers, *needle)?,
                budget,
            )
        }
        StringOperation::Contains => {
            let [value, needle] = arguments else {
                return Err(VmError::Invariant("String contains arity is invalid"));
            };
            Ok(Value::Bool(
                read_string(registers, *value)?.contains(read_string(registers, *needle)?),
            ))
        }
        StringOperation::StartsWith => {
            let [value, prefix] = arguments else {
                return Err(VmError::Invariant("String starts_with arity is invalid"));
            };
            Ok(Value::Bool(
                read_string(registers, *value)?.starts_with(read_string(registers, *prefix)?),
            ))
        }
        StringOperation::EndsWith => {
            let [value, suffix] = arguments else {
                return Err(VmError::Invariant("String ends_with arity is invalid"));
            };
            Ok(Value::Bool(
                read_string(registers, *value)?.ends_with(read_string(registers, *suffix)?),
            ))
        }
        StringOperation::Split => {
            let [value, separator] = arguments else {
                return Err(VmError::Invariant("String split arity is invalid"));
            };
            string_split(
                read_string(registers, *value)?,
                read_string(registers, *separator)?,
                budget,
            )
        }
        StringOperation::Join => {
            let [values, separator] = arguments else {
                return Err(VmError::Invariant("String join arity is invalid"));
            };
            string_join(
                read_string_list(registers, *values)?,
                read_string(registers, *separator)?,
                budget,
            )
        }
        StringOperation::TrimAscii => {
            let [value] = arguments else {
                return Err(VmError::Invariant("String trim_ascii arity is invalid"));
            };
            let trimmed = read_string(registers, *value)?.trim_matches(is_ascii_trim_char);
            allocate_string(trimmed, budget)
        }
        StringOperation::FromUtf8 => {
            let [value] = arguments else {
                return Err(VmError::Invariant("String from_utf8 arity is invalid"));
            };
            let Value::Bytes(value) = read_register(registers, *value)? else {
                return Err(VmError::Invariant("String from_utf8 operand is not Bytes"));
            };
            match std::str::from_utf8(value) {
                Ok(value) => allocated_some_string(value, budget),
                Err(_) => Ok(option_none()),
            }
        }
        StringOperation::TemplateConcat => {
            if arguments.is_empty() {
                return Err(VmError::Invariant(
                    "template concatenation has no String segments",
                ));
            }
            concatenate_string_registers(registers, arguments, budget)
        }
    }
}

fn capability_inspect(
    registers: &[Option<Value>],
    operation: CapabilityOperation,
    arguments: &[Register],
    capabilities: &ExecutionCapabilities,
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    match operation {
        CapabilityOperation::IsGranted => {
            let [name] = arguments else {
                return Err(VmError::Invariant("capability is_granted arity is invalid"));
            };
            Ok(Value::Bool(
                capabilities.contains(read_string(registers, *name)?),
            ))
        }
        CapabilityOperation::Granted => {
            if !arguments.is_empty() {
                return Err(VmError::Invariant("capability granted arity is invalid"));
            }
            let grant_count = capabilities.iter().len();
            let allocation = capabilities.iter().try_fold(
                collection_size(grant_count, VALUE_SLOT_BYTES)?,
                |total, name| checked_allocation_add(total, string_size(name)?),
            )?;
            budget.charge_allocation(allocation)?;
            let values = capabilities
                .iter()
                .map(|name| Value::String(Rc::from(name)))
                .collect::<Vec<_>>();
            Ok(Value::List(values.into()))
        }
    }
}

fn concatenate_string_registers(
    registers: &[Option<Value>],
    arguments: &[Register],
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    let bytes = arguments.iter().try_fold(0_usize, |total, register| {
        total
            .checked_add(read_string(registers, *register)?.len())
            .ok_or(VmError::ResourceLimit {
                resource: RESOURCE_ALLOCATION_BYTES,
            })
    })?;
    budget.charge_allocation(string_size_bytes(bytes)?)?;
    let mut output = String::with_capacity(bytes);
    for register in arguments {
        output.push_str(read_string(registers, *register)?);
    }
    Ok(Value::String(Rc::from(output)))
}

fn string_get(value: &str, index: i64, budget: &mut Budget<'_>) -> Result<Value, VmError> {
    let Ok(index) = usize::try_from(index) else {
        return Ok(option_none());
    };
    let Some((start, scalar)) = value.char_indices().nth(index) else {
        return Ok(option_none());
    };
    let end = start + scalar.len_utf8();
    allocated_some_string(&value[start..end], budget)
}

fn string_slice(
    value: &str,
    start: i64,
    end: i64,
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return Ok(option_none());
    };
    if start > end {
        return Ok(option_none());
    }
    let Some(start) = scalar_byte_index(value, start) else {
        return Ok(option_none());
    };
    let Some(end) = scalar_byte_index(value, end) else {
        return Ok(option_none());
    };
    allocated_some_string(&value[start..end], budget)
}

fn scalar_byte_index(value: &str, index: usize) -> Option<usize> {
    value
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(value.len()))
        .nth(index)
}

fn string_find(value: &str, needle: &str, budget: &mut Budget<'_>) -> Result<Value, VmError> {
    let Some(byte_index) = value.find(needle) else {
        return Ok(option_none());
    };
    let scalar_index = i64::try_from(value[..byte_index].chars().count())
        .map_err(|_| VmError::Invariant("String scalar index exceeds Int"))?;
    budget.charge_allocation(enum_size(1)?)?;
    Ok(option_some(Value::Int(scalar_index)))
}

fn string_split(value: &str, separator: &str, budget: &mut Budget<'_>) -> Result<Value, VmError> {
    if separator.is_empty() {
        return Ok(option_none());
    }
    let mut count = 0_usize;
    let mut allocation = enum_size(1)?;
    for field in value.split(separator) {
        count = count.checked_add(1).ok_or(VmError::ResourceLimit {
            resource: RESOURCE_ALLOCATION_BYTES,
        })?;
        allocation = checked_allocation_add(allocation, string_size(field)?)?;
    }
    allocation = checked_allocation_add(allocation, collection_size(count, VALUE_SLOT_BYTES)?)?;
    budget.charge_allocation(allocation)?;
    let fields = value
        .split(separator)
        .map(|field| Value::String(Rc::from(field)))
        .collect::<Vec<_>>();
    Ok(option_some(Value::List(fields.into())))
}

fn string_join(
    values: &[Value],
    separator: &str,
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    let mut bytes = 0_usize;
    for value in values {
        let Value::String(value) = value else {
            return Err(VmError::Invariant("String join list element is not String"));
        };
        bytes = bytes
            .checked_add(value.len())
            .ok_or(VmError::ResourceLimit {
                resource: RESOURCE_ALLOCATION_BYTES,
            })?;
    }
    let separator_bytes = separator
        .len()
        .checked_mul(values.len().saturating_sub(1))
        .ok_or(VmError::ResourceLimit {
            resource: RESOURCE_ALLOCATION_BYTES,
        })?;
    bytes = bytes
        .checked_add(separator_bytes)
        .ok_or(VmError::ResourceLimit {
            resource: RESOURCE_ALLOCATION_BYTES,
        })?;
    budget.charge_allocation(string_size_bytes(bytes)?)?;
    let mut output = String::with_capacity(bytes);
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push_str(separator);
        }
        let Value::String(value) = value else {
            unreachable!("String list elements were checked");
        };
        output.push_str(value);
    }
    Ok(Value::String(Rc::from(output)))
}

fn read_string(registers: &[Option<Value>], register: Register) -> Result<&str, VmError> {
    let Value::String(value) = read_register(registers, register)? else {
        return Err(VmError::Invariant("String operation operand is not String"));
    };
    Ok(value)
}

fn read_string_list(registers: &[Option<Value>], register: Register) -> Result<&[Value], VmError> {
    let Value::List(values) = read_register(registers, register)? else {
        return Err(VmError::Invariant("String operation operand is not List"));
    };
    Ok(values)
}

fn allocated_some_string(value: &str, budget: &mut Budget<'_>) -> Result<Value, VmError> {
    let allocation = checked_allocation_add(enum_size(1)?, string_size(value)?)?;
    budget.charge_allocation(allocation)?;
    Ok(option_some(Value::String(Rc::from(value))))
}

fn option_none() -> Value {
    Value::Enum(Rc::new(builtin_enum(
        EnumIdentity::Option,
        0,
        EnumPayload::Unit,
    )))
}

fn option_some(value: Value) -> Value {
    Value::Enum(Rc::new(builtin_enum(
        EnumIdentity::Option,
        1,
        EnumPayload::Tuple(vec![value].into()),
    )))
}

fn unavailable_result(
    result_type: &ValueType,
    code: &'static str,
    message: &'static str,
) -> Result<Value, VmError> {
    let ValueType::Result(_, error_type) = result_type else {
        return Err(VmError::Invariant("provider result is not recoverable"));
    };
    let fields = || {
        vec![
            (Rc::from("code"), Value::String(Rc::from(code))),
            (Rc::from("message"), Value::String(Rc::from(message))),
        ]
        .into()
    };
    let error = match error_type.as_ref() {
        ValueType::Record(_) => Value::Record(fields()),
        ValueType::Enum(id) => Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::User(*id),
            type_name: Rc::from("<generated-tool-error>"),
            variant: 1,
            variant_name: Rc::from("Unavailable"),
            payload: EnumPayload::Record(fields()),
        })),
        _ => return Err(VmError::Invariant("provider error type is not closed")),
    };
    Ok(Value::Enum(Rc::new(builtin_enum(
        EnumIdentity::Result,
        1,
        EnumPayload::Tuple(vec![error].into()),
    ))))
}

const fn is_ascii_trim_char(value: char) -> bool {
    matches!(value, ' ' | '\t' | '\n' | '\r' | '\u{000c}' | '\u{000b}')
}

fn string_size(value: &str) -> Result<u64, VmError> {
    string_size_bytes(value.len())
}

fn string_size_bytes(bytes: usize) -> Result<u64, VmError> {
    logical_size(8, bytes, 1)
}

fn checked_allocation_add(left: u64, right: u64) -> Result<u64, VmError> {
    left.checked_add(right).ok_or(VmError::ResourceLimit {
        resource: RESOURCE_ALLOCATION_BYTES,
    })
}

fn safe_collection_call(
    registers: &[Option<Value>],
    operation: SafeCollectionOperation,
    arguments: &[Register],
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    let found = match operation {
        SafeCollectionOperation::ListGet => {
            let [values, index] = arguments else {
                return Err(VmError::Invariant("list.get arity is invalid"));
            };
            let Value::List(values) = read_register(registers, *values)? else {
                return Err(VmError::Invariant("list.get operand is not List"));
            };
            usize::try_from(read_int(registers, *index)?)
                .ok()
                .and_then(|index| values.get(index).cloned())
        }
        SafeCollectionOperation::BytesGet => {
            let [values, index] = arguments else {
                return Err(VmError::Invariant("bytes.get arity is invalid"));
            };
            let Value::Bytes(values) = read_register(registers, *values)? else {
                return Err(VmError::Invariant("bytes.get operand is not Bytes"));
            };
            usize::try_from(read_int(registers, *index)?)
                .ok()
                .and_then(|index| values.get(index))
                .map(|value| Value::Int(i64::from(*value)))
        }
        SafeCollectionOperation::MapGet => {
            let [values, key] = arguments else {
                return Err(VmError::Invariant("map.get arity is invalid"));
            };
            let Value::Map(entries) = read_register(registers, *values)? else {
                return Err(VmError::Invariant("map.get operand is not Map"));
            };
            let key = read_register(registers, *key)?;
            entries
                .iter()
                .find(|(entry_key, _)| language_equal(entry_key, key))
                .map(|(_, value)| value.clone())
        }
        SafeCollectionOperation::ListTrySet => {
            let [values, index, replacement] = arguments else {
                return Err(VmError::Invariant("list.try_set arity is invalid"));
            };
            let Value::List(values) = read_register(registers, *values)? else {
                return Err(VmError::Invariant("list.try_set operand is not List"));
            };
            let Some(index) = usize::try_from(read_int(registers, *index)?)
                .ok()
                .filter(|index| *index < values.len())
            else {
                return Ok(option_none());
            };
            let allocation = checked_allocation_add(
                collection_size(values.len(), VALUE_SLOT_BYTES)?,
                enum_size(1)?,
            )?;
            budget.charge_allocation(allocation)?;
            let mut output = values.to_vec();
            output[index] = read_register(registers, *replacement)?.clone();
            return Ok(option_some(Value::List(output.into())));
        }
    };
    match found {
        Some(value) => {
            budget.charge_allocation(enum_size(1)?)?;
            Ok(option_some(value))
        }
        None => Ok(option_none()),
    }
}

fn checked_int_call(
    registers: &[Option<Value>],
    operation: CheckedIntOperation,
    arguments: &[Register],
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    let value = if operation == CheckedIntOperation::Negate {
        let [value] = arguments else {
            return Err(VmError::Invariant("int.checked_neg arity is invalid"));
        };
        read_int(registers, *value)?.checked_neg()
    } else {
        let [left, right] = arguments else {
            return Err(VmError::Invariant(
                "checked integer operation arity is invalid",
            ));
        };
        let left = read_int(registers, *left)?;
        let right = read_int(registers, *right)?;
        match operation {
            CheckedIntOperation::Add => left.checked_add(right),
            CheckedIntOperation::Subtract => left.checked_sub(right),
            CheckedIntOperation::Multiply => left.checked_mul(right),
            CheckedIntOperation::Divide => left.checked_div(right),
            CheckedIntOperation::Remainder if right == -1 => Some(0),
            CheckedIntOperation::Remainder => left.checked_rem(right),
            CheckedIntOperation::Negate => unreachable!("handled above"),
        }
    };
    match value {
        Some(value) => {
            budget.charge_allocation(enum_size(1)?)?;
            Ok(option_some(Value::Int(value)))
        }
        None => Ok(option_none()),
    }
}

fn map_entry_at(
    registers: &[Option<Value>],
    map: Register,
    index: Register,
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    let Value::Map(entries) = read_register(registers, map)? else {
        return Err(VmError::Invariant(
            "map entry access operand contains another type",
        ));
    };
    let index =
        usize::try_from(read_int(registers, index)?).map_err(|_| VmError::IndexOutOfBounds)?;
    let (key, value) = entries.get(index).ok_or(VmError::IndexOutOfBounds)?;
    budget.charge_allocation(collection_size(2, VALUE_SLOT_BYTES)?)?;
    Ok(Value::Tuple(vec![key.clone(), value.clone()].into()))
}

fn list_append(
    registers: &[Option<Value>],
    values: Register,
    value: Register,
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    let Value::List(input) = read_register(registers, values)? else {
        return Err(VmError::Invariant(
            "list append operand contains another type",
        ));
    };
    let output_length = input.len().checked_add(1).ok_or(VmError::ResourceLimit {
        resource: RESOURCE_ALLOCATION_BYTES,
    })?;
    budget.charge_allocation(collection_size(output_length, VALUE_SLOT_BYTES)?)?;
    let appended = read_register(registers, value)?.clone();
    let mut output = Vec::with_capacity(output_length);
    output.extend(input.iter().cloned());
    output.push(appended);
    Ok(Value::List(output.into()))
}

fn list_set(
    registers: &[Option<Value>],
    values: Register,
    index: Register,
    value: Register,
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    let Value::List(input) = read_register(registers, values)? else {
        return Err(VmError::Invariant("list set operand contains another type"));
    };
    let index = usize::try_from(read_int(registers, index)?)
        .ok()
        .filter(|index| *index < input.len())
        .ok_or(VmError::IndexOutOfBounds)?;
    budget.charge_allocation(collection_size(input.len(), VALUE_SLOT_BYTES)?)?;
    let replacement = read_register(registers, value)?.clone();
    let mut output = input.to_vec();
    output[index] = replacement;
    Ok(Value::List(output.into()))
}

fn sequence_index(values: &[Value], index: i64) -> Result<Value, VmError> {
    let index = usize::try_from(index).map_err(|_| VmError::IndexOutOfBounds)?;
    values.get(index).cloned().ok_or(VmError::IndexOutOfBounds)
}

#[allow(clippy::cast_precision_loss)]
fn convert(
    source: &Value,
    conversion: Conversion,
    budget: &mut Budget<'_>,
) -> Result<Value, VmError> {
    match (conversion, source) {
        (Conversion::IntToFloat, Value::Int(value)) => {
            Ok(Value::Float(FloatValue::new(*value as f64)))
        }
        (Conversion::ToString, Value::String(value)) => Ok(Value::String(Rc::clone(value))),
        (Conversion::ToString, Value::Bool(value)) => {
            allocate_string(if *value { "true" } else { "false" }, budget)
        }
        (Conversion::ToString, Value::Int(value)) => {
            let mut buffer = StackBuffer::<32>::new();
            write!(&mut buffer, "{value}")
                .map_err(|_| VmError::Invariant("Int text is too long"))?;
            allocate_string(buffer.as_str(), budget)
        }
        (Conversion::ToString, Value::Float(value)) => {
            let mut buffer = StackBuffer::<32>::new();
            write_float(&mut buffer, *value)
                .map_err(|_| VmError::Invariant("Float text is too long"))?;
            allocate_string(buffer.as_str(), budget)
        }
        (Conversion::StringToBytes, Value::String(value)) => {
            budget.charge_allocation(logical_size(8, value.len(), 1)?)?;
            Ok(Value::Bytes(Rc::from(value.as_bytes())))
        }
        _ => Err(VmError::Invariant("conversion contains unsupported values")),
    }
}

fn allocate_string(value: &str, budget: &mut Budget<'_>) -> Result<Value, VmError> {
    budget.charge_allocation(logical_size(8, value.len(), 1)?)?;
    Ok(Value::String(Rc::from(value)))
}

fn read_register(registers: &[Option<Value>], register: u16) -> Result<&Value, VmError> {
    registers
        .get(register as usize)
        .and_then(Option::as_ref)
        .ok_or(VmError::Invariant("register has no value"))
}

fn read_int(registers: &[Option<Value>], register: u16) -> Result<i64, VmError> {
    match read_register(registers, register)? {
        Value::Int(value) => Ok(*value),
        _ => Err(VmError::Invariant("integer operand contains another type")),
    }
}

fn read_float(registers: &[Option<Value>], register: u16) -> Result<FloatValue, VmError> {
    match read_register(registers, register)? {
        Value::Float(value) => Ok(*value),
        _ => Err(VmError::Invariant("float operand contains another type")),
    }
}

fn read_bool(registers: &[Option<Value>], register: u16) -> Result<bool, VmError> {
    match read_register(registers, register)? {
        Value::Bool(value) => Ok(*value),
        _ => Err(VmError::Invariant("Boolean operand contains another type")),
    }
}

fn write_register(
    registers: &mut [Option<Value>],
    register: u16,
    value: Value,
) -> Result<(), VmError> {
    let destination = registers
        .get_mut(register as usize)
        .ok_or(VmError::Invariant("destination register is missing"))?;
    *destination = Some(value);
    Ok(())
}

struct StackBuffer<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> StackBuffer<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            length: 0,
        }
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl<const N: usize> fmt::Write for StackBuffer<N> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let end = self.length.checked_add(value.len()).ok_or(fmt::Error)?;
        let destination = self.bytes.get_mut(self.length..end).ok_or(fmt::Error)?;
        destination.copy_from_slice(value.as_bytes());
        self.length = end;
        Ok(())
    }
}

fn write_float(writer: &mut impl fmt::Write, value: FloatValue) -> fmt::Result {
    let number = value.as_f64();
    if number.is_nan() {
        return writer.write_str("NaN");
    }
    if number == f64::INFINITY {
        return writer.write_str("Infinity");
    }
    if number == f64::NEG_INFINITY {
        return writer.write_str("-Infinity");
    }

    let mut buffer = StackBuffer::<32>::new();
    write!(&mut buffer, "{number:?}")?;
    writer.write_str(buffer.as_str())
}

#[cfg(test)]
mod tests {
    use super::{
        BudgetWarning, CancellationSource, CanonicalDecodeError, CanonicalEncodeError, Checkpoint,
        CheckpointObserver, ConcurrentEffectCounter, EffectProvider, EnumIdentity, EnumPayload,
        EnumValue, ExecutionCapabilities, ExecutionLimits, ExecutionOutcome, ExecutionResult,
        FloatValue, MonotonicClock, RESOURCE_CLEANUP_INSTRUCTIONS, RESOURCE_CONCURRENT_EFFECTS,
        RESOURCE_TASKS, TaskEvent, TaskEventKind, Value, VmError, WorkspaceValue, decode_canonical,
        decode_canonical_with_limit, encode_canonical, encode_canonical_with_limit, execute,
        execute_entry_with_capabilities_and_runtime_context, execute_entry_with_runtime_context,
        execute_with_context, execute_with_limits, execute_with_runtime_context, option_none,
        option_some,
    };
    use allen_bytecode::{
        CapabilityOperation, CheckedIntOperation, Constant, DebugInfo, DebugLocation,
        EnumPayloadType, EnumSwitchArm, EnumType, EnumVariant, ExternalFsAccess, FsOperation,
        Function, Instruction, Module, NumericBinaryOp, RecordField, SafeCollectionOperation,
        StringOperation, ValueType, VerifiedModule, external_file_request_type, file_error_type,
        http_response_type, network_error_type, permission_error_type, task_snapshot_type, verify,
    };
    use std::rc::Rc;
    use std::time::Duration;

    fn verified(
        constants: Vec<Constant>,
        registers: Vec<ValueType>,
        return_type: ValueType,
        code: Vec<Instruction>,
    ) -> allen_bytecode::VerifiedModule {
        verify(Module {
            constants,
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![Function {
                name: "main".to_owned(),
                parameters: vec![],
                captures: vec![],
                registers,
                return_type,
                effects: 0,
                code,
            }],
            async_functions: vec![],
            entry: 0,
        })
        .expect("module must verify")
    }

    fn verified_with_enums(
        constants: Vec<Constant>,
        enum_types: Vec<EnumType>,
        registers: Vec<ValueType>,
        return_type: ValueType,
        code: Vec<Instruction>,
    ) -> allen_bytecode::VerifiedModule {
        verify(Module {
            constants,
            enum_types,
            effect_sets: vec![vec![]],
            functions: vec![Function {
                name: "main".to_owned(),
                parameters: vec![],
                captures: vec![],
                registers,
                return_type,
                effects: 0,
                code,
            }],
            async_functions: vec![],
            entry: 0,
        })
        .expect("module must verify")
    }

    fn execute_string_operation(
        constants: Vec<Constant>,
        operation: StringOperation,
        arguments: Vec<u16>,
        result_type: ValueType,
    ) -> super::ExecutionResult {
        let mut registers = constants
            .iter()
            .map(Constant::value_type)
            .collect::<Vec<_>>();
        let destination = u16::try_from(registers.len()).expect("test register count fits");
        registers.push(result_type.clone());
        let mut code = constants
            .iter()
            .enumerate()
            .map(|(index, _)| Instruction::Const {
                destination: u16::try_from(index).expect("test register fits"),
                constant: u32::try_from(index).expect("test constant fits"),
            })
            .collect::<Vec<_>>();
        code.push(Instruction::StringCall {
            destination,
            operation,
            arguments,
        });
        code.push(Instruction::Return {
            source: destination,
        });
        let module = verified(constants, registers, result_type, code);
        execute_with_limits(&module, ExecutionLimits::default()).expect("String operation executes")
    }

    #[test]
    fn string_operations_use_unicode_scalars_and_safe_options() {
        let value = "aé\u{0301}😀";
        assert_eq!(
            execute_string_operation(
                vec![Constant::String(value.to_owned())],
                StringOperation::ByteLength,
                vec![0],
                ValueType::Int,
            )
            .value,
            Value::Int(9)
        );

        let length_module = verified(
            vec![Constant::String(value.to_owned())],
            vec![ValueType::String, ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Length {
                    destination: 1,
                    collection: 0,
                },
                Instruction::Return { source: 1 },
            ],
        );
        assert_eq!(execute(&length_module), Ok(Value::Int(4)));

        let option_string = ValueType::Option(Box::new(ValueType::String));
        assert_eq!(
            execute_string_operation(
                vec![Constant::String(value.to_owned()), Constant::Int(3)],
                StringOperation::Get,
                vec![0, 1],
                option_string.clone(),
            )
            .value
            .to_string(),
            "Some(\"\\u{1f600}\")"
        );
        assert_eq!(
            execute_string_operation(
                vec![
                    Constant::String(value.to_owned()),
                    Constant::Int(1),
                    Constant::Int(4),
                ],
                StringOperation::Slice,
                vec![0, 1, 2],
                option_string,
            )
            .value
            .to_string(),
            "Some(\"\\u{e9}\\u{301}\\u{1f600}\")"
        );
        assert_eq!(
            execute_string_operation(
                vec![
                    Constant::String(value.to_owned()),
                    Constant::String("\u{0301}".to_owned()),
                ],
                StringOperation::Find,
                vec![0, 1],
                ValueType::Option(Box::new(ValueType::Int)),
            )
            .value
            .to_string(),
            "Some(2)"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn string_library_covers_every_operation_and_edge_family() {
        assert_eq!(
            execute_string_operation(
                vec![
                    Constant::String("a\0".to_owned()),
                    Constant::String("😀".to_owned()),
                ],
                StringOperation::Concat,
                vec![0, 1],
                ValueType::String,
            )
            .value
            .to_string(),
            "\"a\\u{0}\\u{1f600}\""
        );
        assert_eq!(
            execute_string_operation(
                vec![
                    Constant::String("abc".to_owned()),
                    Constant::Int(2),
                    Constant::Int(1),
                ],
                StringOperation::Slice,
                vec![0, 1, 2],
                ValueType::Option(Box::new(ValueType::String)),
            )
            .value
            .to_string(),
            "None"
        );
        for (operation, expected) in [
            (StringOperation::Contains, Value::Bool(true)),
            (StringOperation::StartsWith, Value::Bool(true)),
            (StringOperation::EndsWith, Value::Bool(true)),
        ] {
            assert_eq!(
                execute_string_operation(
                    vec![
                        Constant::String("aaaa".to_owned()),
                        Constant::String("aa".to_owned()),
                    ],
                    operation,
                    vec![0, 1],
                    ValueType::Bool,
                )
                .value,
                expected
            );
        }
        assert_eq!(
            execute_string_operation(
                vec![
                    Constant::String("abc".to_owned()),
                    Constant::String(String::new()),
                ],
                StringOperation::Find,
                vec![0, 1],
                ValueType::Option(Box::new(ValueType::Int)),
            )
            .value
            .to_string(),
            "Some(0)"
        );
        assert_eq!(
            execute_string_operation(
                vec![
                    Constant::String("a,,b,".to_owned()),
                    Constant::String(",".to_owned()),
                ],
                StringOperation::Split,
                vec![0, 1],
                ValueType::Option(Box::new(ValueType::List(Box::new(ValueType::String)))),
            )
            .value
            .to_string(),
            "Some([\"a\", \"\", \"b\", \"\"])"
        );
        assert_eq!(
            execute_string_operation(
                vec![
                    Constant::String("abc".to_owned()),
                    Constant::String(String::new()),
                ],
                StringOperation::Split,
                vec![0, 1],
                ValueType::Option(Box::new(ValueType::List(Box::new(ValueType::String)))),
            )
            .value
            .to_string(),
            "None"
        );
        assert_eq!(
            execute_string_operation(
                vec![Constant::String(
                    " \t\n\r\u{000c}\u{000b}\0x\u{00a0} ".to_owned()
                )],
                StringOperation::TrimAscii,
                vec![0],
                ValueType::String,
            )
            .value
            .to_string(),
            "\"\\u{0}x\\u{a0}\""
        );
        assert_eq!(
            execute_string_operation(
                vec![Constant::Bytes(b"a\0\xc3\xa9".to_vec())],
                StringOperation::FromUtf8,
                vec![0],
                ValueType::Option(Box::new(ValueType::String)),
            )
            .value
            .to_string(),
            "Some(\"a\\u{0}\\u{e9}\")"
        );
        assert_eq!(
            execute_string_operation(
                vec![Constant::Bytes(vec![0xff])],
                StringOperation::FromUtf8,
                vec![0],
                ValueType::Option(Box::new(ValueType::String)),
            )
            .value
            .to_string(),
            "None"
        );
        assert_eq!(
            execute_string_operation(
                vec![
                    Constant::String("left".to_owned()),
                    Constant::String(String::new()),
                    Constant::String("right".to_owned()),
                ],
                StringOperation::TemplateConcat,
                vec![0, 1, 2],
                ValueType::String,
            )
            .value
            .to_string(),
            "\"leftright\""
        );

        let join = verified(
            vec![
                Constant::String("a".to_owned()),
                Constant::String(String::new()),
                Constant::String("😀".to_owned()),
                Constant::String("|".to_owned()),
            ],
            vec![
                ValueType::String,
                ValueType::String,
                ValueType::String,
                ValueType::String,
                ValueType::List(Box::new(ValueType::String)),
                ValueType::String,
            ],
            ValueType::String,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::Const {
                    destination: 2,
                    constant: 2,
                },
                Instruction::Const {
                    destination: 3,
                    constant: 3,
                },
                Instruction::ListNew {
                    destination: 4,
                    elements: vec![0, 1, 2],
                },
                Instruction::StringCall {
                    destination: 5,
                    operation: StringOperation::Join,
                    arguments: vec![4, 3],
                },
                Instruction::Return { source: 5 },
            ],
        );
        assert_eq!(execute(&join).unwrap().to_string(), "\"a||\\u{1f600}\"");
    }

    #[test]
    fn string_results_charge_atomically_and_none_charges_zero() {
        let option_string = ValueType::Option(Box::new(ValueType::String));
        let invalid_get = verified(
            vec![Constant::String("é".to_owned()), Constant::Int(9)],
            vec![ValueType::String, ValueType::Int, option_string.clone()],
            option_string,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::StringCall {
                    destination: 2,
                    operation: StringOperation::Get,
                    arguments: vec![0, 1],
                },
                Instruction::Return { source: 2 },
            ],
        );
        let baseline = super::frame_size(3).unwrap() + super::string_size("é").unwrap();
        let result = execute_with_limits(
            &invalid_get,
            ExecutionLimits {
                allocation_bytes: baseline,
                ..ExecutionLimits::default()
            },
        )
        .expect("None adds no logical allocation");
        assert_eq!(result.value.to_string(), "None");
        assert_eq!(result.usage.allocation_bytes, baseline);

        let concat = verified(
            vec![
                Constant::String("a".to_owned()),
                Constant::String("😀".to_owned()),
            ],
            vec![ValueType::String, ValueType::String, ValueType::String],
            ValueType::String,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::StringCall {
                    destination: 2,
                    operation: StringOperation::Concat,
                    arguments: vec![0, 1],
                },
                Instruction::Return { source: 2 },
            ],
        );
        assert_eq!(
            execute_with_limits(
                &concat,
                ExecutionLimits {
                    maximum_allocation_bytes: 12,
                    ..ExecutionLimits::default()
                }
            )
            .unwrap_err(),
            VmError::ResourceLimit {
                resource: "maximum_allocation_bytes"
            }
        );
        assert_eq!(execute(&concat).unwrap().to_string(), "\"a\\u{1f600}\"");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn capability_inspection_uses_only_the_explicit_immutable_snapshot() {
        let module = verify(Module {
            constants: vec![],
            enum_types: vec![],
            effect_sets: vec![vec!["capability.inspect".to_owned()]],
            functions: vec![Function {
                name: "main".to_owned(),
                parameters: vec![],
                captures: vec![],
                registers: vec![ValueType::List(Box::new(ValueType::String))],
                return_type: ValueType::List(Box::new(ValueType::String)),
                effects: 0,
                code: vec![
                    Instruction::CapabilityInspect {
                        destination: 0,
                        operation: CapabilityOperation::Granted,
                        arguments: vec![],
                    },
                    Instruction::Return { source: 0 },
                ],
            }],
            async_functions: vec![],
            entry: 0,
        })
        .expect("capability inspection verifies");
        assert_eq!(execute(&module).unwrap().to_string(), "[]");

        let capabilities = ExecutionCapabilities::new([
            "tool.hidden".to_owned(),
            "agent.ask".to_owned(),
            "fs.write".to_owned(),
            "fs.write".to_owned(),
        ]);
        let outcome = execute_entry_with_capabilities_and_runtime_context(
            &module,
            None,
            0,
            &[],
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut super::NeverCancelled,
            &mut super::NoEffects,
            &capabilities,
        )
        .expect("snapshot execution succeeds");
        let ExecutionOutcome::Completed(result) = outcome else {
            panic!("inspection must complete");
        };
        assert_eq!(result.value.to_string(), "[\"fs.write\"]");

        let child_module = verify(Module {
            constants: vec![Constant::String("fs.write".to_owned())],
            enum_types: vec![],
            effect_sets: vec![
                vec!["capability.inspect".to_owned()],
                vec!["capability.inspect".to_owned(), "task.spawn".to_owned()],
            ],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Bool)),
                        ValueType::Task(Box::new(ValueType::Bool)),
                        ValueType::Bool,
                    ],
                    return_type: ValueType::Bool,
                    effects: 1,
                    code: vec![
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 0,
                        },
                        Instruction::Await {
                            destination: 2,
                            source: 1,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
                Function {
                    name: "child".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::String, ValueType::Bool],
                    return_type: ValueType::Bool,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::CapabilityInspect {
                            destination: 1,
                            operation: CapabilityOperation::IsGranted,
                            arguments: vec![0],
                        },
                        Instruction::Return { source: 1 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        })
        .expect("child capability inspection verifies");
        let child_outcome = execute_entry_with_capabilities_and_runtime_context(
            &child_module,
            None,
            0,
            &[],
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut super::NeverCancelled,
            &mut super::NoEffects,
            &capabilities,
        )
        .expect("child task shares the immutable snapshot");
        let ExecutionOutcome::Completed(child_result) = child_outcome else {
            panic!("child inspection must complete");
        };
        assert_eq!(child_result.value, Value::Bool(true));
    }

    fn shared_enum_dag(depth: usize) -> Vec<EnumType> {
        let mut types = vec![EnumType {
            name: "E0".to_owned(),
            variants: vec![EnumVariant {
                name: "Leaf".to_owned(),
                payload: EnumPayloadType::Unit,
            }],
        }];
        for index in 1..=depth {
            let previous = ValueType::Enum(u32::try_from(index - 1).expect("test index fits"));
            types.push(EnumType {
                name: format!("E{index}"),
                variants: vec![EnumVariant {
                    name: "Pair".to_owned(),
                    payload: EnumPayloadType::Tuple(vec![previous.clone(), previous]),
                }],
            });
        }
        types
    }

    #[test]
    fn executes_verified_integer_addition_repeatedly() {
        let module = verified(
            vec![Constant::Int(40), Constant::Int(2)],
            vec![ValueType::Int, ValueType::Int, ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::IntBinary {
                    operation: NumericBinaryOp::Add,
                    destination: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { source: 2 },
            ],
        );

        assert_eq!(execute(&module), Ok(Value::Int(42)));
        assert_eq!(execute(&module), Ok(Value::Int(42)));
    }

    #[test]
    fn executes_records_enums_and_control_flow() {
        let record_type = ValueType::Record(vec![RecordField {
            name: "value".to_owned(),
            value_type: ValueType::Int,
        }]);
        let module = verified_with_enums(
            vec![Constant::Int(7)],
            vec![EnumType {
                name: "Reading".to_owned(),
                variants: vec![
                    EnumVariant {
                        name: "Empty".to_owned(),
                        payload: EnumPayloadType::Unit,
                    },
                    EnumVariant {
                        name: "Number".to_owned(),
                        payload: EnumPayloadType::Tuple(vec![ValueType::Int]),
                    },
                ],
            }],
            vec![
                ValueType::Int,
                record_type,
                ValueType::Int,
                ValueType::Enum(0),
                ValueType::Int,
            ],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::RecordNew {
                    destination: 1,
                    fields: vec![(0, 0)],
                },
                Instruction::FieldGet {
                    destination: 2,
                    record: 1,
                    field: 0,
                },
                Instruction::EnumNew {
                    destination: 3,
                    variant: 1,
                    payload: vec![2],
                },
                Instruction::SwitchEnum {
                    source: 3,
                    arms: vec![
                        EnumSwitchArm {
                            variant: 0,
                            target: 6,
                            bindings: vec![],
                        },
                        EnumSwitchArm {
                            variant: 1,
                            target: 5,
                            bindings: vec![4],
                        },
                    ],
                },
                Instruction::Return { source: 4 },
                Instruction::Return { source: 2 },
            ],
        );

        assert_eq!(execute(&module), Ok(Value::Int(7)));

        let branch_module = verified(
            vec![Constant::Bool(false), Constant::Int(1), Constant::Int(2)],
            vec![ValueType::Bool, ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::BranchBool {
                    condition: 0,
                    true_target: 2,
                    false_target: 4,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::Jump { target: 5 },
                Instruction::Const {
                    destination: 1,
                    constant: 2,
                },
                Instruction::Return { source: 1 },
            ],
        );
        assert_eq!(execute(&branch_module), Ok(Value::Int(2)));
    }

    #[test]
    fn branch_executes_one_arm_and_return_exits_immediately() {
        let module = verified(
            vec![
                Constant::Bool(true),
                Constant::Int(1),
                Constant::Int(2),
                Constant::Int(3),
            ],
            vec![ValueType::Bool, ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::BranchBool {
                    condition: 0,
                    true_target: 2,
                    false_target: 4,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::Return { source: 1 },
                Instruction::Const {
                    destination: 1,
                    constant: 2,
                },
                Instruction::Jump { target: 6 },
                Instruction::Const {
                    destination: 1,
                    constant: 3,
                },
                Instruction::Return { source: 1 },
            ],
        );
        let mut observer = RecordingObserver::default();
        let result = execute_with_context(
            &module,
            None,
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut observer,
        )
        .expect("selected branch returns normally");

        assert_eq!(result.value, Value::Int(1));
        assert_eq!(
            observer.0,
            vec![
                Checkpoint {
                    function: 0,
                    instruction: 0,
                },
                Checkpoint {
                    function: 0,
                    instruction: 1,
                },
                Checkpoint {
                    function: 0,
                    instruction: 2,
                },
                Checkpoint {
                    function: 0,
                    instruction: 3,
                },
            ]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn skipped_branch_has_no_allocation_spawn_effect_trap_or_stop() {
        let module = verify(Module {
            constants: vec![
                Constant::Bool(false),
                Constant::Unit,
                Constant::String("must stay skipped".to_owned()),
                Constant::Int(0),
            ],
            enum_types: vec![],
            effect_sets: vec![
                vec![],
                vec!["agent.message".to_owned(), "task.spawn".to_owned()],
            ],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Bool,
                        ValueType::Unit,
                        ValueType::String,
                        ValueType::Int,
                        ValueType::List(Box::new(ValueType::Int)),
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Future(Box::new(ValueType::Result(
                            Box::new(ValueType::Unit),
                            Box::new(allen_bytecode::agent_error_type()),
                        ))),
                        ValueType::Result(
                            Box::new(ValueType::Unit),
                            Box::new(allen_bytecode::agent_error_type()),
                        ),
                        ValueType::Int,
                    ],
                    return_type: ValueType::Unit,
                    effects: 1,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Const {
                            destination: 1,
                            constant: 1,
                        },
                        Instruction::BranchBool {
                            condition: 0,
                            true_target: 3,
                            false_target: 12,
                        },
                        Instruction::Const {
                            destination: 2,
                            constant: 2,
                        },
                        Instruction::Const {
                            destination: 3,
                            constant: 3,
                        },
                        Instruction::ListNew {
                            destination: 4,
                            elements: vec![3],
                        },
                        Instruction::AsyncCall {
                            destination: 5,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 6,
                            future: 5,
                            scope: 0,
                        },
                        Instruction::EffectCall {
                            destination: 7,
                            operation: FsOperation::AgentMessage,
                            arguments: vec![2],
                        },
                        Instruction::Await {
                            destination: 8,
                            source: 7,
                        },
                        Instruction::IntBinary {
                            destination: 9,
                            left: 3,
                            right: 3,
                            operation: NumericBinaryOp::Divide,
                        },
                        Instruction::Stop { reason: 2 },
                        Instruction::Return { source: 1 },
                    ],
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 3,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        })
        .expect("the skipped-hazard module verifies");
        let mut provider = AgentEffect {
            calls: 0,
            cancelled: 0,
            value: Value::Unit,
        };
        let outcome = execute_entry_with_runtime_context(
            &module,
            None,
            0,
            &[],
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut super::NeverCancelled,
            &mut provider,
        )
        .expect("the unselected branch cannot affect execution");
        let ExecutionOutcome::Completed(result) = outcome else {
            panic!("false condition must return rather than stop");
        };

        assert_eq!(result.value, Value::Unit);
        assert_eq!(
            result.usage.allocation_bytes,
            super::frame_size(10).unwrap()
        );
        assert_eq!(result.usage.tasks_started, 0);
        assert_eq!(provider.calls, 0);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn try_result_preserves_err_and_narrow_charges_before_construction() {
        let result_type = ValueType::Result(Box::new(ValueType::Int), Box::new(ValueType::String));
        let ok_module = verified(
            vec![Constant::Int(7)],
            vec![
                ValueType::Int,
                result_type.clone(),
                ValueType::Int,
                result_type.clone(),
            ],
            result_type.clone(),
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::EnumNew {
                    destination: 1,
                    variant: 0,
                    payload: vec![0],
                },
                Instruction::TryResult {
                    destination: 2,
                    source: 1,
                },
                Instruction::EnumNew {
                    destination: 3,
                    variant: 0,
                    payload: vec![2],
                },
                Instruction::Return { source: 3 },
            ],
        );
        assert_eq!(execute(&ok_module).unwrap().to_string(), "Ok(7)");

        let err_module = verified(
            vec![Constant::String("bad".to_owned()), Constant::Int(99)],
            vec![
                ValueType::String,
                result_type.clone(),
                ValueType::Int,
                result_type.clone(),
            ],
            result_type.clone(),
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::EnumNew {
                    destination: 1,
                    variant: 1,
                    payload: vec![0],
                },
                Instruction::TryResult {
                    destination: 2,
                    source: 1,
                },
                Instruction::Const {
                    destination: 2,
                    constant: 1,
                },
                Instruction::EnumNew {
                    destination: 3,
                    variant: 0,
                    payload: vec![2],
                },
                Instruction::Return { source: 3 },
            ],
        );
        assert_eq!(execute(&err_module).unwrap().to_string(), "Err(\"bad\")");

        let narrow_module = verified(
            vec![Constant::Int(7)],
            vec![
                ValueType::Int,
                ValueType::Unknown,
                ValueType::Option(Box::new(ValueType::Int)),
            ],
            ValueType::Option(Box::new(ValueType::Int)),
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::ToUnknown {
                    destination: 1,
                    source: 0,
                },
                Instruction::Narrow {
                    destination: 2,
                    source: 1,
                    target: ValueType::Int,
                },
                Instruction::Return { source: 2 },
            ],
        );
        assert_eq!(execute(&narrow_module).unwrap().to_string(), "Some(7)");
        assert_eq!(
            execute_with_limits(
                &narrow_module,
                ExecutionLimits {
                    allocation_bytes: 119,
                    ..ExecutionLimits::default()
                },
            ),
            Err(VmError::ResourceLimit {
                resource: "allocation_bytes"
            })
        );

        let failed_narrow_module = verified(
            vec![Constant::Int(7)],
            vec![
                ValueType::Int,
                ValueType::Unknown,
                ValueType::Option(Box::new(ValueType::String)),
                ValueType::Unknown,
                ValueType::Option(Box::new(ValueType::Option(Box::new(ValueType::String)))),
            ],
            ValueType::Option(Box::new(ValueType::Option(Box::new(ValueType::String)))),
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::ToUnknown {
                    destination: 1,
                    source: 0,
                },
                Instruction::Narrow {
                    destination: 2,
                    source: 1,
                    target: ValueType::String,
                },
                Instruction::ToUnknown {
                    destination: 3,
                    source: 2,
                },
                Instruction::Narrow {
                    destination: 4,
                    source: 3,
                    target: ValueType::Option(Box::new(ValueType::String)),
                },
                Instruction::Return { source: 4 },
            ],
        );
        assert_eq!(
            execute(&failed_narrow_module).unwrap().to_string(),
            "Some(None)"
        );
    }

    #[test]
    fn equality_and_narrow_visit_shared_enum_dags_once() {
        const DEPTH: usize = 24;
        let enum_types = shared_enum_dag(DEPTH);

        let mut registers = Vec::new();
        let mut code = Vec::new();
        for branch in 0..2 {
            let offset = branch * (DEPTH + 1);
            for index in 0..=DEPTH {
                registers.push(ValueType::Enum(
                    u32::try_from(index).expect("test index fits"),
                ));
                code.push(Instruction::EnumNew {
                    destination: u16::try_from(offset + index).expect("test register fits"),
                    variant: 0,
                    payload: if index == 0 {
                        Vec::new()
                    } else {
                        let previous =
                            u16::try_from(offset + index - 1).expect("test register fits");
                        vec![previous, previous]
                    },
                });
            }
        }
        let comparison = u16::try_from(registers.len()).expect("test register fits");
        registers.push(ValueType::Bool);
        code.push(Instruction::Compare {
            destination: comparison,
            left: u16::try_from(DEPTH).expect("test register fits"),
            right: u16::try_from(2 * DEPTH + 1).expect("test register fits"),
            operation: allen_bytecode::CompareOp::Equal,
        });
        code.push(Instruction::Return { source: comparison });
        let equality = verified_with_enums(
            Vec::new(),
            enum_types.clone(),
            registers,
            ValueType::Bool,
            code,
        );
        assert_eq!(execute(&equality), Ok(Value::Bool(true)));

        let mut registers = Vec::new();
        let mut code = Vec::new();
        for index in 0..=DEPTH {
            registers.push(ValueType::Enum(
                u32::try_from(index).expect("test index fits"),
            ));
            code.push(Instruction::EnumNew {
                destination: u16::try_from(index).expect("test register fits"),
                variant: 0,
                payload: if index == 0 {
                    Vec::new()
                } else {
                    let previous = u16::try_from(index - 1).expect("test register fits");
                    vec![previous, previous]
                },
            });
        }
        let unknown = u16::try_from(registers.len()).expect("test register fits");
        registers.push(ValueType::Unknown);
        let target = ValueType::Enum(u32::try_from(DEPTH).expect("test index fits"));
        let narrowed = u16::try_from(registers.len()).expect("test register fits");
        registers.push(ValueType::Option(Box::new(target.clone())));
        code.push(Instruction::ToUnknown {
            destination: unknown,
            source: u16::try_from(DEPTH).expect("test register fits"),
        });
        code.push(Instruction::Narrow {
            destination: narrowed,
            source: unknown,
            target: target.clone(),
        });
        code.push(Instruction::Return { source: narrowed });
        let narrowing = verified_with_enums(
            Vec::new(),
            enum_types,
            registers,
            ValueType::Option(Box::new(target)),
            code,
        );
        let Value::Enum(option) = execute(&narrowing).expect("narrow must execute") else {
            panic!("narrow result must be Option");
        };
        assert_eq!(option.identity, EnumIdentity::Option);
        assert_eq!(option.variant, 1);
    }

    #[test]
    fn canonical_allocations_are_refused_before_construction() {
        let record = ValueType::Record(vec![RecordField {
            name: "x".to_owned(),
            value_type: ValueType::Int,
        }]);
        let record_module = verified(
            vec![Constant::Int(1)],
            vec![ValueType::Int, record.clone()],
            record,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::RecordNew {
                    destination: 1,
                    fields: vec![(0, 0)],
                },
                Instruction::Return { source: 1 },
            ],
        );
        assert_eq!(
            execute_with_limits(
                &record_module,
                ExecutionLimits {
                    allocation_bytes: 87,
                    ..ExecutionLimits::default()
                },
            ),
            Err(VmError::ResourceLimit {
                resource: "allocation_bytes"
            })
        );

        let enum_type = ValueType::Option(Box::new(ValueType::Int));
        let enum_module = verified(
            vec![Constant::Int(1)],
            vec![ValueType::Int, enum_type.clone()],
            enum_type,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::EnumNew {
                    destination: 1,
                    variant: 1,
                    payload: vec![0],
                },
                Instruction::Return { source: 1 },
            ],
        );
        assert_eq!(
            execute_with_limits(
                &enum_module,
                ExecutionLimits {
                    allocation_bytes: 87,
                    ..ExecutionLimits::default()
                },
            ),
            Err(VmError::ResourceLimit {
                resource: "allocation_bytes"
            })
        );

        let unknown_module = verified(
            vec![Constant::Int(1)],
            vec![ValueType::Int, ValueType::Unknown],
            ValueType::Unknown,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::ToUnknown {
                    destination: 1,
                    source: 0,
                },
                Instruction::Return { source: 1 },
            ],
        );
        assert_eq!(
            execute_with_limits(
                &unknown_module,
                ExecutionLimits {
                    allocation_bytes: 79,
                    ..ExecutionLimits::default()
                },
            ),
            Err(VmError::ResourceLimit {
                resource: "allocation_bytes"
            })
        );
    }

    #[test]
    fn reports_stable_integer_failures() {
        for (operation, left, right, expected) in [
            (
                NumericBinaryOp::Add,
                i64::MAX,
                1,
                VmError::ArithmeticOverflow,
            ),
            (
                NumericBinaryOp::Subtract,
                i64::MIN,
                1,
                VmError::ArithmeticOverflow,
            ),
            (
                NumericBinaryOp::Multiply,
                i64::MAX,
                2,
                VmError::ArithmeticOverflow,
            ),
            (
                NumericBinaryOp::Divide,
                i64::MIN,
                -1,
                VmError::ArithmeticOverflow,
            ),
            (NumericBinaryOp::Divide, 1, 0, VmError::DivisionByZero),
        ] {
            let module = verified(
                vec![Constant::Int(left), Constant::Int(right)],
                vec![ValueType::Int, ValueType::Int, ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::IntBinary {
                        operation,
                        destination: 2,
                        left: 0,
                        right: 1,
                    },
                    Instruction::Return { source: 2 },
                ],
            );

            let error = execute(&module).expect_err("operation must fail");
            assert_eq!(error, expected);
            assert!(matches!(
                error.code(),
                "arithmetic.overflow" | "arithmetic.division_by_zero"
            ));
        }
    }

    #[test]
    fn executes_integer_remainder_with_truncating_division_semantics() {
        for (left, right, expected) in [
            (7, 3, 1),
            (-7, 3, -1),
            (7, -3, 1),
            (-7, -3, -1),
            (6, 3, 0),
            (i64::MIN, -1, 0),
            (i64::MAX, 2, 1),
        ] {
            let module = verified(
                vec![Constant::Int(left), Constant::Int(right)],
                vec![ValueType::Int, ValueType::Int, ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::IntRemainder {
                        destination: 2,
                        left: 0,
                        right: 1,
                    },
                    Instruction::Return { source: 2 },
                ],
            );
            assert_eq!(execute(&module), Ok(Value::Int(expected)));
        }

        let division_by_zero = verified(
            vec![Constant::Int(1), Constant::Int(0)],
            vec![ValueType::Int, ValueType::Int, ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::IntRemainder {
                    destination: 2,
                    left: 0,
                    right: 1,
                },
                Instruction::Return { source: 2 },
            ],
        );
        let error = execute(&division_by_zero).expect_err("remainder by zero must fail");
        assert_eq!(error, VmError::DivisionByZero);
        assert_eq!(error.code(), "arithmetic.division_by_zero");
        assert_eq!(
            execute_with_limits(
                &division_by_zero,
                ExecutionLimits {
                    instructions: 2,
                    ..ExecutionLimits::default()
                },
            ),
            Err(VmError::ResourceLimit {
                resource: super::RESOURCE_INSTRUCTIONS,
            }),
            "the remainder instruction must charge before checking its divisor"
        );
    }

    #[test]
    fn reports_integer_negation_overflow() {
        let module = verified(
            vec![Constant::Int(i64::MIN)],
            vec![ValueType::Int, ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::IntNegate {
                    destination: 1,
                    source: 0,
                },
                Instruction::Return { source: 1 },
            ],
        );

        assert_eq!(execute(&module), Err(VmError::ArithmeticOverflow));
    }

    #[test]
    fn integer_division_truncates_toward_zero() {
        for (left, right, expected) in [(-7, 3, -2), (7, -3, -2), (-7, -3, 2)] {
            let module = verified(
                vec![Constant::Int(left), Constant::Int(right)],
                vec![ValueType::Int, ValueType::Int, ValueType::Int],
                ValueType::Int,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::IntBinary {
                        operation: NumericBinaryOp::Divide,
                        destination: 2,
                        left: 0,
                        right: 1,
                    },
                    Instruction::Return { source: 2 },
                ],
            );

            assert_eq!(execute(&module), Ok(Value::Int(expected)));
        }
    }

    #[test]
    fn float_text_and_equality_follow_the_profile() {
        assert_eq!(FloatValue::new(f64::NAN).bits(), 0x7ff8_0000_0000_0000);
        assert_eq!(
            FloatValue::from_canonical_bits(0x7ff0_0000_0000_0001).bits(),
            0x7ff8_0000_0000_0000
        );
        assert_ne!(FloatValue::new(f64::NAN), FloatValue::new(f64::NAN));
        assert_eq!(FloatValue::new(0.0), FloatValue::new(-0.0));
        assert_eq!(Value::Float(FloatValue::new(-0.0)).to_string(), "-0.0");
        assert_eq!(Value::Float(FloatValue::new(0.0)).to_string(), "0.0");
        assert_eq!(Value::Float(FloatValue::new(1.0)).to_string(), "1.0");
        assert_eq!(
            Value::Float(FloatValue::new(f64::INFINITY)).to_string(),
            "Infinity"
        );
        for number in [f64::MAX, f64::MIN, f64::MIN_POSITIVE, f64::from_bits(1)] {
            let text = Value::Float(FloatValue::new(number)).to_string();
            assert_eq!(text.parse::<f64>().unwrap().to_bits(), number.to_bits());
            assert!(text.len() <= 24, "float text is not shortest: {text}");
        }
    }

    #[test]
    fn canonical_scalar_vectors_are_stable() {
        assert_eq!(encode_canonical(&Value::Unit).unwrap(), vec![0x00]);
        assert_eq!(encode_canonical(&Value::Bool(false)).unwrap(), vec![0x01]);
        assert_eq!(encode_canonical(&Value::Bool(true)).unwrap(), vec![0x02]);
        assert_eq!(
            encode_canonical(&Value::Int(-1)).unwrap(),
            vec![0x03, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(
            encode_canonical(&Value::Float(FloatValue::new(-0.0))).unwrap(),
            vec![0x04, 0x80, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            encode_canonical(&Value::String(Rc::from("A"))).unwrap(),
            vec![0x05, 0, 0, 0, 1, b'A']
        );
        assert_eq!(
            encode_canonical(&Value::Bytes(Rc::from([0xff_u8].as_slice()))).unwrap(),
            vec![0x06, 0, 0, 0, 1, 0xff]
        );
    }

    #[test]
    fn canonical_collections_preserve_tags_and_map_order() {
        let list = Value::List(Rc::from([Value::Int(1)].as_slice()));
        let tuple = Value::Tuple(Rc::from([Value::Int(1)].as_slice()));
        assert_eq!(
            encode_canonical(&list).unwrap(),
            vec![0x07, 0, 0, 0, 1, 0x03, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        assert_eq!(
            encode_canonical(&tuple).unwrap(),
            vec![0x09, 0, 0, 0, 1, 0x03, 0, 0, 0, 0, 0, 0, 0, 1]
        );

        let first = Value::Map(Rc::from(
            [
                (Value::Int(1), Value::Bool(false)),
                (Value::Int(2), Value::Bool(true)),
            ]
            .as_slice(),
        ));
        let second = Value::Map(Rc::from(
            [
                (Value::Int(2), Value::Bool(true)),
                (Value::Int(1), Value::Bool(false)),
            ]
            .as_slice(),
        ));
        let expected = vec![
            0x08, 0, 0, 0, 2, 0x03, 0, 0, 0, 0, 0, 0, 0, 1, 0x01, 0x03, 0, 0, 0, 0, 0, 0, 0, 2,
            0x02,
        ];
        assert_eq!(encode_canonical(&first).unwrap(), expected);
        assert_eq!(
            encode_canonical(&first).unwrap(),
            encode_canonical(&second).unwrap()
        );
    }

    #[test]
    fn canonical_value_vectors_are_stable() {
        let record = Value::Record(Rc::from(
            [
                (Rc::from("x"), Value::Int(1)),
                (Rc::from("y"), Value::Int(2)),
            ]
            .as_slice(),
        ));
        assert_eq!(
            encode_canonical(&record).unwrap(),
            vec![
                0x0a, 0, 0, 0, 2, 0, 0, 0, 1, b'x', 0x03, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, b'y',
                0x03, 0, 0, 0, 0, 0, 0, 0, 2,
            ]
        );

        let enum_value = Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::User(7),
            type_name: Rc::from("main.allen::Reading"),
            variant: 2,
            variant_name: Rc::from("Named"),
            payload: EnumPayload::Record(Rc::from([(Rc::from("value"), Value::Int(7))].as_slice())),
        }));
        assert_eq!(
            encode_canonical(&enum_value).unwrap(),
            vec![
                0x0b, 0x00, 0, 0, 0, 7, 0, 0, 0, 2, 0x02, 0, 0, 0, 1, 0, 0, 0, 5, b'v', b'a', b'l',
                b'u', b'e', 0x03, 0, 0, 0, 0, 0, 0, 0, 7,
            ]
        );
        assert_eq!(enum_value.to_string(), "Reading.Named {value: 7}");

        let unknown = Value::Unknown(Rc::new(Value::Int(7)));
        assert_eq!(
            encode_canonical(&unknown).unwrap(),
            vec![0x0c, 0x03, 0, 0, 0, 0, 0, 0, 0, 7]
        );
        assert_eq!(unknown.to_string(), "unknown(7)");
    }

    #[test]
    fn canonical_output_limit_is_checked_before_allocation() {
        assert_eq!(
            encode_canonical_with_limit(&Value::Int(1), 8),
            Err(CanonicalEncodeError::ResourceLimit)
        );
    }

    #[test]
    fn canonical_encoding_rejects_excessive_nesting() {
        let mut value = Value::Int(1);
        for _ in 0..=allen_bytecode::MAX_VALUE_NESTING {
            value = Value::List(Rc::from([value].as_slice()));
        }

        assert_eq!(
            encode_canonical(&value),
            Err(CanonicalEncodeError::InvalidValue)
        );
    }

    #[test]
    fn canonical_decoder_round_trips_and_preserves_bytes() {
        let values = [
            Value::Unit,
            Value::String(Rc::from("hello")),
            Value::Map(Rc::from([(Value::Int(1), Value::Bool(true))].as_slice())),
            Value::Record(Rc::from(
                [(Rc::from("value"), Value::Unknown(Rc::new(Value::Int(7))))].as_slice(),
            )),
        ];
        for value in values {
            let bytes = encode_canonical(&value).unwrap();
            let decoded = decode_canonical_with_limit(&bytes, 1024).unwrap();
            assert_eq!(encode_canonical(&decoded).unwrap(), bytes);
        }
    }

    #[test]
    fn canonical_decoder_rejects_hostile_forms() {
        assert_eq!(decode_canonical(&[]), Err(CanonicalDecodeError::Truncated));
        assert_eq!(
            decode_canonical(&[0x00, 0x00]),
            Err(CanonicalDecodeError::TrailingBytes)
        );
        assert_eq!(
            decode_canonical(&[0x04, 0x7f, 0xf8, 0, 0, 0, 0, 0, 1]),
            Err(CanonicalDecodeError::InvalidValue)
        );
        assert_eq!(
            decode_canonical(&[0x09, 0, 0, 0, 0]),
            Err(CanonicalDecodeError::InvalidValue)
        );
        assert_eq!(
            decode_canonical(&[0x0d]),
            Err(CanonicalDecodeError::InvalidValue)
        );
        assert_eq!(
            decode_canonical_with_limit(&[0x03, 0, 0, 0, 0, 0, 0, 0, 1], 8),
            Err(CanonicalDecodeError::ResourceLimit)
        );

        let unsorted_record = vec![
            0x0a, 0, 0, 0, 2, 0, 0, 0, 1, b'b', 0x00, 0, 0, 0, 1, b'a', 0x00,
        ];
        assert_eq!(
            decode_canonical(&unsorted_record),
            Err(CanonicalDecodeError::InvalidValue)
        );
        let mixed_map = vec![
            0x08, 0, 0, 0, 2, 0x01, 0x00, 0x03, 0, 0, 0, 0, 0, 0, 0, 1, 0x00,
        ];
        assert_eq!(
            decode_canonical(&mixed_map),
            Err(CanonicalDecodeError::InvalidValue)
        );
    }

    #[test]
    fn accounting_rejects_before_each_vm_operation() {
        let module = verified(
            vec![Constant::Int(1)],
            vec![ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Return { source: 0 },
            ],
        );

        let depth_error = execute_with_limits(
            &module,
            ExecutionLimits {
                call_depth: 0,
                ..ExecutionLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            depth_error,
            VmError::ResourceLimit {
                resource: "call_depth"
            }
        );

        let allocation_error = execute_with_limits(
            &module,
            ExecutionLimits {
                allocation_bytes: 47,
                ..ExecutionLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            allocation_error,
            VmError::ResourceLimit {
                resource: "allocation_bytes"
            }
        );

        let instruction_error = execute_with_limits(
            &module,
            ExecutionLimits {
                instructions: 0,
                ..ExecutionLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            instruction_error,
            VmError::ResourceLimit {
                resource: "instructions"
            }
        );
    }

    #[test]
    fn collection_allocation_is_refused_before_construction() {
        let module = verified(
            vec![Constant::Int(1), Constant::Int(2)],
            vec![
                ValueType::Int,
                ValueType::Int,
                ValueType::List(Box::new(ValueType::Int)),
            ],
            ValueType::List(Box::new(ValueType::Int)),
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::ListNew {
                    destination: 2,
                    elements: vec![0, 1],
                },
                Instruction::Return { source: 2 },
            ],
        );
        // The frame costs 80 bytes. The two-element list costs 40 bytes.
        let error = execute_with_limits(
            &module,
            ExecutionLimits {
                allocation_bytes: 119,
                ..ExecutionLimits::default()
            },
        )
        .unwrap_err();

        assert_eq!(
            error,
            VmError::ResourceLimit {
                resource: "allocation_bytes"
            }
        );
    }

    #[test]
    fn dynamic_collection_operations_preserve_lengths_order_and_aliases() {
        let list_type = ValueType::List(Box::new(ValueType::Int));
        let result_type = ValueType::Tuple(vec![
            ValueType::Int,
            ValueType::Int,
            list_type.clone(),
            list_type.clone(),
            list_type.clone(),
        ]);
        let module = verified(
            vec![
                Constant::Bytes(Vec::new()),
                Constant::Int(1),
                Constant::Int(2),
                Constant::Int(3),
                Constant::Int(0),
            ],
            vec![
                ValueType::Bytes,
                ValueType::Int,
                ValueType::Int,
                list_type.clone(),
                ValueType::Int,
                ValueType::Int,
                list_type.clone(),
                ValueType::Int,
                list_type.clone(),
                result_type.clone(),
            ],
            result_type,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Length {
                    destination: 1,
                    collection: 0,
                },
                Instruction::Const {
                    destination: 2,
                    constant: 1,
                },
                Instruction::ListNew {
                    destination: 3,
                    elements: vec![2],
                },
                Instruction::Length {
                    destination: 4,
                    collection: 3,
                },
                Instruction::Const {
                    destination: 5,
                    constant: 2,
                },
                Instruction::ListAppend {
                    destination: 6,
                    values: 3,
                    value: 5,
                },
                Instruction::Const {
                    destination: 7,
                    constant: 4,
                },
                Instruction::Const {
                    destination: 5,
                    constant: 3,
                },
                Instruction::ListSet {
                    destination: 8,
                    values: 6,
                    index: 7,
                    value: 5,
                },
                Instruction::TupleNew {
                    destination: 9,
                    elements: vec![1, 4, 3, 6, 8],
                },
                Instruction::Return { source: 9 },
            ],
        );

        assert_eq!(
            execute(&module),
            Ok(Value::Tuple(
                vec![
                    Value::Int(0),
                    Value::Int(1),
                    Value::List(vec![Value::Int(1)].into()),
                    Value::List(vec![Value::Int(1), Value::Int(2)].into()),
                    Value::List(vec![Value::Int(3), Value::Int(2)].into()),
                ]
                .into()
            ))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn sorted_map_entry_access_uses_canonical_order_for_every_key_type() {
        let cases = [
            (
                ValueType::Bool,
                Constant::Bool(true),
                Value::Bool(true),
                Constant::Bool(false),
                Value::Bool(false),
            ),
            (
                ValueType::Int,
                Constant::Int(2),
                Value::Int(2),
                Constant::Int(-1),
                Value::Int(-1),
            ),
            (
                ValueType::String,
                Constant::String("z".to_owned()),
                Value::String(Rc::from("z")),
                Constant::String("a".to_owned()),
                Value::String(Rc::from("a")),
            ),
            (
                ValueType::Bytes,
                Constant::Bytes(vec![0xff]),
                Value::Bytes(vec![0xff].into()),
                Constant::Bytes(vec![0]),
                Value::Bytes(vec![0].into()),
            ),
        ];

        for (key_type, high_constant, high_value, low_constant, low_value) in cases {
            let map_type = ValueType::Map(Box::new(key_type.clone()), Box::new(ValueType::Int));
            let entry_type = ValueType::Tuple(vec![key_type.clone(), ValueType::Int]);
            let result_type =
                ValueType::Tuple(vec![entry_type.clone(), entry_type.clone(), ValueType::Int]);
            let module = verified(
                vec![
                    high_constant,
                    Constant::Int(20),
                    low_constant,
                    Constant::Int(10),
                    Constant::Int(0),
                    Constant::Int(1),
                ],
                vec![
                    key_type.clone(),
                    ValueType::Int,
                    key_type,
                    ValueType::Int,
                    map_type,
                    ValueType::Int,
                    entry_type.clone(),
                    ValueType::Int,
                    entry_type.clone(),
                    ValueType::Int,
                    result_type.clone(),
                ],
                result_type,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::Const {
                        destination: 2,
                        constant: 2,
                    },
                    Instruction::Const {
                        destination: 3,
                        constant: 3,
                    },
                    Instruction::MapNew {
                        destination: 4,
                        entries: vec![(0, 1), (2, 3)],
                    },
                    Instruction::Const {
                        destination: 5,
                        constant: 4,
                    },
                    Instruction::MapEntryAt {
                        destination: 6,
                        map: 4,
                        index: 5,
                    },
                    Instruction::Const {
                        destination: 7,
                        constant: 5,
                    },
                    Instruction::MapEntryAt {
                        destination: 8,
                        map: 4,
                        index: 7,
                    },
                    Instruction::Length {
                        destination: 9,
                        collection: 4,
                    },
                    Instruction::TupleNew {
                        destination: 10,
                        elements: vec![6, 8, 9],
                    },
                    Instruction::Return { source: 10 },
                ],
            );

            assert_eq!(
                execute(&module),
                Ok(Value::Tuple(
                    vec![
                        Value::Tuple(vec![low_value, Value::Int(10)].into()),
                        Value::Tuple(vec![high_value, Value::Int(20)].into()),
                        Value::Int(2),
                    ]
                    .into()
                ))
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn sorted_map_entry_access_checks_indexes_before_atomic_allocation() {
        let map_type = ValueType::Map(Box::new(ValueType::Int), Box::new(ValueType::Bool));
        let entry_type = ValueType::Tuple(vec![ValueType::Int, ValueType::Bool]);
        let module = verify(Module {
            constants: vec![],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![Function {
                name: "main".to_owned(),
                parameters: vec![0, 1],
                captures: vec![],
                registers: vec![map_type, ValueType::Int, entry_type.clone()],
                return_type: entry_type,
                effects: 0,
                code: vec![
                    Instruction::MapEntryAt {
                        destination: 2,
                        map: 0,
                        index: 1,
                    },
                    Instruction::Return { source: 2 },
                ],
            }],
            async_functions: vec![],
            entry: 0,
        })
        .expect("Map entry module verifies");
        let original_entries: Rc<[(Value, Value)]> =
            vec![(Value::Int(1), Value::Bool(true))].into();
        let map_argument = Value::Map(original_entries.clone());

        for index in [-1, 1] {
            let error = execute_entry_with_runtime_context(
                &module,
                None,
                0,
                &[map_argument.clone(), Value::Int(index)],
                ExecutionLimits {
                    // The three-register frame costs 80 bytes. No tuple is charged.
                    allocation_bytes: 80,
                    ..ExecutionLimits::default()
                },
                &mut FixedClock,
                &mut super::IgnoreCheckpoints,
                &mut super::NeverCancelled,
                &mut super::NoEffects,
            )
            .expect_err("invalid sorted Map index must fail");
            assert_eq!(error.error, VmError::IndexOutOfBounds);
            assert_eq!(
                original_entries.as_ref(),
                &[(Value::Int(1), Value::Bool(true))]
            );
        }

        let allocation_error = execute_entry_with_runtime_context(
            &module,
            None,
            0,
            &[map_argument.clone(), Value::Int(0)],
            ExecutionLimits {
                // Frame is 80 bytes; the exact two-slot tuple allocation is 40 bytes.
                allocation_bytes: 119,
                ..ExecutionLimits::default()
            },
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut super::NeverCancelled,
            &mut super::NoEffects,
        )
        .expect_err("tuple allocation must be refused before cloning or destination write");
        assert_eq!(
            allocation_error.error,
            VmError::ResourceLimit {
                resource: "allocation_bytes"
            }
        );
        assert_eq!(
            original_entries.as_ref(),
            &[(Value::Int(1), Value::Bool(true))]
        );

        let instruction_error = execute_entry_with_runtime_context(
            &module,
            None,
            0,
            &[map_argument, Value::Int(0)],
            ExecutionLimits {
                instructions: 0,
                ..ExecutionLimits::default()
            },
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut super::NeverCancelled,
            &mut super::NoEffects,
        )
        .expect_err("instruction refusal must happen before Map entry access");
        assert_eq!(
            instruction_error.error,
            VmError::ResourceLimit {
                resource: "instructions"
            }
        );
        assert_eq!(
            original_entries.as_ref(),
            &[(Value::Int(1), Value::Bool(true))]
        );
    }

    #[test]
    fn list_set_rejects_negative_and_past_end_indexes() {
        for index in [-1, 1] {
            let list_type = ValueType::List(Box::new(ValueType::Int));
            let module = verified(
                vec![Constant::Int(10), Constant::Int(index), Constant::Int(20)],
                vec![
                    ValueType::Int,
                    list_type.clone(),
                    ValueType::Int,
                    ValueType::Int,
                    list_type.clone(),
                ],
                list_type,
                vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::ListNew {
                        destination: 1,
                        elements: vec![0],
                    },
                    Instruction::Const {
                        destination: 2,
                        constant: 1,
                    },
                    Instruction::Const {
                        destination: 3,
                        constant: 2,
                    },
                    Instruction::ListSet {
                        destination: 4,
                        values: 1,
                        index: 2,
                        value: 3,
                    },
                    Instruction::Return { source: 4 },
                ],
            );
            assert_eq!(execute(&module), Err(VmError::IndexOutOfBounds));
        }
    }

    #[test]
    fn dynamic_collection_charges_are_refused_before_input_changes() {
        let list_type = ValueType::List(Box::new(ValueType::Int));
        let module = verify(Module {
            constants: vec![Constant::Int(2)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![Function {
                name: "main".to_owned(),
                parameters: vec![0],
                captures: vec![],
                registers: vec![list_type.clone(), ValueType::Int, list_type.clone()],
                return_type: list_type,
                effects: 0,
                code: vec![
                    Instruction::Const {
                        destination: 1,
                        constant: 0,
                    },
                    Instruction::ListAppend {
                        destination: 2,
                        values: 0,
                        value: 1,
                    },
                    Instruction::Return { source: 2 },
                ],
            }],
            async_functions: vec![],
            entry: 0,
        })
        .expect("dynamic collection module verifies");
        let original_values: Rc<[Value]> = vec![Value::Int(1)].into();
        let argument = Value::List(original_values.clone());

        let error = execute_entry_with_runtime_context(
            &module,
            None,
            0,
            std::slice::from_ref(&argument),
            ExecutionLimits {
                // The frame costs 80 bytes and the returned two-item list costs 40.
                allocation_bytes: 119,
                ..ExecutionLimits::default()
            },
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut super::NeverCancelled,
            &mut super::NoEffects,
        )
        .expect_err("append allocation must be refused");

        assert_eq!(
            error.error,
            VmError::ResourceLimit {
                resource: "allocation_bytes"
            }
        );
        assert_eq!(original_values.as_ref(), &[Value::Int(1)]);

        let instruction_error = execute_entry_with_runtime_context(
            &module,
            None,
            0,
            &[argument],
            ExecutionLimits {
                instructions: 1,
                ..ExecutionLimits::default()
            },
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut super::NeverCancelled,
            &mut super::NoEffects,
        )
        .expect_err("append instruction charge must be refused");
        assert_eq!(
            instruction_error.error,
            VmError::ResourceLimit {
                resource: "instructions"
            }
        );
        assert_eq!(original_values.as_ref(), &[Value::Int(1)]);
    }

    #[test]
    fn indexing_has_stable_success_and_failure_behavior() {
        let list_module = verified(
            vec![Constant::Int(10), Constant::Int(20), Constant::Int(1)],
            vec![
                ValueType::Int,
                ValueType::Int,
                ValueType::List(Box::new(ValueType::Int)),
                ValueType::Int,
                ValueType::Int,
            ],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::ListNew {
                    destination: 2,
                    elements: vec![0, 1],
                },
                Instruction::Const {
                    destination: 3,
                    constant: 2,
                },
                Instruction::IndexGet {
                    destination: 4,
                    collection: 2,
                    index: 3,
                },
                Instruction::Return { source: 4 },
            ],
        );
        assert_eq!(execute(&list_module), Ok(Value::Int(20)));

        let missing_map_module = verified(
            vec![Constant::Int(1), Constant::Bool(true), Constant::Int(2)],
            vec![
                ValueType::Int,
                ValueType::Bool,
                ValueType::Map(Box::new(ValueType::Int), Box::new(ValueType::Bool)),
                ValueType::Int,
                ValueType::Bool,
            ],
            ValueType::Bool,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::MapNew {
                    destination: 2,
                    entries: vec![(0, 1)],
                },
                Instruction::Const {
                    destination: 3,
                    constant: 2,
                },
                Instruction::IndexGet {
                    destination: 4,
                    collection: 2,
                    index: 3,
                },
                Instruction::Return { source: 4 },
            ],
        );
        assert_eq!(execute(&missing_map_module), Err(VmError::MapKeyNotFound));
    }

    #[test]
    fn out_of_bounds_index_has_a_stable_error() {
        let module = verified(
            vec![Constant::Int(10), Constant::Int(1)],
            vec![
                ValueType::Int,
                ValueType::List(Box::new(ValueType::Int)),
                ValueType::Int,
                ValueType::Int,
            ],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::ListNew {
                    destination: 1,
                    elements: vec![0],
                },
                Instruction::Const {
                    destination: 2,
                    constant: 1,
                },
                Instruction::IndexGet {
                    destination: 3,
                    collection: 1,
                    index: 2,
                },
                Instruction::Return { source: 3 },
            ],
        );

        let error = execute(&module).expect_err("index must fail");
        assert_eq!(error, VmError::IndexOutOfBounds);
        assert_eq!(error.code(), "index.out_of_bounds");
    }

    #[test]
    fn duplicate_map_keys_have_a_stable_error() {
        let module = verified(
            vec![
                Constant::Int(1),
                Constant::Bool(true),
                Constant::Bool(false),
            ],
            vec![
                ValueType::Int,
                ValueType::Bool,
                ValueType::Bool,
                ValueType::Map(Box::new(ValueType::Int), Box::new(ValueType::Bool)),
            ],
            ValueType::Map(Box::new(ValueType::Int), Box::new(ValueType::Bool)),
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 1,
                    constant: 1,
                },
                Instruction::Const {
                    destination: 2,
                    constant: 2,
                },
                Instruction::MapNew {
                    destination: 3,
                    entries: vec![(0, 1), (0, 2)],
                },
                Instruction::Return { source: 3 },
            ],
        );

        let error = execute(&module).expect_err("duplicate key must fail");
        assert_eq!(error, VmError::DuplicateMapKey);
        assert_eq!(error.code(), "map.duplicate_key");
    }

    #[test]
    fn successful_usage_matches_the_profile() {
        let module = verified(
            vec![Constant::Int(1)],
            vec![ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Return { source: 0 },
            ],
        );

        let result = execute_with_limits(&module, ExecutionLimits::default()).unwrap();
        assert_eq!(result.usage.instructions, 2);
        assert_eq!(result.usage.allocation_bytes, 48);
        assert_eq!(result.usage.maximum_call_depth, 1);
    }

    fn verified_call_module() -> allen_bytecode::VerifiedModule {
        let callback_type = ValueType::Function {
            parameters: vec![ValueType::Int],
            return_type: Box::new(ValueType::Int),
            effects: 0,
        };
        verify(Module {
            constants: vec![Constant::Int(40), Constant::Int(2)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Int,
                        ValueType::Int,
                        ValueType::Int,
                        callback_type,
                        ValueType::Int,
                        ValueType::Int,
                    ],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Const {
                            destination: 1,
                            constant: 1,
                        },
                        Instruction::DirectCall {
                            destination: 2,
                            function: 1,
                            arguments: vec![0, 1],
                        },
                        Instruction::ClosureNew {
                            destination: 3,
                            function: 2,
                            captures: vec![2],
                        },
                        Instruction::Const {
                            destination: 2,
                            constant: 1,
                        },
                        Instruction::Const {
                            destination: 4,
                            constant: 1,
                        },
                        Instruction::ClosureCall {
                            destination: 5,
                            closure: 3,
                            arguments: vec![4],
                        },
                        Instruction::Return { source: 5 },
                    ],
                },
                Function {
                    name: "add".to_owned(),
                    parameters: vec![0, 1],
                    captures: vec![],
                    registers: vec![ValueType::Int, ValueType::Int, ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::IntBinary {
                            destination: 2,
                            left: 0,
                            right: 1,
                            operation: NumericBinaryOp::Add,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
                Function {
                    name: "add_captured".to_owned(),
                    parameters: vec![0],
                    captures: vec![1],
                    registers: vec![ValueType::Int, ValueType::Int, ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::IntBinary {
                            destination: 2,
                            left: 0,
                            right: 1,
                            operation: NumericBinaryOp::Add,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
            ],
            async_functions: vec![],
            entry: 0,
        })
        .expect("call module must verify")
    }

    #[test]
    fn executes_nested_calls_and_immutable_closure_captures() {
        let module = verified_call_module();
        let result = execute_with_limits(&module, ExecutionLimits::default()).unwrap();

        assert_eq!(result.value, Value::Int(44));
        assert_eq!(result.usage.instructions, 12);
        assert_eq!(result.usage.allocation_bytes, 312);
        assert_eq!(result.usage.maximum_call_depth, 2);
    }

    #[test]
    fn enforces_call_depth_before_a_nested_frame() {
        let module = verified_call_module();
        assert_eq!(
            execute_with_limits(
                &module,
                ExecutionLimits {
                    call_depth: 1,
                    ..ExecutionLimits::default()
                }
            ),
            Err(VmError::ResourceLimit {
                resource: "call_depth"
            })
        );
    }

    #[test]
    fn enforces_maximum_single_allocation_before_cumulative_accounting() {
        let module = verified(
            vec![Constant::Int(1)],
            vec![ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Return { source: 0 },
            ],
        );

        assert_eq!(
            execute_with_limits(
                &module,
                ExecutionLimits {
                    maximum_allocation_bytes: 47,
                    ..ExecutionLimits::default()
                },
            ),
            Err(VmError::ResourceLimit {
                resource: "maximum_allocation_bytes",
            })
        );
    }

    struct SequenceClock {
        values: std::vec::IntoIter<Duration>,
    }

    struct FixedClock;

    impl MonotonicClock for FixedClock {
        fn now(&mut self) -> Duration {
            Duration::ZERO
        }
    }

    struct ReadyEffect {
        operation: FsOperation,
        result: Value,
        calls: usize,
    }

    struct PendingWrongEffect {
        polls: usize,
        cancelled: usize,
    }

    struct SemanticValueProvider {
        value: Value,
        pending: bool,
        replayed: bool,
    }

    impl EffectProvider for SemanticValueProvider {
        fn is_replayed(&self) -> bool {
            self.replayed
        }

        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(
            &mut self,
            _operation: FsOperation,
            _arguments: &[Value],
        ) -> Result<Value, VmError> {
            Ok(self.value.clone())
        }

        fn start_call(
            &mut self,
            _pending: super::PendingEffectId,
            _operation: FsOperation,
            _arguments: &[Value],
            _cancellation: &mut dyn super::CancellationSource,
        ) -> Result<super::EffectPoll, VmError> {
            if self.pending {
                Ok(super::EffectPoll::Pending)
            } else {
                Ok(super::EffectPoll::Ready(self.value.clone()))
            }
        }

        fn poll_effect(
            &mut self,
            _pending: super::PendingEffectId,
            _cancellation: &mut dyn super::CancellationSource,
        ) -> Result<super::EffectPoll, VmError> {
            Ok(super::EffectPoll::Ready(self.value.clone()))
        }
    }

    impl EffectProvider for PendingWrongEffect {
        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(
            &mut self,
            _operation: FsOperation,
            _arguments: &[Value],
        ) -> Result<Value, VmError> {
            unreachable!("the nonblocking path is used")
        }

        fn start_call(
            &mut self,
            _pending: super::PendingEffectId,
            operation: FsOperation,
            _arguments: &[Value],
            _cancellation: &mut dyn super::CancellationSource,
        ) -> Result<super::EffectPoll, VmError> {
            assert_eq!(operation, FsOperation::HttpGet);
            Ok(super::EffectPoll::Pending)
        }

        fn poll_effect(
            &mut self,
            _pending: super::PendingEffectId,
            _cancellation: &mut dyn super::CancellationSource,
        ) -> Result<super::EffectPoll, VmError> {
            self.polls += 1;
            Ok(super::EffectPoll::Ready(Value::String("wrong".into())))
        }

        fn cancel_pending(&mut self) {
            self.cancelled += 1;
        }
    }

    impl EffectProvider for ReadyEffect {
        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(&mut self, operation: FsOperation, _arguments: &[Value]) -> Result<Value, VmError> {
            assert_eq!(operation, self.operation);
            self.calls += 1;
            Ok(self.result.clone())
        }
    }

    struct AgentEffect {
        calls: usize,
        cancelled: usize,
        value: Value,
    }

    impl EffectProvider for AgentEffect {
        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(
            &mut self,
            _operation: FsOperation,
            _arguments: &[Value],
        ) -> Result<Value, VmError> {
            Err(VmError::Invariant("unexpected ordinary effect"))
        }

        fn agent(
            &mut self,
            operation: FsOperation,
            _arguments: &[Value],
            _result_type: &ValueType,
            _cancellation: &mut dyn super::CancellationSource,
        ) -> Result<Value, VmError> {
            assert_eq!(operation, FsOperation::AgentAsk);
            self.calls += 1;
            Ok(self.value.clone())
        }

        fn cancel_pending(&mut self) {
            self.cancelled += 1;
        }
    }

    #[test]
    fn default_response_provider_errors_keep_targets_distinct() {
        let mut provider = super::NoEffects;
        let mut cancellation = super::NeverCancelled;
        for (operation, expected) in [
            (FsOperation::AgentAsk, VmError::AgentUnavailable),
            (FsOperation::ModelRequest, VmError::ModelUnavailable),
            (FsOperation::UserAsk, VmError::UserUnavailable),
        ] {
            let error = provider
                .agent(operation, &[], &ValueType::String, &mut cancellation)
                .expect_err("the default provider is unavailable");
            assert_eq!(error, expected);
        }
    }

    fn start_missing_agent(
        operation: FsOperation,
        result_type: ValueType,
    ) -> Result<super::EffectPoll, VmError> {
        let mut clock = FixedClock;
        let mut observer = super::IgnoreCheckpoints;
        let mut cancellation = super::NeverCancelled;
        let mut budget = super::Budget::new(
            ExecutionLimits::default(),
            &mut clock,
            &mut observer,
            &mut cancellation,
        );
        let mut machine = super::TaskMachine::default();
        machine
            .start_effect_future(
                &super::FutureValue::Agent {
                    operation,
                    arguments: Vec::new().into(),
                    result_type,
                },
                &mut budget,
                &mut super::NoEffects,
            )
            .map(|(_, poll, _, _)| poll)
    }

    #[test]
    fn missing_agent_providers_return_closed_result_errors() {
        let response_families = [
            (
                FsOperation::AgentAsk,
                VmError::AgentUnavailable,
                "agent.unavailable",
                ValueType::Bool,
            ),
            (
                FsOperation::ModelRequest,
                VmError::ModelUnavailable,
                "model.unavailable",
                ValueType::List(Box::new(ValueType::Int)),
            ),
            (
                FsOperation::UserAsk,
                VmError::UserUnavailable,
                "user.unavailable",
                ValueType::Tuple(vec![ValueType::String, ValueType::Bool]),
            ),
            (
                FsOperation::SubAgentRun,
                VmError::SubAgentUnavailable,
                "sub_agent.unavailable",
                ValueType::Option(Box::new(ValueType::Bytes)),
            ),
            (
                FsOperation::SubAgentAsk,
                VmError::SubAgentUnavailable,
                "sub_agent.unavailable",
                ValueType::Map(Box::new(ValueType::String), Box::new(ValueType::Int)),
            ),
        ];
        let standard_error = allen_bytecode::standard_error_type();

        for (operation, _provider_error, code, response_type) in response_families {
            let result =
                ValueType::Result(Box::new(response_type), Box::new(standard_error.clone()));
            let super::EffectPoll::Ready(value) = start_missing_agent(operation, result)
                .expect("missing providers return a closed Result error")
            else {
                panic!("default provider must be ready")
            };
            let Value::Enum(result) = &value else {
                panic!("provider response must be a Result")
            };
            assert_eq!(result.variant, 1, "operation {operation:?}");
            assert!(value.to_string().contains(code), "operation {operation:?}");
        }
    }

    #[test]
    fn public_terminal_codes_use_current_runtime_identity() {
        for (error, current) in [
            (VmError::Cancelled, "runtime.cancelled"),
            (
                VmError::Timeout {
                    resource: super::RESOURCE_WALL_TIME,
                },
                "runtime.timeout",
            ),
            (
                VmError::Invariant("CANARY internal detail"),
                "runtime.panic",
            ),
            (VmError::AgentResponseSchema, "agent.validation_failed"),
            (VmError::ModelValidationError, "model.validation_failed"),
            (
                VmError::SubAgentResponseSchema,
                "sub_agent.validation_failed",
            ),
            (VmError::ResponseValidationError, "user.validation_failed"),
            (VmError::ToolSchemaError, "tool.schema"),
            (VmError::CapabilityMissing, "protocol.violation"),
        ] {
            assert_eq!(error.code(), current);
            let execution = super::ExecutionError {
                error,
                frames: vec![],
            };
            assert_eq!(execution.code(), current);
        }
    }

    fn result_ok(value: Value) -> Value {
        Value::Enum(Rc::new(super::builtin_enum(
            EnumIdentity::Result,
            0,
            EnumPayload::Tuple(vec![value].into()),
        )))
    }

    fn http_response(headers: Vec<(Value, Value)>) -> Value {
        result_ok(Value::Record(
            vec![
                (Rc::from("body"), Value::Bytes(Rc::from(&b"ok"[..]))),
                (
                    Rc::from("final_url"),
                    Value::String(Rc::from("https://example.test/final")),
                ),
                (Rc::from("headers"), Value::Map(headers.into())),
                (Rc::from("status"), Value::Int(200)),
            ]
            .into(),
        ))
    }

    fn http_effect_module(spawn: bool) -> allen_bytecode::VerifiedModule {
        let result_type = ValueType::Result(
            Box::new(http_response_type()),
            Box::new(network_error_type()),
        );
        let mut registers = vec![
            ValueType::String,
            ValueType::Future(Box::new(result_type.clone())),
        ];
        let mut code = vec![
            Instruction::Const {
                destination: 0,
                constant: 0,
            },
            Instruction::EffectCall {
                destination: 1,
                operation: FsOperation::HttpGet,
                arguments: vec![0],
            },
        ];
        let return_register = if spawn {
            registers.push(ValueType::Task(Box::new(result_type.clone())));
            registers.push(result_type.clone());
            code.push(Instruction::Spawn {
                destination: 2,
                future: 1,
                scope: 0,
            });
            code.push(Instruction::Await {
                destination: 3,
                source: 2,
            });
            3
        } else {
            registers.push(result_type.clone());
            code.push(Instruction::Await {
                destination: 2,
                source: 1,
            });
            2
        };
        code.push(Instruction::Return {
            source: return_register,
        });
        verify(Module {
            constants: vec![Constant::String("https://example.test/data".to_owned())],
            enum_types: vec![],
            effect_sets: vec![vec!["net.http_get".to_owned(), "task.spawn".to_owned()]],
            functions: vec![Function {
                name: "main.allen::main".to_owned(),
                parameters: vec![],
                captures: vec![],
                registers,
                return_type: result_type,
                effects: 0,
                code,
            }],
            async_functions: vec![0],
            entry: 0,
        })
        .expect("HTTP module verifies")
    }

    fn permission_effect_module() -> allen_bytecode::VerifiedModule {
        let result_type = ValueType::Result(
            Box::new(ValueType::Workspace),
            Box::new(permission_error_type()),
        );
        verify(Module {
            constants: vec![
                Constant::ExternalFsAccess(ExternalFsAccess::Read),
                Constant::String("/outside/report.txt".to_owned()),
                Constant::String("Read the report.".to_owned()),
            ],
            enum_types: vec![],
            effect_sets: vec![vec!["permission.request_external_fs".to_owned()]],
            functions: vec![Function {
                name: "main.allen::main".to_owned(),
                parameters: vec![],
                captures: vec![],
                registers: vec![
                    ValueType::ExternalFsAccess,
                    ValueType::String,
                    ValueType::String,
                    external_file_request_type(),
                    ValueType::Future(Box::new(result_type.clone())),
                    result_type.clone(),
                ],
                return_type: result_type,
                effects: 0,
                code: vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Const {
                        destination: 1,
                        constant: 1,
                    },
                    Instruction::Const {
                        destination: 2,
                        constant: 2,
                    },
                    Instruction::RecordNew {
                        destination: 3,
                        fields: vec![(0, 0), (1, 1), (2, 2)],
                    },
                    Instruction::EffectCall {
                        destination: 4,
                        operation: FsOperation::PermissionRequestFile,
                        arguments: vec![3],
                    },
                    Instruction::Await {
                        destination: 5,
                        source: 4,
                    },
                    Instruction::Return { source: 5 },
                ],
            }],
            async_functions: vec![0],
            entry: 0,
        })
        .expect("permission module verifies")
    }

    fn run_effect(
        module: &allen_bytecode::VerifiedModule,
        provider: &mut dyn EffectProvider,
    ) -> Result<ExecutionOutcome, super::ExecutionError> {
        execute_entry_with_runtime_context(
            module,
            None,
            0,
            &[],
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut super::NeverCancelled,
            provider,
        )
    }

    fn result_error_code(outcome: ExecutionOutcome) -> String {
        let ExecutionOutcome::Completed(ExecutionResult {
            value: Value::Enum(result),
            ..
        }) = outcome
        else {
            panic!("execution must complete with Result::Err")
        };
        let (1, EnumPayload::Tuple(payload)) = (result.variant, &result.payload) else {
            panic!("execution must complete with Result::Err")
        };
        let [Value::Record(fields)] = payload.as_ref() else {
            panic!("Result::Err must contain a standard error")
        };
        let Some((_, Value::String(code))) =
            fields.iter().find(|(name, _)| name.as_ref() == "code")
        else {
            panic!("standard error must contain a code")
        };
        code.to_string()
    }

    fn filesystem_effect_module() -> allen_bytecode::VerifiedModule {
        let result_type =
            ValueType::Result(Box::new(ValueType::String), Box::new(file_error_type()));
        verify(Module {
            constants: vec![Constant::String("note.txt".to_owned())],
            enum_types: vec![],
            effect_sets: vec![vec!["fs.read".to_owned()]],
            functions: vec![Function {
                name: "main.allen::main".to_owned(),
                parameters: vec![],
                captures: vec![],
                effects: 0,
                registers: vec![
                    ValueType::Workspace,
                    ValueType::String,
                    ValueType::Future(Box::new(result_type.clone())),
                    result_type.clone(),
                ],
                return_type: result_type,
                code: vec![
                    Instruction::WorkspaceGet { destination: 0 },
                    Instruction::Const {
                        destination: 1,
                        constant: 0,
                    },
                    Instruction::EffectCall {
                        destination: 2,
                        operation: FsOperation::ReadText,
                        arguments: vec![0, 1],
                    },
                    Instruction::Await {
                        destination: 3,
                        source: 2,
                    },
                    Instruction::Return { source: 3 },
                ],
            }],
            async_functions: vec![0],
            entry: 0,
        })
        .expect("filesystem module verifies")
    }

    #[test]
    fn stop_cancels_pending_provider_work_before_the_terminal_outcome() {
        let module = verify(Module {
            constants: vec![Constant::String("done".to_owned())],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![Function {
                name: "main.allen::main".to_owned(),
                parameters: vec![],
                captures: vec![],
                registers: vec![ValueType::String],
                return_type: ValueType::Never,
                effects: 0,
                code: vec![
                    Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    Instruction::Stop { reason: 0 },
                ],
            }],
            async_functions: vec![],
            entry: 0,
        })
        .expect("stop module verifies");
        let mut provider = AgentEffect {
            calls: 0,
            cancelled: 0,
            value: Value::Unit,
        };
        assert!(matches!(
            run_effect(&module, &mut provider).unwrap(),
            ExecutionOutcome::Stopped { .. }
        ));
        assert_eq!(provider.cancelled, 1);
    }

    struct PendingProviderError(VmError);

    struct WorkspaceProviderError(VmError);

    struct TerminalProviderError {
        error: VmError,
        pending: bool,
        cleanup_calls: usize,
    }

    impl EffectProvider for WorkspaceProviderError {
        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(self.0.clone())
        }

        fn call(
            &mut self,
            _operation: FsOperation,
            _arguments: &[Value],
        ) -> Result<Value, VmError> {
            unreachable!("workspace acquisition must fail first")
        }
    }

    impl EffectProvider for PendingProviderError {
        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Ok(WorkspaceValue::new(1, 0, 1))
        }

        fn call(
            &mut self,
            _operation: FsOperation,
            _arguments: &[Value],
        ) -> Result<Value, VmError> {
            unreachable!("pending hook is used")
        }

        fn start_call(
            &mut self,
            _pending: super::PendingEffectId,
            _operation: FsOperation,
            _arguments: &[Value],
            _cancellation: &mut dyn CancellationSource,
        ) -> Result<super::EffectPoll, VmError> {
            Ok(super::EffectPoll::Pending)
        }

        fn poll_effect(
            &mut self,
            _pending: super::PendingEffectId,
            _cancellation: &mut dyn CancellationSource,
        ) -> Result<super::EffectPoll, VmError> {
            Err(self.0.clone())
        }
    }

    impl EffectProvider for TerminalProviderError {
        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(
            &mut self,
            _operation: FsOperation,
            _arguments: &[Value],
        ) -> Result<Value, VmError> {
            Err(self.error.clone())
        }

        fn start_call(
            &mut self,
            _pending: super::PendingEffectId,
            _operation: FsOperation,
            _arguments: &[Value],
            _cancellation: &mut dyn CancellationSource,
        ) -> Result<super::EffectPoll, VmError> {
            if self.pending {
                Ok(super::EffectPoll::Pending)
            } else {
                Err(self.error.clone())
            }
        }

        fn poll_effect(
            &mut self,
            _pending: super::PendingEffectId,
            _cancellation: &mut dyn CancellationSource,
        ) -> Result<super::EffectPoll, VmError> {
            Err(self.error.clone())
        }

        fn cancel_pending(&mut self) {
            self.cleanup_calls += 1;
        }
    }

    #[test]
    fn pending_ordinary_provider_errors_close_with_the_originating_family() {
        for (module, expected) in [
            (filesystem_effect_module(), "fs.unavailable"),
            (http_effect_module(false), "network.unavailable"),
            (permission_effect_module(), "permission.unavailable"),
        ] {
            let mut provider = PendingProviderError(VmError::CapabilityMissing);
            let outcome = run_effect(&module, &mut provider).unwrap();
            assert_eq!(result_error_code(outcome), expected);
        }
    }

    #[test]
    fn workspace_provider_accepts_only_capability_missing() {
        for error in [
            VmError::Stopped {
                reason: "CANARY raw stop".to_owned(),
            },
            VmError::ReplayDiverged,
            VmError::ArithmeticOverflow,
        ] {
            let mut provider = WorkspaceProviderError(error.clone());
            assert_eq!(
                run_effect(&filesystem_effect_module(), &mut provider)
                    .unwrap_err()
                    .error,
                VmError::ProtocolViolation
            );
        }
    }

    #[test]
    fn provider_error_closure_matrix_covers_every_standard_and_tool_family() {
        for (operation, error, expected) in [
            (
                FsOperation::AgentAsk,
                VmError::AgentUnavailable,
                "agent.unavailable",
            ),
            (
                FsOperation::ModelRequest,
                VmError::ModelUnavailable,
                "model.unavailable",
            ),
            (
                FsOperation::UserAsk,
                VmError::UserUnavailable,
                "user.unavailable",
            ),
            (
                FsOperation::SubAgentAsk,
                VmError::SubAgentUnavailable,
                "sub_agent.unavailable",
            ),
        ] {
            let result_type = ValueType::Result(
                Box::new(ValueType::String),
                Box::new(
                    if matches!(
                        operation,
                        FsOperation::SubAgentCreate
                            | FsOperation::SubAgentRun
                            | FsOperation::SubAgentMessage
                            | FsOperation::SubAgentAsk
                    ) {
                        allen_bytecode::sub_agent_error_type()
                    } else {
                        allen_bytecode::standard_error_type()
                    },
                ),
            );
            let value = super::close_provider_error(
                super::EffectFailureFamily::Operation(operation),
                &result_type,
                error,
            )
            .unwrap();
            assert!(value.to_string().contains(expected));
        }
        let tool_type =
            ValueType::Result(Box::new(ValueType::String), Box::new(ValueType::Enum(0)));
        for (error, expected) in [
            (VmError::ToolUnavailable, "tool.unavailable"),
            (VmError::CapabilityMissing, "tool.denied"),
            (VmError::ToolSchemaError, "tool.schema"),
        ] {
            let value =
                super::close_provider_error(super::EffectFailureFamily::Tool, &tool_type, error)
                    .unwrap();
            assert!(value.to_string().contains(expected));
        }
        for (error, expected) in [
            (
                VmError::Stopped {
                    reason: "CANARY raw stop".to_owned(),
                },
                VmError::ProtocolViolation,
            ),
            (VmError::ReplayDiverged, VmError::ReplayRuntimeDiverged),
            (VmError::ArithmeticOverflow, VmError::ProtocolViolation),
        ] {
            assert_eq!(
                super::close_provider_error(
                    super::EffectFailureFamily::Operation(FsOperation::AgentAsk),
                    &ValueType::Result(
                        Box::new(ValueType::String),
                        Box::new(allen_bytecode::standard_error_type()),
                    ),
                    error,
                ),
                Err(expected)
            );
        }
    }

    fn forged_standard_error(code: &str, message: &str) -> Value {
        Value::Enum(Rc::new(super::builtin_enum(
            EnumIdentity::Result,
            1,
            EnumPayload::Tuple(
                vec![Value::Record(
                    vec![
                        ("code".into(), Value::String(code.into())),
                        ("message".into(), Value::String(message.into())),
                    ]
                    .into(),
                )]
                .into(),
            ),
        )))
    }

    #[test]
    fn operation_result_allowlists_exclude_every_terminal_channel_code() {
        let operations = [
            FsOperation::ReadText,
            FsOperation::ReadBytes,
            FsOperation::WriteText,
            FsOperation::WriteBytes,
            FsOperation::List,
            FsOperation::Search,
            FsOperation::HttpGet,
            FsOperation::PermissionRequestFile,
            FsOperation::PermissionRequestDirectory,
            FsOperation::AgentMessage,
            FsOperation::AgentAsk,
            FsOperation::AgentTranscript,
            FsOperation::ModelRequest,
            FsOperation::UserAsk,
            FsOperation::SubAgentCreate,
            FsOperation::SubAgentRun,
            FsOperation::SubAgentMessage,
            FsOperation::SubAgentAsk,
        ];
        for operation in operations {
            for code in [
                "resource.limit",
                "runtime.cancelled",
                "runtime.timeout",
                "runtime.panic",
                "replay.diverged",
                "replay.runtime_diverged",
                "protocol.violation",
                "stopped",
            ] {
                assert!(
                    !super::operation_allows_error_code(operation, code),
                    "{operation:?} admitted terminal-only code {code}"
                );
            }
        }
        assert!(super::operation_allows_error_code(
            FsOperation::ReadText,
            "fs.not_found"
        ));
        assert!(super::operation_allows_error_code(
            FsOperation::HttpGet,
            "net.dns"
        ));
        assert!(super::operation_allows_error_code(
            FsOperation::AgentAsk,
            "agent.validation_failed"
        ));
    }

    #[test]
    fn ready_and_pending_provider_values_enforce_closed_error_semantics() {
        for value in [
            forged_standard_error("unknown.code", "safe"),
            forged_standard_error("resource.limit", "resource exhausted"),
            forged_standard_error("network.unavailable", &"CANARY".repeat(300)),
            forged_standard_error("agent.unavailable", "wrong family"),
        ] {
            for (pending, replayed) in [(false, false), (true, false), (true, true)] {
                let mut provider = SemanticValueProvider {
                    value: value.clone(),
                    pending,
                    replayed,
                };
                assert_eq!(
                    run_effect(&http_effect_module(false), &mut provider)
                        .unwrap_err()
                        .error,
                    VmError::ProtocolViolation
                );
            }
        }
        for pending in [false, true] {
            let mut provider = SemanticValueProvider {
                value: forged_standard_error(
                    "network.unavailable",
                    "network provider is unavailable",
                ),
                pending,
                replayed: false,
            };
            assert!(run_effect(&http_effect_module(false), &mut provider).is_ok());

            let mut provider = SemanticValueProvider {
                value: forged_standard_error("fs.not_found", "file not found"),
                pending,
                replayed: false,
            };
            assert!(run_effect(&filesystem_effect_module(), &mut provider).is_ok());
        }
    }

    #[test]
    fn provider_resource_limit_remains_terminal_after_cleanup() {
        let terminal = VmError::ResourceLimit {
            resource: "resource.http_requests",
        };
        for pending in [false, true] {
            let mut provider = TerminalProviderError {
                error: terminal.clone(),
                pending,
                cleanup_calls: 0,
            };
            let failure = run_effect(&http_effect_module(false), &mut provider)
                .expect_err("a real provider resource limit must remain terminal");
            assert_eq!(failure.error, terminal);
            assert_eq!(provider.cleanup_calls, 1);
        }
    }

    impl MonotonicClock for SequenceClock {
        fn now(&mut self) -> Duration {
            self.values.next().expect("test clock has another value")
        }
    }

    struct ReadyFilesystem {
        calls: usize,
    }

    impl EffectProvider for ReadyFilesystem {
        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Ok(WorkspaceValue::new(7, 0, 11))
        }

        fn call(&mut self, operation: FsOperation, arguments: &[Value]) -> Result<Value, VmError> {
            assert_eq!(operation, FsOperation::ReadText);
            assert!(matches!(
                arguments,
                [Value::Workspace(workspace), Value::String(path)]
                    if *workspace == WorkspaceValue::new(7, 0, 11)
                        && path.as_ref() == "note.txt"
            ));
            self.calls += 1;
            Ok(Value::Enum(Rc::new(super::builtin_enum(
                EnumIdentity::Result,
                0,
                EnumPayload::Tuple(vec![Value::String(Rc::from("hello"))].into()),
            ))))
        }
    }

    #[test]
    fn lazy_filesystem_effect_uses_execution_provider_at_await() {
        let result_type =
            ValueType::Result(Box::new(ValueType::String), Box::new(file_error_type()));
        let module = verify(Module {
            constants: vec![Constant::String("note.txt".to_owned())],
            enum_types: vec![],
            effect_sets: vec![vec!["fs.read".to_owned()]],
            functions: vec![Function {
                name: "main.allen::main".to_owned(),
                parameters: vec![],
                captures: vec![],
                effects: 0,
                registers: vec![
                    ValueType::Workspace,
                    ValueType::String,
                    ValueType::Future(Box::new(result_type.clone())),
                    result_type.clone(),
                ],
                return_type: result_type,
                code: vec![
                    Instruction::WorkspaceGet { destination: 0 },
                    Instruction::Const {
                        destination: 1,
                        constant: 0,
                    },
                    Instruction::EffectCall {
                        destination: 2,
                        operation: FsOperation::ReadText,
                        arguments: vec![0, 1],
                    },
                    Instruction::Await {
                        destination: 3,
                        source: 2,
                    },
                    Instruction::Return { source: 3 },
                ],
            }],
            async_functions: vec![0],
            entry: 0,
        })
        .expect("filesystem module verifies");
        let mut provider = ReadyFilesystem { calls: 0 };
        let outcome = execute_entry_with_runtime_context(
            &module,
            None,
            0,
            &[],
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut super::NeverCancelled,
            &mut provider,
        )
        .expect("filesystem effect executes");
        assert_eq!(provider.calls, 1);
        assert!(matches!(outcome, ExecutionOutcome::Completed(_)));
    }

    #[test]
    fn ready_and_spawned_provider_type_mismatches_are_protocol_violations() {
        let http = http_effect_module(false);
        let header_value =
            || Value::List(vec![Value::String(Rc::from("application/octet-stream"))].into());
        let mut ready_http = ReadyEffect {
            operation: FsOperation::HttpGet,
            result: http_response(vec![
                (Value::String(Rc::from("content-type")), header_value()),
                (Value::String(Rc::from("x-test")), header_value()),
            ]),
            calls: 0,
        };
        assert!(matches!(
            run_effect(&http, &mut ready_http).unwrap(),
            ExecutionOutcome::Completed(_)
        ));
        assert_eq!(ready_http.calls, 1);

        let mut noncanonical_headers = ReadyEffect {
            operation: FsOperation::HttpGet,
            result: http_response(vec![
                (Value::String(Rc::from("x-test")), header_value()),
                (Value::String(Rc::from("content-type")), header_value()),
            ]),
            calls: 0,
        };
        assert_eq!(
            run_effect(&http, &mut noncanonical_headers)
                .unwrap_err()
                .error,
            VmError::ProtocolViolation
        );

        let permission = permission_effect_module();
        let workspace = WorkspaceValue::new(9, 4, 0xa55a);
        let mut ready_permission = ReadyEffect {
            operation: FsOperation::PermissionRequestFile,
            result: result_ok(Value::Workspace(workspace)),
            calls: 0,
        };
        let outcome = run_effect(&permission, &mut ready_permission).unwrap();
        assert!(matches!(
            outcome,
            ExecutionOutcome::Completed(result)
                if result.value == result_ok(Value::Workspace(workspace))
        ));

        let mut wrong_grant = ReadyEffect {
            operation: FsOperation::PermissionRequestFile,
            result: result_ok(Value::String(Rc::from("forge"))),
            calls: 0,
        };
        assert_eq!(
            run_effect(&permission, &mut wrong_grant).unwrap_err().error,
            VmError::ProtocolViolation
        );

        let spawned_http = http_effect_module(true);
        let mut wrong_spawned_http = ReadyEffect {
            operation: FsOperation::HttpGet,
            result: result_ok(Value::String(Rc::from("wrong"))),
            calls: 0,
        };
        assert_eq!(
            run_effect(&spawned_http, &mut wrong_spawned_http)
                .unwrap_err()
                .error,
            VmError::ProtocolViolation
        );
    }

    #[test]
    fn pending_provider_type_mismatch_is_a_protocol_violation_with_cleanup() {
        let mut provider = PendingWrongEffect {
            polls: 0,
            cancelled: 0,
        };
        let failure = run_effect(&http_effect_module(false), &mut provider).unwrap_err();

        assert_eq!(failure.error, VmError::ProtocolViolation);
        assert_eq!(provider.polls, 1);
        assert_eq!(provider.cancelled, 1);
    }

    #[test]
    fn external_access_is_scalar_and_handles_remain_opaque() {
        let access = Value::ExternalFsAccess(ExternalFsAccess::ReadWrite);
        assert_eq!(access.to_string(), "ExternalFsAccess.ReadWrite");
        assert!(super::language_equal(&access, &access));
        assert!(!super::language_equal(
            &access,
            &Value::ExternalFsAccess(ExternalFsAccess::Read)
        ));
        assert_eq!(
            encode_canonical(&access),
            Err(CanonicalEncodeError::InvalidValue)
        );

        let access_module = verified(
            vec![Constant::ExternalFsAccess(ExternalFsAccess::ReadWrite)],
            vec![ValueType::ExternalFsAccess],
            ValueType::ExternalFsAccess,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Return { source: 0 },
            ],
        );
        let unit_module = verified(
            vec![Constant::Unit],
            vec![ValueType::Unit],
            ValueType::Unit,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Return { source: 0 },
            ],
        );
        assert_eq!(
            execute_with_limits(&access_module, ExecutionLimits::default())
                .unwrap()
                .usage
                .allocation_bytes,
            execute_with_limits(&unit_module, ExecutionLimits::default())
                .unwrap()
                .usage
                .allocation_bytes
        );

        let workspace = WorkspaceValue::new(17, 3, 0xdead_beef);
        assert_eq!(format!("{workspace:?}"), "WorkspaceValue(<opaque>)");
        let workspace = Value::Workspace(workspace);
        assert_eq!(workspace.to_string(), "<workspace>");
        assert_eq!(
            format!("{workspace:?}"),
            "Workspace(WorkspaceValue(<opaque>))"
        );
        assert!(!super::language_equal(&workspace, &workspace));
        assert_eq!(
            encode_canonical(&workspace),
            Err(CanonicalEncodeError::InvalidValue)
        );
        assert_eq!(
            encode_canonical(&result_ok(workspace)),
            Err(CanonicalEncodeError::InvalidValue)
        );
    }

    #[test]
    fn missing_permission_agent_is_a_typed_unavailable_error() {
        let permission = permission_effect_module();
        let outcome = execute_with_runtime_context(
            &permission,
            None,
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut super::NeverCancelled,
        )
        .unwrap();
        assert_eq!(result_error_code(outcome), "permission.unavailable");
    }

    #[derive(Default)]
    struct RecordingObserver(Vec<Checkpoint>);

    impl CheckpointObserver for RecordingObserver {
        fn checkpoint(&mut self, checkpoint: Checkpoint) {
            self.0.push(checkpoint);
        }
    }

    #[derive(Default)]
    struct LifecycleObserver(Vec<TaskEvent>);

    impl CheckpointObserver for LifecycleObserver {
        fn checkpoint(&mut self, _checkpoint: Checkpoint) {}

        fn task_event(&mut self, event: TaskEvent) {
            self.0.push(event);
        }
    }

    #[derive(Default)]
    struct BudgetObserver(Vec<BudgetWarning>);

    impl CheckpointObserver for BudgetObserver {
        fn checkpoint(&mut self, _checkpoint: Checkpoint) {}

        fn budget_warning(&mut self, warning: BudgetWarning) {
            self.0.push(warning);
        }
    }

    #[test]
    fn finite_instruction_budget_warns_at_the_frozen_threshold_once() {
        let module = verified(
            vec![Constant::Int(1)],
            vec![ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Return { source: 0 },
            ],
        );
        let mut observer = BudgetObserver::default();

        execute_with_context(
            &module,
            None,
            ExecutionLimits {
                instructions: 10,
                ..ExecutionLimits::default()
            },
            &mut FixedClock,
            &mut observer,
        )
        .expect("ten successful charges fit the limit");

        assert_eq!(
            observer.0,
            vec![BudgetWarning {
                resource: super::RESOURCE_INSTRUCTIONS,
                used: 9,
                limit: 10,
            }]
        );
    }

    #[test]
    fn unlimited_and_failed_charges_do_not_warn() {
        let module = verified(
            vec![Constant::Int(1)],
            vec![ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Return { source: 0 },
            ],
        );
        let mut unlimited = BudgetObserver::default();
        execute_with_context(
            &module,
            None,
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut unlimited,
        )
        .expect("unlimited execution succeeds");
        assert!(unlimited.0.is_empty());

        let mut failed = BudgetObserver::default();
        execute_with_context(
            &module,
            None,
            ExecutionLimits {
                instructions: 0,
                ..ExecutionLimits::default()
            },
            &mut FixedClock,
            &mut failed,
        )
        .expect_err("the first instruction exceeds a zero limit");
        assert!(failed.0.is_empty());
    }

    #[test]
    fn every_finite_vm_resource_uses_the_same_warning_rule() {
        let mut observer = BudgetObserver::default();
        let mut clock = SequenceClock {
            values: std::iter::once(Duration::ZERO)
                .chain(std::iter::repeat_n(Duration::from_millis(90), 9))
                .collect::<Vec<_>>()
                .into_iter(),
        };
        let mut cancellation = super::NeverCancelled;
        let limits = ExecutionLimits {
            instructions: 10,
            allocation_bytes: 100,
            maximum_allocation_bytes: 50,
            call_depth: 10,
            wall_time: Duration::from_millis(100),
            tasks: 10,
            concurrent_effects: 10,
            cleanup_instructions: 10,
        };
        {
            let mut budget =
                super::Budget::new(limits, &mut clock, &mut observer, &mut cancellation);

            budget.enter_frame(9).unwrap();
            for instruction in 0..9 {
                budget
                    .charge_instruction(Checkpoint {
                        function: 0,
                        instruction,
                    })
                    .unwrap();
            }
            budget.charge_allocation(45).unwrap();
            budget.charge_allocation(45).unwrap();
            budget.record_task_started(9);
            for _ in 0..9 {
                budget.start_effect().unwrap();
            }
        }

        let mut machine = super::TaskMachine {
            cleanup_limit: 10,
            cleanup_remaining: 10,
            ..super::TaskMachine::default()
        };
        assert_eq!(machine.charge_cleanup(9, &mut observer), None);

        assert_eq!(
            observer.0,
            vec![
                BudgetWarning {
                    resource: super::RESOURCE_CALL_DEPTH,
                    used: 9,
                    limit: 10,
                },
                BudgetWarning {
                    resource: super::RESOURCE_WALL_TIME,
                    used: 90,
                    limit: 100,
                },
                BudgetWarning {
                    resource: super::RESOURCE_INSTRUCTIONS,
                    used: 9,
                    limit: 10,
                },
                BudgetWarning {
                    resource: super::RESOURCE_MAXIMUM_ALLOCATION_BYTES,
                    used: 45,
                    limit: 50,
                },
                BudgetWarning {
                    resource: super::RESOURCE_ALLOCATION_BYTES,
                    used: 90,
                    limit: 100,
                },
                BudgetWarning {
                    resource: super::RESOURCE_TASKS,
                    used: 9,
                    limit: 10,
                },
                BudgetWarning {
                    resource: super::RESOURCE_CONCURRENT_EFFECTS,
                    used: 9,
                    limit: 10,
                },
                BudgetWarning {
                    resource: super::RESOURCE_CLEANUP_INSTRUCTIONS,
                    used: 9,
                    limit: 10,
                },
            ]
        );
    }

    #[test]
    fn wall_limit_and_checkpoint_boundaries_are_deterministic() {
        let module = verified(
            vec![Constant::Int(1)],
            vec![ValueType::Int],
            ValueType::Int,
            vec![
                Instruction::Const {
                    destination: 0,
                    constant: 0,
                },
                Instruction::Return { source: 0 },
            ],
        );
        let mut clock = SequenceClock {
            values: vec![Duration::ZERO, Duration::ZERO, Duration::from_nanos(2)].into_iter(),
        };
        let mut observer = RecordingObserver::default();

        let failure = execute_with_context(
            &module,
            None,
            ExecutionLimits {
                wall_time: Duration::from_nanos(2),
                ..ExecutionLimits::default()
            },
            &mut clock,
            &mut observer,
        )
        .expect_err("second checkpoint reaches the deadline");

        assert_eq!(
            failure.error,
            VmError::Timeout {
                resource: "wall_time",
            }
        );
        assert_eq!(failure.code(), "runtime.timeout");
        assert_eq!(
            observer.0,
            vec![
                Checkpoint {
                    function: 0,
                    instruction: 0,
                },
                Checkpoint {
                    function: 0,
                    instruction: 1,
                },
            ]
        );
        assert_eq!(failure.frames[0].instruction, 1);
    }

    fn failing_nested_call_module() -> allen_bytecode::VerifiedModule {
        let closure_type = ValueType::Function {
            parameters: vec![],
            return_type: Box::new(ValueType::Int),
            effects: 0,
        };
        verify(Module {
            constants: vec![Constant::Int(1), Constant::Int(0)],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::DirectCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
                Function {
                    name: "middle".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![closure_type, ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::ClosureNew {
                            destination: 0,
                            function: 2,
                            captures: vec![],
                        },
                        Instruction::ClosureCall {
                            destination: 1,
                            closure: 0,
                            arguments: vec![],
                        },
                        Instruction::Return { source: 1 },
                    ],
                },
                Function {
                    name: "inner".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int, ValueType::Int, ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Const {
                            destination: 1,
                            constant: 1,
                        },
                        Instruction::IntBinary {
                            operation: NumericBinaryOp::Divide,
                            destination: 2,
                            left: 0,
                            right: 1,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
            ],
            async_functions: vec![],
            entry: 0,
        })
        .expect("nested failure module verifies")
    }

    #[test]
    fn nested_failure_has_stable_innermost_first_frames() {
        let module = failing_nested_call_module();
        let debug = DebugInfo {
            sources: vec!["main.allen".to_owned()],
            locations: vec![
                DebugLocation {
                    function: 0,
                    instruction: 0,
                    source: 0,
                    start: 0,
                    end: 1,
                },
                DebugLocation {
                    function: 1,
                    instruction: 1,
                    source: 0,
                    start: 101,
                    end: 102,
                },
                DebugLocation {
                    function: 2,
                    instruction: 2,
                    source: 0,
                    start: 202,
                    end: 203,
                },
            ],
        };
        let mut clock = FixedClock;
        let mut observer = super::IgnoreCheckpoints;
        let failure = execute_with_context(
            &module,
            Some(&debug),
            ExecutionLimits::default(),
            &mut clock,
            &mut observer,
        )
        .expect_err("division must fail");

        assert_eq!(failure.error, VmError::DivisionByZero);
        assert_eq!(
            failure
                .frames
                .iter()
                .map(|frame| (
                    frame.function,
                    frame.function_name.as_str(),
                    frame.instruction
                ))
                .collect::<Vec<_>>(),
            vec![(2, "inner", 2), (1, "middle", 1), (0, "main", 0)]
        );
        assert_eq!(failure.frames[0].source.as_ref().unwrap().start, 202);

        let mut clock = FixedClock;
        let mut observer = super::IgnoreCheckpoints;
        let stripped = execute_with_context(
            &module,
            None,
            ExecutionLimits::default(),
            &mut clock,
            &mut observer,
        )
        .expect_err("division must fail");
        assert_eq!(
            stripped
                .frames
                .iter()
                .map(|frame| (
                    frame.function,
                    frame.function_name.as_str(),
                    frame.instruction
                ))
                .collect::<Vec<_>>(),
            vec![(2, "inner", 2), (1, "middle", 1), (0, "main", 0)]
        );
        assert!(stripped.frames.iter().all(|frame| frame.source.is_none()));
    }

    fn verified_async_module(implicit_join: bool) -> allen_bytecode::VerifiedModule {
        let mut code = vec![
            Instruction::TaskScopeEnter { scope: 1 },
            Instruction::AsyncCall {
                destination: 0,
                function: 1,
                arguments: vec![],
            },
            Instruction::Spawn {
                destination: 1,
                future: 0,
                scope: 1,
            },
        ];
        if !implicit_join {
            code.push(Instruction::Await {
                destination: 2,
                source: 1,
            });
        }
        code.extend([
            Instruction::TaskScopeExit { scope: 1 },
            Instruction::Const {
                destination: 3,
                constant: 1,
            },
            Instruction::Return { source: 3 },
        ]);
        verify(Module {
            constants: vec![Constant::Int(7), Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Int,
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Unit,
                    effects: 1,
                    code,
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        })
        .expect("async module must verify")
    }

    #[test]
    fn executes_explicit_await_and_implicit_scope_join_without_orphans() {
        for implicit_join in [false, true] {
            let result = execute_with_limits(
                &verified_async_module(implicit_join),
                ExecutionLimits::default(),
            )
            .unwrap();
            assert_eq!(result.value, Value::Unit);
            assert_eq!(result.usage.tasks_started, 1);
            assert_eq!(result.usage.maximum_live_tasks, 1);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn branch_return_waits_for_explicit_scope_cleanup() {
        let module = verify(Module {
            constants: vec![Constant::Bool(true), Constant::Unit, Constant::Int(7)],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Bool,
                        ValueType::Unit,
                        ValueType::Int,
                    ],
                    return_type: ValueType::Unit,
                    effects: 1,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 1,
                        },
                        Instruction::Const {
                            destination: 2,
                            constant: 0,
                        },
                        Instruction::BranchBool {
                            condition: 2,
                            true_target: 5,
                            false_target: 8,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Const {
                            destination: 3,
                            constant: 1,
                        },
                        Instruction::Return { source: 3 },
                        Instruction::Await {
                            destination: 4,
                            source: 1,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Const {
                            destination: 3,
                            constant: 1,
                        },
                        Instruction::Return { source: 3 },
                    ],
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 2,
                        },
                        Instruction::Move {
                            destination: 0,
                            source: 0,
                        },
                        Instruction::Move {
                            destination: 0,
                            source: 0,
                        },
                        Instruction::Move {
                            destination: 0,
                            source: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        })
        .expect("both branch exits discharge the scoped task");
        let mut observer = LifecycleObserver::default();
        let result = execute_with_context(
            &module,
            None,
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut observer,
        )
        .expect("scope cleanup completes before the branch returns");

        assert_eq!(result.value, Value::Unit);
        assert_eq!(result.usage.tasks_started, 1);
        let root_waiting = observer
            .0
            .iter()
            .position(|event| event.task_id == 0 && event.kind == TaskEventKind::Waiting)
            .expect("scope exit waits for the still-running child");
        let child_completed = observer
            .0
            .iter()
            .position(|event| event.task_id == 1 && event.kind == TaskEventKind::Completed)
            .expect("scoped child completes");
        let root_completed = observer
            .0
            .iter()
            .position(|event| event.task_id == 0 && event.kind == TaskEventKind::Completed)
            .expect("root completes");
        assert!(root_waiting < child_completed);
        assert!(child_completed < root_completed);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn task_scopes_are_distinct_across_async_call_frames() {
        let module = verify(Module {
            constants: vec![Constant::Int(7), Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Future(Box::new(ValueType::Unit)),
                        ValueType::Unit,
                        ValueType::Int,
                    ],
                    return_type: ValueType::Int,
                    effects: 1,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 2,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 1,
                        },
                        Instruction::AsyncCall {
                            destination: 2,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Await {
                            destination: 3,
                            source: 2,
                        },
                        Instruction::Await {
                            destination: 4,
                            source: 1,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Return { source: 4 },
                    ],
                },
                Function {
                    name: "helper".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Unit,
                    effects: 1,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 2,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 1,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Const {
                            destination: 2,
                            constant: 1,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1, 2],
            entry: 0,
        })
        .unwrap();

        assert_eq!(execute(&module).unwrap(), Value::Int(7));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn explicitly_awaited_task_result_joins_the_current_scope() {
        let module = verify(Module {
            constants: vec![Constant::Int(7), Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Task(Box::new(ValueType::Int)))),
                        ValueType::Task(Box::new(ValueType::Task(Box::new(ValueType::Int)))),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Unit,
                    effects: 1,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 1,
                        },
                        Instruction::Await {
                            destination: 2,
                            source: 1,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Const {
                            destination: 3,
                            constant: 1,
                        },
                        Instruction::Return { source: 3 },
                    ],
                },
                Function {
                    name: "producer".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                    ],
                    return_type: ValueType::Task(Box::new(ValueType::Int)),
                    effects: 1,
                    code: vec![
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 2,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 0,
                        },
                        Instruction::Return { source: 1 },
                    ],
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1, 2],
            entry: 0,
        })
        .unwrap();

        let result = execute_with_limits(&module, ExecutionLimits::default()).unwrap();
        assert_eq!(result.value, Value::Unit);
        assert_eq!(result.usage.tasks_started, 2);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn task_snapshot_is_non_consuming_source_aware_and_safe() {
        let snapshot_type = task_snapshot_type();
        let module = verify(Module {
            constants: vec![Constant::Int(42)],
            enum_types: vec![],
            effect_sets: vec![
                vec![],
                vec!["debug.inspect".to_owned(), "task.spawn".to_owned()],
            ],
            functions: vec![
                Function {
                    name: "main.allen::main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        snapshot_type.clone(),
                        ValueType::Int,
                    ],
                    return_type: snapshot_type,
                    effects: 1,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 1,
                        },
                        Instruction::TaskSnapshot {
                            destination: 2,
                            source: 1,
                        },
                        Instruction::Await {
                            destination: 3,
                            source: 1,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Return { source: 2 },
                    ],
                },
                Function {
                    name: "main.allen::answer".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        })
        .unwrap();
        let debug = DebugInfo {
            sources: vec!["main.allen".to_owned()],
            locations: vec![DebugLocation {
                function: 1,
                instruction: 0,
                source: 0,
                start: 10,
                end: 20,
            }],
        };
        let mut clock = FixedClock;
        let mut observer = LifecycleObserver::default();
        let result = execute_with_context(
            &module,
            Some(&debug),
            ExecutionLimits::default(),
            &mut clock,
            &mut observer,
        )
        .unwrap();
        assert_eq!(
            result.value,
            Value::Record(
                vec![
                    (
                        Rc::from("function"),
                        Value::String(Rc::from("main.allen::answer"))
                    ),
                    (Rc::from("id"), Value::Int(1)),
                    (
                        Rc::from("location"),
                        Value::Enum(Rc::new(super::builtin_enum(
                            EnumIdentity::Option,
                            1,
                            EnumPayload::Tuple(
                                vec![Value::String(Rc::from("main.allen:10..20"))].into(),
                            ),
                        ))),
                    ),
                    (Rc::from("owner_id"), Value::Int(0)),
                    (Rc::from("state"), Value::String(Rc::from("ready"))),
                ]
                .into(),
            )
        );
        assert_eq!(observer.0[0].kind, TaskEventKind::Spawned);
        assert!(
            observer
                .0
                .iter()
                .any(|event| { event.task_id == 1 && event.kind == TaskEventKind::Completed })
        );
    }

    #[test]
    fn task_snapshot_maps_all_scheduler_states_to_stable_text() {
        let module = Module {
            constants: vec![],
            enum_types: vec![],
            effect_sets: vec![vec![]],
            functions: vec![Function {
                name: "main.allen::worker".to_owned(),
                parameters: vec![],
                captures: vec![],
                registers: vec![ValueType::Unit],
                return_type: ValueType::Unit,
                effects: 0,
                code: vec![Instruction::Return { source: 0 }],
            }],
            async_functions: vec![0],
            entry: 0,
        };
        let states = [
            (super::MachineTaskState::Ready, "ready"),
            (
                super::MachineTaskState::Waiting(super::WaitState::Task {
                    handle: 2,
                    destination: 0,
                }),
                "waiting",
            ),
            (
                super::MachineTaskState::Completed(Ok(Value::Unit)),
                "completed",
            ),
            (
                super::MachineTaskState::Completed(Err(super::TaskFailure {
                    error: VmError::DivisionByZero,
                    stack: vec![],
                })),
                "failed",
            ),
        ];
        let mut clock = FixedClock;
        let mut observer = LifecycleObserver::default();
        let mut cancellation = super::NeverCancelled;
        let mut budget = super::Budget::new(
            ExecutionLimits::default(),
            &mut clock,
            &mut observer,
            &mut cancellation,
        );
        for (state, expected) in states {
            let mut machine = super::TaskMachine::default();
            machine.tasks.insert(
                1,
                super::MachineTask {
                    owner: 0,
                    scope: None,
                    entry_function: 0,
                    frames: vec![],
                    state,
                },
            );
            let Value::Record(fields) = machine
                .task_snapshot(&module, None, &mut budget, 0, super::TaskValue { id: 1 })
                .unwrap()
            else {
                panic!("task snapshot must be a record");
            };
            assert_eq!(
                fields.last(),
                Some(&(Rc::from("state"), Value::String(Rc::from(expected))))
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn ready_children_rotate_after_one_instruction_checkpoint() {
        let module = verify(Module {
            constants: vec![Constant::Int(1), Constant::Unit],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Unit,
                    effects: 1,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::AsyncCall {
                            destination: 2,
                            function: 2,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 1,
                        },
                        Instruction::Spawn {
                            destination: 3,
                            future: 2,
                            scope: 1,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Const {
                            destination: 4,
                            constant: 1,
                        },
                        Instruction::Return { source: 4 },
                    ],
                },
                Function {
                    name: "first".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int, ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Move {
                            destination: 1,
                            source: 0,
                        },
                        Instruction::Return { source: 1 },
                    ],
                },
                Function {
                    name: "second".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int, ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Move {
                            destination: 1,
                            source: 0,
                        },
                        Instruction::Return { source: 1 },
                    ],
                },
            ],
            async_functions: vec![0, 1, 2],
            entry: 0,
        })
        .unwrap();
        let mut observer = RecordingObserver::default();
        execute_with_context(
            &module,
            None,
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut observer,
        )
        .unwrap();
        let child_trace = observer
            .0
            .into_iter()
            .filter(|checkpoint| checkpoint.function != 0)
            .collect::<Vec<_>>();
        assert_eq!(
            child_trace,
            vec![
                Checkpoint {
                    function: 1,
                    instruction: 0
                },
                Checkpoint {
                    function: 1,
                    instruction: 1
                },
                Checkpoint {
                    function: 2,
                    instruction: 0
                },
                Checkpoint {
                    function: 1,
                    instruction: 2
                },
                Checkpoint {
                    function: 2,
                    instruction: 1
                },
                Checkpoint {
                    function: 2,
                    instruction: 2
                },
            ]
        );
    }

    #[test]
    fn spawn_transfers_captured_task_ownership_to_the_child() {
        let module = verify(Module {
            constants: vec![Constant::Int(7)],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Int,
                    ],
                    return_type: ValueType::Int,
                    effects: 1,
                    code: vec![
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 0,
                        },
                        Instruction::AsyncCall {
                            destination: 2,
                            function: 2,
                            arguments: vec![1],
                        },
                        Instruction::Spawn {
                            destination: 3,
                            future: 2,
                            scope: 0,
                        },
                        Instruction::Await {
                            destination: 4,
                            source: 3,
                        },
                        Instruction::Return { source: 4 },
                    ],
                },
                Function {
                    name: "produce".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
                Function {
                    name: "consume".to_owned(),
                    parameters: vec![0],
                    captures: vec![],
                    registers: vec![ValueType::Task(Box::new(ValueType::Int)), ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Await {
                            destination: 1,
                            source: 0,
                        },
                        Instruction::Return { source: 1 },
                    ],
                },
            ],
            async_functions: vec![0, 1, 2],
            entry: 0,
        })
        .expect("task ownership transfer must verify");

        let result = execute_with_limits(&module, ExecutionLimits::default()).unwrap();
        assert_eq!(result.value, Value::Int(7));
        assert_eq!(result.usage.tasks_started, 2);
        assert_eq!(result.usage.maximum_live_tasks, 2);
    }

    #[test]
    fn task_limit_fails_before_spawn() {
        let failure = execute_with_limits(
            &verified_async_module(false),
            ExecutionLimits {
                tasks: 0,
                ..ExecutionLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            failure,
            VmError::ResourceLimit {
                resource: RESOURCE_TASKS
            }
        );
    }

    #[test]
    fn stopped_outcome_wins_over_cleanup_budget_failure() {
        let module = verify(Module {
            constants: vec![Constant::String("done".to_owned()), Constant::Int(7)],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::String,
                    ],
                    return_type: ValueType::Unit,
                    effects: 1,
                    code: vec![
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 0,
                        },
                        Instruction::Const {
                            destination: 2,
                            constant: 0,
                        },
                        Instruction::Stop { reason: 2 },
                    ],
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 1,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        })
        .unwrap();
        let mut clock = FixedClock;
        let mut observer = super::IgnoreCheckpoints;
        let mut cancellation = super::NeverCancelled;
        let outcome = execute_with_runtime_context(
            &module,
            None,
            ExecutionLimits {
                cleanup_instructions: 0,
                ..ExecutionLimits::default()
            },
            &mut clock,
            &mut observer,
            &mut cancellation,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            ExecutionOutcome::Stopped {
                reason,
                cleanup_failure: Some("cleanup_instructions"),
                ..
            } if reason == "done"
        ));

        let unavailable_failure = execute_with_context(
            &module,
            None,
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
        )
        .unwrap_err();
        assert_eq!(
            unavailable_failure.error,
            VmError::Stopped {
                reason: "done".to_owned()
            }
        );
    }

    #[test]
    fn root_failure_reports_exhausted_cleanup_budget() {
        let module = verify(Module {
            constants: vec![Constant::Int(7), Constant::Int(0)],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Int,
                        ValueType::Int,
                        ValueType::Int,
                    ],
                    return_type: ValueType::Int,
                    effects: 1,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 1,
                        },
                        Instruction::Const {
                            destination: 2,
                            constant: 0,
                        },
                        Instruction::Const {
                            destination: 3,
                            constant: 1,
                        },
                        Instruction::IntBinary {
                            destination: 4,
                            left: 2,
                            right: 3,
                            operation: NumericBinaryOp::Divide,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Return { source: 4 },
                    ],
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        })
        .unwrap();

        let failure = execute_with_limits(
            &module,
            ExecutionLimits {
                cleanup_instructions: 0,
                ..ExecutionLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            failure,
            VmError::ResourceLimit {
                resource: RESOURCE_CLEANUP_INSTRUCTIONS,
            }
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn stop_in_spawned_child_is_terminal_before_sibling_progress() {
        let module = verify(Module {
            constants: vec![Constant::String("child stop".to_owned()), Constant::Int(1)],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Unit)),
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Unit)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Int,
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Int,
                    effects: 1,
                    code: vec![
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::AsyncCall {
                            destination: 1,
                            function: 2,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 2,
                            future: 0,
                            scope: 0,
                        },
                        Instruction::Spawn {
                            destination: 3,
                            future: 1,
                            scope: 0,
                        },
                        Instruction::Await {
                            destination: 5,
                            source: 2,
                        },
                        Instruction::Await {
                            destination: 4,
                            source: 3,
                        },
                        Instruction::Return { source: 4 },
                    ],
                },
                Function {
                    name: "stopper".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::String],
                    return_type: ValueType::Unit,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Stop { reason: 0 },
                    ],
                },
                Function {
                    name: "sibling".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 1,
                        },
                        Instruction::Move {
                            destination: 0,
                            source: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1, 2],
            entry: 0,
        })
        .unwrap();
        let mut observer = RecordingObserver::default();
        let mut cancellation = super::NeverCancelled;
        let outcome = execute_with_runtime_context(
            &module,
            None,
            ExecutionLimits {
                cleanup_instructions: 0,
                ..ExecutionLimits::default()
            },
            &mut FixedClock,
            &mut observer,
            &mut cancellation,
        )
        .unwrap();
        assert!(matches!(outcome, ExecutionOutcome::Stopped {
            reason, cleanup_failure: Some(resource), ..
        } if reason == "child stop" && resource == "cleanup_instructions"));
        let stop_position = observer
            .0
            .iter()
            .position(|checkpoint| checkpoint.function == 1 && checkpoint.instruction == 1)
            .expect("child STOP checkpoint");
        assert!(observer.0[stop_position + 1..].is_empty());
    }

    #[test]
    fn try_result_err_joins_active_scope_before_early_return() {
        let result_type = ValueType::Result(Box::new(ValueType::Int), Box::new(ValueType::String));
        let module = verify(Module {
            constants: vec![Constant::String("no".to_owned()), Constant::Int(1)],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::String,
                        result_type.clone(),
                        ValueType::Int,
                    ],
                    return_type: result_type.clone(),
                    effects: 1,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 1,
                        },
                        Instruction::Const {
                            destination: 2,
                            constant: 0,
                        },
                        Instruction::EnumNew {
                            destination: 3,
                            variant: 1,
                            payload: vec![2],
                        },
                        Instruction::TryResult {
                            destination: 4,
                            source: 3,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::EnumNew {
                            destination: 3,
                            variant: 0,
                            payload: vec![4],
                        },
                        Instruction::Return { source: 3 },
                    ],
                },
                Function {
                    name: "worker".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 1,
                        },
                        Instruction::Return { source: 0 },
                    ],
                },
            ],
            async_functions: vec![0, 1],
            entry: 0,
        })
        .unwrap();
        let result = execute_with_limits(&module, ExecutionLimits::default()).unwrap();
        assert!(matches!(result.value, Value::Enum(value) if value.variant == 1));
        assert_eq!(result.usage.tasks_started, 1);
    }

    struct CancelAfter {
        remaining: usize,
    }

    impl CancellationSource for CancelAfter {
        fn is_cancelled(&mut self) -> bool {
            if self.remaining == 0 {
                true
            } else {
                self.remaining -= 1;
                false
            }
        }
    }

    #[test]
    fn external_cancellation_cleans_pending_tasks() {
        let module = verified_async_module(false);
        let mut clock = FixedClock;
        let mut observer = super::IgnoreCheckpoints;
        let mut cancellation = CancelAfter { remaining: 3 };
        let failure = execute_with_runtime_context(
            &module,
            None,
            ExecutionLimits::default(),
            &mut clock,
            &mut observer,
            &mut cancellation,
        )
        .unwrap_err();
        assert_eq!(failure.error, VmError::Cancelled);

        let cleanup_failure = execute_with_runtime_context(
            &module,
            None,
            ExecutionLimits {
                cleanup_instructions: 0,
                ..ExecutionLimits::default()
            },
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
            &mut CancelAfter { remaining: 3 },
        )
        .unwrap_err();
        assert_eq!(
            cleanup_failure.error,
            VmError::ResourceLimit {
                resource: RESOURCE_CLEANUP_INSTRUCTIONS,
            }
        );
    }

    #[test]
    fn concurrent_effect_counter_charges_before_start_and_releases() {
        let mut counter = ConcurrentEffectCounter::new(1);
        counter.start().unwrap();
        assert_eq!(counter.maximum(), 1);
        assert_eq!(
            counter.start().unwrap_err(),
            VmError::ResourceLimit {
                resource: RESOURCE_CONCURRENT_EFFECTS
            }
        );
        assert_eq!(counter.active(), 1);
        counter.complete().unwrap();
        assert_eq!(counter.active(), 0);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lowest_task_id_selects_scope_failure_and_keeps_awaiter_frame() {
        let module = verify(Module {
            constants: vec![
                Constant::Int(7),
                Constant::Int(0),
                Constant::Int(i64::MAX),
                Constant::Int(1),
                Constant::Unit,
            ],
            enum_types: vec![],
            effect_sets: vec![vec![], vec!["task.spawn".to_owned()]],
            functions: vec![
                Function {
                    name: "main".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Future(Box::new(ValueType::Int)),
                        ValueType::Task(Box::new(ValueType::Int)),
                        ValueType::Unit,
                    ],
                    return_type: ValueType::Unit,
                    effects: 1,
                    code: vec![
                        Instruction::TaskScopeEnter { scope: 1 },
                        Instruction::AsyncCall {
                            destination: 0,
                            function: 1,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 1,
                            future: 0,
                            scope: 1,
                        },
                        Instruction::AsyncCall {
                            destination: 2,
                            function: 2,
                            arguments: vec![],
                        },
                        Instruction::Spawn {
                            destination: 3,
                            future: 2,
                            scope: 1,
                        },
                        Instruction::TaskScopeExit { scope: 1 },
                        Instruction::Const {
                            destination: 4,
                            constant: 4,
                        },
                        Instruction::Return { source: 4 },
                    ],
                },
                Function {
                    name: "first".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int, ValueType::Int, ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Const {
                            destination: 1,
                            constant: 1,
                        },
                        Instruction::IntBinary {
                            destination: 2,
                            left: 0,
                            right: 1,
                            operation: NumericBinaryOp::Divide,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
                Function {
                    name: "second".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::Int, ValueType::Int, ValueType::Int],
                    return_type: ValueType::Int,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 2,
                        },
                        Instruction::Const {
                            destination: 1,
                            constant: 3,
                        },
                        Instruction::IntBinary {
                            destination: 2,
                            left: 0,
                            right: 1,
                            operation: NumericBinaryOp::Add,
                        },
                        Instruction::Return { source: 2 },
                    ],
                },
            ],
            async_functions: vec![0, 1, 2],
            entry: 0,
        })
        .unwrap();
        let failure = execute_with_context(
            &module,
            None,
            ExecutionLimits::default(),
            &mut FixedClock,
            &mut super::IgnoreCheckpoints,
        )
        .unwrap_err();
        assert_eq!(failure.error, VmError::DivisionByZero);
        assert_eq!(
            failure
                .frames
                .iter()
                .map(|frame| frame.function)
                .collect::<Vec<_>>(),
            vec![1, 0]
        );
    }

    #[test]
    fn checked_integer_operations_cover_success_and_invalid_data() {
        fn run(operation: CheckedIntOperation, left: i64, right: Option<i64>) -> ExecutionResult {
            let mut constants = vec![Constant::Int(left)];
            let mut registers = vec![ValueType::Int];
            let mut code = vec![Instruction::Const {
                destination: 0,
                constant: 0,
            }];
            let arguments = if let Some(right) = right {
                constants.push(Constant::Int(right));
                registers.push(ValueType::Int);
                code.push(Instruction::Const {
                    destination: 1,
                    constant: 1,
                });
                vec![0, 1]
            } else {
                vec![0]
            };
            let destination = u16::try_from(registers.len()).unwrap();
            registers.push(ValueType::Option(Box::new(ValueType::Int)));
            code.push(Instruction::CheckedIntCall {
                destination,
                operation,
                arguments,
            });
            code.push(Instruction::Return {
                source: destination,
            });
            execute_with_limits(
                &verify(Module {
                    constants,
                    enum_types: vec![],
                    effect_sets: vec![vec![]],
                    functions: vec![Function {
                        name: "checked".to_owned(),
                        parameters: vec![],
                        captures: vec![],
                        registers,
                        return_type: ValueType::Option(Box::new(ValueType::Int)),
                        effects: 0,
                        code,
                    }],
                    async_functions: vec![],
                    entry: 0,
                })
                .unwrap(),
                ExecutionLimits::default(),
            )
            .unwrap()
        }
        let some = |value| option_some(Value::Int(value));
        for (operation, left, right, expected) in [
            (CheckedIntOperation::Add, 40, Some(2), 42),
            (CheckedIntOperation::Subtract, 44, Some(2), 42),
            (CheckedIntOperation::Multiply, 6, Some(7), 42),
            (CheckedIntOperation::Divide, 84, Some(2), 42),
            (CheckedIntOperation::Remainder, 85, Some(43), 42),
            (CheckedIntOperation::Negate, -42, None, 42),
        ] {
            assert_eq!(run(operation, left, right).value, some(expected));
        }
        assert_eq!(
            run(CheckedIntOperation::Remainder, i64::MIN, Some(-1)).value,
            some(0)
        );
        for (operation, left, right) in [
            (CheckedIntOperation::Add, i64::MAX, Some(1)),
            (CheckedIntOperation::Subtract, i64::MIN, Some(1)),
            (CheckedIntOperation::Multiply, i64::MAX, Some(2)),
            (CheckedIntOperation::Divide, 1, Some(0)),
            (CheckedIntOperation::Divide, i64::MIN, Some(-1)),
            (CheckedIntOperation::Remainder, 1, Some(0)),
            (CheckedIntOperation::Negate, i64::MIN, None),
        ] {
            let invalid = run(operation, left, right);
            assert_eq!(invalid.value, option_none());
            let success = if operation == CheckedIntOperation::Negate {
                run(operation, 1, None)
            } else {
                run(operation, 1, Some(1))
            };
            assert!(
                invalid.usage.allocation_bytes < success.usage.allocation_bytes,
                "{operation:?} invalid path allocated an aggregate result"
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn safe_collection_gets_cover_bounds_keys_aliases_and_allocations() {
        fn list_get(index: i64) -> ExecutionResult {
            let module = verify(Module {
                constants: vec![Constant::Int(7), Constant::Int(index)],
                enum_types: vec![],
                effect_sets: vec![vec![]],
                functions: vec![Function {
                    name: "list_get".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Int,
                        ValueType::Int,
                        ValueType::List(Box::new(ValueType::Int)),
                        ValueType::Option(Box::new(ValueType::Int)),
                        ValueType::Tuple(vec![
                            ValueType::List(Box::new(ValueType::Int)),
                            ValueType::Option(Box::new(ValueType::Int)),
                        ]),
                    ],
                    return_type: ValueType::Tuple(vec![
                        ValueType::List(Box::new(ValueType::Int)),
                        ValueType::Option(Box::new(ValueType::Int)),
                    ]),
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Const {
                            destination: 1,
                            constant: 1,
                        },
                        Instruction::ListNew {
                            destination: 2,
                            elements: vec![0],
                        },
                        Instruction::SafeCollectionCall {
                            destination: 3,
                            operation: SafeCollectionOperation::ListGet,
                            arguments: vec![2, 1],
                        },
                        Instruction::TupleNew {
                            destination: 4,
                            elements: vec![2, 3],
                        },
                        Instruction::Return { source: 4 },
                    ],
                }],
                async_functions: vec![],
                entry: 0,
            })
            .unwrap();
            execute_with_limits(&module, ExecutionLimits::default()).unwrap()
        }

        fn bytes_get(index: i64) -> ExecutionResult {
            let module = verify(Module {
                constants: vec![Constant::Bytes(vec![7]), Constant::Int(index)],
                enum_types: vec![],
                effect_sets: vec![vec![]],
                functions: vec![Function {
                    name: "bytes_get".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Bytes,
                        ValueType::Int,
                        ValueType::Option(Box::new(ValueType::Int)),
                        ValueType::Tuple(vec![
                            ValueType::Bytes,
                            ValueType::Option(Box::new(ValueType::Int)),
                        ]),
                    ],
                    return_type: ValueType::Tuple(vec![
                        ValueType::Bytes,
                        ValueType::Option(Box::new(ValueType::Int)),
                    ]),
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Const {
                            destination: 1,
                            constant: 1,
                        },
                        Instruction::SafeCollectionCall {
                            destination: 2,
                            operation: SafeCollectionOperation::BytesGet,
                            arguments: vec![0, 1],
                        },
                        Instruction::TupleNew {
                            destination: 3,
                            elements: vec![0, 2],
                        },
                        Instruction::Return { source: 3 },
                    ],
                }],
                async_functions: vec![],
                entry: 0,
            })
            .unwrap();
            execute_with_limits(&module, ExecutionLimits::default()).unwrap()
        }

        fn map_get(key_type: &ValueType, entry: Constant, lookup: Constant) -> ExecutionResult {
            let map_type = ValueType::Map(Box::new(key_type.clone()), Box::new(ValueType::Int));
            let result_type = ValueType::Option(Box::new(ValueType::Int));
            let return_type = ValueType::Tuple(vec![map_type.clone(), result_type.clone()]);
            let module = verify(Module {
                constants: vec![entry, Constant::Int(42), lookup],
                enum_types: vec![],
                effect_sets: vec![vec![]],
                functions: vec![Function {
                    name: "map_get".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        key_type.clone(),
                        ValueType::Int,
                        key_type.clone(),
                        map_type,
                        result_type,
                        return_type.clone(),
                    ],
                    return_type,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Const {
                            destination: 1,
                            constant: 1,
                        },
                        Instruction::Const {
                            destination: 2,
                            constant: 2,
                        },
                        Instruction::MapNew {
                            destination: 3,
                            entries: vec![(0, 1)],
                        },
                        Instruction::SafeCollectionCall {
                            destination: 4,
                            operation: SafeCollectionOperation::MapGet,
                            arguments: vec![3, 2],
                        },
                        Instruction::TupleNew {
                            destination: 5,
                            elements: vec![3, 4],
                        },
                        Instruction::Return { source: 5 },
                    ],
                }],
                async_functions: vec![],
                entry: 0,
            })
            .unwrap();
            execute_with_limits(&module, ExecutionLimits::default()).unwrap()
        }

        let list_success = list_get(0);
        let list_negative = list_get(-1);
        let list_past = list_get(1);
        for (result, expected) in [
            (&list_success, option_some(Value::Int(7))),
            (&list_negative, option_none()),
            (&list_past, option_none()),
        ] {
            let Value::Tuple(values) = &result.value else {
                panic!("list tuple result")
            };
            assert_eq!(values[0], Value::List(vec![Value::Int(7)].into()));
            assert_eq!(values[1], expected);
        }
        assert!(list_negative.usage.allocation_bytes < list_success.usage.allocation_bytes);
        assert_eq!(
            list_negative.usage.allocation_bytes,
            list_past.usage.allocation_bytes
        );

        let bytes_success = bytes_get(0);
        let bytes_negative = bytes_get(-1);
        let bytes_past = bytes_get(1);
        for (result, expected) in [
            (&bytes_success, option_some(Value::Int(7))),
            (&bytes_negative, option_none()),
            (&bytes_past, option_none()),
        ] {
            let Value::Tuple(values) = &result.value else {
                panic!("bytes tuple result")
            };
            assert_eq!(values[0], Value::Bytes(vec![7].into()));
            assert_eq!(values[1], expected);
        }
        assert!(bytes_negative.usage.allocation_bytes < bytes_success.usage.allocation_bytes);
        assert_eq!(
            bytes_negative.usage.allocation_bytes,
            bytes_past.usage.allocation_bytes
        );

        let string_present = map_get(
            &ValueType::String,
            Constant::String("present".to_owned()),
            Constant::String("present".to_owned()),
        );
        let string_absent = map_get(
            &ValueType::String,
            Constant::String("present".to_owned()),
            Constant::String("absent".to_owned()),
        );
        let int_present = map_get(&ValueType::Int, Constant::Int(7), Constant::Int(7));
        let int_absent = map_get(&ValueType::Int, Constant::Int(7), Constant::Int(8));
        for (result, key, expected) in [
            (
                &string_present,
                Value::String("present".into()),
                option_some(Value::Int(42)),
            ),
            (
                &string_absent,
                Value::String("present".into()),
                option_none(),
            ),
            (&int_present, Value::Int(7), option_some(Value::Int(42))),
            (&int_absent, Value::Int(7), option_none()),
        ] {
            let Value::Tuple(values) = &result.value else {
                panic!("map tuple result")
            };
            assert_eq!(values[0], Value::Map(vec![(key, Value::Int(42))].into()));
            assert_eq!(values[1], expected);
        }
        assert!(string_absent.usage.allocation_bytes < string_present.usage.allocation_bytes);
        assert!(int_absent.usage.allocation_bytes < int_present.usage.allocation_bytes);
    }

    #[test]
    fn list_try_set_preserves_alias_and_failed_path_avoids_aggregate_result() {
        fn module(index: i64) -> VerifiedModule {
            verify(Module {
                constants: vec![Constant::Int(5), Constant::Int(index), Constant::Int(9)],
                enum_types: vec![],
                effect_sets: vec![vec![]],
                functions: vec![Function {
                    name: "try_set".to_owned(),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Int,
                        ValueType::Int,
                        ValueType::Int,
                        ValueType::List(Box::new(ValueType::Int)),
                        ValueType::Option(Box::new(ValueType::List(Box::new(ValueType::Int)))),
                        ValueType::Tuple(vec![
                            ValueType::List(Box::new(ValueType::Int)),
                            ValueType::Option(Box::new(ValueType::List(Box::new(ValueType::Int)))),
                        ]),
                    ],
                    return_type: ValueType::Tuple(vec![
                        ValueType::List(Box::new(ValueType::Int)),
                        ValueType::Option(Box::new(ValueType::List(Box::new(ValueType::Int)))),
                    ]),
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Const {
                            destination: 1,
                            constant: 1,
                        },
                        Instruction::Const {
                            destination: 2,
                            constant: 2,
                        },
                        Instruction::ListNew {
                            destination: 3,
                            elements: vec![0],
                        },
                        Instruction::SafeCollectionCall {
                            destination: 4,
                            operation: SafeCollectionOperation::ListTrySet,
                            arguments: vec![3, 1, 2],
                        },
                        Instruction::TupleNew {
                            destination: 5,
                            elements: vec![3, 4],
                        },
                        Instruction::Return { source: 5 },
                    ],
                }],
                async_functions: vec![],
                entry: 0,
            })
            .unwrap()
        }
        let success = execute_with_limits(&module(0), ExecutionLimits::default()).unwrap();
        let negative = execute_with_limits(&module(-1), ExecutionLimits::default()).unwrap();
        let past_end = execute_with_limits(&module(1), ExecutionLimits::default()).unwrap();
        let Value::Tuple(values) = &success.value else {
            panic!("tuple result")
        };
        assert_eq!(values[0], Value::List(vec![Value::Int(5)].into()));
        assert_eq!(
            values[1],
            option_some(Value::List(vec![Value::Int(9)].into()))
        );
        for failed in [&negative, &past_end] {
            let Value::Tuple(values) = &failed.value else {
                panic!("tuple result")
            };
            assert_eq!(values[0], Value::List(vec![Value::Int(5)].into()));
            assert_eq!(values[1], option_none());
            assert!(failed.usage.allocation_bytes < success.usage.allocation_bytes);
        }
        assert_eq!(
            negative.usage.allocation_bytes,
            past_end.usage.allocation_bytes
        );
    }
}
