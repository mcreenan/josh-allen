#![forbid(unsafe_code)]

//! ALLEN launch preflight, exact entry validation, and local effect routing.

mod error;
mod typed_response;

#[cfg(test)]
use error::runtime_vm_error_code;
pub use error::{RuntimeError, RuntimeErrorCode};
use error::{post_start_runtime_vm_error_code, safe_terminal_message};
pub use typed_response::{
    AgentAskCall, PromptPayload, ResponseAuditOutcome, ResponseAuditRecord, ResponseHostError,
    ResponseProvider, ResponseProviderKind, ResponseProviderPoll, ResponseSchema, StructuredPrompt,
    TextPromptCall, TextPromptProvider, TextPromptProviderAdapter, ValidationIssue,
    canonical_text_prompt,
};

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasher, Hash, Hasher};
use std::net::IpAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use allen_bytecode::{
    ExternalFsAccess, FsOperation, StrictSchema, ToolContract, ValueType, VerifiedArtifact,
    canonical_value_type_bytes, compute_strict_schema_digest, prompt_output_type,
};
use allen_http_get::{HttpBroker, HttpError, HttpErrorCode, HttpLimits, HttpUsage};
use allen_sandbox_fs::{
    ExecutionAccounting, ExternalTargetKind, FileError, FileErrorCode, RetainedExternalTarget,
    Rights, SearchMatch, WorkspaceBroker, WorkspaceLimits,
};
use allen_schema::{
    Descriptor, FrozenCatalog, SchemaLimits, ToolDefinition, ToolName, VersionRange,
    generated_tool_effect,
};
use allen_vm::{
    CancellationSource, Checkpoint, CheckpointObserver, EffectExecutionBinding,
    EffectExecutionOutcome, EffectProvider, EnumIdentity, EnumPayload, EnumValue,
    ExecutionCapabilities, ExecutionLimits, ExecutionOutcome, PendingEffectId, RESOURCE_WALL_TIME,
    SubAgentValue, SystemMonotonicClock, Value, VmError, WorkspaceValue,
    execute_entry_with_capabilities_and_runtime_context,
};
use base64::Engine;
use sha2::{Digest, Sha256};

static EXECUTION_GENERATION: AtomicU64 = AtomicU64::new(1);
const RUNTIME_MAX_RESPONSE_ATTEMPTS: u32 = 3;

#[derive(Clone, Debug)]
pub struct HostPolicy {
    pub granted_capabilities: BTreeSet<String>,
    pub limits: ExecutionLimits,
    pub input_bytes: usize,
    pub output_bytes: usize,
    /// Maximum bytes in a host-filtered transcript snapshot after validation.
    pub transcript_bytes: usize,
    pub effects: u64,
    /// Maximum total typed-response attempts, including the initial request.
    pub response_attempts: u32,
    pub workspace_root: Option<PathBuf>,
    pub workspace_rights: Rights,
    pub workspace_limits: WorkspaceLimits,
    pub http_origins: BTreeSet<String>,
    pub denied_net_addresses: BTreeSet<IpAddr>,
    pub http_limits: HttpLimits,
    pub granted_tools: BTreeSet<String>,
    pub tool_catalog: Option<FrozenCatalog>,
    /// Require selected granted authority to be physically available at preparation.
    pub strict_preflight_authority: bool,
}
impl Default for HostPolicy {
    fn default() -> Self {
        Self {
            granted_capabilities: BTreeSet::new(),
            limits: ExecutionLimits::default(),
            input_bytes: 1024 * 1024,
            output_bytes: 1024 * 1024,
            transcript_bytes: 1024 * 1024,
            effects: 10_000,
            response_attempts: 3,
            workspace_root: None,
            workspace_rights: Rights::NONE,
            workspace_limits: WorkspaceLimits::default(),
            http_origins: BTreeSet::new(),
            denied_net_addresses: BTreeSet::new(),
            http_limits: HttpLimits::default(),
            granted_tools: BTreeSet::new(),
            tool_catalog: None,
            strict_preflight_authority: false,
        }
    }
}
#[derive(Clone, Debug)]
pub struct LaunchRequest {
    pub entry: String,
    pub input: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeOutcome {
    pub output: serde_json::Value,
    pub execution: ExecutionOutcome,
    pub effects: u64,
    pub effective_limits: ExecutionLimits,
    pub effective_input_bytes: usize,
    pub effective_output_bytes: usize,
    pub effective_effects: u64,
    pub effective_response_attempts: u32,
    pub effective_workspace_limits: WorkspaceLimits,
    pub effective_http_limits: HttpLimits,
    pub effective_http_origins: BTreeSet<String>,
    pub http_usage: HttpUsage,
    pub optional_grants: BTreeSet<String>,
    /// Frozen canonical manifest authority names visible to capability inspection.
    pub effective_manifest_grants: BTreeSet<String>,
    /// Safe typed-response audit metadata. Prompt and response values are never recorded.
    pub response_audit: Vec<ResponseAuditRecord>,
}

/// One fully preflighted, one-shot launch with opened execution authority.
#[derive(Debug)]
pub struct PreparedLaunch {
    artifact: VerifiedArtifact,
    entry_function: u32,
    has_input: bool,
    input: serde_json::Value,
    input_type: ValueType,
    output_type: ValueType,
    broker: Option<WorkspaceBroker>,
    accounting: ExecutionAccounting,
    http: Option<HttpBroker>,
    tool_catalog: Option<FrozenCatalog>,
    tool_contracts: Vec<ToolContract>,
    limits: ExecutionLimits,
    effective_input_bytes: usize,
    effective_output_bytes: usize,
    effective_transcript_bytes: usize,
    effective_effects: u64,
    effective_response_attempts: u32,
    effective_workspace_limits: WorkspaceLimits,
    effective_http_limits: HttpLimits,
    effective_http_origins: BTreeSet<String>,
    workspace_root_identity: Option<PathBuf>,
    denied_net_addresses: BTreeSet<IpAddr>,
    optional_grants: BTreeSet<String>,
    effective_manifest_grants: BTreeSet<String>,
    authority: EffectiveAuthority,
}

impl PreparedLaunch {
    /// Return the supervisor-derived identity used by record/replay providers.
    #[must_use]
    pub fn effect_execution_binding(&self) -> EffectExecutionBinding {
        build_effect_execution_binding(self)
    }

    #[must_use]
    pub const fn effective_limits(&self) -> ExecutionLimits {
        self.limits
    }

    #[must_use]
    pub const fn effective_workspace_limits(&self) -> WorkspaceLimits {
        self.effective_workspace_limits
    }

    #[must_use]
    pub const fn effective_http_limits(&self) -> HttpLimits {
        self.effective_http_limits
    }

    #[must_use]
    pub const fn effective_input_bytes(&self) -> usize {
        self.effective_input_bytes
    }

    #[must_use]
    pub const fn effective_output_bytes(&self) -> usize {
        self.effective_output_bytes
    }

    #[must_use]
    pub const fn effective_transcript_bytes(&self) -> usize {
        self.effective_transcript_bytes
    }

    #[must_use]
    pub const fn effective_effects(&self) -> u64 {
        self.effective_effects
    }

    #[must_use]
    pub const fn effective_response_attempts(&self) -> u32 {
        self.effective_response_attempts
    }

    #[must_use]
    pub const fn effective_http_origins(&self) -> &BTreeSet<String> {
        &self.effective_http_origins
    }

    #[must_use]
    pub const fn effective_manifest_grants(&self) -> &BTreeSet<String> {
        &self.effective_manifest_grants
    }
}

/// An opaque provider-visible external grant identity.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ExternalGrantId {
    generation: u64,
    nonce: u64,
}

impl std::fmt::Debug for ExternalGrantId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ExternalGrantId(<opaque>)")
    }
}

/// An opaque identity for one running execution.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ExternalExecutionId {
    generation: u64,
    nonce: u64,
}

impl std::fmt::Debug for ExternalExecutionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ExternalExecutionId(<opaque>)")
    }
}

/// The requested lifetime of an external grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantDuration {
    /// The grant closes on every terminal execution path.
    Execution(ExternalExecutionId),
}

/// One request after the runtime retained its exact target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalGrantRequest {
    pub execution_id: ExternalExecutionId,
    /// Runtime-issued identity for the one permission decision operation.
    pub operation_id: u64,
    pub pending_target_id: u64,
    pub kind: ExternalTargetKind,
    pub path: PathBuf,
    pub rights: Rights,
    pub recursive: bool,
    pub max_bytes: u64,
    pub duration: GrantDuration,
    pub reason: String,
}

/// The invoking agent's explicit external filesystem decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalGrantDecision {
    Deny,
    Allow {
        execution_id: ExternalExecutionId,
        kind: ExternalTargetKind,
        path: PathBuf,
        rights: Rights,
        recursive: bool,
        max_bytes: u64,
        duration: GrantDuration,
    },
}

/// A nonblocking external-grant decision provider response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalGrantPoll {
    Decision(ExternalGrantDecision),
    Pending,
}

/// Provider that can approve external filesystem access.
pub trait ExternalGrantDecisionProvider {
    /// Decide one retained request. The provider cannot create a capability.
    ///
    /// # Errors
    ///
    /// Returns `AgentUnavailable` when the bound invoking session is absent or lost.
    fn decide(&mut self, request: &ExternalGrantRequest) -> Result<ExternalGrantDecision, VmError>;

    /// Observe the opaque identity assigned after a successful decision.
    fn grant_issued(&mut self, _pending_target_id: u64, _grant_id: ExternalGrantId) {}

    /// Return queued execution-scoped revocations.
    ///
    /// # Errors
    ///
    /// Returns `AgentUnavailable` when the invoking session was lost.
    fn take_revocations(&mut self) -> Result<Vec<ExternalGrantId>, VmError> {
        Ok(Vec::new())
    }

    /// Start a retained-target decision without blocking the VM scheduler.
    ///
    /// # Errors
    ///
    /// Returns a stable permission-provider error.
    fn start_decide(
        &mut self,
        _pending: PendingEffectId,
        request: &ExternalGrantRequest,
    ) -> Result<ExternalGrantPoll, VmError> {
        self.decide(request).map(ExternalGrantPoll::Decision)
    }

    /// Poll an already-issued retained-target decision.
    ///
    /// # Errors
    ///
    /// Returns a stable permission-provider error.
    fn poll(&mut self, _pending: PendingEffectId) -> Result<ExternalGrantPoll, VmError> {
        Err(VmError::AgentUnavailable)
    }

    /// Cancel an already-issued retained-target decision.
    fn cancel_pending(&mut self, _pending: PendingEffectId) {}
}

/// Constructs an HTTP broker from the runtime's already-intersected policy.
pub trait HttpBrokerFactory {
    /// # Errors
    ///
    /// Returns a safe broker construction error.
    fn create(
        &mut self,
        origins: &[String],
        denied_addresses: &BTreeSet<IpAddr>,
        limits: HttpLimits,
    ) -> Result<HttpBroker, HttpError>;
}

/// One immutable tool catalog snapshot used for compilation and preflight.
pub type ToolCatalogSnapshot = FrozenCatalog;

/// Metadata for one typed tool dispatch. It contains no tool value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolInvocation {
    pub execution_id: ExternalExecutionId,
    pub operation_id: u64,
    pub name: String,
    pub version: String,
    pub catalog_digest: String,
    pub deadline: Duration,
}

/// A schema-validated tool result before conversion back to a VM value.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolOutcome {
    Output(serde_json::Value),
    DeclaredError(serde_json::Value),
}

/// A nonblocking typed-tool provider response.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolProviderPoll {
    Outcome(ToolOutcome),
    Pending,
}

/// Stable host-side failures that remain distinct from declared tool errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolHostError {
    Unavailable,
    Cancelled,
    Timeout,
    Transport,
    Rejected,
    /// The provider received a response that cannot represent either outcome.
    InvalidOutcome,
}

/// A cooperative signal visible while a provider waits for an outcome.
pub trait ToolCancellationSignal {
    fn is_cancelled(&mut self) -> bool;
}

/// Host-neutral typed tool provider.
pub trait ToolProvider {
    /// Invoke one preflighted tool contract.
    ///
    /// # Errors
    ///
    /// Returns a host-level failure. A declared tool error uses [`ToolOutcome`].
    fn invoke(
        &mut self,
        invocation: &ToolInvocation,
        input: serde_json::Value,
        cancellation: &mut dyn ToolCancellationSignal,
    ) -> Result<ToolOutcome, ToolHostError>;

    /// Best-effort cancellation for an already-issued operation.
    fn cancel(&mut self, _execution_id: ExternalExecutionId, _operation_id: u64) {}

    /// Start a typed tool operation without blocking the VM scheduler.
    ///
    /// # Errors
    ///
    /// Returns a stable tool-provider error.
    fn start_invoke(
        &mut self,
        _pending: PendingEffectId,
        invocation: &ToolInvocation,
        input: serde_json::Value,
        cancellation: &mut dyn ToolCancellationSignal,
    ) -> Result<ToolProviderPoll, ToolHostError> {
        self.invoke(invocation, input, cancellation)
            .map(ToolProviderPoll::Outcome)
    }

    /// Poll an already-issued typed tool operation.
    ///
    /// # Errors
    ///
    /// Returns a stable tool-provider error.
    fn poll(
        &mut self,
        _pending: PendingEffectId,
        _cancellation: &mut dyn ToolCancellationSignal,
    ) -> Result<ToolProviderPoll, ToolHostError> {
        Err(ToolHostError::InvalidOutcome)
    }

    /// Cancel an already-issued nonblocking typed tool operation.
    fn cancel_pending(
        &mut self,
        _pending: PendingEffectId,
        execution_id: ExternalExecutionId,
        operation_id: u64,
    ) {
        self.cancel(execution_id, operation_id);
    }
}

/// One host-neutral request to the agent attached by the embedding host.
///
/// The session binding belongs to the host adapter. In particular, this type
/// intentionally has no session identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentMessageCall {
    pub execution_id: ExternalExecutionId,
    pub operation_id: u64,
    pub message: String,
    pub deadline: Duration,
}

/// One validated provider-owned child identity. It is never exposed directly
/// to ALLEN source; the runtime maps it to an opaque execution-local handle.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SubAgentId(String);

impl SubAgentId {
    /// Validate a provider identity before it enters the execution handle table.
    ///
    /// # Errors
    ///
    /// Returns `InvalidOutcome` for an empty, oversized, whitespace, control,
    /// or non-ASCII identity.
    pub fn parse(value: impl Into<String>) -> Result<Self, SubAgentHostError> {
        let value = value.into();
        if !(1..=128).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(SubAgentHostError::InvalidOutcome);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Explicit child authority. Prompt context remains a separate prompt segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubAgentProjection {
    pub capabilities: BTreeSet<String>,
    pub limits: BTreeMap<String, u64>,
    pub tools: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubAgentCreateCall {
    pub execution_id: ExternalExecutionId,
    pub operation_id: u64,
    pub prompt: StructuredPrompt,
    pub projection: SubAgentProjection,
    pub deadline: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubAgentRunCall {
    pub response: AgentAskCall,
    pub projection: SubAgentProjection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubAgentMessageCall {
    pub execution_id: ExternalExecutionId,
    pub operation_id: u64,
    pub target: SubAgentId,
    pub message: String,
    pub deadline: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubAgentAskCall {
    pub target: SubAgentId,
    pub response: AgentAskCall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubAgentHostError {
    Unavailable,
    Cancelled,
    Timeout,
    Transport,
    Rejected,
    InvalidOutcome,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SubAgentProviderPoll {
    Created(SubAgentId),
    Message(bool),
    Response(serde_json::Value),
    Pending,
}

/// Host-neutral child-agent provider. It is deliberately independent of the
/// invoking-agent provider and receives only explicit call fields.
pub trait SubAgentProvider {
    fn identity(&self) -> &str;

    /// # Errors
    /// Returns a stable provider failure.
    fn create(
        &mut self,
        call: &SubAgentCreateCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentId, SubAgentHostError>;

    /// # Errors
    /// Returns a stable provider failure.
    fn run(
        &mut self,
        call: &SubAgentRunCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<serde_json::Value, SubAgentHostError>;

    /// # Errors
    /// Returns a stable provider failure.
    fn message(
        &mut self,
        call: &SubAgentMessageCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<bool, SubAgentHostError>;

    /// # Errors
    /// Returns a stable provider failure.
    fn ask(
        &mut self,
        call: &SubAgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<serde_json::Value, SubAgentHostError>;

    /// Start one operation without blocking the VM scheduler.
    ///
    /// # Errors
    /// Returns a stable provider failure.
    fn start_create(
        &mut self,
        _pending: PendingEffectId,
        call: &SubAgentCreateCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        self.create(call, cancellation)
            .map(SubAgentProviderPoll::Created)
    }

    /// # Errors
    /// Returns a stable provider failure.
    fn start_run(
        &mut self,
        _pending: PendingEffectId,
        call: &SubAgentRunCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        self.run(call, cancellation)
            .map(SubAgentProviderPoll::Response)
    }

    /// # Errors
    /// Returns a stable provider failure.
    fn start_message(
        &mut self,
        _pending: PendingEffectId,
        call: &SubAgentMessageCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        self.message(call, cancellation)
            .map(SubAgentProviderPoll::Message)
    }

    /// # Errors
    /// Returns a stable provider failure.
    fn start_ask(
        &mut self,
        _pending: PendingEffectId,
        call: &SubAgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        self.ask(call, cancellation)
            .map(SubAgentProviderPoll::Response)
    }

    /// # Errors
    /// Returns a stable provider failure.
    fn poll(
        &mut self,
        _pending: PendingEffectId,
        _cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        Err(SubAgentHostError::InvalidOutcome)
    }

    fn cancel(
        &mut self,
        _pending: PendingEffectId,
        _execution_id: ExternalExecutionId,
        _operation_id: u64,
    ) {
    }
}

/// One bounded request for the host-filtered transcript projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptQuery {
    pub execution_id: ExternalExecutionId,
    pub operation_id: u64,
    pub limit: u8,
    pub deadline: Duration,
    pub maximum_bytes: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptSnapshot {
    pub snapshot_id: String,
    pub session_id: String,
    pub policy_version: String,
    pub captured_at: String,
    pub truncated: bool,
    pub messages: Vec<TranscriptMessage>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptMessage {
    pub id: Option<String>,
    pub role: TranscriptRole,
    pub time: Option<String>,
    pub content: Vec<TranscriptPart>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptRole {
    User,
    Assistant,
    SystemVisible,
    Tool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptPart {
    Text {
        text: String,
    },
    Json {
        value: serde_json::Value,
    },
    ToolCall {
        name: String,
        call_id: String,
        input: Option<serde_json::Value>,
    },
    ToolResult {
        call_id: String,
        output: Option<serde_json::Value>,
        is_error: bool,
    },
    Attachment {
        media_type: String,
        name: Option<String>,
        content_ref: Option<String>,
    },
    Redacted {
        reason_code: String,
    },
    Omitted {
        content_kind: String,
        count: u32,
    },
}

/// Stable host-side failures for invoking-agent operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentHostError {
    Unavailable,
    Cancelled,
    Timeout,
    Transport,
    Rejected,
    /// The provider could not represent its result in the host-neutral type.
    InvalidOutcome,
}

/// A nonblocking invoking-agent provider response.
#[derive(Clone, Debug, PartialEq)]
pub enum AgentProviderPoll {
    Message(bool),
    Ask(serde_json::Value),
    Transcript(TranscriptSnapshot),
    Pending,
}

/// A cooperative signal visible while an invoking-agent provider waits.
pub trait AgentCancellationSignal {
    fn is_cancelled(&mut self) -> bool;
}

/// Host-neutral provider for the one invoking agent selected by the host.
///
/// This trait deliberately cannot select a session, user, model, sub-agent,
/// or general callback. A host adapter supplies its own immutable binding.
pub trait InvokingAgentProvider {
    /// Stable, content-free identity used in response audit records.
    #[allow(clippy::unnecessary_literal_bound)]
    fn identity(&self) -> &str {
        "invoking-agent"
    }

    /// Wait for delivery acknowledgement. `accepted` must be true.
    ///
    /// # Errors
    ///
    /// Returns a stable invoking-agent provider error.
    fn message(
        &mut self,
        call: &AgentMessageCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<bool, AgentHostError>;

    /// Return the raw JSON reply for exact runtime validation.
    ///
    /// # Errors
    ///
    /// Returns a stable invoking-agent provider error.
    fn ask(
        &mut self,
        call: &AgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<serde_json::Value, AgentHostError>;

    /// Return the already filtered transcript projection.
    ///
    /// # Errors
    ///
    /// Returns a stable invoking-agent provider error.
    fn transcript(
        &mut self,
        query: &TranscriptQuery,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<TranscriptSnapshot, AgentHostError>;

    /// Best-effort cancellation for an already-issued operation.
    fn cancel(&mut self, _execution_id: ExternalExecutionId, _operation_id: u64) {}

    /// Start a message request without requiring the VM scheduler to block.
    ///
    /// # Errors
    ///
    /// Returns a stable invoking-agent provider error.
    fn start_message(
        &mut self,
        _pending: PendingEffectId,
        call: &AgentMessageCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<AgentProviderPoll, AgentHostError> {
        self.message(call, cancellation)
            .map(AgentProviderPoll::Message)
    }

    /// Start an ask request without requiring the VM scheduler to block.
    ///
    /// # Errors
    ///
    /// Returns a stable invoking-agent provider error.
    fn start_ask(
        &mut self,
        _pending: PendingEffectId,
        call: &AgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<AgentProviderPoll, AgentHostError> {
        self.ask(call, cancellation).map(AgentProviderPoll::Ask)
    }

    /// Start a transcript request without requiring the VM scheduler to block.
    ///
    /// # Errors
    ///
    /// Returns a stable invoking-agent provider error.
    fn start_transcript(
        &mut self,
        _pending: PendingEffectId,
        query: &TranscriptQuery,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<AgentProviderPoll, AgentHostError> {
        self.transcript(query, cancellation)
            .map(AgentProviderPoll::Transcript)
    }

    /// Poll an already-issued request.
    ///
    /// # Errors
    ///
    /// Returns a stable invoking-agent provider error.
    fn poll(
        &mut self,
        _pending: PendingEffectId,
        _cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<AgentProviderPoll, AgentHostError> {
        Err(AgentHostError::InvalidOutcome)
    }

    /// Cancel an already-issued nonblocking request.
    fn cancel_pending(
        &mut self,
        _pending: PendingEffectId,
        execution_id: ExternalExecutionId,
        operation_id: u64,
    ) {
        self.cancel(execution_id, operation_id);
    }
}

enum AgentDispatch {
    Message(AgentMessageCall),
    Ask(AgentAskCall),
    Transcript(TranscriptQuery),
}

/// Optional execution providers supplied by the embedding host.
#[derive(Default)]
pub struct RuntimeProviders<'provider> {
    /// Replaces the production capability broker for a complete replayed
    /// execution. When present, no live provider from this struct is routed
    /// into the VM.
    pub effect_override: Option<&'provider mut dyn EffectProvider>,
    pub external_grants: Option<&'provider mut dyn ExternalGrantDecisionProvider>,
    pub http_factory: Option<&'provider mut dyn HttpBrokerFactory>,
    pub tools: Option<&'provider mut dyn ToolProvider>,
    pub invoking_agent: Option<&'provider mut dyn InvokingAgentProvider>,
    pub model: Option<&'provider mut dyn ResponseProvider>,
    pub user: Option<&'provider mut dyn ResponseProvider>,
    pub sub_agent: Option<&'provider mut dyn SubAgentProvider>,
}

struct ProductionHttpFactory;
impl HttpBrokerFactory for ProductionHttpFactory {
    fn create(
        &mut self,
        origins: &[String],
        denied_addresses: &BTreeSet<IpAddr>,
        limits: HttpLimits,
    ) -> Result<HttpBroker, HttpError> {
        HttpBroker::production_with_denied_addresses(
            origins.iter().cloned(),
            denied_addresses.clone(),
            limits,
        )
    }
}

/// Validate and execute one manifest-selected artifact entry.
///
/// # Errors
///
/// Returns a stable runtime error when preflight, boundary validation, the
/// capability broker, or verified execution fails.
#[allow(clippy::too_many_lines)]
pub fn launch(
    artifact: &VerifiedArtifact,
    request: &LaunchRequest,
    policy: &HostPolicy,
) -> Result<RuntimeOutcome, RuntimeError> {
    launch_with_providers(artifact, request, policy, &mut RuntimeProviders::default())
}

/// Validate and execute one entry with explicit host-neutral providers.
///
/// # Errors
///
/// Returns the same stable preflight, boundary, broker, and VM errors as [`launch`].
#[allow(clippy::too_many_lines)]
pub fn launch_with_providers(
    artifact: &VerifiedArtifact,
    request: &LaunchRequest,
    policy: &HostPolicy,
    providers: &mut RuntimeProviders<'_>,
) -> Result<RuntimeOutcome, RuntimeError> {
    let mut observer = NoObserver;
    let mut cancellation = NeverCancel;
    launch_with_context(
        artifact,
        request,
        policy,
        providers,
        &mut cancellation,
        &mut observer,
    )
}

/// Complete launch preflight and open its execution-scoped authority.
///
/// The returned value is one-shot and keeps the verified artifact, validated
/// JSON input, opened workspace, prepared HTTP broker, catalog, and effective
/// budgets fixed until execution. The pure JSON-to-VM conversion is repeated
/// at execution so the prepared value remains safe to move between host threads.
///
/// # Errors
///
/// Returns a stable error before execution is accepted.
#[allow(clippy::too_many_lines)]
pub fn prepare_launch(
    artifact: &VerifiedArtifact,
    request: &LaunchRequest,
    policy: &HostPolicy,
) -> Result<PreparedLaunch, RuntimeError> {
    let mut factory = ProductionHttpFactory;
    prepare_launch_with_http_factory(artifact, request, policy, &mut factory)
}

#[allow(clippy::too_many_lines)]
fn prepare_launch_with_http_factory(
    artifact: &VerifiedArtifact,
    request: &LaunchRequest,
    policy: &HostPolicy,
    http_factory: &mut dyn HttpBrokerFactory,
) -> Result<PreparedLaunch, RuntimeError> {
    let entry = artifact
        .entries()
        .iter()
        .find(|entry| entry.name == request.entry)
        .ok_or_else(|| {
            RuntimeError::new(RuntimeErrorCode::EntryNotFound, "entry is not declared")
        })?;
    let manifest = artifact.manifest().ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorCode::ManifestInvalid,
            "artifact has no manifest contract",
        )
    })?;
    if policy.strict_preflight_authority
        && policy.granted_capabilities.iter().any(|granted| {
            !manifest.required_capabilities.contains(granted)
                && !manifest.optional_capabilities.contains(granted)
        })
    {
        return Err(RuntimeError::new(
            RuntimeErrorCode::CapabilityDenied,
            "granted capability is not declared by the artifact",
        ));
    }
    if policy.strict_preflight_authority
        && policy.granted_tools.iter().any(|granted| {
            !manifest
                .required_tools
                .iter()
                .any(|tool| &tool.name == granted)
        })
    {
        return Err(RuntimeError::new(
            RuntimeErrorCode::CapabilityDenied,
            "granted tool is not declared by the artifact",
        ));
    }
    let entry_effects = &artifact.verified_module().module().effect_sets
        [function_effects(artifact, entry)? as usize];
    if !manifest.required_tools.is_empty() {
        let catalog = policy.tool_catalog.as_ref().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::CatalogMismatch,
                "artifact requires a frozen tool catalog",
            )
        })?;
        for contract in &manifest.required_tools {
            let name = ToolName::parse(contract.name.clone()).map_err(|_| {
                RuntimeError::new(RuntimeErrorCode::CatalogMismatch, "tool name is invalid")
            })?;
            let definition = catalog.get(&name).ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::CatalogMismatch,
                    "required tool is not in the frozen catalog",
                )
            })?;
            if !tool_contract_matches(contract, definition) {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::CatalogMismatch,
                    "artifact tool contract does not match the frozen catalog",
                ));
            }
            if entry_effects.contains(&contract.effect)
                && !policy.granted_tools.contains(&contract.name)
            {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::CapabilityDenied,
                    "required tool authority is denied",
                ));
            }
        }
    }
    for cap in &manifest.required_capabilities {
        if entry_effects.contains(cap)
            && !is_host_response_effect(cap)
            && !policy.granted_capabilities.contains(cap)
        {
            return Err(RuntimeError::new(
                RuntimeErrorCode::CapabilityDenied,
                "required capability is denied",
            ));
        }
    }
    for effect in entry_effects {
        if effect != "task.spawn"
            && effect != "debug.inspect"
            && effect != "capability.inspect"
            && !manifest
                .required_capabilities
                .iter()
                .chain(&manifest.optional_capabilities)
                .any(|cap| cap == effect)
            && !manifest
                .required_tools
                .iter()
                .any(|tool| &tool.effect == effect)
        {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ManifestInvalid,
                "entry authority effect is not declared",
            ));
        }
    }
    let effective_input_bytes = manifest_limit(&manifest.limits, "input_bytes", policy.input_bytes);
    let effective_output_bytes =
        manifest_limit(&manifest.limits, "output_bytes", policy.output_bytes);
    let effective_effects = manifest
        .limits
        .iter()
        .find(|(name, _)| name == "effects")
        .map_or(policy.effects, |(_, value)| policy.effects.min(*value));
    let manifest_response_attempts = manifest
        .limits
        .iter()
        .find(|(name, _)| name == "response_attempts")
        .and_then(|(_, value)| u32::try_from(*value).ok())
        .unwrap_or(RUNTIME_MAX_RESPONSE_ATTEMPTS);
    let effective_response_attempts = policy
        .response_attempts
        .max(1)
        .min(manifest_response_attempts.max(1))
        .min(RUNTIME_MAX_RESPONSE_ATTEMPTS);
    let effective_workspace_limits =
        effective_workspace_limits(policy.workspace_limits, &manifest.limits);
    let effective_http_limits = effective_http_limits(policy.http_limits, &manifest.limits);
    let limits = effective_limits(policy.limits, &manifest.limits);
    let effective_http_origins = manifest
        .https_origins
        .iter()
        .filter(|origin| policy.http_origins.contains(*origin))
        .cloned()
        .collect::<BTreeSet<_>>();
    let input_bytes = serde_json::to_vec(&request.input)
        .map_err(|_| RuntimeError::new(RuntimeErrorCode::InvalidInput, "input is not JSON"))?;
    if input_bytes.len() > effective_input_bytes {
        return Err(RuntimeError::new(
            RuntimeErrorCode::InputTooLarge,
            "input exceeds host limit",
        ));
    }
    let input_schema = artifact
        .schemas()
        .get(entry.input_schema as usize)
        .ok_or_else(|| {
            RuntimeError::new(RuntimeErrorCode::ManifestInvalid, "input schema is invalid")
        })?;
    let output_schema = artifact
        .schemas()
        .get(entry.output_schema as usize)
        .ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ManifestInvalid,
                "output schema is invalid",
            )
        })?;
    let _validated_input = json_to_value(
        &request.input,
        &input_schema.value_type,
        &artifact.verified_module().module().enum_types,
    )?;
    let function = artifact
        .verified_module()
        .module()
        .functions
        .get(entry.function as usize)
        .ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ManifestInvalid,
                "entry function is invalid",
            )
        })?;
    if function.parameters.len() > 1 {
        return Err(RuntimeError::new(
            RuntimeErrorCode::ManifestInvalid,
            "entry cannot accept more than one input",
        ));
    }
    let accounting = ExecutionAccounting::new(effective_workspace_limits);
    let requested_capability = |capability: &str| {
        manifest
            .required_capabilities
            .binary_search_by(|requested| requested.as_str().cmp(capability))
            .is_ok()
            || manifest
                .optional_capabilities
                .binary_search_by(|requested| requested.as_str().cmp(capability))
                .is_ok()
    };
    let workspace_rights = Rights::new(
        policy.workspace_rights.read
            && policy.granted_capabilities.contains("fs.read")
            && (entry_effects.iter().any(|effect| effect == "fs.read")
                || requested_capability("fs.read")),
        policy.workspace_rights.write
            && policy.granted_capabilities.contains("fs.write")
            && (entry_effects.iter().any(|effect| effect == "fs.write")
                || requested_capability("fs.write")),
    );
    if policy.strict_preflight_authority
        && !(workspace_rights.read || workspace_rights.write)
        && policy.workspace_root.is_some()
    {
        return Err(RuntimeError::new(
            RuntimeErrorCode::ManifestInvalid,
            "a work directory is not valid without selected filesystem authority",
        ));
    }
    let broker = if workspace_rights.read || workspace_rights.write {
        match policy.workspace_root.as_ref() {
            Some(root) => match WorkspaceBroker::open_ambient_with_accounting(
                root,
                workspace_rights,
                accounting.clone(),
            ) {
                Ok(broker) => Some(broker),
                Err(_) if policy.strict_preflight_authority => {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::ManifestInvalid,
                        "the work directory could not be opened",
                    ));
                }
                Err(_) => None,
            },
            None if policy.strict_preflight_authority => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::ManifestInvalid,
                    "selected filesystem authority requires a work directory",
                ));
            }
            None => None,
        }
    } else {
        None
    };
    for capability in ["fs.read", "fs.write"] {
        if entry_effects.iter().any(|effect| effect == capability)
            && manifest
                .required_capabilities
                .iter()
                .any(|required| required == capability)
            && !workspace_capability_available(broker.as_ref(), capability)
        {
            return Err(RuntimeError::new(
                RuntimeErrorCode::CapabilityDenied,
                "required workspace capability is unavailable",
            ));
        }
    }
    if entry_effects.iter().any(|effect| effect == "net.http_get")
        && manifest
            .required_capabilities
            .iter()
            .any(|required| required == "net.http_get")
        && effective_http_origins.is_empty()
    {
        return Err(RuntimeError::new(
            RuntimeErrorCode::CapabilityDenied,
            "required HTTP capability has no effective origin",
        ));
    }
    let optional_grants = manifest
        .optional_capabilities
        .iter()
        .filter(|capability| {
            entry_effects.contains(*capability)
                && policy.granted_capabilities.contains(*capability)
                && capability_available(broker.as_ref(), capability, &effective_http_origins)
        })
        .cloned()
        .collect();
    let http = if (entry_effects.iter().any(|effect| effect == "net.http_get")
        || requested_capability("net.http_get"))
        && policy.granted_capabilities.contains("net.http_get")
        && !effective_http_origins.is_empty()
    {
        let origins = effective_http_origins.iter().cloned().collect::<Vec<_>>();
        Some(
            catch_unwind(AssertUnwindSafe(|| {
                http_factory.create(
                    &origins,
                    &policy.denied_net_addresses,
                    effective_http_limits,
                )
            }))
            .map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::Panic,
                    safe_terminal_message(RuntimeErrorCode::Panic),
                )
            })?
            .map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::CapabilityDenied,
                    "the HTTP broker could not apply the effective policy",
                )
            })?,
        )
    } else {
        None
    };
    let effective_manifest_grants = manifest
        .required_capabilities
        .iter()
        .chain(&manifest.optional_capabilities)
        .filter(|capability| match capability.as_str() {
            "fs.read" | "fs.write" => {
                policy.granted_capabilities.contains(*capability)
                    && workspace_capability_available(broker.as_ref(), capability)
            }
            "net.http_get" => policy.granted_capabilities.contains(*capability) && http.is_some(),
            "permission.request_external_fs" => policy.granted_capabilities.contains(*capability),
            _ => false,
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    if manifest.required_capabilities.iter().any(|capability| {
        is_manifest_grantable_capability(capability)
            && !effective_manifest_grants.contains(capability)
    }) {
        return Err(RuntimeError::new(
            RuntimeErrorCode::CapabilityDenied,
            "required manifest capability is not effectively granted",
        ));
    }
    let authority = EffectiveAuthority {
        filesystem: Rights::new(
            entry_effects.iter().any(|effect| effect == "fs.read")
                && policy.granted_capabilities.contains("fs.read"),
            entry_effects.iter().any(|effect| effect == "fs.write")
                && policy.granted_capabilities.contains("fs.write"),
        ),
        permission_request: entry_effects
            .iter()
            .any(|effect| effect == "permission.request_external_fs")
            && policy
                .granted_capabilities
                .contains("permission.request_external_fs"),
        http_get: entry_effects.iter().any(|effect| effect == "net.http_get")
            && policy.granted_capabilities.contains("net.http_get")
            && !effective_http_origins.is_empty(),
    };
    Ok(PreparedLaunch {
        artifact: artifact.clone(),
        entry_function: entry.function,
        has_input: !function.parameters.is_empty(),
        input: request.input.clone(),
        input_type: input_schema.value_type.clone(),
        output_type: output_schema.value_type.clone(),
        broker,
        accounting,
        http,
        tool_catalog: policy.tool_catalog.clone(),
        tool_contracts: manifest.required_tools.clone(),
        limits,
        effective_input_bytes,
        effective_output_bytes,
        effective_transcript_bytes: policy.transcript_bytes,
        effective_effects,
        effective_response_attempts,
        effective_workspace_limits,
        effective_http_limits,
        effective_http_origins,
        workspace_root_identity: policy
            .workspace_root
            .as_ref()
            .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.clone())),
        denied_net_addresses: policy.denied_net_addresses.clone(),
        optional_grants,
        effective_manifest_grants,
        authority,
    })
}

fn is_host_response_effect(effect: &str) -> bool {
    matches!(
        effect,
        "agent.message"
            | "agent.ask"
            | "agent.transcript"
            | "model.request"
            | "user.ask"
            | "sub_agent.create"
            | "sub_agent.run"
            | "sub_agent.message"
            | "sub_agent.ask"
    )
}

fn is_manifest_grantable_capability(capability: &str) -> bool {
    matches!(
        capability,
        "fs.read" | "fs.write" | "net.http_get" | "permission.request_external_fs"
    )
}

fn build_effect_execution_binding(prepared: &PreparedLaunch) -> EffectExecutionBinding {
    let bytecode = prepared.artifact.metadata().bytecode_version;
    let input_type = canonical_value_type_bytes(&prepared.input_type);
    let output_type = canonical_value_type_bytes(&prepared.output_type);
    let entry = prepared.entry_function.to_le_bytes();
    let contracts = format!("{:?}", prepared.tool_contracts);
    let contract_digest = replay_digest(&[&entry, &input_type, &output_type, contracts.as_bytes()]);
    let language = format!("allen-language/{bytecode}");
    let runtime = format!("allen-runtime/{}", env!("CARGO_PKG_VERSION"));
    let policy = format!(
        "{:?}|{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        prepared.limits,
        prepared.effective_input_bytes,
        prepared.effective_output_bytes,
        prepared.effective_transcript_bytes,
        prepared.effective_workspace_limits,
        prepared.effective_http_limits,
        prepared.effective_http_origins,
        prepared.optional_grants,
        prepared.authority,
        prepared.effective_response_attempts,
    );
    let policy = format!(
        "{policy}|{}|{:?}|{:?}",
        prepared.effective_effects, prepared.workspace_root_identity, prepared.denied_net_addresses,
    );
    let catalog = prepared
        .tool_catalog
        .as_ref()
        .map_or("<none>", FrozenCatalog::digest);
    let grants = prepared
        .effective_manifest_grants
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let grant_text = grants.join("\0");
    let authority = format!("{:?}", prepared.authority);
    EffectExecutionBinding {
        bytecode_version: bytecode,
        artifact_digest: *prepared.artifact.content_digest(),
        contract_digest,
        language_digest: replay_digest(&[language.as_bytes()]),
        runtime_digest: replay_digest(&[runtime.as_bytes()]),
        policy_digest: replay_digest(&[policy.as_bytes()]),
        catalog_digest: replay_digest(&[catalog.as_bytes()]),
        capability_digest: replay_digest(&[grant_text.as_bytes(), authority.as_bytes()]),
        error_registry_digest: replay_digest(&[include_bytes!(
            "../../../docs/conformance/errors-0.1.json"
        )]),
        effective_manifest_grants: grants,
    }
}

fn replay_digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
        digest.update(part);
    }
    digest.finalize().into()
}

/// Execute one already-preflighted launch without reopening its authority.
///
/// # Errors
///
/// Returns stable provider, VM, or output-boundary failures.
#[allow(clippy::too_many_lines)]
pub fn execute_prepared_with_context(
    prepared: PreparedLaunch,
    providers: &mut RuntimeProviders<'_>,
    cancellation: &mut dyn CancellationSource,
    observer: &mut dyn CheckpointObserver,
) -> Result<RuntimeOutcome, RuntimeError> {
    let execution_binding = build_effect_execution_binding(&prepared);
    let PreparedLaunch {
        artifact,
        entry_function,
        has_input,
        input,
        input_type,
        output_type,
        broker,
        accounting,
        http,
        tool_catalog,
        tool_contracts,
        limits,
        effective_input_bytes,
        effective_output_bytes,
        effective_transcript_bytes,
        effective_effects,
        effective_response_attempts,
        effective_workspace_limits,
        effective_http_limits,
        effective_http_origins,
        workspace_root_identity: _,
        denied_net_addresses: _,
        optional_grants,
        effective_manifest_grants,
        authority,
    } = prepared;
    let input = json_to_value(
        &input,
        &input_type,
        &artifact.verified_module().module().enum_types,
    )
    .map_err(|_| {
        RuntimeError::new(
            RuntimeErrorCode::Panic,
            "prepared input no longer matches its verified type",
        )
    })?;
    let mut live_effects = None;
    let effects: &mut dyn EffectProvider =
        if let Some(override_effects) = providers.effect_override.take() {
            // A replay override is all-or-nothing: leave every live provider
            // untouched and route no capability through the production broker.
            let replayed = catch_unwind(AssertUnwindSafe(|| override_effects.is_replayed()))
                .map_err(|_| {
                    RuntimeError::new(
                        RuntimeErrorCode::Panic,
                        safe_terminal_message(RuntimeErrorCode::Panic),
                    )
                })?;
            if !replayed {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::ReplayDiverged,
                    "effect override must declare replay provenance",
                ));
            }
            override_effects
        } else {
            let generation = EXECUTION_GENERATION.fetch_add(1, Ordering::Relaxed);
            let projection_limits = effective_projection_limits(
                limits,
                effective_workspace_limits,
                effective_http_limits,
                effective_input_bytes,
                effective_output_bytes,
                effective_effects,
            );
            live_effects = Some(BrokerEffects::new(
                generation,
                broker,
                accounting,
                http,
                providers.external_grants.take(),
                providers.tools.take(),
                providers.invoking_agent.take(),
                providers.model.take(),
                providers.user.take(),
                providers.sub_agent.take(),
                projection_limits,
                tool_catalog,
                tool_contracts,
                artifact.schemas().to_vec(),
                artifact.verified_module().module().enum_types.clone(),
                limits.wall_time,
                effective_transcript_bytes,
                effective_response_attempts,
                authority,
                effective_effects,
            ));
            let Some(live_effects) = live_effects.as_mut() else {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::Panic,
                    "production effect broker could not be initialized",
                ));
            };
            live_effects
        };
    let binding = catch_unwind(AssertUnwindSafe(|| {
        effects.bind_execution(&execution_binding)
    }))
    .map_err(|_| {
        RuntimeError::new(
            RuntimeErrorCode::Panic,
            safe_terminal_message(RuntimeErrorCode::Panic),
        )
    })?;
    binding.map_err(|_| {
        RuntimeError::new(
            RuntimeErrorCode::ReplayDiverged,
            safe_terminal_message(RuntimeErrorCode::ReplayDiverged),
        )
    })?;
    let mut clock = SystemMonotonicClock::default();
    let execution_capabilities =
        ExecutionCapabilities::new(effective_manifest_grants.iter().cloned());
    let execution_result = catch_unwind(AssertUnwindSafe(|| {
        execute_entry_with_capabilities_and_runtime_context(
            artifact.verified_module(),
            artifact
                .debug()
                .map(|debug| debug as &dyn allen_vm::DebugSourceMap),
            entry_function,
            if has_input {
                std::slice::from_ref(&input)
            } else {
                &[]
            },
            limits,
            &mut clock,
            observer,
            cancellation,
            effects,
            &execution_capabilities,
        )
    }));
    let Ok(execution_result) = execution_result else {
        // The VM normally performs this structured cancellation itself. A Rust
        // panic can interrupt that path, so the trusted boundary closes every
        // still-issued provider operation before returning a content-free
        // terminal failure. A broken provider is not allowed to unwind again
        // while cancellation is being attempted.
        let _ = catch_unwind(AssertUnwindSafe(|| effects.cancel_pending()));
        let _ = catch_unwind(AssertUnwindSafe(|| {
            effects.finish_execution(EffectExecutionOutcome::RuntimePanic)
        }));
        if let Some(effects) = live_effects.as_mut() {
            effects.expire();
        }
        return Err(RuntimeError::new(
            RuntimeErrorCode::Panic,
            safe_terminal_message(RuntimeErrorCode::Panic),
        ));
    };
    let prepared_output = match &execution_result {
        Ok(ExecutionOutcome::Completed(result)) => {
            let output = value_to_json(
                &result.value,
                &output_type,
                &artifact.verified_module().module().enum_types,
            )
            .map_err(|_| {
                RuntimeError::new(
                    RuntimeErrorCode::Panic,
                    "verified output no longer matches its declared type",
                )
            });
            output.and_then(|output| {
                let bytes = serde_json::to_vec(&output).map_err(|_| {
                    RuntimeError::new(RuntimeErrorCode::Panic, "output is not JSON")
                })?;
                if bytes.len() > effective_output_bytes {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::ResourceLimit,
                        "output exceeds host limit",
                    ));
                }
                Ok(output)
            })
        }
        Ok(ExecutionOutcome::Stopped { .. }) | Err(_) => Ok(serde_json::Value::Null),
    };
    let prepared_output = match prepared_output {
        Ok(output) => output,
        Err(boundary_error) => {
            let vm_error = match boundary_error.code {
                RuntimeErrorCode::ResourceLimit => VmError::ResourceLimit {
                    resource: "maximum_output_bytes",
                },
                _ => VmError::Invariant("verified output boundary failed"),
            };
            let validation = catch_unwind(AssertUnwindSafe(|| {
                effects.finish_execution(EffectExecutionOutcome::Terminal { error: &vm_error })
            }));
            if let Some(effects) = live_effects.as_mut() {
                effects.expire();
            }
            match validation {
                Ok(Ok(())) => return Err(boundary_error),
                Ok(Err(error)) => {
                    let code = post_start_runtime_vm_error_code(&error);
                    return Err(RuntimeError::new(code, safe_terminal_message(code)));
                }
                Err(_) => {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::Panic,
                        safe_terminal_message(RuntimeErrorCode::Panic),
                    ));
                }
            }
        }
    };
    let replay_outcome = match &execution_result {
        Ok(ExecutionOutcome::Completed(_)) => EffectExecutionOutcome::Completed,
        Ok(ExecutionOutcome::Stopped { reason, .. }) => EffectExecutionOutcome::Stopped { reason },
        Err(error) => EffectExecutionOutcome::Terminal {
            error: &error.error,
        },
    };
    let replay_validation = catch_unwind(AssertUnwindSafe(|| {
        effects.finish_execution(replay_outcome)
    }));
    let Ok(replay_validation) = replay_validation else {
        let _ = catch_unwind(AssertUnwindSafe(|| effects.cancel_pending()));
        if let Some(effects) = live_effects.as_mut() {
            effects.expire();
        }
        return Err(RuntimeError::new(
            RuntimeErrorCode::Panic,
            safe_terminal_message(RuntimeErrorCode::Panic),
        ));
    };
    if let Err(error) = replay_validation {
        let _ = catch_unwind(AssertUnwindSafe(|| effects.cancel_pending()));
        if let Some(effects) = live_effects.as_mut() {
            effects.expire();
        }
        let code = post_start_runtime_vm_error_code(&error);
        return Err(RuntimeError::new(code, safe_terminal_message(code)));
    }
    let http_usage = live_effects
        .as_ref()
        .map_or_else(HttpUsage::default, BrokerEffects::http_usage);
    let response_audit = live_effects
        .as_ref()
        .map_or_else(Vec::new, |effects| effects.response_audit.clone());
    let charged_effects = live_effects.as_ref().map_or(0, |effects| effects.effects);
    if let Some(effects) = live_effects.as_mut() {
        effects.expire();
    }
    let execution = execution_result.map_err(|error| {
        let code = post_start_runtime_vm_error_code(&error.error);
        let message = safe_terminal_message(code).to_owned();
        RuntimeError::new(code, message).with_response_audit(response_audit.clone())
    })?;
    let output = prepared_output;
    Ok(RuntimeOutcome {
        output,
        execution,
        effects: charged_effects,
        effective_limits: limits,
        effective_input_bytes,
        effective_output_bytes,
        effective_effects,
        effective_response_attempts,
        effective_workspace_limits,
        effective_http_limits,
        effective_http_origins,
        http_usage,
        optional_grants,
        effective_manifest_grants,
        response_audit,
    })
}

/// Validate and execute one entry with caller-owned cancellation and events.
///
/// # Errors
///
/// Returns stable preflight, boundary, provider, and VM failures.
pub fn launch_with_context(
    artifact: &VerifiedArtifact,
    request: &LaunchRequest,
    policy: &HostPolicy,
    providers: &mut RuntimeProviders<'_>,
    cancellation: &mut dyn CancellationSource,
    observer: &mut dyn CheckpointObserver,
) -> Result<RuntimeOutcome, RuntimeError> {
    let mut production = ProductionHttpFactory;
    let factory: &mut dyn HttpBrokerFactory = providers
        .http_factory
        .as_deref_mut()
        .unwrap_or(&mut production);
    let prepared = prepare_launch_with_http_factory(artifact, request, policy, factory)?;
    execute_prepared_with_context(prepared, providers, cancellation, observer)
}

fn workspace_capability_available(broker: Option<&WorkspaceBroker>, capability: &str) -> bool {
    broker.is_some_and(|broker| match capability {
        "fs.read" => broker.rights().read,
        "fs.write" => broker.rights().write,
        _ => false,
    })
}

fn tool_contract_matches(contract: &ToolContract, definition: &ToolDefinition) -> bool {
    let requirement = VersionRange::parse(&contract.version_requirement);
    contract.name == definition.name.as_str()
        && contract.version == definition.version.to_string()
        && requirement.is_ok_and(|range| range.contains(definition.version))
        && generated_tool_effect(&definition.name, definition.version)
            .is_ok_and(|effect| effect == contract.effect)
        && digest_text(&contract.input_digest) == definition.input_schema.digest()
        && digest_text(&contract.output_digest) == definition.output_schema.digest()
        && digest_text(&contract.error_digest) == definition.error_schema.digest()
}

fn digest_text(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::from("sha256:");
    output.reserve(64);
    for byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn capability_available(
    broker: Option<&WorkspaceBroker>,
    capability: &str,
    http_origins: &BTreeSet<String>,
) -> bool {
    match capability {
        "fs.read" | "fs.write" => workspace_capability_available(broker, capability),
        "net.http_get" => !http_origins.is_empty(),
        "permission.request_external_fs" => true,
        _ => false,
    }
}

fn function_effects(
    artifact: &VerifiedArtifact,
    entry: &allen_bytecode::EntryContract,
) -> Result<u32, RuntimeError> {
    artifact
        .verified_module()
        .module()
        .functions
        .get(entry.function as usize)
        .map(|function| function.effects)
        .ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ManifestInvalid,
                "entry function is invalid",
            )
        })
}
fn effective_limits(mut host: ExecutionLimits, limits: &[(String, u64)]) -> ExecutionLimits {
    for (name, value) in limits {
        match name.as_str() {
            "instructions" => host.instructions = host.instructions.min(*value),
            "wall_ms" => {
                host.wall_time = host.wall_time.min(std::time::Duration::from_millis(*value));
            }
            "heap_bytes" => host.allocation_bytes = host.allocation_bytes.min(*value),
            "maximum_allocation_bytes" => {
                host.maximum_allocation_bytes = host.maximum_allocation_bytes.min(*value);
            }
            "call_depth" => {
                host.call_depth = host
                    .call_depth
                    .min(u32::try_from(*value).unwrap_or(u32::MAX));
            }
            "tasks" => {
                host.tasks = host.tasks.min(u32::try_from(*value).unwrap_or(u32::MAX));
            }
            "concurrent_effects" => {
                host.concurrent_effects = host
                    .concurrent_effects
                    .min(u32::try_from(*value).unwrap_or(u32::MAX));
            }
            "cleanup_instructions" => {
                host.cleanup_instructions = host.cleanup_instructions.min(*value);
            }
            _ => {}
        }
    }
    host
}
fn effective_http_limits(mut host: HttpLimits, limits: &[(String, u64)]) -> HttpLimits {
    for (name, value) in limits {
        match name.as_str() {
            "http_requests" => host.max_requests = host.max_requests.min(to_u32(*value)),
            "http_redirects" => host.max_redirects = host.max_redirects.min(to_u32(*value)),
            "http_dns_addresses" => {
                host.max_dns_candidates = host.max_dns_candidates.min(to_usize(*value));
            }
            "http_response_headers" => {
                host.max_response_headers = host.max_response_headers.min(to_usize(*value));
            }
            "http_response_header_bytes" => {
                host.max_header_bytes = host.max_header_bytes.min(to_usize(*value));
            }
            "http_compressed_bytes" => {
                host.max_compressed_bytes = host.max_compressed_bytes.min(*value);
            }
            "http_decoded_bytes" => {
                host.max_decoded_bytes = host.max_decoded_bytes.min(*value);
            }
            "http_decompression_ratio" => {
                host.max_decompression_ratio = host.max_decompression_ratio.min(to_u32(*value));
            }
            "http_connect_ms" => {
                host.connect_timeout = host.connect_timeout.min(Duration::from_millis(*value));
            }
            "http_first_byte_ms" => {
                host.first_byte_timeout =
                    host.first_byte_timeout.min(Duration::from_millis(*value));
            }
            "http_idle_ms" => {
                host.idle_timeout = host.idle_timeout.min(Duration::from_millis(*value));
            }
            "http_total_ms" => {
                host.total_timeout = host.total_timeout.min(Duration::from_millis(*value));
            }
            _ => {}
        }
    }
    host
}

fn to_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn effective_workspace_limits(
    mut host: WorkspaceLimits,
    limits: &[(String, u64)],
) -> WorkspaceLimits {
    for (name, value) in limits {
        match name.as_str() {
            "fs_operations" => host.max_operations = host.max_operations.min(*value),
            "fs_read_bytes" => host.max_read_bytes = host.max_read_bytes.min(*value),
            "fs_write_bytes" => host.max_write_bytes = host.max_write_bytes.min(*value),
            "fs_file_bytes" => host.max_file_bytes = host.max_file_bytes.min(*value),
            "fs_entries" => {
                host.max_entries = host
                    .max_entries
                    .min(usize::try_from(*value).unwrap_or(usize::MAX));
            }
            _ => {}
        }
    }
    host
}
fn manifest_limit(limits: &[(String, u64)], name: &str, host: usize) -> usize {
    limits
        .iter()
        .find(|(key, _)| key == name)
        .map_or(host, |(_, value)| {
            host.min(usize::try_from(*value).unwrap_or(usize::MAX))
        })
}

struct NoObserver;
impl CheckpointObserver for NoObserver {
    fn checkpoint(&mut self, _: Checkpoint) {}
}
struct NeverCancel;
impl CancellationSource for NeverCancel {
    fn is_cancelled(&mut self) -> bool {
        false
    }
}
struct ToolCancellationBridge<'source> {
    source: &'source mut dyn CancellationSource,
}

struct AgentCancellationBridge<'source> {
    source: &'source mut dyn CancellationSource,
}
impl AgentCancellationSignal for AgentCancellationBridge<'_> {
    fn is_cancelled(&mut self) -> bool {
        self.source.is_cancelled()
    }
}
impl ToolCancellationSignal for ToolCancellationBridge<'_> {
    fn is_cancelled(&mut self) -> bool {
        self.source.is_cancelled()
    }
}

fn effective_projection_limits(
    execution: ExecutionLimits,
    workspace: WorkspaceLimits,
    http: HttpLimits,
    input_bytes: usize,
    output_bytes: usize,
    effects: u64,
) -> BTreeMap<String, u64> {
    let usize_limit = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
    BTreeMap::from([
        ("call_depth".to_owned(), u64::from(execution.call_depth)),
        (
            "cleanup_instructions".to_owned(),
            execution.cleanup_instructions,
        ),
        (
            "concurrent_effects".to_owned(),
            u64::from(execution.concurrent_effects),
        ),
        ("effects".to_owned(), effects),
        ("fs_entries".to_owned(), usize_limit(workspace.max_entries)),
        ("fs_file_bytes".to_owned(), workspace.max_file_bytes),
        ("fs_operations".to_owned(), workspace.max_operations),
        ("fs_read_bytes".to_owned(), workspace.max_read_bytes),
        ("fs_write_bytes".to_owned(), workspace.max_write_bytes),
        ("heap_bytes".to_owned(), execution.allocation_bytes),
        (
            "http_compressed_bytes".to_owned(),
            http.max_compressed_bytes,
        ),
        (
            "http_connect_ms".to_owned(),
            u64::try_from(http.connect_timeout.as_millis()).unwrap_or(u64::MAX),
        ),
        ("http_decoded_bytes".to_owned(), http.max_decoded_bytes),
        (
            "http_decompression_ratio".to_owned(),
            u64::from(http.max_decompression_ratio),
        ),
        (
            "http_dns_addresses".to_owned(),
            usize_limit(http.max_dns_candidates),
        ),
        (
            "http_first_byte_ms".to_owned(),
            u64::try_from(http.first_byte_timeout.as_millis()).unwrap_or(u64::MAX),
        ),
        (
            "http_idle_ms".to_owned(),
            u64::try_from(http.idle_timeout.as_millis()).unwrap_or(u64::MAX),
        ),
        ("http_redirects".to_owned(), u64::from(http.max_redirects)),
        ("http_requests".to_owned(), u64::from(http.max_requests)),
        (
            "http_response_header_bytes".to_owned(),
            usize_limit(http.max_header_bytes),
        ),
        (
            "http_response_headers".to_owned(),
            usize_limit(http.max_response_headers),
        ),
        (
            "http_total_ms".to_owned(),
            u64::try_from(http.total_timeout.as_millis()).unwrap_or(u64::MAX),
        ),
        ("input_bytes".to_owned(), usize_limit(input_bytes)),
        ("instructions".to_owned(), execution.instructions),
        (
            "maximum_allocation_bytes".to_owned(),
            execution.maximum_allocation_bytes,
        ),
        ("output_bytes".to_owned(), usize_limit(output_bytes)),
        ("tasks".to_owned(), u64::from(execution.tasks)),
        (
            "wall_ms".to_owned(),
            u64::try_from(execution.wall_time.as_millis()).unwrap_or(u64::MAX),
        ),
    ])
}

#[derive(Clone, Copy, Debug)]
struct EffectiveAuthority {
    filesystem: Rights,
    permission_request: bool,
    http_get: bool,
}

struct CapabilityEntry {
    nonce: u64,
    broker: Option<WorkspaceBroker>,
    external_id: Option<ExternalGrantId>,
}

struct SubAgentHandleEntry {
    nonce: u64,
    id: SubAgentId,
}

#[derive(Clone)]
enum SubAgentResponseCall {
    Run(SubAgentRunCall),
    Ask(SubAgentAskCall),
}

impl SubAgentResponseCall {
    fn response(&self) -> &AgentAskCall {
        match self {
            Self::Run(call) => &call.response,
            Self::Ask(call) => &call.response,
        }
    }

    fn response_mut(&mut self) -> &mut AgentAskCall {
        match self {
            Self::Run(call) => &mut call.response,
            Self::Ask(call) => &mut call.response,
        }
    }
}

#[derive(Clone)]
enum PendingSubAgent {
    Create {
        operation_id: u64,
    },
    Message {
        operation_id: u64,
    },
    Response {
        operation_id: u64,
        call: SubAgentResponseCall,
        result_type: ValueType,
    },
}

const fn pending_sub_agent_operation_id(pending: &PendingSubAgent) -> u64 {
    match pending {
        PendingSubAgent::Create { operation_id }
        | PendingSubAgent::Message { operation_id }
        | PendingSubAgent::Response { operation_id, .. } => *operation_id,
    }
}

const fn pending_sub_agent_operation(pending: &PendingSubAgent) -> FsOperation {
    match pending {
        PendingSubAgent::Create { .. } => FsOperation::SubAgentCreate,
        PendingSubAgent::Message { .. } => FsOperation::SubAgentMessage,
        PendingSubAgent::Response { call, .. } => match call {
            SubAgentResponseCall::Run(_) => FsOperation::SubAgentRun,
            SubAgentResponseCall::Ask(_) => FsOperation::SubAgentAsk,
        },
    }
}

#[derive(Clone)]
enum PendingAgent {
    Message {
        operation_id: u64,
    },
    Ask {
        operation_id: u64,
        call: AgentAskCall,
        result_type: ValueType,
        operation: FsOperation,
    },
    Transcript {
        operation_id: u64,
        limit: u8,
        result_type: ValueType,
    },
}

const fn pending_agent_operation(pending: &PendingAgent) -> FsOperation {
    match pending {
        PendingAgent::Message { .. } => FsOperation::AgentMessage,
        PendingAgent::Ask { operation, .. } => *operation,
        PendingAgent::Transcript { .. } => FsOperation::AgentTranscript,
    }
}

struct PendingTool {
    operation_id: u64,
    contract: ToolContract,
    definition: ToolDefinition,
    error_enum: Option<u32>,
}

struct PendingPermission {
    retained: RetainedExternalTarget,
    request: ExternalGrantRequest,
}

struct BrokerEffects<'provider> {
    generation: u64,
    execution_id: ExternalExecutionId,
    capabilities: Vec<CapabilityEntry>,
    accounting: ExecutionAccounting,
    http: Option<HttpBroker>,
    external_grants: Option<&'provider mut dyn ExternalGrantDecisionProvider>,
    tools: Option<&'provider mut dyn ToolProvider>,
    invoking_agent: Option<&'provider mut dyn InvokingAgentProvider>,
    model: Option<&'provider mut dyn ResponseProvider>,
    user: Option<&'provider mut dyn ResponseProvider>,
    sub_agent: Option<&'provider mut dyn SubAgentProvider>,
    sub_agent_handles: Vec<SubAgentHandleEntry>,
    projection_limits: BTreeMap<String, u64>,
    tool_catalog: Option<FrozenCatalog>,
    tool_contracts: Vec<ToolContract>,
    schemas: Vec<allen_bytecode::StrictSchema>,
    enum_types: Vec<allen_bytecode::EnumType>,
    tool_deadline: Option<Instant>,
    transcript_bytes: usize,
    next_tool_operation: u64,
    next_agent_operation: u64,
    next_agent_interaction: u64,
    next_permission_operation: u64,
    pending_agent_operations: HashSet<u64>,
    pending_agents: HashMap<PendingEffectId, PendingAgent>,
    pending_tools: HashMap<PendingEffectId, PendingTool>,
    pending_permissions: HashMap<PendingEffectId, PendingPermission>,
    pending_sub_agents: HashMap<PendingEffectId, PendingSubAgent>,
    response_audit: Vec<ResponseAuditRecord>,
    maximum_response_attempts: u32,
    authority: EffectiveAuthority,
    effects: u64,
    maximum_effects: u64,
    next_pending_target: u64,
    expired: bool,
    provider_lost: bool,
}

impl<'provider> BrokerEffects<'provider> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        generation: u64,
        workspace: Option<WorkspaceBroker>,
        accounting: ExecutionAccounting,
        http: Option<HttpBroker>,
        external_grants: Option<&'provider mut dyn ExternalGrantDecisionProvider>,
        tools: Option<&'provider mut dyn ToolProvider>,
        invoking_agent: Option<&'provider mut dyn InvokingAgentProvider>,
        model: Option<&'provider mut dyn ResponseProvider>,
        user: Option<&'provider mut dyn ResponseProvider>,
        sub_agent: Option<&'provider mut dyn SubAgentProvider>,
        projection_limits: BTreeMap<String, u64>,
        tool_catalog: Option<FrozenCatalog>,
        tool_contracts: Vec<ToolContract>,
        schemas: Vec<allen_bytecode::StrictSchema>,
        enum_types: Vec<allen_bytecode::EnumType>,
        tool_deadline: Duration,
        transcript_bytes: usize,
        maximum_response_attempts: u32,
        authority: EffectiveAuthority,
        maximum_effects: u64,
    ) -> Self {
        let execution_id = ExternalExecutionId {
            generation,
            nonce: capability_nonce(generation, u32::MAX),
        };
        Self {
            generation,
            execution_id,
            capabilities: vec![CapabilityEntry {
                nonce: capability_nonce(generation, 0),
                broker: workspace,
                external_id: None,
            }],
            accounting,
            http,
            external_grants,
            tools,
            invoking_agent,
            model,
            user,
            sub_agent,
            sub_agent_handles: Vec::new(),
            projection_limits,
            tool_catalog,
            tool_contracts,
            schemas,
            enum_types,
            tool_deadline: Instant::now().checked_add(tool_deadline),
            transcript_bytes,
            next_tool_operation: 1,
            next_agent_operation: 1,
            next_agent_interaction: 1,
            next_permission_operation: 1,
            pending_agent_operations: HashSet::new(),
            pending_agents: HashMap::new(),
            pending_tools: HashMap::new(),
            pending_permissions: HashMap::new(),
            pending_sub_agents: HashMap::new(),
            response_audit: Vec::new(),
            maximum_response_attempts,
            authority,
            effects: 0,
            maximum_effects,
            next_pending_target: 1,
            expired: false,
            provider_lost: false,
        }
    }

    fn effect_success_type(result_type: &ValueType) -> Result<&ValueType, VmError> {
        let ValueType::Result(success, error) = result_type else {
            return Err(VmError::Invariant("effect result type"));
        };
        if error.as_ref() != &allen_bytecode::standard_error_type() {
            return Err(VmError::Invariant("effect error type"));
        }
        Ok(success)
    }

    fn close_expected_result(
        operation: FsOperation,
        result: Result<Value, VmError>,
    ) -> Result<Value, VmError> {
        match result {
            Ok(value) => Ok(ok_result(value)),
            Err(error) => match expected_provider_error(operation, &error) {
                Some((code, message)) => Ok(error_result(code, message)),
                None => Err(error),
            },
        }
    }

    fn close_expected_poll(
        operation: FsOperation,
        result: Result<allen_vm::EffectPoll, VmError>,
    ) -> Result<allen_vm::EffectPoll, VmError> {
        match result {
            Ok(allen_vm::EffectPoll::Ready(value)) => {
                Ok(allen_vm::EffectPoll::Ready(ok_result(value)))
            }
            Ok(allen_vm::EffectPoll::Pending) => Ok(allen_vm::EffectPoll::Pending),
            Err(error) => match expected_provider_error(operation, &error) {
                Some((code, message)) => {
                    Ok(allen_vm::EffectPoll::Ready(error_result(code, message)))
                }
                None => Err(error),
            },
        }
    }

    fn file_effect_result(result: Result<Value, FileError>) -> Result<Value, VmError> {
        if let Err(error) = &result {
            if file_error_is_resource(error.code) {
                return Err(VmError::ResourceLimit {
                    resource: error.code.as_str(),
                });
            }
        }
        Ok(file_result(result))
    }

    fn http_effect_result(result: Result<Value, HttpError>) -> Result<Value, VmError> {
        match result {
            Ok(value) => Ok(ok_result(value)),
            Err(error) if http_error_is_resource(error.code) => Err(VmError::ResourceLimit {
                resource: error.code.as_str(),
            }),
            Err(error) => Ok(error_result(error.code.as_str(), error.message)),
        }
    }

    fn permission_denied_result() -> Value {
        error_result(
            "permission.denied",
            "the external filesystem grant was denied",
        )
    }

    fn permission_unavailable_result() -> Value {
        error_result(
            "permission.unavailable",
            "the external filesystem grant provider is unavailable",
        )
    }

    fn permission_retention_error(error: &FileError) -> Result<Value, VmError> {
        if file_error_is_resource(error.code) {
            return Err(VmError::ResourceLimit {
                resource: error.code.as_str(),
            });
        }
        Ok(Self::permission_denied_result())
    }

    fn permission_provider_error(error: VmError) -> Result<Value, VmError> {
        if matches!(error, VmError::AgentUnavailable | VmError::Timeout { .. }) {
            Ok(Self::permission_unavailable_result())
        } else {
            Err(error)
        }
    }

    fn permission_invalid_provider_decision() -> Result<Value, VmError> {
        Err(VmError::ProtocolViolation)
    }

    fn unavailable_effect_result(operation: FsOperation, error: VmError) -> Result<Value, VmError> {
        match operation {
            FsOperation::ReadText
            | FsOperation::ReadBytes
            | FsOperation::WriteText
            | FsOperation::WriteBytes
            | FsOperation::List
            | FsOperation::Search => Ok(error_result(
                "fs.unavailable",
                "the filesystem provider is unavailable",
            )),
            FsOperation::HttpGet => Ok(error_result(
                "network.unavailable",
                "the network provider is unavailable",
            )),
            FsOperation::PermissionRequestFile | FsOperation::PermissionRequestDirectory => {
                Self::permission_provider_error(error)
            }
            _ => Err(error),
        }
    }

    fn expire(&mut self) {
        self.expired = true;
        self.capabilities.clear();
        self.http = None;
        self.sub_agent_handles.clear();
    }

    fn http_usage(&self) -> HttpUsage {
        self.http
            .as_ref()
            .map_or_else(HttpUsage::default, HttpBroker::usage)
    }

    fn charge_effect(&mut self) -> Result<(), VmError> {
        if self.effects >= self.maximum_effects {
            return Err(VmError::ResourceLimit {
                resource: "effects",
            });
        }
        self.effects += 1;
        Ok(())
    }

    fn sub_agent_projection(&self, value: &Value) -> Result<SubAgentProjection, VmError> {
        let Value::Record(fields) = value else {
            return Err(VmError::Invariant("sub-agent projection type"));
        };
        let capabilities = projection_names(record_field(fields, "capabilities")?)?;
        let tools = projection_names(record_field(fields, "tools")?)?;
        let Value::Map(limit_entries) = record_field(fields, "limits")? else {
            return Err(VmError::Invariant("sub-agent projection limits type"));
        };
        let mut limits = BTreeMap::new();
        for (name, value) in limit_entries.iter() {
            let (Value::String(name), Value::Int(value)) = (name, value) else {
                return Err(VmError::Invariant("sub-agent projection limit type"));
            };
            let value = u64::try_from(*value).map_err(|_| VmError::CapabilityMissing)?;
            let Some(parent) = self.projection_limits.get(name.as_ref()) else {
                return Err(VmError::CapabilityMissing);
            };
            if value == 0 || value > *parent {
                return Err(VmError::CapabilityMissing);
            }
            limits.insert(name.to_string(), value);
        }

        for capability in &capabilities {
            let allowed = match capability.as_str() {
                "fs.read" => self.authority.filesystem.read,
                "fs.write" => self.authority.filesystem.write,
                "net.http_get" => self.authority.http_get,
                "permission.request_external_fs" => self.authority.permission_request,
                _ => false,
            };
            if !allowed {
                return Err(VmError::CapabilityMissing);
            }
        }
        let granted_tools = self
            .tool_contracts
            .iter()
            .map(|contract| contract.name.as_str())
            .collect::<BTreeSet<_>>();
        if tools.iter().any(|tool| {
            ToolName::parse(tool.clone()).is_err() || !granted_tools.contains(tool.as_str())
        }) {
            return Err(VmError::CapabilityMissing);
        }
        Ok(SubAgentProjection {
            capabilities,
            limits,
            tools,
        })
    }

    fn sub_agent_target(&self, handle: SubAgentValue) -> Result<SubAgentId, VmError> {
        if handle.generation() != self.generation {
            return Err(VmError::CapabilityMissing);
        }
        let entry = self
            .sub_agent_handles
            .get(handle.index() as usize)
            .ok_or(VmError::CapabilityMissing)?;
        if entry.nonce != handle.nonce() {
            return Err(VmError::CapabilityMissing);
        }
        Ok(entry.id.clone())
    }

    fn issue_sub_agent_handle(&mut self, id: SubAgentId) -> Result<Value, VmError> {
        let index =
            u32::try_from(self.sub_agent_handles.len()).map_err(|_| VmError::ResourceLimit {
                resource: "sub_agents",
            })?;
        let nonce = capability_nonce(self.generation, index);
        self.sub_agent_handles
            .push(SubAgentHandleEntry { nonce, id });
        Ok(Value::SubAgent(SubAgentValue::new(
            self.generation,
            index,
            nonce,
        )))
    }

    fn agent_deadline(&self) -> Result<Duration, VmError> {
        let deadline = self.tool_deadline.map_or(Duration::MAX, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        if deadline.is_zero() {
            Err(VmError::Timeout {
                resource: RESOURCE_WALL_TIME,
            })
        } else {
            Ok(deadline)
        }
    }

    fn next_agent_operation(&mut self) -> Result<u64, VmError> {
        let operation_id = self.next_agent_operation;
        self.next_agent_operation =
            self.next_agent_operation
                .checked_add(1)
                .ok_or(VmError::ResourceLimit {
                    resource: "agent_operations",
                })?;
        Ok(operation_id)
    }

    fn next_agent_interaction(&mut self) -> Result<u64, VmError> {
        let interaction_id = self.next_agent_interaction;
        self.next_agent_interaction =
            self.next_agent_interaction
                .checked_add(1)
                .ok_or(VmError::ResourceLimit {
                    resource: "agent_interactions",
                })?;
        Ok(interaction_id)
    }

    fn refresh_external_state(&mut self) -> Result<(), VmError> {
        if self.provider_lost {
            return Err(VmError::AgentUnavailable);
        }
        if !self
            .capabilities
            .iter()
            .any(|entry| entry.external_id.is_some() && entry.broker.is_some())
        {
            return Ok(());
        }
        let Some(provider) = self.external_grants.as_deref_mut() else {
            self.revoke_all_external();
            self.provider_lost = true;
            return Err(VmError::AgentUnavailable);
        };
        let revocations = match provider.take_revocations() {
            Ok(revocations) => revocations,
            Err(error) => {
                self.revoke_all_external();
                self.provider_lost = true;
                return Err(error);
            }
        };
        for entry in &mut self.capabilities {
            if entry
                .external_id
                .is_some_and(|id| revocations.contains(&id))
            {
                entry.broker = None;
            }
        }
        Ok(())
    }

    fn revoke_all_external(&mut self) {
        for entry in &mut self.capabilities {
            if entry.external_id.is_some() {
                entry.broker = None;
            }
        }
    }

    fn workspace_entry(&self, handle: WorkspaceValue) -> Result<&CapabilityEntry, VmError> {
        if self.expired || handle.generation() != self.generation {
            return Err(VmError::CapabilityMissing);
        }
        let entry = self
            .capabilities
            .get(handle.index() as usize)
            .ok_or(VmError::CapabilityMissing)?;
        if entry.nonce != handle.nonce() {
            return Err(VmError::CapabilityMissing);
        }
        Ok(entry)
    }

    fn filesystem_call(
        &mut self,
        operation: FsOperation,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let Value::Workspace(handle) = args
            .first()
            .ok_or(VmError::Invariant("filesystem call needs workspace"))?
        else {
            return Err(VmError::Invariant("filesystem workspace type"));
        };
        let path = match args.get(1) {
            Some(Value::String(value)) => value.as_ref(),
            _ => return Err(VmError::Invariant("filesystem path type")),
        };
        let authorized = match operation {
            FsOperation::ReadText
            | FsOperation::ReadBytes
            | FsOperation::List
            | FsOperation::Search => self.authority.filesystem.read,
            FsOperation::WriteText | FsOperation::WriteBytes => self.authority.filesystem.write,
            _ => return Err(VmError::Invariant("filesystem operation type")),
        };
        let entry = self.workspace_entry(*handle)?;
        if !authorized {
            return Ok(file_result(Err(FileError::permission_denied())));
        }
        let Some(broker) = &entry.broker else {
            return if entry.external_id.is_some() {
                Ok(error_result(
                    "fs.unavailable",
                    "the filesystem provider is unavailable",
                ))
            } else {
                Ok(file_result(Err(FileError::permission_denied())))
            };
        };
        let result = match operation {
            FsOperation::ReadText => broker
                .read_text(path)
                .map(|value| Value::String(value.into())),
            FsOperation::ReadBytes => broker
                .read_bytes(path)
                .map(|value| Value::Bytes(value.into())),
            FsOperation::WriteText => match args.get(2) {
                Some(Value::String(value)) => broker.write_text(path, value).map(|()| Value::Unit),
                _ => return Err(VmError::Invariant("write text type")),
            },
            FsOperation::WriteBytes => match args.get(2) {
                Some(Value::Bytes(value)) => broker.write_bytes(path, value).map(|()| Value::Unit),
                _ => return Err(VmError::Invariant("write bytes type")),
            },
            FsOperation::List => broker.list(path).map(|values| {
                Value::List(
                    values
                        .into_iter()
                        .map(|value| Value::String(value.into()))
                        .collect::<Vec<_>>()
                        .into(),
                )
            }),
            FsOperation::Search => match args.get(2) {
                Some(Value::String(query)) => broker.search(path, query).map(|matches| {
                    Value::List(
                        matches
                            .into_iter()
                            .map(search_match_value)
                            .collect::<Vec<_>>()
                            .into(),
                    )
                }),
                _ => return Err(VmError::Invariant("filesystem search query type")),
            },
            _ => unreachable!("filesystem operation was checked above"),
        };
        Self::file_effect_result(result)
    }

    fn http_call(&mut self, args: &[Value]) -> Result<Value, VmError> {
        let url = match args {
            [Value::String(value)] => value.as_ref(),
            _ => return Err(VmError::Invariant("HTTP GET argument type")),
        };
        if !self.authority.http_get {
            return Ok(error_result(
                "net.permission_denied",
                "the HTTP capability was not granted",
            ));
        }
        let Some(http) = &mut self.http else {
            return Ok(error_result(
                "network.unavailable",
                "the network provider is unavailable",
            ));
        };
        let result = match http.get(url) {
            Ok(response) => Ok(Value::Record(
                vec![
                    ("body".into(), Value::Bytes(response.body.into())),
                    ("final_url".into(), Value::String(response.final_url.into())),
                    (
                        "headers".into(),
                        Value::Map(
                            response
                                .headers
                                .into_iter()
                                .map(|(name, values)| {
                                    (
                                        Value::String(name.into()),
                                        Value::List(
                                            values
                                                .into_iter()
                                                .map(|value| Value::String(value.into()))
                                                .collect::<Vec<_>>()
                                                .into(),
                                        ),
                                    )
                                })
                                .collect::<Vec<_>>()
                                .into(),
                        ),
                    ),
                    ("status".into(), Value::Int(i64::from(response.status))),
                ]
                .into(),
            )),
            Err(error) => Err(error),
        };
        Self::http_effect_result(result)
    }

    #[allow(clippy::too_many_lines)]
    fn permission_call(
        &mut self,
        operation: FsOperation,
        args: &[Value],
    ) -> Result<Value, VmError> {
        if !self.authority.permission_request {
            return Ok(Self::permission_denied_result());
        }
        let [Value::Record(fields)] = args else {
            return Err(VmError::Invariant("external permission request type"));
        };
        let access = record_field(fields, "access")?;
        let Value::ExternalFsAccess(access) = access else {
            return Err(VmError::Invariant("external permission access type"));
        };
        let rights = access_rights(*access);
        if rights.read && !self.authority.filesystem.read
            || rights.write && !self.authority.filesystem.write
        {
            return Ok(Self::permission_denied_result());
        }
        let Value::String(path) = record_field(fields, "path")? else {
            return Err(VmError::Invariant("external permission path type"));
        };
        let Value::String(reason) = record_field(fields, "reason")? else {
            return Err(VmError::Invariant("external permission reason type"));
        };
        let recursive = match operation {
            FsOperation::PermissionRequestFile => false,
            FsOperation::PermissionRequestDirectory => match record_field(fields, "recursive")? {
                Value::Bool(value) => *value,
                _ => return Err(VmError::Invariant("external permission recursion type")),
            },
            _ => return Err(VmError::Invariant("external permission operation type")),
        };
        let retained = match operation {
            FsOperation::PermissionRequestFile => RetainedExternalTarget::retain_file(
                Path::new(path.as_ref()),
                rights,
                self.accounting.limits(),
            ),
            FsOperation::PermissionRequestDirectory => RetainedExternalTarget::retain_directory(
                Path::new(path.as_ref()),
                rights,
                recursive,
                self.accounting.limits(),
            ),
            _ => unreachable!("permission operation was checked above"),
        };
        let retained = match retained {
            Ok(retained) => retained,
            Err(error) => return Self::permission_retention_error(&error),
        };
        let pending_target_id = self.next_pending_target;
        self.next_pending_target = self.next_pending_target.saturating_add(1);
        let operation_id = self.next_permission_operation;
        self.next_permission_operation =
            self.next_permission_operation
                .checked_add(1)
                .ok_or(VmError::ResourceLimit {
                    resource: "permission_operations",
                })?;
        let request = ExternalGrantRequest {
            execution_id: self.execution_id,
            operation_id,
            pending_target_id,
            kind: retained.kind(),
            path: retained.diagnostic_path().to_path_buf(),
            rights,
            recursive,
            max_bytes: grant_max_bytes(self.accounting.limits(), rights),
            duration: GrantDuration::Execution(self.execution_id),
            reason: reason.to_string(),
        };
        let Some(provider) = self.external_grants.as_deref_mut() else {
            return Ok(Self::permission_unavailable_result());
        };
        let decision = provider.decide(&request);
        let decision = match decision {
            Ok(decision) => decision,
            Err(error) => {
                self.revoke_all_external();
                self.provider_lost = true;
                return Self::permission_provider_error(error);
            }
        };
        let ExternalGrantDecision::Allow {
            execution_id,
            kind,
            path,
            rights,
            recursive,
            max_bytes,
            duration,
        } = decision
        else {
            return Ok(Self::permission_denied_result());
        };
        if execution_id != request.execution_id
            || kind != request.kind
            || path != request.path
            || rights.read && !request.rights.read
            || rights.write && !request.rights.write
            || recursive && !request.recursive
            || duration != request.duration
            || max_bytes > request.max_bytes
        {
            return Self::permission_invalid_provider_decision();
        }
        let grant_limits = grant_limits(self.accounting.limits(), max_bytes);
        let broker = match retained.into_grant_with_limits(
            path,
            rights,
            recursive,
            grant_limits,
            self.accounting.clone(),
        ) {
            Ok(broker) => broker,
            Err(error) => return Self::permission_retention_error(&error),
        };
        let index = u32::try_from(self.capabilities.len()).map_err(|_| VmError::ResourceLimit {
            resource: "handles",
        })?;
        let nonce = capability_nonce(self.generation, index);
        let grant_id = ExternalGrantId {
            generation: self.generation,
            nonce,
        };
        self.capabilities.push(CapabilityEntry {
            nonce,
            broker: Some(broker),
            external_id: Some(grant_id),
        });
        provider.grant_issued(pending_target_id, grant_id);
        Ok(ok_result(Value::Workspace(WorkspaceValue::new(
            self.generation,
            index,
            nonce,
        ))))
    }

    fn finish_permission_decision(
        &mut self,
        pending: PendingPermission,
        decision: ExternalGrantDecision,
    ) -> Result<Value, VmError> {
        let request = pending.request;
        let ExternalGrantDecision::Allow {
            execution_id,
            kind,
            path,
            rights,
            recursive,
            max_bytes,
            duration,
        } = decision
        else {
            return Ok(Self::permission_denied_result());
        };
        if execution_id != request.execution_id
            || kind != request.kind
            || path != request.path
            || rights.read && !request.rights.read
            || rights.write && !request.rights.write
            || recursive && !request.recursive
            || duration != request.duration
            || max_bytes > request.max_bytes
        {
            return Self::permission_invalid_provider_decision();
        }
        let broker = match pending.retained.into_grant_with_limits(
            path,
            rights,
            recursive,
            grant_limits(self.accounting.limits(), max_bytes),
            self.accounting.clone(),
        ) {
            Ok(broker) => broker,
            Err(error) => return Self::permission_retention_error(&error),
        };
        let index = u32::try_from(self.capabilities.len()).map_err(|_| VmError::ResourceLimit {
            resource: "handles",
        })?;
        let nonce = capability_nonce(self.generation, index);
        let grant_id = ExternalGrantId {
            generation: self.generation,
            nonce,
        };
        self.capabilities.push(CapabilityEntry {
            nonce,
            broker: Some(broker),
            external_id: Some(grant_id),
        });
        if let Some(provider) = self.external_grants.as_deref_mut() {
            provider.grant_issued(request.pending_target_id, grant_id);
        }
        Ok(ok_result(Value::Workspace(WorkspaceValue::new(
            self.generation,
            index,
            nonce,
        ))))
    }
}

impl Drop for BrokerEffects<'_> {
    fn drop(&mut self) {
        self.expire();
    }
}

impl EffectProvider for BrokerEffects<'_> {
    fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
        if self.expired {
            return Err(VmError::CapabilityMissing);
        }
        let entry = self
            .capabilities
            .first()
            .ok_or(VmError::CapabilityMissing)?;
        Ok(WorkspaceValue::new(self.generation, 0, entry.nonce))
    }

    fn call(&mut self, operation: FsOperation, args: &[Value]) -> Result<Value, VmError> {
        self.charge_effect()?;
        if let Err(error) = self.refresh_external_state() {
            return Self::unavailable_effect_result(operation, error);
        }
        match operation {
            FsOperation::ReadText
            | FsOperation::ReadBytes
            | FsOperation::WriteText
            | FsOperation::WriteBytes
            | FsOperation::List
            | FsOperation::Search => self.filesystem_call(operation, args),
            FsOperation::HttpGet => self.http_call(args),
            FsOperation::PermissionRequestFile | FsOperation::PermissionRequestDirectory => {
                self.permission_call(operation, args)
            }
            FsOperation::AgentMessage
            | FsOperation::AgentAsk
            | FsOperation::AgentTranscript
            | FsOperation::ModelRequest
            | FsOperation::UserAsk
            | FsOperation::SubAgentCreate
            | FsOperation::SubAgentRun
            | FsOperation::SubAgentMessage
            | FsOperation::SubAgentAsk => Err(VmError::Invariant(
                "agent operation must use agent provider",
            )),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn start_call(
        &mut self,
        pending: PendingEffectId,
        operation: FsOperation,
        args: &[Value],
        cancellation: &mut dyn CancellationSource,
    ) -> Result<allen_vm::EffectPoll, VmError> {
        if !matches!(
            operation,
            FsOperation::PermissionRequestFile | FsOperation::PermissionRequestDirectory
        ) {
            return self.call(operation, args).map(allen_vm::EffectPoll::Ready);
        }
        self.charge_effect()?;
        if let Err(error) = self.refresh_external_state() {
            return Self::permission_provider_error(error).map(allen_vm::EffectPoll::Ready);
        }
        if cancellation.is_cancelled() {
            return Err(VmError::Cancelled);
        }
        if !self.authority.permission_request {
            return Ok(allen_vm::EffectPoll::Ready(Self::permission_denied_result()));
        }
        let [Value::Record(fields)] = args else {
            return Err(VmError::Invariant("external permission request type"));
        };
        let Value::ExternalFsAccess(access) = record_field(fields, "access")? else {
            return Err(VmError::Invariant("external permission access type"));
        };
        let rights = access_rights(*access);
        if rights.read && !self.authority.filesystem.read
            || rights.write && !self.authority.filesystem.write
        {
            return Ok(allen_vm::EffectPoll::Ready(Self::permission_denied_result()));
        }
        let Value::String(path) = record_field(fields, "path")? else {
            return Err(VmError::Invariant("external permission path type"));
        };
        let Value::String(reason) = record_field(fields, "reason")? else {
            return Err(VmError::Invariant("external permission reason type"));
        };
        let recursive = match operation {
            FsOperation::PermissionRequestFile => false,
            FsOperation::PermissionRequestDirectory => match record_field(fields, "recursive")? {
                Value::Bool(value) => *value,
                _ => return Err(VmError::Invariant("external permission recursion type")),
            },
            _ => unreachable!("permission operation was checked"),
        };
        let retained = match operation {
            FsOperation::PermissionRequestFile => RetainedExternalTarget::retain_file(
                Path::new(path.as_ref()),
                rights,
                self.accounting.limits(),
            ),
            FsOperation::PermissionRequestDirectory => RetainedExternalTarget::retain_directory(
                Path::new(path.as_ref()),
                rights,
                recursive,
                self.accounting.limits(),
            ),
            _ => unreachable!("permission operation was checked"),
        };
        let retained = match retained {
            Ok(retained) => retained,
            Err(error) => {
                return Self::permission_retention_error(&error).map(allen_vm::EffectPoll::Ready);
            }
        };
        let operation_id = self.next_permission_operation;
        self.next_permission_operation =
            self.next_permission_operation
                .checked_add(1)
                .ok_or(VmError::ResourceLimit {
                    resource: "permission_operations",
                })?;
        let pending_target_id = self.next_pending_target;
        self.next_pending_target = self.next_pending_target.saturating_add(1);
        let request = ExternalGrantRequest {
            execution_id: self.execution_id,
            operation_id,
            pending_target_id,
            kind: retained.kind(),
            path: retained.diagnostic_path().to_path_buf(),
            rights,
            recursive,
            max_bytes: grant_max_bytes(self.accounting.limits(), rights),
            duration: GrantDuration::Execution(self.execution_id),
            reason: reason.to_string(),
        };
        let poll = match self.external_grants.as_deref_mut() {
            Some(provider) => provider.start_decide(pending, &request),
            None => Err(VmError::AgentUnavailable),
        };
        let poll = match poll {
            Ok(poll) => poll,
            Err(error) => {
                self.revoke_all_external();
                self.provider_lost = true;
                return Self::permission_provider_error(error).map(allen_vm::EffectPoll::Ready);
            }
        };
        match poll {
            ExternalGrantPoll::Pending => {
                self.pending_permissions
                    .insert(pending, PendingPermission { retained, request });
                Ok(allen_vm::EffectPoll::Pending)
            }
            ExternalGrantPoll::Decision(decision) => self
                .finish_permission_decision(PendingPermission { retained, request }, decision)
                .map(allen_vm::EffectPoll::Ready),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn agent(
        &mut self,
        operation: FsOperation,
        arguments: &[Value],
        result_type: &ValueType,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<Value, VmError> {
        self.charge_effect()?;
        let result_type = Self::effect_success_type(result_type)?.clone();
        let result = (|| {
            if self.expired {
                return Err(VmError::AgentUnavailable);
            }
            let deadline = self.agent_deadline()?;
            let operation_id = self.next_agent_operation()?;
            let execution_id = self.execution_id;
            let transcript_bytes = self.transcript_bytes;
            let dispatch = match operation {
                FsOperation::AgentMessage => {
                    let [Value::String(message)] = arguments else {
                        return Err(VmError::Invariant("agent message argument type"));
                    };
                    AgentDispatch::Message(AgentMessageCall {
                        execution_id,
                        operation_id,
                        message: message.to_string(),
                        deadline,
                    })
                }
                FsOperation::AgentAsk => {
                    let [Value::String(message)] = arguments else {
                        return Err(VmError::Invariant("agent ask argument type"));
                    };
                    AgentDispatch::Ask(AgentAskCall {
                        execution_id,
                        operation_id,
                        interaction_id: self.next_agent_interaction()?,
                        prompt: PromptPayload::Text(message.to_string()),
                        response_schema: response_schema(
                            &ValueType::String,
                            &self.enum_types,
                            true,
                        ),
                        attempt: 1,
                        validation_issues: Vec::new(),
                        deadline,
                    })
                }
                FsOperation::AgentTranscript => {
                    let [Value::Record(fields)] = arguments else {
                        return Err(VmError::Invariant("agent transcript argument type"));
                    };
                    let Value::Int(limit) = record_field(fields, "limit")? else {
                        return Err(VmError::Invariant("agent transcript limit type"));
                    };
                    let limit = u8::try_from(*limit)
                        .ok()
                        .filter(|limit| (1..=100).contains(limit))
                        .ok_or(VmError::AgentResponseSchema)?;
                    AgentDispatch::Transcript(TranscriptQuery {
                        execution_id,
                        operation_id,
                        limit,
                        deadline,
                        maximum_bytes: transcript_bytes,
                    })
                }
                _ => return Err(VmError::Invariant("agent operation type")),
            };
            if cancellation.is_cancelled() {
                return Err(VmError::Cancelled);
            }
            if self.invoking_agent.is_none() {
                return Err(VmError::AgentUnavailable);
            }
            self.pending_agent_operations.insert(operation_id);
            let (result, cancelled) = {
                let provider = self
                    .invoking_agent
                    .as_deref_mut()
                    .ok_or(VmError::AgentUnavailable)?;
                let mut signal = AgentCancellationBridge {
                    source: cancellation,
                };
                let result = match dispatch {
                    AgentDispatch::Message(call) => {
                        let accepted = provider
                            .message(&call, &mut signal)
                            .map_err(agent_host_error)?;
                        if accepted {
                            Ok(Value::Unit)
                        } else {
                            Err(VmError::AgentResponseSchema)
                        }
                    }
                    AgentDispatch::Ask(call) => {
                        match provider.ask(&call, &mut signal).map_err(agent_host_error)? {
                            serde_json::Value::String(value) => Ok(Value::String(value.into())),
                            _ => Err(VmError::AgentResponseSchema),
                        }
                    }
                    AgentDispatch::Transcript(query) => {
                        let snapshot = provider
                            .transcript(&query, &mut signal)
                            .map_err(agent_host_error)?;
                        validate_transcript(&snapshot, query.limit, query.maximum_bytes)?;
                        transcript_to_value(&snapshot, &result_type, &self.enum_types)
                    }
                };
                let cancelled = signal.is_cancelled();
                if cancelled {
                    provider.cancel(execution_id, operation_id);
                }
                (result, cancelled)
            };
            self.pending_agent_operations.remove(&operation_id);
            if cancelled {
                return Err(VmError::Cancelled);
            }
            result
        })();
        Self::close_expected_result(operation, result)
    }

    fn start_tool(
        &mut self,
        pending: PendingEffectId,
        tool: u32,
        input: &Value,
        result_type: &ValueType,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<allen_vm::EffectPoll, VmError> {
        self.charge_effect()?;
        let error_enum = tool_error_enum(
            result_type,
            &self.schemas,
            &self.enum_types,
            tool,
            &self.tool_contracts,
        )?;
        let result = (|| {
            let contract = self
                .tool_contracts
                .get(tool as usize)
                .cloned()
                .ok_or(VmError::ToolUnavailable)?;
            let catalog = self.tool_catalog.as_ref().ok_or(VmError::ToolUnavailable)?;
            let name =
                ToolName::parse(contract.name.clone()).map_err(|_| VmError::ToolUnavailable)?;
            let definition = catalog.get(&name).ok_or(VmError::ToolUnavailable)?.clone();
            if !tool_contract_matches(&contract, &definition) {
                return Err(VmError::ToolUnavailable);
            }
            let input_type = &self
                .schemas
                .get(contract.input_schema as usize)
                .ok_or(VmError::ToolSchemaError)?
                .value_type;
            let input_json = tool_value_to_json(
                input,
                definition.input_schema.descriptor(),
                input_type,
                &self.enum_types,
            )
            .map_err(|_| VmError::ToolSchemaError)?;
            definition
                .input_schema
                .validate(&input_json, &SchemaLimits::default())
                .map_err(|_| VmError::ToolSchemaError)?;
            let operation_id = self.next_tool_operation;
            self.next_tool_operation =
                self.next_tool_operation
                    .checked_add(1)
                    .ok_or(VmError::ResourceLimit {
                        resource: "tool_operations",
                    })?;
            let deadline = self.tool_deadline.map_or(Duration::MAX, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            if deadline.is_zero() {
                return Err(VmError::Timeout {
                    resource: RESOURCE_WALL_TIME,
                });
            }
            let invocation = ToolInvocation {
                execution_id: self.execution_id,
                operation_id,
                name: contract.name.clone(),
                version: contract.version.clone(),
                catalog_digest: catalog.digest().to_owned(),
                deadline,
            };
            let provider = self.tools.as_deref_mut().ok_or(VmError::ToolUnavailable)?;
            let mut signal = ToolCancellationBridge {
                source: cancellation,
            };
            let poll = provider
                .start_invoke(pending, &invocation, input_json, &mut signal)
                .map_err(tool_host_error)?;
            if signal.is_cancelled() {
                provider.cancel_pending(pending, self.execution_id, operation_id);
                return Err(VmError::Cancelled);
            }
            let pending_tool = PendingTool {
                operation_id,
                contract,
                definition,
                error_enum,
            };
            match poll {
                ToolProviderPoll::Pending => {
                    self.pending_tools.insert(pending, pending_tool);
                    Ok(allen_vm::EffectPoll::Pending)
                }
                ToolProviderPoll::Outcome(outcome) => {
                    tool_outcome_value(outcome, &pending_tool, &self.schemas, &self.enum_types)
                        .map(allen_vm::EffectPoll::Ready)
                }
            }
        })();
        close_tool_poll(result, error_enum, &self.enum_types)
    }

    #[allow(clippy::too_many_lines)]
    fn start_sub_agent(
        &mut self,
        pending: PendingEffectId,
        operation: FsOperation,
        arguments: &[Value],
        result_type: &ValueType,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<allen_vm::EffectPoll, VmError> {
        let deadline = self.agent_deadline()?;
        let operation_id = self.next_agent_operation()?;
        let execution_id = self.execution_id;
        let mut signal = AgentCancellationBridge {
            source: cancellation,
        };
        if self.sub_agent.is_none() {
            return Err(VmError::SubAgentUnavailable);
        }
        let (poll, pending_call) = match operation {
            FsOperation::SubAgentCreate => {
                let [prompt_value, projection_value] = arguments else {
                    return Err(VmError::Invariant("sub-agent create argument type"));
                };
                let (prompt, output_type, _) = prompt_from_value(
                    prompt_value,
                    &allen_bytecode::prompt_type(ValueType::Unit),
                    &self.enum_types,
                )?;
                if output_type != ValueType::Unit || result_type != &ValueType::SubAgent {
                    return Err(VmError::SubAgentResponseSchema);
                }
                let PromptPayload::Structured(prompt) = prompt else {
                    return Err(VmError::SubAgentResponseSchema);
                };
                validate_sub_agent_context(&prompt, self.transcript_bytes)?;
                let projection = self.sub_agent_projection(projection_value)?;
                let call = SubAgentCreateCall {
                    execution_id,
                    operation_id,
                    prompt,
                    projection,
                    deadline,
                };
                (
                    self.sub_agent
                        .as_deref_mut()
                        .ok_or(VmError::SubAgentUnavailable)?
                        .start_create(pending, &call, &mut signal)
                        .map_err(sub_agent_host_error)?,
                    PendingSubAgent::Create { operation_id },
                )
            }
            FsOperation::SubAgentRun => {
                let [prompt_value, projection_value] = arguments else {
                    return Err(VmError::Invariant("sub-agent run argument type"));
                };
                let (prompt, output_type, _) = prompt_from_value(
                    prompt_value,
                    &allen_bytecode::prompt_type(result_type.clone()),
                    &self.enum_types,
                )?;
                if &output_type != result_type {
                    return Err(VmError::SubAgentResponseSchema);
                }
                let PromptPayload::Structured(structured) = &prompt else {
                    return Err(VmError::SubAgentResponseSchema);
                };
                validate_sub_agent_context(structured, self.transcript_bytes)?;
                let call = SubAgentRunCall {
                    response: AgentAskCall {
                        execution_id,
                        operation_id,
                        interaction_id: self.next_agent_interaction()?,
                        prompt,
                        response_schema: response_schema(result_type, &self.enum_types, false),
                        attempt: 1,
                        validation_issues: Vec::new(),
                        deadline,
                    },
                    projection: self.sub_agent_projection(projection_value)?,
                };
                (
                    self.sub_agent
                        .as_deref_mut()
                        .ok_or(VmError::SubAgentUnavailable)?
                        .start_run(pending, &call, &mut signal)
                        .map_err(sub_agent_host_error)?,
                    PendingSubAgent::Response {
                        operation_id,
                        call: SubAgentResponseCall::Run(call),
                        result_type: result_type.clone(),
                    },
                )
            }
            FsOperation::SubAgentMessage => {
                let [Value::SubAgent(handle), Value::String(message)] = arguments else {
                    return Err(VmError::Invariant("sub-agent message argument type"));
                };
                let call = SubAgentMessageCall {
                    execution_id,
                    operation_id,
                    target: self.sub_agent_target(*handle)?,
                    message: message.to_string(),
                    deadline,
                };
                (
                    self.sub_agent
                        .as_deref_mut()
                        .ok_or(VmError::SubAgentUnavailable)?
                        .start_message(pending, &call, &mut signal)
                        .map_err(sub_agent_host_error)?,
                    PendingSubAgent::Message { operation_id },
                )
            }
            FsOperation::SubAgentAsk => {
                let [Value::SubAgent(handle), prompt_value] = arguments else {
                    return Err(VmError::Invariant("sub-agent ask argument type"));
                };
                let (prompt, output_type, _) = prompt_from_value(
                    prompt_value,
                    &allen_bytecode::prompt_type(result_type.clone()),
                    &self.enum_types,
                )?;
                if &output_type != result_type {
                    return Err(VmError::SubAgentResponseSchema);
                }
                let PromptPayload::Structured(structured) = &prompt else {
                    return Err(VmError::SubAgentResponseSchema);
                };
                validate_sub_agent_context(structured, self.transcript_bytes)?;
                let call = SubAgentAskCall {
                    target: self.sub_agent_target(*handle)?,
                    response: AgentAskCall {
                        execution_id,
                        operation_id,
                        interaction_id: self.next_agent_interaction()?,
                        prompt,
                        response_schema: response_schema(result_type, &self.enum_types, false),
                        attempt: 1,
                        validation_issues: Vec::new(),
                        deadline,
                    },
                };
                (
                    self.sub_agent
                        .as_deref_mut()
                        .ok_or(VmError::SubAgentUnavailable)?
                        .start_ask(pending, &call, &mut signal)
                        .map_err(sub_agent_host_error)?,
                    PendingSubAgent::Response {
                        operation_id,
                        call: SubAgentResponseCall::Ask(call),
                        result_type: result_type.clone(),
                    },
                )
            }
            _ => return Err(VmError::Invariant("sub-agent operation type")),
        };
        if signal.is_cancelled() {
            if let Some(provider) = self.sub_agent.as_deref_mut() {
                provider.cancel(pending, execution_id, operation_id);
            }
            return Err(VmError::Cancelled);
        }
        match poll {
            SubAgentProviderPoll::Pending => {
                self.pending_sub_agents.insert(pending, pending_call);
                Ok(allen_vm::EffectPoll::Pending)
            }
            poll => self.finish_sub_agent_poll_unwrapped(pending, poll, pending_call, cancellation),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn start_agent(
        &mut self,
        pending: PendingEffectId,
        operation: FsOperation,
        arguments: &[Value],
        result_type: &ValueType,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<allen_vm::EffectPoll, VmError> {
        self.charge_effect()?;
        let result_type = Self::effect_success_type(result_type)?.clone();
        let result = (|| {
            if self.expired {
                return Err(unavailable_for(operation));
            }
            if matches!(
                operation,
                FsOperation::SubAgentCreate
                    | FsOperation::SubAgentRun
                    | FsOperation::SubAgentMessage
                    | FsOperation::SubAgentAsk
            ) {
                return self.start_sub_agent(
                    pending,
                    operation,
                    arguments,
                    &result_type,
                    cancellation,
                );
            }
            match operation {
                FsOperation::AgentMessage
                | FsOperation::AgentAsk
                | FsOperation::AgentTranscript
                    if self.invoking_agent.is_none() =>
                {
                    return Err(VmError::AgentUnavailable);
                }
                FsOperation::ModelRequest if self.model.is_none() => {
                    return Err(VmError::ModelUnavailable);
                }
                FsOperation::UserAsk if self.user.is_none() => {
                    return Err(VmError::UserUnavailable);
                }
                _ => {}
            }
            let deadline = self.agent_deadline()?;
            let operation_id = self.next_agent_operation()?;
            let execution_id = self.execution_id;
            let dispatch = match operation {
                FsOperation::AgentMessage => {
                    let [Value::String(message)] = arguments else {
                        return Err(VmError::Invariant("agent message argument type"));
                    };
                    AgentDispatch::Message(AgentMessageCall {
                        execution_id,
                        operation_id,
                        message: message.to_string(),
                        deadline,
                    })
                }
                FsOperation::AgentAsk | FsOperation::ModelRequest | FsOperation::UserAsk => {
                    let [argument] = arguments else {
                        return Err(VmError::Invariant("typed request argument type"));
                    };
                    let argument_type = if matches!(argument, Value::String(_)) {
                        ValueType::String
                    } else {
                        allen_bytecode::prompt_type(result_type.clone())
                    };
                    let (prompt, output_type, _) =
                        prompt_from_value(argument, &argument_type, &self.enum_types)?;
                    if output_type != result_type {
                        return Err(VmError::AgentResponseSchema);
                    }
                    let plain_text = matches!(prompt, PromptPayload::Text(_));
                    AgentDispatch::Ask(AgentAskCall {
                        execution_id,
                        operation_id,
                        interaction_id: self.next_agent_interaction()?,
                        prompt,
                        response_schema: response_schema(
                            &result_type,
                            &self.enum_types,
                            plain_text,
                        ),
                        attempt: 1,
                        validation_issues: Vec::new(),
                        deadline,
                    })
                }
                FsOperation::AgentTranscript => {
                    let [Value::Record(fields)] = arguments else {
                        return Err(VmError::Invariant("agent transcript argument type"));
                    };
                    let Value::Int(limit) = record_field(fields, "limit")? else {
                        return Err(VmError::Invariant("agent transcript limit type"));
                    };
                    let limit = u8::try_from(*limit)
                        .ok()
                        .filter(|limit| (1..=100).contains(limit))
                        .ok_or(VmError::AgentResponseSchema)?;
                    AgentDispatch::Transcript(TranscriptQuery {
                        execution_id,
                        operation_id,
                        limit,
                        deadline,
                        maximum_bytes: self.transcript_bytes,
                    })
                }
                _ => return Err(VmError::Invariant("agent operation type")),
            };
            if cancellation.is_cancelled() {
                return Err(VmError::Cancelled);
            }
            let mut signal = AgentCancellationBridge {
                source: cancellation,
            };
            let (poll, pending_agent) = match dispatch {
                AgentDispatch::Message(call) => {
                    let provider = self
                        .invoking_agent
                        .as_deref_mut()
                        .ok_or(VmError::AgentUnavailable)?;
                    (
                        provider
                            .start_message(pending, &call, &mut signal)
                            .map_err(agent_host_error)?,
                        PendingAgent::Message { operation_id },
                    )
                }
                AgentDispatch::Ask(call) => {
                    let poll =
                        self.start_response_provider(pending, operation, &call, &mut signal)?;
                    (
                        poll,
                        PendingAgent::Ask {
                            operation_id,
                            call,
                            result_type: result_type.clone(),
                            operation,
                        },
                    )
                }
                AgentDispatch::Transcript(query) => {
                    let provider = self
                        .invoking_agent
                        .as_deref_mut()
                        .ok_or(VmError::AgentUnavailable)?;
                    (
                        provider
                            .start_transcript(pending, &query, &mut signal)
                            .map_err(agent_host_error)?,
                        PendingAgent::Transcript {
                            operation_id,
                            limit: query.limit,
                            result_type: result_type.clone(),
                        },
                    )
                }
            };
            if signal.is_cancelled() {
                self.cancel_external_pending(pending, operation, operation_id);
                if let PendingAgent::Ask {
                    call, operation, ..
                } = &pending_agent
                {
                    self.record_response_audit(*operation, call, ResponseAuditOutcome::Cancelled);
                }
                return Err(VmError::Cancelled);
            }
            match poll {
                AgentProviderPoll::Pending => {
                    self.pending_agents.insert(pending, pending_agent);
                    Ok(allen_vm::EffectPoll::Pending)
                }
                poll => {
                    self.finish_agent_poll_unwrapped(pending, poll, pending_agent, cancellation)
                }
            }
        })();
        Self::close_expected_poll(operation, result)
    }

    #[allow(clippy::too_many_lines)]
    fn poll_effect(
        &mut self,
        pending: PendingEffectId,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<allen_vm::EffectPoll, VmError> {
        if cancellation.is_cancelled() {
            self.cancel_effect(pending);
            return Err(VmError::Cancelled);
        }
        if self.pending_permissions.contains_key(&pending) {
            let poll = match self.external_grants.as_deref_mut() {
                Some(provider) => provider.poll(pending),
                None => Err(VmError::AgentUnavailable),
            };
            let poll = match poll {
                Ok(poll) => poll,
                Err(error) => {
                    self.revoke_all_external();
                    self.provider_lost = true;
                    self.pending_permissions.remove(&pending);
                    return Self::permission_provider_error(error).map(allen_vm::EffectPoll::Ready);
                }
            };
            if matches!(poll, ExternalGrantPoll::Pending) {
                return Ok(allen_vm::EffectPoll::Pending);
            }
            let pending_permission = self
                .pending_permissions
                .remove(&pending)
                .ok_or(VmError::Invariant("pending effect is unknown"))?;
            return match poll {
                ExternalGrantPoll::Decision(decision) => self
                    .finish_permission_decision(pending_permission, decision)
                    .map(allen_vm::EffectPoll::Ready),
                ExternalGrantPoll::Pending => unreachable!("pending grant was checked"),
            };
        }
        if self.pending_tools.contains_key(&pending) {
            let provider = self.tools.as_deref_mut().ok_or(VmError::ToolUnavailable)?;
            let mut signal = ToolCancellationBridge {
                source: cancellation,
            };
            let poll = provider.poll(pending, &mut signal).map_err(tool_host_error);
            if let Err(error) = poll {
                let pending_tool = self
                    .pending_tools
                    .remove(&pending)
                    .ok_or(VmError::Invariant("pending effect is unknown"))?;
                return close_tool_poll(Err(error), pending_tool.error_enum, &self.enum_types);
            }
            let poll = poll.expect("tool poll error was handled");
            if matches!(poll, ToolProviderPoll::Pending) {
                return Ok(allen_vm::EffectPoll::Pending);
            }
            let pending_tool = self
                .pending_tools
                .remove(&pending)
                .ok_or(VmError::Invariant("pending effect is unknown"))?;
            let result = match poll {
                ToolProviderPoll::Outcome(outcome) => {
                    tool_outcome_value(outcome, &pending_tool, &self.schemas, &self.enum_types)
                        .map(allen_vm::EffectPoll::Ready)
                }
                ToolProviderPoll::Pending => unreachable!("pending tool was checked"),
            };
            return close_tool_poll(result, pending_tool.error_enum, &self.enum_types);
        }
        if self.pending_sub_agents.contains_key(&pending) {
            let mut signal = AgentCancellationBridge {
                source: cancellation,
            };
            let poll = self
                .sub_agent
                .as_deref_mut()
                .ok_or(VmError::SubAgentUnavailable)?
                .poll(pending, &mut signal)
                .map_err(sub_agent_host_error);
            let pending_call = self
                .pending_sub_agents
                .remove(&pending)
                .ok_or(VmError::Invariant("pending sub-agent effect is unknown"))?;
            let poll = match poll {
                Ok(poll) => poll,
                Err(error) => {
                    return Self::close_expected_poll(
                        pending_sub_agent_operation(&pending_call),
                        Err(error),
                    );
                }
            };
            if signal.is_cancelled() {
                let operation_id = pending_sub_agent_operation_id(&pending_call);
                if let Some(provider) = self.sub_agent.as_deref_mut() {
                    provider.cancel(pending, self.execution_id, operation_id);
                }
                return Err(VmError::Cancelled);
            }
            if matches!(poll, SubAgentProviderPoll::Pending) {
                self.pending_sub_agents.insert(pending, pending_call);
                return Ok(allen_vm::EffectPoll::Pending);
            }
            return self.finish_sub_agent_poll(pending, poll, pending_call, cancellation);
        }
        let pending_agent = self
            .pending_agents
            .get(&pending)
            .cloned()
            .ok_or(VmError::Invariant("pending effect is unknown"))?;
        let operation_id = match &pending_agent {
            PendingAgent::Message { operation_id }
            | PendingAgent::Ask { operation_id, .. }
            | PendingAgent::Transcript { operation_id, .. } => *operation_id,
        };
        let mut signal = AgentCancellationBridge {
            source: cancellation,
        };
        let operation = pending_agent_operation(&pending_agent);
        let poll_result = match operation {
            FsOperation::ModelRequest => self
                .model
                .as_deref_mut()
                .ok_or(VmError::ModelUnavailable)?
                .poll(pending, &mut signal)
                .map(response_poll_to_agent)
                .map_err(|error| response_host_error(operation, error)),
            FsOperation::UserAsk => self
                .user
                .as_deref_mut()
                .ok_or(VmError::UserUnavailable)?
                .poll(pending, &mut signal)
                .map(response_poll_to_agent)
                .map_err(|error| response_host_error(operation, error)),
            _ => self
                .invoking_agent
                .as_deref_mut()
                .ok_or(VmError::AgentUnavailable)?
                .poll(pending, &mut signal)
                .map_err(agent_host_error),
        };
        let poll = match poll_result {
            Ok(poll) => poll,
            Err(error) => {
                self.pending_agents.remove(&pending);
                if let PendingAgent::Ask {
                    call, operation, ..
                } = &pending_agent
                {
                    self.record_response_audit(
                        *operation,
                        call,
                        ResponseAuditOutcome::ProviderFailed,
                    );
                }
                return Self::close_expected_poll(operation, Err(error));
            }
        };
        if signal.is_cancelled() {
            self.cancel_external_pending(pending, operation, operation_id);
            self.pending_agents.remove(&pending);
            if let PendingAgent::Ask {
                call, operation, ..
            } = &pending_agent
            {
                self.record_response_audit(*operation, call, ResponseAuditOutcome::Cancelled);
            }
            return Err(VmError::Cancelled);
        }
        if matches!(poll, AgentProviderPoll::Pending) {
            return Ok(allen_vm::EffectPoll::Pending);
        }
        self.pending_agents.remove(&pending);
        self.finish_agent_poll(pending, poll, pending_agent, cancellation)
    }

    fn cancel_effect(&mut self, pending: PendingEffectId) {
        if self.pending_permissions.remove(&pending).is_some() {
            if let Some(provider) = self.external_grants.as_deref_mut() {
                provider.cancel_pending(pending);
            }
            return;
        }
        if let Some(pending_tool) = self.pending_tools.remove(&pending) {
            if let Some(provider) = self.tools.as_deref_mut() {
                provider.cancel_pending(pending, self.execution_id, pending_tool.operation_id);
            }
            return;
        }
        if let Some(pending_sub_agent) = self.pending_sub_agents.remove(&pending) {
            if let Some(provider) = self.sub_agent.as_deref_mut() {
                provider.cancel(
                    pending,
                    self.execution_id,
                    pending_sub_agent_operation_id(&pending_sub_agent),
                );
            }
            if let PendingSubAgent::Response { call, .. } = &pending_sub_agent {
                self.record_sub_agent_audit(call.response(), ResponseAuditOutcome::Cancelled);
            }
            return;
        }
        let Some(pending_agent) = self.pending_agents.remove(&pending) else {
            return;
        };
        let operation_id = match &pending_agent {
            PendingAgent::Message { operation_id }
            | PendingAgent::Ask { operation_id, .. }
            | PendingAgent::Transcript { operation_id, .. } => *operation_id,
        };
        let operation = match &pending_agent {
            PendingAgent::Ask { operation, .. } => *operation,
            _ => FsOperation::AgentMessage,
        };
        self.cancel_external_pending(pending, operation, operation_id);
        if let PendingAgent::Ask {
            call, operation, ..
        } = &pending_agent
        {
            self.record_response_audit(*operation, call, ResponseAuditOutcome::Cancelled);
        }
    }

    fn cancel_pending(&mut self) {
        let pending = self
            .pending_permissions
            .keys()
            .chain(self.pending_tools.keys())
            .chain(self.pending_agents.keys())
            .chain(self.pending_sub_agents.keys())
            .copied()
            .collect::<Vec<_>>();
        let mut provider_panicked = false;
        for pending in pending {
            provider_panicked |=
                catch_unwind(AssertUnwindSafe(|| self.cancel_effect(pending))).is_err();
        }
        let Some(provider) = self.invoking_agent.as_deref_mut() else {
            self.pending_agent_operations.clear();
            assert!(!provider_panicked, "provider cancellation panicked");
            return;
        };
        for operation_id in self.pending_agent_operations.drain() {
            provider_panicked |= catch_unwind(AssertUnwindSafe(|| {
                provider.cancel(self.execution_id, operation_id);
            }))
            .is_err();
        }
        assert!(!provider_panicked, "provider cancellation panicked");
    }
}

impl BrokerEffects<'_> {
    fn start_sub_agent_response(
        &mut self,
        pending: PendingEffectId,
        call: &SubAgentResponseCall,
        signal: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, VmError> {
        let provider = self
            .sub_agent
            .as_deref_mut()
            .ok_or(VmError::SubAgentUnavailable)?;
        match call {
            SubAgentResponseCall::Run(call) => provider
                .start_run(pending, call, signal)
                .map_err(sub_agent_host_error),
            SubAgentResponseCall::Ask(call) => provider
                .start_ask(pending, call, signal)
                .map_err(sub_agent_host_error),
        }
    }

    fn finish_sub_agent_poll(
        &mut self,
        pending_id: PendingEffectId,
        poll: SubAgentProviderPoll,
        pending: PendingSubAgent,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<allen_vm::EffectPoll, VmError> {
        let operation = pending_sub_agent_operation(&pending);
        let result = self.finish_sub_agent_poll_unwrapped(pending_id, poll, pending, cancellation);
        Self::close_expected_poll(operation, result)
    }

    fn finish_sub_agent_poll_unwrapped(
        &mut self,
        pending_id: PendingEffectId,
        mut poll: SubAgentProviderPoll,
        mut pending: PendingSubAgent,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<allen_vm::EffectPoll, VmError> {
        loop {
            match &mut pending {
                PendingSubAgent::Create { .. } => {
                    let SubAgentProviderPoll::Created(id) = poll else {
                        return Err(provider_poll_protocol_error());
                    };
                    return self
                        .issue_sub_agent_handle(id)
                        .map(allen_vm::EffectPoll::Ready);
                }
                PendingSubAgent::Message { .. } => {
                    let SubAgentProviderPoll::Message(true) = poll else {
                        return Err(provider_poll_protocol_error());
                    };
                    return Ok(allen_vm::EffectPoll::Ready(Value::Unit));
                }
                PendingSubAgent::Response {
                    call, result_type, ..
                } => {
                    let SubAgentProviderPoll::Response(response) = poll else {
                        self.record_sub_agent_audit(
                            call.response(),
                            ResponseAuditOutcome::ValidationFailed,
                        );
                        return Err(provider_poll_protocol_error());
                    };
                    if let Ok(value) = json_to_value(&response, result_type, &self.enum_types) {
                        self.record_sub_agent_audit(call.response(), ResponseAuditOutcome::Valid);
                        return Ok(allen_vm::EffectPoll::Ready(value));
                    }
                    {
                        let response_call = call.response_mut();
                        let maximum = match &response_call.prompt {
                            PromptPayload::Structured(prompt) => {
                                prompt.max_attempts.min(self.maximum_response_attempts)
                            }
                            PromptPayload::Text(_) => 1,
                        };
                        if response_call.attempt >= maximum {
                            self.record_sub_agent_audit(
                                response_call,
                                ResponseAuditOutcome::ValidationFailed,
                            );
                            return Err(VmError::SubAgentResponseSchema);
                        }
                        response_call.validation_issues =
                            exact_validation_issues(&response, result_type, &self.enum_types);
                        response_call.attempt += 1;
                    }
                    let mut signal = AgentCancellationBridge {
                        source: cancellation,
                    };
                    poll = self.start_sub_agent_response(pending_id, call, &mut signal)?;
                    if signal.is_cancelled() {
                        let operation_id = call.response().operation_id;
                        if let Some(provider) = self.sub_agent.as_deref_mut() {
                            provider.cancel(pending_id, self.execution_id, operation_id);
                        }
                        self.record_sub_agent_audit(
                            call.response(),
                            ResponseAuditOutcome::Cancelled,
                        );
                        return Err(VmError::Cancelled);
                    }
                    if matches!(poll, SubAgentProviderPoll::Pending) {
                        self.pending_sub_agents.insert(pending_id, pending);
                        return Ok(allen_vm::EffectPoll::Pending);
                    }
                }
            }
        }
    }

    fn record_sub_agent_audit(&mut self, call: &AgentAskCall, outcome: ResponseAuditOutcome) {
        let identity = self
            .sub_agent
            .as_deref()
            .map_or("sub-agent", SubAgentProvider::identity);
        self.response_audit.push(ResponseAuditRecord {
            provider_kind: ResponseProviderKind::SubAgent,
            provider_identity: truncate_utf8(identity, 256).to_owned(),
            schema_digest: call.response_schema.digest.clone(),
            attempts: call.attempt,
            outcome,
        });
    }

    fn start_response_provider(
        &mut self,
        pending: PendingEffectId,
        operation: FsOperation,
        call: &AgentAskCall,
        signal: &mut dyn AgentCancellationSignal,
    ) -> Result<AgentProviderPoll, VmError> {
        let result = match operation {
            FsOperation::AgentAsk => self
                .invoking_agent
                .as_deref_mut()
                .ok_or(VmError::AgentUnavailable)?
                .start_ask(pending, call, signal)
                .map_err(agent_host_error),
            FsOperation::ModelRequest => self
                .model
                .as_deref_mut()
                .ok_or(VmError::ModelUnavailable)?
                .start_request(pending, call, signal)
                .map(response_poll_to_agent)
                .map_err(|error| response_host_error(operation, error)),
            FsOperation::UserAsk => self
                .user
                .as_deref_mut()
                .ok_or(VmError::UserUnavailable)?
                .start_request(pending, call, signal)
                .map(response_poll_to_agent)
                .map_err(|error| response_host_error(operation, error)),
            _ => Err(VmError::Invariant(
                "operation is not a typed response request",
            )),
        };
        if result.is_err() {
            self.record_response_audit(operation, call, ResponseAuditOutcome::ProviderFailed);
        }
        result
    }

    fn finish_agent_poll(
        &mut self,
        pending_id: PendingEffectId,
        poll: AgentProviderPoll,
        pending: PendingAgent,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<allen_vm::EffectPoll, VmError> {
        let operation = pending_agent_operation(&pending);
        let result = self.finish_agent_poll_unwrapped(pending_id, poll, pending, cancellation);
        Self::close_expected_poll(operation, result)
    }

    fn finish_agent_poll_unwrapped(
        &mut self,
        pending_id: PendingEffectId,
        mut poll: AgentProviderPoll,
        mut pending: PendingAgent,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<allen_vm::EffectPoll, VmError> {
        loop {
            let PendingAgent::Ask {
                call,
                result_type,
                operation,
                ..
            } = &mut pending
            else {
                return agent_provider_value(
                    poll,
                    &pending,
                    self.transcript_bytes,
                    &self.enum_types,
                )
                .map_err(|_| provider_poll_protocol_error())
                .map(allen_vm::EffectPoll::Ready);
            };
            let AgentProviderPoll::Ask(response) = poll else {
                self.record_response_audit(
                    *operation,
                    call,
                    ResponseAuditOutcome::ValidationFailed,
                );
                return Err(provider_poll_protocol_error());
            };
            if let Ok(value) = json_to_value(&response, result_type, &self.enum_types) {
                self.record_response_audit(*operation, call, ResponseAuditOutcome::Valid);
                return Ok(allen_vm::EffectPoll::Ready(value));
            }
            let max_attempts = match &call.prompt {
                PromptPayload::Text(_) => 1,
                PromptPayload::Structured(prompt) => {
                    prompt.max_attempts.min(self.maximum_response_attempts)
                }
            };
            if call.attempt >= max_attempts {
                self.record_response_audit(
                    *operation,
                    call,
                    ResponseAuditOutcome::ValidationFailed,
                );
                return Err(validation_error_for(*operation));
            }
            call.validation_issues =
                exact_validation_issues(&response, result_type, &self.enum_types);
            call.attempt += 1;
            let mut signal = AgentCancellationBridge {
                source: cancellation,
            };
            poll = self.start_response_provider(pending_id, *operation, call, &mut signal)?;
            if signal.is_cancelled() {
                self.cancel_external_pending(pending_id, *operation, call.operation_id);
                self.record_response_audit(*operation, call, ResponseAuditOutcome::Cancelled);
                return Err(VmError::Cancelled);
            }
            if matches!(poll, AgentProviderPoll::Pending) {
                self.pending_agents.insert(pending_id, pending);
                return Ok(allen_vm::EffectPoll::Pending);
            }
        }
    }

    fn cancel_external_pending(
        &mut self,
        pending: PendingEffectId,
        operation: FsOperation,
        operation_id: u64,
    ) {
        match operation {
            FsOperation::ModelRequest => {
                if let Some(provider) = self.model.as_deref_mut() {
                    provider.cancel(pending, self.execution_id, operation_id);
                }
            }
            FsOperation::UserAsk => {
                if let Some(provider) = self.user.as_deref_mut() {
                    provider.cancel(pending, self.execution_id, operation_id);
                }
            }
            _ => {
                if let Some(provider) = self.invoking_agent.as_deref_mut() {
                    provider.cancel_pending(pending, self.execution_id, operation_id);
                }
            }
        }
    }

    fn record_response_audit(
        &mut self,
        operation: FsOperation,
        call: &AgentAskCall,
        outcome: ResponseAuditOutcome,
    ) {
        let (provider_kind, identity) = match operation {
            FsOperation::AgentAsk => (
                ResponseProviderKind::InvokingAgent,
                self.invoking_agent
                    .as_deref()
                    .map_or("invoking-agent", InvokingAgentProvider::identity),
            ),
            FsOperation::ModelRequest => (
                ResponseProviderKind::Model,
                self.model
                    .as_deref()
                    .map_or("model", ResponseProvider::identity),
            ),
            FsOperation::UserAsk => (
                ResponseProviderKind::User,
                self.user
                    .as_deref()
                    .map_or("user", ResponseProvider::identity),
            ),
            _ => return,
        };
        let provider_identity = truncate_utf8(identity, 256).to_owned();
        self.response_audit.push(ResponseAuditRecord {
            provider_kind,
            provider_identity,
            schema_digest: call.response_schema.digest.clone(),
            attempts: call.attempt,
            outcome,
        });
    }
}

fn tool_outcome_value(
    outcome: ToolOutcome,
    pending: &PendingTool,
    schemas: &[allen_bytecode::StrictSchema],
    enums: &[allen_bytecode::EnumType],
) -> Result<Value, VmError> {
    let (json, schema, value_type, variant) = match outcome {
        ToolOutcome::Output(value) => (
            value,
            &pending.definition.output_schema,
            &schemas[pending.contract.output_schema as usize].value_type,
            0,
        ),
        ToolOutcome::DeclaredError(value) => (
            value,
            &pending.definition.error_schema,
            &schemas[pending.contract.error_schema as usize].value_type,
            1,
        ),
    };
    schema
        .validate(&json, &SchemaLimits::default())
        .map_err(|_| VmError::ToolSchemaError)?;
    let value = tool_json_to_value(&json, schema.descriptor(), value_type, enums)
        .map_err(|_| VmError::ToolSchemaError)?;
    if variant == 1 {
        if let Some(error_enum) = pending.error_enum {
            return generated_tool_error(
                error_enum,
                0,
                EnumPayload::Tuple(vec![value].into()),
                enums,
            )
            .map(|error| result_value(1, error));
        }
    }
    Ok(result_value(variant, value))
}

fn tool_error_enum(
    result_type: &ValueType,
    schemas: &[allen_bytecode::StrictSchema],
    enums: &[allen_bytecode::EnumType],
    tool: u32,
    contracts: &[ToolContract],
) -> Result<Option<u32>, VmError> {
    let contract = contracts
        .get(tool as usize)
        .ok_or(VmError::ToolUnavailable)?;
    let ValueType::Result(output, error) = result_type else {
        return Err(VmError::Invariant("tool result type"));
    };
    if output.as_ref()
        != &schemas
            .get(contract.output_schema as usize)
            .ok_or(VmError::ToolSchemaError)?
            .value_type
    {
        return Err(VmError::Invariant("tool output type"));
    }
    let ValueType::Enum(error_enum) = error.as_ref() else {
        return Err(VmError::Invariant("generated tool error type"));
    };
    let enum_type = enums
        .get(*error_enum as usize)
        .ok_or(VmError::Invariant("generated tool error enum"))?;
    let declared = &schemas
        .get(contract.error_schema as usize)
        .ok_or(VmError::ToolSchemaError)?
        .value_type;
    let ValueType::Record(standard_fields) = allen_bytecode::standard_error_type() else {
        unreachable!("standard error type is a record")
    };
    let valid = enum_type.variants.as_slice()
        == [
            allen_bytecode::EnumVariant {
                name: "Declared".to_owned(),
                payload: allen_bytecode::EnumPayloadType::Tuple(vec![declared.clone()]),
            },
            allen_bytecode::EnumVariant {
                name: "Unavailable".to_owned(),
                payload: allen_bytecode::EnumPayloadType::Record(standard_fields.clone()),
            },
            allen_bytecode::EnumVariant {
                name: "Schema".to_owned(),
                payload: allen_bytecode::EnumPayloadType::Record(standard_fields),
            },
        ];
    if !valid {
        return Err(VmError::Invariant("generated tool error layout"));
    }
    Ok(Some(*error_enum))
}

fn generated_tool_error(
    error_enum: u32,
    variant: u32,
    payload: EnumPayload,
    enums: &[allen_bytecode::EnumType],
) -> Result<Value, VmError> {
    let enum_type = enums
        .get(error_enum as usize)
        .ok_or(VmError::Invariant("generated tool error enum"))?;
    let variant_type = enum_type
        .variants
        .get(variant as usize)
        .ok_or(VmError::Invariant("generated tool error variant"))?;
    Ok(Value::Enum(std::rc::Rc::new(EnumValue {
        identity: EnumIdentity::User(error_enum),
        type_name: enum_type.name.clone().into(),
        variant_name: variant_type.name.clone().into(),
        variant,
        payload,
    })))
}

fn close_tool_poll(
    result: Result<allen_vm::EffectPoll, VmError>,
    error_enum: Option<u32>,
    enums: &[allen_bytecode::EnumType],
) -> Result<allen_vm::EffectPoll, VmError> {
    let Some(error_enum) = error_enum else {
        return result;
    };
    match result {
        Ok(poll) => Ok(poll),
        Err(VmError::ToolUnavailable) => generated_tool_operational_error(
            error_enum,
            1,
            "tool.unavailable",
            "the tool provider is unavailable",
            enums,
        ),
        Err(VmError::CapabilityMissing) => generated_tool_operational_error(
            error_enum,
            1,
            "tool.denied",
            "the tool operation was denied",
            enums,
        ),
        Err(VmError::ToolSchemaError) => generated_tool_operational_error(
            error_enum,
            2,
            "tool.schema",
            "the tool response failed schema validation",
            enums,
        ),
        Err(error) => Err(error),
    }
}

fn generated_tool_operational_error(
    error_enum: u32,
    variant: u32,
    code: &str,
    message: &str,
    enums: &[allen_bytecode::EnumType],
) -> Result<allen_vm::EffectPoll, VmError> {
    let payload = EnumPayload::Record(
        vec![
            ("code".into(), Value::String(code.into())),
            ("message".into(), Value::String(message.into())),
        ]
        .into(),
    );
    generated_tool_error(error_enum, variant, payload, enums)
        .map(|error| allen_vm::EffectPoll::Ready(result_value(1, error)))
}

fn capability_nonce(generation: u64, index: u32) -> u64 {
    static NONCE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    generation.hash(&mut hasher);
    index.hash(&mut hasher);
    NONCE_SEQUENCE
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    hasher.finish()
}

fn record_field<'value>(
    fields: &'value [(std::rc::Rc<str>, Value)],
    name: &str,
) -> Result<&'value Value, VmError> {
    fields
        .iter()
        .find(|(field, _)| field.as_ref() == name)
        .map(|(_, value)| value)
        .ok_or(VmError::Invariant("external permission request field"))
}

fn projection_names(value: &Value) -> Result<BTreeSet<String>, VmError> {
    let Value::List(values) = value else {
        return Err(VmError::Invariant("sub-agent projection names type"));
    };
    let mut previous: Option<&str> = None;
    let mut names = BTreeSet::new();
    for value in values.iter() {
        let Value::String(name) = value else {
            return Err(VmError::Invariant("sub-agent projection name type"));
        };
        if previous.is_some_and(|previous| previous >= name.as_ref()) {
            return Err(VmError::CapabilityMissing);
        }
        previous = Some(name);
        names.insert(name.to_string());
    }
    Ok(names)
}

fn validate_sub_agent_context(
    prompt: &StructuredPrompt,
    maximum_bytes: usize,
) -> Result<(), VmError> {
    let Some(context) = &prompt.context else {
        return Ok(());
    };
    if allen_schema::canonical_json(context).len() > maximum_bytes {
        return Err(VmError::ResourceLimit {
            resource: "sub_agent_context_bytes",
        });
    }
    Ok(())
}

fn sub_agent_host_error(error: SubAgentHostError) -> VmError {
    match error {
        SubAgentHostError::Unavailable | SubAgentHostError::Transport => {
            VmError::SubAgentUnavailable
        }
        SubAgentHostError::Rejected => VmError::CapabilityMissing,
        SubAgentHostError::Cancelled => VmError::Cancelled,
        SubAgentHostError::Timeout => VmError::SubAgentUnavailable,
        SubAgentHostError::InvalidOutcome => VmError::ProtocolViolation,
    }
}

const fn access_rights(access: ExternalFsAccess) -> Rights {
    match access {
        ExternalFsAccess::Read => Rights::READ_ONLY,
        ExternalFsAccess::Write => Rights::new(false, true),
        ExternalFsAccess::ReadWrite => Rights::READ_WRITE,
    }
}

const fn grant_max_bytes(limits: WorkspaceLimits, rights: Rights) -> u64 {
    let mut maximum = limits.max_file_bytes;
    if rights.read && limits.max_read_bytes < maximum {
        maximum = limits.max_read_bytes;
    }
    if rights.write && limits.max_write_bytes < maximum {
        maximum = limits.max_write_bytes;
    }
    maximum
}

const fn grant_limits(mut limits: WorkspaceLimits, max_bytes: u64) -> WorkspaceLimits {
    if max_bytes < limits.max_file_bytes {
        limits.max_file_bytes = max_bytes;
    }
    if max_bytes < limits.max_read_bytes {
        limits.max_read_bytes = max_bytes;
    }
    if max_bytes < limits.max_write_bytes {
        limits.max_write_bytes = max_bytes;
    }
    limits
}

const TRANSCRIPT_TEXT_LIMIT: usize = 1024;

fn validate_transcript(
    snapshot: &TranscriptSnapshot,
    limit: u8,
    maximum_bytes: usize,
) -> Result<(), VmError> {
    if snapshot.snapshot_id.is_empty()
        || snapshot.session_id.is_empty()
        || snapshot.policy_version.is_empty()
        || !transcript_text_is_bounded(&snapshot.snapshot_id)
        || !transcript_text_is_bounded(&snapshot.session_id)
        || !transcript_text_is_bounded(&snapshot.policy_version)
        || !canonical_utc_timestamp(&snapshot.captured_at)
        || snapshot.messages.len() > usize::from(limit)
    {
        return Err(VmError::AgentResponseSchema);
    }
    let mut previous_time = None;
    for message in &snapshot.messages {
        if let Some(time) = message.time.as_deref() {
            let key = timestamp_sort_key(time).ok_or(VmError::AgentResponseSchema)?;
            if previous_time
                .as_ref()
                .is_some_and(|previous| previous > &key)
            {
                return Err(VmError::AgentResponseSchema);
            }
            previous_time = Some(key);
        }
        if message
            .id
            .as_deref()
            .is_some_and(|value| !transcript_text_is_bounded(value))
            || message
                .time
                .as_deref()
                .is_some_and(|value| !canonical_utc_timestamp(value))
        {
            return Err(VmError::AgentResponseSchema);
        }
        for part in &message.content {
            let valid = match part {
                TranscriptPart::Text { text } => text.len() <= maximum_bytes,
                TranscriptPart::Json { .. } => true,
                TranscriptPart::ToolCall { name, call_id, .. } => {
                    transcript_text_is_bounded(name) && transcript_text_is_bounded(call_id)
                }
                TranscriptPart::ToolResult { call_id, .. } => transcript_text_is_bounded(call_id),
                TranscriptPart::Attachment {
                    media_type,
                    name,
                    content_ref,
                } => {
                    transcript_text_is_bounded(media_type)
                        && name.as_deref().is_none_or(transcript_text_is_bounded)
                        && content_ref
                            .as_deref()
                            .is_none_or(transcript_text_is_bounded)
                }
                TranscriptPart::Redacted { reason_code } => transcript_text_is_bounded(reason_code),
                TranscriptPart::Omitted {
                    content_kind,
                    count,
                } => *count > 0 && transcript_text_is_bounded(content_kind),
            };
            if !valid {
                return Err(VmError::AgentResponseSchema);
            }
        }
    }
    let bytes =
        serde_json::to_vec(&transcript_json(snapshot)).map_err(|_| VmError::AgentResponseSchema)?;
    if bytes.len() > maximum_bytes {
        return Err(VmError::ResourceLimit {
            resource: "transcript_bytes",
        });
    }
    Ok(())
}

fn transcript_text_is_bounded(value: &str) -> bool {
    !value.is_empty() && value.len() <= TRANSCRIPT_TEXT_LIMIT
}

fn canonical_utc_timestamp(value: &str) -> bool {
    let Some((date, time)) = value.split_once('T') else {
        return false;
    };
    let Some(time) = time.strip_suffix('Z') else {
        return false;
    };
    let Some((year, month, day)) = parse_date(date) else {
        return false;
    };
    let (clock, fractional) = time
        .split_once('.')
        .map_or((time, None), |(clock, fraction)| (clock, Some(fraction)));
    if fractional.is_some_and(|fraction| {
        fraction.is_empty()
            || fraction.len() > 9
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return false;
    }
    let mut fields = clock.split(':');
    let (Some(hour), Some(minute), Some(second), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    let (Some(hour), Some(minute), Some(second)) = (
        parse_two_digits(hour),
        parse_two_digits(minute),
        parse_two_digits(second),
    ) else {
        return false;
    };
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return false,
    };
    day >= 1 && day <= month_days && hour < 24 && minute < 60 && second < 60
}

fn timestamp_sort_key(value: &str) -> Option<(u32, u32, u32, u8, u8, u8, u32)> {
    if !canonical_utc_timestamp(value) {
        return None;
    }
    let (year, month, day) = parse_date(&value[..10])?;
    let hour = parse_two_digits(&value[11..13])?;
    let minute = parse_two_digits(&value[14..16])?;
    let second = parse_two_digits(&value[17..19])?;
    let nanos = if value.len() == 20 {
        0
    } else {
        let fraction = &value[20..value.len() - 1];
        fraction.parse::<u32>().ok()? * 10_u32.pow(u32::try_from(9 - fraction.len()).ok()?)
    };
    Some((year, month, day, hour, minute, second, nanos))
}

fn parse_date(value: &str) -> Option<(u32, u32, u32)> {
    let mut fields = value.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return None;
    };
    if year.len() != 4 || !year.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((
        year.parse().ok()?,
        u32::from(parse_two_digits(month)?),
        u32::from(parse_two_digits(day)?),
    ))
}

fn parse_two_digits(value: &str) -> Option<u8> {
    (value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn transcript_json(snapshot: &TranscriptSnapshot) -> serde_json::Value {
    let messages = snapshot
        .messages
        .iter()
        .map(|message| {
            serde_json::json!({
                "id": message.id,
                "role": transcript_role_name(message.role),
                "time": message.time,
                "content": message.content.iter().map(transcript_part_json).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "snapshot_id": snapshot.snapshot_id,
        "session_id": snapshot.session_id,
        "policy_version": snapshot.policy_version,
        "captured_at": snapshot.captured_at,
        "truncated": snapshot.truncated,
        "messages": messages,
    })
}

const fn transcript_role_name(role: TranscriptRole) -> &'static str {
    match role {
        TranscriptRole::User => "user",
        TranscriptRole::Assistant => "assistant",
        TranscriptRole::SystemVisible => "system_visible",
        TranscriptRole::Tool => "tool",
    }
}

fn transcript_part_json(part: &TranscriptPart) -> serde_json::Value {
    match part {
        TranscriptPart::Text { text } => serde_json::json!({"kind":"text", "text":text}),
        TranscriptPart::Json { value } => serde_json::json!({"kind":"json", "value":value}),
        TranscriptPart::ToolCall {
            name,
            call_id,
            input,
        } => {
            serde_json::json!({"kind":"tool_call", "name":name, "call_id":call_id, "input":input})
        }
        TranscriptPart::ToolResult {
            call_id,
            output,
            is_error,
        } => serde_json::json!({
            "kind":"tool_result", "call_id":call_id, "output":output, "is_error":is_error,
        }),
        TranscriptPart::Attachment {
            media_type,
            name,
            content_ref,
        } => serde_json::json!({
            "kind":"attachment", "media_type":media_type, "name":name, "content_ref":content_ref,
        }),
        TranscriptPart::Redacted { reason_code } => {
            serde_json::json!({"kind":"redacted", "reason_code":reason_code})
        }
        TranscriptPart::Omitted {
            content_kind,
            count,
        } => {
            serde_json::json!({"kind":"omitted", "content_kind":content_kind, "count":count})
        }
    }
}

const fn agent_host_error(error: AgentHostError) -> VmError {
    match error {
        AgentHostError::Unavailable | AgentHostError::Transport | AgentHostError::Timeout => {
            VmError::AgentUnavailable
        }
        AgentHostError::Rejected => VmError::CapabilityMissing,
        AgentHostError::Cancelled => VmError::Cancelled,
        AgentHostError::InvalidOutcome => VmError::ProtocolViolation,
    }
}

const AGENT_UNAVAILABLE_CODE: &str = "agent.unavailable";
const AGENT_DENIED_CODE: &str = "agent.denied";
const AGENT_VALIDATION_CODE: &str = "agent.validation_failed";
const MODEL_UNAVAILABLE_CODE: &str = "model.unavailable";
const MODEL_DENIED_CODE: &str = "model.denied";
const MODEL_VALIDATION_CODE: &str = "model.validation_failed";
const USER_UNAVAILABLE_CODE: &str = "user.unavailable";
const USER_DENIED_CODE: &str = "user.denied";
const USER_VALIDATION_CODE: &str = "user.validation_failed";
const SUB_AGENT_UNAVAILABLE_CODE: &str = "sub_agent.unavailable";
const SUB_AGENT_DENIED_CODE: &str = "sub_agent.denied";
const SUB_AGENT_VALIDATION_CODE: &str = "sub_agent.validation_failed";

fn expected_provider_error(
    operation: FsOperation,
    error: &VmError,
) -> Option<(&'static str, &'static str)> {
    match (operation, error) {
        (
            FsOperation::AgentMessage | FsOperation::AgentAsk | FsOperation::AgentTranscript,
            VmError::AgentUnavailable,
        ) => Some((AGENT_UNAVAILABLE_CODE, "the invoking agent is unavailable")),
        (
            FsOperation::AgentMessage | FsOperation::AgentAsk | FsOperation::AgentTranscript,
            VmError::CapabilityMissing,
        ) => Some((AGENT_DENIED_CODE, "the invoking-agent operation was denied")),
        (FsOperation::AgentAsk, VmError::AgentResponseSchema) => Some((
            AGENT_VALIDATION_CODE,
            "the agent response failed validation",
        )),
        (FsOperation::ModelRequest, VmError::ModelUnavailable) => {
            Some((MODEL_UNAVAILABLE_CODE, "the model provider is unavailable"))
        }
        (FsOperation::ModelRequest, VmError::CapabilityMissing) => {
            Some((MODEL_DENIED_CODE, "the model operation was denied"))
        }
        (FsOperation::ModelRequest, VmError::ModelValidationError) => Some((
            MODEL_VALIDATION_CODE,
            "the model response failed validation",
        )),
        (FsOperation::UserAsk, VmError::UserUnavailable) => {
            Some((USER_UNAVAILABLE_CODE, "the user provider is unavailable"))
        }
        (FsOperation::UserAsk, VmError::CapabilityMissing) => {
            Some((USER_DENIED_CODE, "the user operation was denied"))
        }
        (FsOperation::UserAsk, VmError::ResponseValidationError) => {
            Some((USER_VALIDATION_CODE, "the user response failed validation"))
        }
        (
            FsOperation::SubAgentCreate
            | FsOperation::SubAgentRun
            | FsOperation::SubAgentMessage
            | FsOperation::SubAgentAsk,
            VmError::SubAgentUnavailable,
        ) => Some((
            SUB_AGENT_UNAVAILABLE_CODE,
            "the sub-agent provider is unavailable",
        )),
        (
            FsOperation::SubAgentCreate
            | FsOperation::SubAgentRun
            | FsOperation::SubAgentMessage
            | FsOperation::SubAgentAsk,
            VmError::CapabilityMissing,
        ) => Some((SUB_AGENT_DENIED_CODE, "the sub-agent operation was denied")),
        (FsOperation::SubAgentRun | FsOperation::SubAgentAsk, VmError::SubAgentResponseSchema) => {
            Some((
                SUB_AGENT_VALIDATION_CODE,
                "the sub-agent response failed validation",
            ))
        }
        _ => None,
    }
}

const fn unavailable_for(operation: FsOperation) -> VmError {
    match operation {
        FsOperation::ModelRequest => VmError::ModelUnavailable,
        FsOperation::UserAsk => VmError::UserUnavailable,
        FsOperation::SubAgentCreate
        | FsOperation::SubAgentRun
        | FsOperation::SubAgentMessage
        | FsOperation::SubAgentAsk => VmError::SubAgentUnavailable,
        _ => VmError::AgentUnavailable,
    }
}

const fn validation_error_for(operation: FsOperation) -> VmError {
    match operation {
        FsOperation::ModelRequest => VmError::ModelValidationError,
        FsOperation::UserAsk => VmError::ResponseValidationError,
        FsOperation::SubAgentRun | FsOperation::SubAgentAsk => VmError::SubAgentResponseSchema,
        _ => VmError::AgentResponseSchema,
    }
}

const fn provider_poll_protocol_error() -> VmError {
    VmError::ProtocolViolation
}

const fn response_host_error(operation: FsOperation, error: ResponseHostError) -> VmError {
    match error {
        ResponseHostError::Cancelled => VmError::Cancelled,
        ResponseHostError::Timeout
        | ResponseHostError::Unavailable
        | ResponseHostError::Transport => unavailable_for(operation),
        ResponseHostError::InvalidOutcome => VmError::ProtocolViolation,
        ResponseHostError::Rejected => VmError::CapabilityMissing,
    }
}

fn response_poll_to_agent(poll: ResponseProviderPoll) -> AgentProviderPoll {
    match poll {
        ResponseProviderPoll::Response(value) => AgentProviderPoll::Ask(value),
        ResponseProviderPoll::Pending => AgentProviderPoll::Pending,
    }
}

const fn tool_host_error(error: ToolHostError) -> VmError {
    match error {
        ToolHostError::Cancelled => VmError::Cancelled,
        ToolHostError::Timeout | ToolHostError::Unavailable | ToolHostError::Transport => {
            VmError::ToolUnavailable
        }
        ToolHostError::InvalidOutcome => VmError::ToolSchemaError,
        ToolHostError::Rejected => VmError::CapabilityMissing,
    }
}

fn agent_provider_value(
    poll: AgentProviderPoll,
    pending: &PendingAgent,
    transcript_bytes: usize,
    enums: &[allen_bytecode::EnumType],
) -> Result<Value, VmError> {
    match (poll, pending) {
        (AgentProviderPoll::Message(true), PendingAgent::Message { .. }) => Ok(Value::Unit),
        (
            AgentProviderPoll::Transcript(snapshot),
            PendingAgent::Transcript {
                limit, result_type, ..
            },
        ) => {
            validate_transcript(&snapshot, *limit, transcript_bytes)?;
            transcript_to_value(&snapshot, result_type, enums)
        }
        _ => Err(VmError::AgentResponseSchema),
    }
}

fn transcript_to_value(
    snapshot: &TranscriptSnapshot,
    result_type: &ValueType,
    enums: &[allen_bytecode::EnumType],
) -> Result<Value, VmError> {
    let part_id = enums
        .iter()
        .position(|enum_type| enum_type == &allen_bytecode::transcript_part_enum_type())
        .and_then(|index| u32::try_from(index).ok())
        .ok_or(VmError::AgentResponseSchema)?;
    if result_type != &allen_bytecode::transcript_snapshot_type(part_id) {
        return Err(VmError::AgentResponseSchema);
    }
    let messages = snapshot
        .messages
        .iter()
        .map(|message| transcript_message_to_value(message, part_id, enums))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Record(
        vec![
            (
                "captured_at".into(),
                Value::String(snapshot.captured_at.clone().into()),
            ),
            ("messages".into(), Value::List(messages.into())),
            (
                "policy_version".into(),
                Value::String(snapshot.policy_version.clone().into()),
            ),
            (
                "session_id".into(),
                Value::String(snapshot.session_id.clone().into()),
            ),
            (
                "snapshot_id".into(),
                Value::String(snapshot.snapshot_id.clone().into()),
            ),
            ("truncated".into(), Value::Bool(snapshot.truncated)),
        ]
        .into(),
    ))
}

fn transcript_message_to_value(
    message: &TranscriptMessage,
    part_id: u32,
    enums: &[allen_bytecode::EnumType],
) -> Result<Value, VmError> {
    let content = message
        .content
        .iter()
        .map(|part| transcript_part_to_value(part, part_id, enums))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Record(
        vec![
            ("content".into(), Value::List(content.into())),
            ("id".into(), option_string(message.id.as_deref())),
            (
                "role".into(),
                Value::String(transcript_role_name(message.role).into()),
            ),
            ("time".into(), option_string(message.time.as_deref())),
        ]
        .into(),
    ))
}

fn transcript_part_to_value(
    part: &TranscriptPart,
    part_id: u32,
    enums: &[allen_bytecode::EnumType],
) -> Result<Value, VmError> {
    let (name, fields) = match part {
        TranscriptPart::Text { text } => {
            ("Text", vec![("text", Value::String(text.clone().into()))])
        }
        TranscriptPart::Json { value } => (
            "Json",
            vec![(
                "value",
                Value::Unknown(std::rc::Rc::new(json_unknown_value(value)?)),
            )],
        ),
        TranscriptPart::ToolCall {
            name,
            call_id,
            input,
        } => (
            "ToolCall",
            vec![
                ("call_id", Value::String(call_id.clone().into())),
                ("input", option_unknown(input.as_ref())?),
                ("name", Value::String(name.clone().into())),
            ],
        ),
        TranscriptPart::ToolResult {
            call_id,
            output,
            is_error,
        } => (
            "ToolResult",
            vec![
                ("call_id", Value::String(call_id.clone().into())),
                ("is_error", Value::Bool(*is_error)),
                ("output", option_unknown(output.as_ref())?),
            ],
        ),
        TranscriptPart::Attachment {
            media_type,
            name,
            content_ref,
        } => (
            "Attachment",
            vec![
                ("content_ref", option_string(content_ref.as_deref())),
                ("media_type", Value::String(media_type.clone().into())),
                ("name", option_string(name.as_deref())),
            ],
        ),
        TranscriptPart::Redacted { reason_code } => (
            "Redacted",
            vec![("reason_code", Value::String(reason_code.clone().into()))],
        ),
        TranscriptPart::Omitted {
            content_kind,
            count,
        } => (
            "Omitted",
            vec![
                ("content_kind", Value::String(content_kind.clone().into())),
                ("count", Value::Int(i64::from(*count))),
            ],
        ),
    };
    let enum_type = enums
        .get(part_id as usize)
        .ok_or(VmError::AgentResponseSchema)?;
    let (variant, variant_type) = enum_type
        .variants
        .iter()
        .enumerate()
        .find(|(_, variant)| variant.name == name)
        .ok_or(VmError::AgentResponseSchema)?;
    let allen_bytecode::EnumPayloadType::Record(expected) = &variant_type.payload else {
        return Err(VmError::AgentResponseSchema);
    };
    if expected.len() != fields.len()
        || expected
            .iter()
            .zip(&fields)
            .any(|(expected, (actual, _))| expected.name != *actual)
    {
        return Err(VmError::AgentResponseSchema);
    }
    Ok(Value::Enum(std::rc::Rc::new(EnumValue {
        identity: EnumIdentity::User(part_id),
        type_name: enum_type.name.clone().into(),
        variant: u32::try_from(variant).map_err(|_| VmError::AgentResponseSchema)?,
        variant_name: name.into(),
        payload: EnumPayload::Record(
            fields
                .into_iter()
                .map(|(name, value)| (name.into(), value))
                .collect::<Vec<_>>()
                .into(),
        ),
    })))
}

fn option_string(value: Option<&str>) -> Value {
    option_value(value.map(|value| Value::String(value.into())))
}

fn option_unknown(value: Option<&serde_json::Value>) -> Result<Value, VmError> {
    Ok(option_value(
        value
            .map(json_unknown_value)
            .transpose()?
            .map(|value| Value::Unknown(std::rc::Rc::new(value))),
    ))
}

fn option_value(value: Option<Value>) -> Value {
    let (variant, payload) = match value {
        Some(value) => (1, EnumPayload::Tuple(vec![value].into())),
        None => (0, EnumPayload::Unit),
    };
    Value::Enum(std::rc::Rc::new(EnumValue {
        identity: EnumIdentity::Option,
        type_name: "Option".into(),
        variant,
        variant_name: if variant == 0 { "None" } else { "Some" }.into(),
        payload,
    }))
}

fn json_unknown_value(value: &serde_json::Value) -> Result<Value, VmError> {
    match value {
        serde_json::Value::Null => Ok(Value::Unit),
        serde_json::Value::Bool(value) => Ok(Value::Bool(*value)),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(Value::Int)
            .or_else(|| {
                value
                    .as_f64()
                    .map(|value| Value::Float(allen_vm::FloatValue::new(value)))
            })
            .ok_or(VmError::AgentResponseSchema),
        serde_json::Value::String(value) => Ok(Value::String(value.clone().into())),
        serde_json::Value::Array(values) => Ok(Value::List(
            values
                .iter()
                .map(json_unknown_value)
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        )),
        serde_json::Value::Object(values) => Ok(Value::Record(
            values
                .iter()
                .map(|(name, value)| Ok((name.clone().into(), json_unknown_value(value)?)))
                .collect::<Result<Vec<_>, VmError>>()?
                .into(),
        )),
    }
}

#[allow(clippy::too_many_lines)]
fn unknown_value_to_json(value: &Value) -> Result<serde_json::Value, VmError> {
    match value {
        Value::Unit => Ok(serde_json::Value::Null),
        Value::Bool(value) => Ok((*value).into()),
        Value::Int(value) => Ok((*value).into()),
        Value::Float(value) => {
            let value = value.as_f64();
            serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .map_or_else(
                    || {
                        Ok(serde_json::Value::String(
                            if value.is_nan() {
                                "NaN"
                            } else if value.is_sign_positive() {
                                "Infinity"
                            } else {
                                "-Infinity"
                            }
                            .to_owned(),
                        ))
                    },
                    Ok,
                )
        }
        Value::String(value) => Ok(value.as_ref().into()),
        Value::Bytes(value) => Ok(serde_json::json!({
            "$bytes": base64::engine::general_purpose::STANDARD.encode(value.as_ref())
        })),
        Value::List(values) | Value::Tuple(values) => Ok(serde_json::Value::Array(
            values
                .iter()
                .map(unknown_value_to_json)
                .collect::<Result<_, _>>()?,
        )),
        Value::Record(fields) => Ok(serde_json::Value::Object(
            fields
                .iter()
                .map(|(name, value)| Ok((name.to_string(), unknown_value_to_json(value)?)))
                .collect::<Result<_, VmError>>()?,
        )),
        Value::Map(entries) => Ok(serde_json::Value::Array(
            entries
                .iter()
                .map(|(key, value)| {
                    Ok(serde_json::Value::Array(vec![
                        unknown_value_to_json(key)?,
                        unknown_value_to_json(value)?,
                    ]))
                })
                .collect::<Result<_, VmError>>()?,
        )),
        Value::Enum(value) => {
            let mut object = serde_json::Map::new();
            object.insert("tag".to_owned(), value.variant_name.as_ref().into());
            match &value.payload {
                EnumPayload::Unit => {}
                EnumPayload::Tuple(values) if value.identity == EnumIdentity::Option => {
                    let [payload] = values.as_ref() else {
                        return Err(VmError::AgentResponseSchema);
                    };
                    object.insert("value".to_owned(), unknown_value_to_json(payload)?);
                }
                EnumPayload::Tuple(values) if value.identity == EnumIdentity::Result => {
                    let [payload] = values.as_ref() else {
                        return Err(VmError::AgentResponseSchema);
                    };
                    if !matches!(payload, Value::Unit) {
                        object.insert("value".to_owned(), unknown_value_to_json(payload)?);
                    }
                }
                EnumPayload::Tuple(values) => {
                    object.insert(
                        "value".to_owned(),
                        serde_json::Value::Array(
                            values
                                .iter()
                                .map(unknown_value_to_json)
                                .collect::<Result<_, _>>()?,
                        ),
                    );
                }
                EnumPayload::Record(fields) => {
                    object.insert(
                        "value".to_owned(),
                        serde_json::Value::Object(
                            fields
                                .iter()
                                .map(|(name, value)| {
                                    Ok((name.to_string(), unknown_value_to_json(value)?))
                                })
                                .collect::<Result<_, VmError>>()?,
                        ),
                    );
                }
            }
            Ok(serde_json::Value::Object(object))
        }
        Value::Unknown(value) => unknown_value_to_json(value),
        Value::ExternalFsAccess(_)
        | Value::Closure(_)
        | Value::Future(_)
        | Value::Task(_)
        | Value::Workspace(_)
        | Value::SubAgent(_) => Err(VmError::AgentResponseSchema),
    }
}

fn prompt_optional_json_segment(value: &Value) -> Result<Option<serde_json::Value>, VmError> {
    let Value::Enum(option) = value else {
        return Err(VmError::AgentResponseSchema);
    };
    if option.identity != EnumIdentity::Option {
        return Err(VmError::AgentResponseSchema);
    }
    match (option.variant, &option.payload) {
        (0, EnumPayload::Unit) => Ok(None),
        (1, EnumPayload::Tuple(values)) if values.len() == 1 => {
            let Value::Unknown(value) = &values[0] else {
                return Err(VmError::AgentResponseSchema);
            };
            unknown_value_to_json(value).map(Some)
        }
        _ => Err(VmError::AgentResponseSchema),
    }
}

fn prompt_from_value(
    value: &Value,
    value_type: &ValueType,
    enums: &[allen_bytecode::EnumType],
) -> Result<(PromptPayload, ValueType, u32), VmError> {
    if let (Value::String(message), ValueType::String) = (value, value_type) {
        return Ok((
            PromptPayload::Text(message.to_string()),
            ValueType::String,
            1,
        ));
    }
    let output_type = prompt_output_type(value_type)
        .cloned()
        .ok_or(VmError::AgentResponseSchema)?;
    let Value::Record(fields) = value else {
        return Err(VmError::AgentResponseSchema);
    };
    if fields.len() != 5 {
        return Err(VmError::AgentResponseSchema);
    }
    let Value::String(system) = &fields[0].1 else {
        return Err(VmError::AgentResponseSchema);
    };
    if system.is_empty() {
        return Err(VmError::AgentResponseSchema);
    }
    let context = prompt_optional_json_segment(&fields[1].1)?;
    let data = prompt_optional_json_segment(&fields[2].1)?;
    let Value::Enum(output) = &fields[3].1 else {
        return Err(VmError::AgentResponseSchema);
    };
    if output.identity != EnumIdentity::Option
        || output.variant != 0
        || !matches!(output.payload, EnumPayload::Unit)
    {
        return Err(VmError::AgentResponseSchema);
    }
    let Value::Int(max_attempts) = fields[4].1 else {
        return Err(VmError::AgentResponseSchema);
    };
    let max_attempts = u32::try_from(max_attempts)
        .ok()
        .filter(|value| (1..=3).contains(value))
        .ok_or(VmError::AgentResponseSchema)?;
    let _ = enums;
    Ok((
        PromptPayload::Structured(StructuredPrompt {
            system: system.to_string(),
            context,
            data,
            max_attempts,
        }),
        output_type,
        max_attempts,
    ))
}

fn response_schema(
    value_type: &ValueType,
    enums: &[allen_bytecode::EnumType],
    plain_text: bool,
) -> ResponseSchema {
    let digest = if plain_text {
        "allen:string/0.1".to_owned()
    } else {
        use std::fmt::Write as _;

        let bytes = compute_strict_schema_digest(&StrictSchema {
            value_type: value_type.clone(),
        });
        bytes.iter().fold("sha256:".to_owned(), |mut digest, byte| {
            write!(digest, "{byte:02x}").expect("writing to a String cannot fail");
            digest
        })
    };
    ResponseSchema {
        digest,
        descriptor: schema_descriptor(value_type, enums),
    }
}

fn schema_descriptor(
    value_type: &ValueType,
    enums: &[allen_bytecode::EnumType],
) -> serde_json::Value {
    match value_type {
        ValueType::Int => serde_json::json!({"type":"integer"}),
        ValueType::Bool => serde_json::json!({"type":"boolean"}),
        ValueType::Float => serde_json::json!({
            "type":"float",
            "finite":"number",
            "nonfinite":["NaN","Infinity","-Infinity"]
        }),
        ValueType::String => serde_json::json!({"type":"string"}),
        ValueType::Bytes => serde_json::json!({
            "type":"bytes",
            "representation":{"type":"object","exact":true,"field":"$bytes","encoding":"base64"}
        }),
        ValueType::Unit => serde_json::json!({"type":"null"}),
        ValueType::List(item) => {
            serde_json::json!({"type":"array","items":schema_descriptor(item, enums)})
        }
        ValueType::Tuple(items) => {
            serde_json::json!({"type":"tuple","exactLength":true,"items":items.iter().map(|item| schema_descriptor(item, enums)).collect::<Vec<_>>() })
        }
        ValueType::Record(fields) => serde_json::json!({
            "type":"object",
            "additionalProperties":false,
            "required":fields.iter().map(|field| field.name.clone()).collect::<Vec<_>>(),
            "properties":fields.iter().map(|field| (field.name.clone(), schema_descriptor(&field.value_type, enums))).collect::<serde_json::Map<_,_>>()
        }),
        ValueType::Option(item) => {
            serde_json::json!({
                "type":"tagged_union",
                "tag":"tag",
                "exact":true,
                "variants":[
                    {"tag":"None","fields":{}},
                    {"tag":"Some","fields":{"value":schema_descriptor(item, enums)}}
                ]
            })
        }
        ValueType::Result(ok, error) => {
            let fields = |item: &ValueType| {
                if matches!(item, ValueType::Unit) {
                    serde_json::json!({})
                } else {
                    serde_json::json!({"value":schema_descriptor(item, enums)})
                }
            };
            serde_json::json!({
                "type":"tagged_union",
                "tag":"tag",
                "exact":true,
                "variants":[
                    {"tag":"Ok","fields":fields(ok)},
                    {"tag":"Err","fields":fields(error)}
                ]
            })
        }
        ValueType::Enum(id) => {
            let variants = enums
                .get(*id as usize)
                .map(|enum_type| {
                    enum_type
                        .variants
                        .iter()
                        .map(|variant| {
                            let fields = match &variant.payload {
                                allen_bytecode::EnumPayloadType::Unit => serde_json::json!({}),
                                allen_bytecode::EnumPayloadType::Tuple(items) => serde_json::json!({
                                    "value":schema_descriptor(&ValueType::Tuple(items.clone()), enums)
                                }),
                                allen_bytecode::EnumPayloadType::Record(fields) => serde_json::json!({
                                    "value":schema_descriptor(&ValueType::Record(fields.clone()), enums)
                                }),
                            };
                            serde_json::json!({"tag":variant.name,"fields":fields})
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            serde_json::json!({"type":"tagged_union","tag":"tag","exact":true,"variants":variants})
        }
        ValueType::Map(key, value) => {
            serde_json::json!({
                "type":"map",
                "representation":"ordered_pairs",
                "strictlyIncreasingKeys":true,
                "key":schema_descriptor(key, enums),
                "value":schema_descriptor(value, enums)
            })
        }
        _ => serde_json::json!({"type":"unsupported"}),
    }
}

fn truncate_utf8(value: &str, maximum_bytes: usize) -> &str {
    if value.len() <= maximum_bytes {
        return value;
    }
    let mut end = maximum_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn pointer_path(parent: &str, component: &str) -> String {
    format!(
        "{parent}/{}",
        component.replace('~', "~0").replace('/', "~1")
    )
}

#[allow(clippy::too_many_lines)]
fn exact_validation_issues(
    value: &serde_json::Value,
    value_type: &ValueType,
    enums: &[allen_bytecode::EnumType],
) -> Vec<ValidationIssue> {
    fn push(issues: &mut Vec<ValidationIssue>, path: &str, code: &'static str) {
        if issues.len() < 16 {
            issues.push(ValidationIssue {
                path: truncate_utf8(path, 256).to_owned(),
                code: code.to_owned(),
            });
        }
    }

    fn tagged_value<'a>(
        value: &'a serde_json::Value,
        path: &str,
        issues: &mut Vec<ValidationIssue>,
    ) -> Option<(&'a serde_json::Map<String, serde_json::Value>, &'a str)> {
        let Some(object) = value.as_object() else {
            push(issues, path, "type");
            return None;
        };
        let Some(tag) = object.get("tag") else {
            push(issues, &pointer_path(path, "tag"), "required");
            return None;
        };
        let Some(tag) = tag.as_str() else {
            push(issues, &pointer_path(path, "tag"), "type");
            return None;
        };
        Some((object, tag))
    }

    fn visit(
        value: &serde_json::Value,
        value_type: &ValueType,
        path: &str,
        enums: &[allen_bytecode::EnumType],
        issues: &mut Vec<ValidationIssue>,
    ) {
        match (value_type, value) {
            (ValueType::Int, serde_json::Value::Number(number)) => {
                if number.as_i64().is_none() {
                    push(issues, path, "range");
                }
            }
            (ValueType::Bool, serde_json::Value::Bool(_))
            | (ValueType::String, serde_json::Value::String(_))
            | (ValueType::Unit, serde_json::Value::Null) => {}
            (ValueType::Float, serde_json::Value::Number(number)) => {
                if number.as_f64().is_none() {
                    push(issues, path, "range");
                }
            }
            (ValueType::Float, serde_json::Value::String(text))
                if matches!(text.as_str(), "NaN" | "Infinity" | "-Infinity") => {}
            (ValueType::Bytes, serde_json::Value::Object(object)) => {
                if object.len() != 1 {
                    push(issues, path, "fields");
                }
                match object.get("$bytes").and_then(serde_json::Value::as_str) {
                    Some(text)
                        if base64::engine::general_purpose::STANDARD
                            .decode(text)
                            .is_ok_and(|decoded| {
                                base64::engine::general_purpose::STANDARD.encode(decoded) == text
                            }) => {}
                    Some(_) => push(issues, &pointer_path(path, "$bytes"), "encoding"),
                    None => push(issues, &pointer_path(path, "$bytes"), "required"),
                }
            }
            (ValueType::List(item), serde_json::Value::Array(values)) => {
                for (index, value) in values.iter().enumerate() {
                    visit(
                        value,
                        item,
                        &pointer_path(path, &index.to_string()),
                        enums,
                        issues,
                    );
                }
            }
            (ValueType::Tuple(items), serde_json::Value::Array(values)) => {
                if items.len() != values.len() {
                    push(issues, path, "length");
                }
                for (index, (value, item)) in values.iter().zip(items).enumerate() {
                    visit(
                        value,
                        item,
                        &pointer_path(path, &index.to_string()),
                        enums,
                        issues,
                    );
                }
            }
            (ValueType::Record(fields), serde_json::Value::Object(object)) => {
                for field in fields {
                    let field_path = pointer_path(path, &field.name);
                    match object.get(&field.name) {
                        Some(value) => {
                            visit(value, &field.value_type, &field_path, enums, issues);
                        }
                        None => push(issues, &field_path, "required"),
                    }
                }
                for name in object
                    .keys()
                    .filter(|name| !fields.iter().any(|field| &field.name == *name))
                {
                    push(issues, &pointer_path(path, name), "unknown");
                }
            }
            (ValueType::Map(key_type, item_type), serde_json::Value::Array(entries)) => {
                let mut last_key = None;
                for (index, entry) in entries.iter().enumerate() {
                    let entry_path = pointer_path(path, &index.to_string());
                    let Some(pair) = entry.as_array() else {
                        push(issues, &entry_path, "type");
                        continue;
                    };
                    if pair.len() != 2 {
                        push(issues, &entry_path, "length");
                        continue;
                    }
                    visit(
                        &pair[0],
                        key_type,
                        &pointer_path(&entry_path, "0"),
                        enums,
                        issues,
                    );
                    visit(
                        &pair[1],
                        item_type,
                        &pointer_path(&entry_path, "1"),
                        enums,
                        issues,
                    );
                    if let Ok(key) = json_to_value(&pair[0], key_type, enums) {
                        if last_key.as_ref().is_some_and(|previous| {
                            compare_map_keys(previous, &key) != Some(std::cmp::Ordering::Less)
                        }) {
                            push(issues, &pointer_path(&entry_path, "0"), "order");
                        }
                        last_key = Some(key);
                    }
                }
            }
            (ValueType::Option(item), _) => {
                let Some((object, tag)) = tagged_value(value, path, issues) else {
                    return;
                };
                match tag {
                    "None" => {
                        if object.len() != 1 {
                            push(issues, path, "fields");
                        }
                    }
                    "Some" => match object.get("value") {
                        Some(value) => {
                            if object.len() != 2 {
                                push(issues, path, "fields");
                            }
                            visit(value, item, &pointer_path(path, "value"), enums, issues);
                        }
                        None => push(issues, &pointer_path(path, "value"), "required"),
                    },
                    _ => push(issues, &pointer_path(path, "tag"), "tag"),
                }
            }
            (ValueType::Result(ok, error), _) => {
                let Some((object, tag)) = tagged_value(value, path, issues) else {
                    return;
                };
                let item = match tag {
                    "Ok" => ok.as_ref(),
                    "Err" => error.as_ref(),
                    _ => {
                        push(issues, &pointer_path(path, "tag"), "tag");
                        return;
                    }
                };
                if matches!(item, ValueType::Unit) {
                    if object.len() != 1 {
                        push(issues, path, "fields");
                    }
                } else {
                    match object.get("value") {
                        Some(value) => {
                            if object.len() != 2 {
                                push(issues, path, "fields");
                            }
                            visit(value, item, &pointer_path(path, "value"), enums, issues);
                        }
                        None => push(issues, &pointer_path(path, "value"), "required"),
                    }
                }
            }
            (ValueType::Enum(enum_id), _) => {
                let Some((object, tag)) = tagged_value(value, path, issues) else {
                    return;
                };
                let Some(variant) = enums.get(*enum_id as usize).and_then(|enum_type| {
                    enum_type
                        .variants
                        .iter()
                        .find(|variant| variant.name == tag)
                }) else {
                    push(issues, &pointer_path(path, "tag"), "tag");
                    return;
                };
                match &variant.payload {
                    allen_bytecode::EnumPayloadType::Unit => {
                        if object.len() != 1 {
                            push(issues, path, "fields");
                        }
                    }
                    allen_bytecode::EnumPayloadType::Tuple(items) => {
                        let Some(values) =
                            object.get("value").and_then(serde_json::Value::as_array)
                        else {
                            push(issues, &pointer_path(path, "value"), "required");
                            return;
                        };
                        if object.len() != 2 {
                            push(issues, path, "fields");
                        }
                        if values.len() != items.len() {
                            push(issues, &pointer_path(path, "value"), "length");
                        }
                        for (index, (value, item)) in values.iter().zip(items).enumerate() {
                            visit(
                                value,
                                item,
                                &pointer_path(&pointer_path(path, "value"), &index.to_string()),
                                enums,
                                issues,
                            );
                        }
                    }
                    allen_bytecode::EnumPayloadType::Record(fields) => {
                        if object.len() != 2 {
                            push(issues, path, "fields");
                        }
                        match object.get("value") {
                            Some(value) => visit(
                                value,
                                &ValueType::Record(fields.clone()),
                                &pointer_path(path, "value"),
                                enums,
                                issues,
                            ),
                            None => push(issues, &pointer_path(path, "value"), "required"),
                        }
                    }
                }
            }
            _ => push(issues, path, "type"),
        }
    }

    let mut issues = Vec::new();
    visit(value, value_type, "", enums, &mut issues);
    if issues.is_empty() && json_to_value(value, value_type, enums).is_err() {
        push(&mut issues, "", "invalid");
    }
    issues
}

fn file_result(result: Result<Value, FileError>) -> Value {
    match result {
        Ok(value) => ok_result(value),
        Err(error) => error_result(error.code.as_str(), error.message),
    }
}

fn search_match_value(value: SearchMatch) -> Value {
    Value::Record(
        vec![
            ("column".into(), Value::Int(value.column)),
            ("line".into(), Value::Int(value.line)),
            ("path".into(), Value::String(value.path.into())),
            ("text".into(), Value::String(value.text.into())),
        ]
        .into(),
    )
}

const fn file_error_is_resource(code: FileErrorCode) -> bool {
    matches!(
        code,
        FileErrorCode::FileTooLarge
            | FileErrorCode::TooManyEntries
            | FileErrorCode::OperationLimit
            | FileErrorCode::ReadLimit
            | FileErrorCode::WriteLimit
    )
}

const fn http_error_is_resource(code: HttpErrorCode) -> bool {
    matches!(
        code,
        HttpErrorCode::RequestLimit
            | HttpErrorCode::RedirectLimit
            | HttpErrorCode::HeaderLimit
            | HttpErrorCode::CompressedLimit
            | HttpErrorCode::DecodedLimit
            | HttpErrorCode::DecompressionRatio
    )
}

fn ok_result(value: Value) -> Value {
    result_value(0, value)
}

fn error_result(code: &str, message: &str) -> Value {
    result_value(
        1,
        Value::Record(
            vec![
                ("code".into(), Value::String(code.into())),
                ("message".into(), Value::String(message.into())),
            ]
            .into(),
        ),
    )
}

fn result_value(variant: u32, value: Value) -> Value {
    Value::Enum(std::rc::Rc::new(EnumValue {
        identity: EnumIdentity::Result,
        type_name: "Result".into(),
        variant_name: if variant == 0 {
            "Ok".into()
        } else {
            "Err".into()
        },
        variant,
        payload: EnumPayload::Tuple(vec![value].into()),
    }))
}

#[allow(clippy::too_many_lines)]
fn tool_json_to_value(
    value: &serde_json::Value,
    descriptor: &Descriptor,
    ty: &ValueType,
    enums: &[allen_bytecode::EnumType],
) -> Result<Value, RuntimeError> {
    let bad = || {
        RuntimeError::new(
            RuntimeErrorCode::InvalidInput,
            "tool value does not match its descriptor",
        )
    };
    match (descriptor, ty, value) {
        (
            Descriptor::List { items, .. },
            ValueType::List(item_type),
            serde_json::Value::Array(items_json),
        ) => Ok(Value::List(
            items_json
                .iter()
                .map(|item| tool_json_to_value(item, items, item_type, enums))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        )),
        (
            Descriptor::Tuple { items },
            ValueType::Tuple(item_types),
            serde_json::Value::Array(items_json),
        ) if items.len() == item_types.len() && items.len() == items_json.len() => {
            Ok(Value::Tuple(
                items_json
                    .iter()
                    .zip(items)
                    .zip(item_types)
                    .map(|((item, descriptor), ty)| tool_json_to_value(item, descriptor, ty, enums))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ))
        }
        (
            Descriptor::Record { fields },
            ValueType::Record(field_types),
            serde_json::Value::Object(object),
        ) if fields.len() == field_types.len() && fields.len() == object.len() => {
            let values = fields
                .iter()
                .zip(field_types)
                .map(|(field, field_type)| {
                    if field.name != field_type.name {
                        return Err(bad());
                    }
                    Ok((
                        field.name.clone().into(),
                        tool_json_to_value(
                            object.get(&field.name).ok_or_else(bad)?,
                            &field.schema,
                            &field_type.value_type,
                            enums,
                        )?,
                    ))
                })
                .collect::<Result<Vec<_>, RuntimeError>>()?;
            Ok(Value::Record(values.into()))
        }
        (
            Descriptor::StringMap { values },
            ValueType::Map(key_type, value_type),
            serde_json::Value::Object(object),
        ) if matches!(key_type.as_ref(), ValueType::String) => {
            let mut entries = object
                .iter()
                .map(|(key, value)| {
                    Ok((
                        Value::String(key.clone().into()),
                        tool_json_to_value(value, values, value_type, enums)?,
                    ))
                })
                .collect::<Result<Vec<_>, RuntimeError>>()?;
            entries.sort_by(|(left, _), (right, _)| {
                compare_map_keys(left, right).unwrap_or(std::cmp::Ordering::Equal)
            });
            Ok(Value::Map(entries.into()))
        }
        (
            Descriptor::TaggedUnion { variants },
            ValueType::Enum(enum_id),
            serde_json::Value::Object(object),
        ) => {
            let tag = object
                .get("tag")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(bad)?;
            let (variant_index, descriptor_variant) = variants
                .iter()
                .enumerate()
                .find(|(_, variant)| variant.tag == tag)
                .ok_or_else(bad)?;
            let enum_type = enums.get(*enum_id as usize).ok_or_else(bad)?;
            let enum_variant = enum_type.variants.get(variant_index).ok_or_else(bad)?;
            let payload = if descriptor_variant.fields.is_empty() {
                if !matches!(enum_variant.payload, allen_bytecode::EnumPayloadType::Unit) {
                    return Err(bad());
                }
                EnumPayload::Unit
            } else {
                let allen_bytecode::EnumPayloadType::Record(field_types) = &enum_variant.payload
                else {
                    return Err(bad());
                };
                if descriptor_variant.fields.len() != field_types.len()
                    || object.len() != descriptor_variant.fields.len() + 1
                {
                    return Err(bad());
                }
                let fields = descriptor_variant
                    .fields
                    .iter()
                    .zip(field_types)
                    .map(|(field, field_type)| {
                        if field.name != field_type.name {
                            return Err(bad());
                        }
                        Ok((
                            field.name.clone().into(),
                            tool_json_to_value(
                                object.get(&field.name).ok_or_else(bad)?,
                                &field.schema,
                                &field_type.value_type,
                                enums,
                            )?,
                        ))
                    })
                    .collect::<Result<Vec<_>, RuntimeError>>()?;
                EnumPayload::Record(fields.into())
            };
            Ok(Value::Enum(std::rc::Rc::new(EnumValue {
                identity: EnumIdentity::User(*enum_id),
                type_name: enum_type.name.clone().into(),
                variant_name: enum_variant.name.clone().into(),
                variant: u32::try_from(variant_index).map_err(|_| bad())?,
                payload,
            })))
        }
        (
            Descriptor::Unit
            | Descriptor::Bool
            | Descriptor::Int { .. }
            | Descriptor::Float { .. }
            | Descriptor::String { .. },
            _,
            _,
        ) => json_to_value(value, ty, enums),
        _ => Err(bad()),
    }
}

#[allow(clippy::too_many_lines)]
fn tool_value_to_json(
    value: &Value,
    descriptor: &Descriptor,
    ty: &ValueType,
    enums: &[allen_bytecode::EnumType],
) -> Result<serde_json::Value, RuntimeError> {
    let bad = || {
        RuntimeError::new(
            RuntimeErrorCode::Panic,
            "tool value does not match its descriptor",
        )
    };
    match (descriptor, ty, value) {
        (Descriptor::List { items, .. }, ValueType::List(item_type), Value::List(items_value)) => {
            Ok(serde_json::Value::Array(
                items_value
                    .iter()
                    .map(|item| tool_value_to_json(item, items, item_type, enums))
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        }
        (Descriptor::Tuple { items }, ValueType::Tuple(item_types), Value::Tuple(items_value))
            if items.len() == item_types.len() && items.len() == items_value.len() =>
        {
            Ok(serde_json::Value::Array(
                items_value
                    .iter()
                    .zip(items)
                    .zip(item_types)
                    .map(|((item, descriptor), ty)| tool_value_to_json(item, descriptor, ty, enums))
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        }
        (
            Descriptor::Record { fields },
            ValueType::Record(field_types),
            Value::Record(field_values),
        ) if fields.len() == field_types.len() && fields.len() == field_values.len() => {
            let mut object = serde_json::Map::new();
            for ((field, field_type), (name, value)) in
                fields.iter().zip(field_types).zip(field_values.iter())
            {
                if field.name != field_type.name || field.name != name.as_ref() {
                    return Err(bad());
                }
                object.insert(
                    field.name.clone(),
                    tool_value_to_json(value, &field.schema, &field_type.value_type, enums)?,
                );
            }
            Ok(serde_json::Value::Object(object))
        }
        (
            Descriptor::StringMap { values },
            ValueType::Map(key_type, value_type),
            Value::Map(entries),
        ) if matches!(key_type.as_ref(), ValueType::String) => {
            let mut object = serde_json::Map::new();
            let mut previous: Option<&str> = None;
            for (key, value) in entries.iter() {
                let Value::String(key) = key else {
                    return Err(bad());
                };
                if previous.is_some_and(|previous| previous.as_bytes() >= key.as_bytes()) {
                    return Err(bad());
                }
                previous = Some(key);
                object.insert(
                    key.to_string(),
                    tool_value_to_json(value, values, value_type, enums)?,
                );
            }
            Ok(serde_json::Value::Object(object))
        }
        (
            Descriptor::TaggedUnion { variants },
            ValueType::Enum(enum_id),
            Value::Enum(enum_value),
        ) => {
            if enum_value.identity != EnumIdentity::User(*enum_id) {
                return Err(bad());
            }
            let variant_index = usize::try_from(enum_value.variant).map_err(|_| bad())?;
            let descriptor_variant = variants.get(variant_index).ok_or_else(bad)?;
            let enum_type = enums.get(*enum_id as usize).ok_or_else(bad)?;
            let enum_variant = enum_type.variants.get(variant_index).ok_or_else(bad)?;
            if enum_value.type_name.as_ref() != enum_type.name
                || enum_value.variant_name.as_ref() != enum_variant.name
            {
                return Err(bad());
            }
            let mut object = serde_json::Map::new();
            object.insert("tag".to_owned(), descriptor_variant.tag.clone().into());
            match (
                descriptor_variant.fields.as_slice(),
                &enum_variant.payload,
                &enum_value.payload,
            ) {
                ([], allen_bytecode::EnumPayloadType::Unit, EnumPayload::Unit) => {}
                (
                    fields,
                    allen_bytecode::EnumPayloadType::Record(field_types),
                    EnumPayload::Record(field_values),
                ) if fields.len() == field_types.len() && fields.len() == field_values.len() => {
                    for ((field, field_type), (name, value)) in
                        fields.iter().zip(field_types).zip(field_values.iter())
                    {
                        if field.name != field_type.name || field.name != name.as_ref() {
                            return Err(bad());
                        }
                        object.insert(
                            field.name.clone(),
                            tool_value_to_json(
                                value,
                                &field.schema,
                                &field_type.value_type,
                                enums,
                            )?,
                        );
                    }
                }
                _ => return Err(bad()),
            }
            Ok(serde_json::Value::Object(object))
        }
        (
            Descriptor::Unit
            | Descriptor::Bool
            | Descriptor::Int { .. }
            | Descriptor::Float { .. }
            | Descriptor::String { .. },
            _,
            _,
        ) => value_to_json(value, ty, enums),
        _ => Err(bad()),
    }
}

fn json_to_value(
    value: &serde_json::Value,
    ty: &ValueType,
    enums: &[allen_bytecode::EnumType],
) -> Result<Value, RuntimeError> {
    if matches!(
        ty,
        ValueType::Option(_) | ValueType::Result(_, _) | ValueType::Enum(_) | ValueType::Map(_, _)
    ) {
        return tagged_or_map_input(value, ty, enums);
    }
    match (ty, value) {
        (ValueType::Int, serde_json::Value::Number(v)) => v
            .as_i64()
            .map(Value::Int)
            .ok_or_else(|| RuntimeError::new(RuntimeErrorCode::InvalidInput, "expected Int")),
        (ValueType::Bool, serde_json::Value::Bool(v)) => Ok(Value::Bool(*v)),
        (ValueType::String, serde_json::Value::String(v)) => Ok(Value::String(v.clone().into())),
        (ValueType::Float, serde_json::Value::Number(v)) if v.is_f64() => v
            .as_f64()
            .map(|v| Value::Float(allen_vm::FloatValue::new(v)))
            .ok_or_else(|| RuntimeError::new(RuntimeErrorCode::InvalidInput, "expected Float")),
        (ValueType::Float, serde_json::Value::String(v)) => match v.as_str() {
            "NaN" => Ok(Value::Float(allen_vm::FloatValue::new(f64::NAN))),
            "Infinity" => Ok(Value::Float(allen_vm::FloatValue::new(f64::INFINITY))),
            "-Infinity" => Ok(Value::Float(allen_vm::FloatValue::new(f64::NEG_INFINITY))),
            _ => Err(RuntimeError::new(
                RuntimeErrorCode::InvalidInput,
                "expected Float",
            )),
        },
        (ValueType::Bytes, serde_json::Value::Object(o)) if o.len() == 1 => {
            let v = o
                .get("$bytes")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    RuntimeError::new(RuntimeErrorCode::InvalidInput, "expected Bytes")
                })?;
            base64::engine::general_purpose::STANDARD
                .decode(v)
                .and_then(|decoded| {
                    if base64::engine::general_purpose::STANDARD.encode(&decoded) == v {
                        Ok(decoded)
                    } else {
                        Err(base64::DecodeError::InvalidPadding)
                    }
                })
                .map(|v| Value::Bytes(v.into()))
                .map_err(|_| {
                    RuntimeError::new(RuntimeErrorCode::InvalidInput, "expected base64 Bytes")
                })
        }
        (ValueType::Unit, serde_json::Value::Null) => Ok(Value::Unit),
        (ValueType::List(element), serde_json::Value::Array(values)) => Ok(Value::List(
            values
                .iter()
                .map(|v| json_to_value(v, element, enums))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        )),
        (ValueType::Tuple(elements), serde_json::Value::Array(values))
            if elements.len() == values.len() =>
        {
            Ok(Value::Tuple(
                values
                    .iter()
                    .zip(elements)
                    .map(|(v, t)| json_to_value(v, t, enums))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ))
        }
        (ValueType::Record(fields), serde_json::Value::Object(object))
            if fields.len() == object.len()
                && fields.iter().all(|field| object.contains_key(&field.name)) =>
        {
            Ok(Value::Record(
                fields
                    .iter()
                    .map(|field| {
                        Ok((
                            field.name.clone().into(),
                            json_to_value(&object[&field.name], &field.value_type, enums)?,
                        ))
                    })
                    .collect::<Result<Vec<_>, RuntimeError>>()?
                    .into(),
            ))
        }
        _ => Err(RuntimeError::new(
            RuntimeErrorCode::InvalidInput,
            "input does not match schema",
        )),
    }
}
fn value_to_json(
    value: &Value,
    ty: &ValueType,
    enums: &[allen_bytecode::EnumType],
) -> Result<serde_json::Value, RuntimeError> {
    if matches!(
        ty,
        ValueType::Option(_) | ValueType::Result(_, _) | ValueType::Enum(_) | ValueType::Map(_, _)
    ) {
        return tagged_or_map_output(value, ty, enums);
    }
    match (ty, value) {
        (ValueType::Int, Value::Int(v)) => Ok((*v).into()),
        (ValueType::Bool, Value::Bool(v)) => Ok((*v).into()),
        (ValueType::String, Value::String(v)) => Ok(v.as_ref().into()),
        (ValueType::Float, Value::Float(v)) => {
            let f = v.as_f64();
            if f.is_finite() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .ok_or_else(|| RuntimeError::new(RuntimeErrorCode::Panic, "invalid Float"))
            } else {
                Ok(serde_json::Value::String(
                    if f.is_nan() {
                        "NaN"
                    } else if f.is_sign_positive() {
                        "Infinity"
                    } else {
                        "-Infinity"
                    }
                    .into(),
                ))
            }
        }
        (ValueType::Bytes, Value::Bytes(v)) => Ok(
            serde_json::json!({"$bytes":base64::engine::general_purpose::STANDARD.encode(v.as_ref())}),
        ),
        (ValueType::Unit, Value::Unit) => Ok(serde_json::Value::Null),
        (ValueType::List(element), Value::List(values)) => Ok(serde_json::Value::Array(
            values
                .iter()
                .map(|v| value_to_json(v, element, enums))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        (ValueType::Tuple(elements), Value::Tuple(values)) if elements.len() == values.len() => {
            Ok(serde_json::Value::Array(
                values
                    .iter()
                    .zip(elements)
                    .map(|(v, t)| value_to_json(v, t, enums))
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        }
        (ValueType::Record(fields), Value::Record(values)) if fields.len() == values.len() => {
            let mut object = serde_json::Map::new();
            for (field, (name, value)) in fields.iter().zip(values.iter()) {
                if name.as_ref() != field.name {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::Panic,
                        "record does not match schema",
                    ));
                }
                let _ = object.insert(
                    field.name.clone(),
                    value_to_json(value, &field.value_type, enums)?,
                );
            }
            Ok(serde_json::Value::Object(object))
        }
        _ => Err(RuntimeError::new(
            RuntimeErrorCode::Panic,
            "output does not match schema",
        )),
    }
}

#[allow(clippy::too_many_lines)]
fn tagged_or_map_input(
    value: &serde_json::Value,
    ty: &ValueType,
    enums: &[allen_bytecode::EnumType],
) -> Result<Value, RuntimeError> {
    let bad = || {
        RuntimeError::new(
            RuntimeErrorCode::InvalidInput,
            "input does not match schema",
        )
    };
    if let ValueType::Map(k, v) = ty {
        let a = value.as_array().ok_or_else(bad)?;
        let mut out = Vec::new();
        let mut last: Option<Value> = None;
        for p in a {
            let p = p.as_array().ok_or_else(bad)?;
            if p.len() != 2 {
                return Err(bad());
            }
            let key = json_to_value(&p[0], k, enums)?;
            if let Some(previous) = &last {
                if compare_map_keys(previous, &key) != Some(std::cmp::Ordering::Less) {
                    return Err(bad());
                }
            }
            last = Some(key.clone());
            out.push((key, json_to_value(&p[1], v, enums)?));
        }
        return Ok(Value::Map(out.into()));
    }
    let o = value.as_object().ok_or_else(bad)?;
    let tag = o
        .get("tag")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(bad)?;
    if let ValueType::Enum(id) = ty {
        let e = enums.get(*id as usize).ok_or_else(bad)?;
        let (index, variant) = e
            .variants
            .iter()
            .enumerate()
            .find(|(_, variant)| variant.name == tag)
            .ok_or_else(bad)?;
        let payload = match &variant.payload {
            allen_bytecode::EnumPayloadType::Unit => {
                if o.len() != 1 {
                    return Err(bad());
                }
                EnumPayload::Unit
            }
            allen_bytecode::EnumPayloadType::Tuple(types) => {
                if o.len() != 2 {
                    return Err(bad());
                }
                let values = o
                    .get("value")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(bad)?;
                if values.len() != types.len() {
                    return Err(bad());
                }
                EnumPayload::Tuple(
                    values
                        .iter()
                        .zip(types)
                        .map(|(v, t)| json_to_value(v, t, enums))
                        .collect::<Result<Vec<_>, _>>()?
                        .into(),
                )
            }
            allen_bytecode::EnumPayloadType::Record(fields) => {
                if o.len() != 2 {
                    return Err(bad());
                }
                let value = json_to_value(
                    o.get("value").ok_or_else(bad)?,
                    &ValueType::Record(fields.clone()),
                    enums,
                )?;
                let Value::Record(fields) = value else {
                    unreachable!()
                };
                EnumPayload::Record(fields)
            }
        };
        return Ok(Value::Enum(std::rc::Rc::new(EnumValue {
            identity: EnumIdentity::User(*id),
            type_name: e.name.clone().into(),
            variant_name: variant.name.clone().into(),
            variant: u32::try_from(index).map_err(|_| bad())?,
            payload,
        })));
    }
    let (identity, type_name, variant, variant_name, payload_ty) = match ty {
        ValueType::Option(t) => match tag {
            "None" => (EnumIdentity::Option, "Option", 0, "None", None),
            "Some" => (EnumIdentity::Option, "Option", 1, "Some", Some(t.as_ref())),
            _ => return Err(bad()),
        },
        ValueType::Result(a, b) => match tag {
            "Ok" => (EnumIdentity::Result, "Result", 0, "Ok", Some(a.as_ref())),
            "Err" => (EnumIdentity::Result, "Result", 1, "Err", Some(b.as_ref())),
            _ => return Err(bad()),
        },
        _ => unreachable!(),
    };
    let payload = match payload_ty {
        None => {
            if o.len() != 1 {
                return Err(bad());
            }
            EnumPayload::Unit
        }
        Some(t) if identity == EnumIdentity::Option => {
            if o.len() != 2 {
                return Err(bad());
            }
            EnumPayload::Tuple(
                vec![json_to_value(o.get("value").ok_or_else(bad)?, t, enums)?].into(),
            )
        }
        Some(ValueType::Unit) => {
            if o.len() != 1 {
                return Err(bad());
            }
            EnumPayload::Tuple(vec![Value::Unit].into())
        }
        Some(t) => {
            if o.len() != 2 {
                return Err(bad());
            }
            EnumPayload::Tuple(
                vec![json_to_value(o.get("value").ok_or_else(bad)?, t, enums)?].into(),
            )
        }
    };
    Ok(Value::Enum(std::rc::Rc::new(EnumValue {
        identity,
        type_name: type_name.into(),
        variant_name: variant_name.into(),
        variant,
        payload,
    })))
}
#[allow(clippy::too_many_lines)]
fn tagged_or_map_output(
    value: &Value,
    ty: &ValueType,
    enums: &[allen_bytecode::EnumType],
) -> Result<serde_json::Value, RuntimeError> {
    let bad = || RuntimeError::new(RuntimeErrorCode::Panic, "output does not match schema");
    if let (ValueType::Map(k, t), Value::Map(items)) = (ty, value) {
        let mut a = Vec::new();
        let mut last: Option<&Value> = None;
        for (key, value) in items.iter() {
            if let Some(previous) = last {
                if compare_map_keys(previous, key) != Some(std::cmp::Ordering::Less) {
                    return Err(bad());
                }
            }
            last = Some(key);
            a.push(serde_json::Value::Array(vec![
                value_to_json(key, k, enums)?,
                value_to_json(value, t, enums)?,
            ]));
        }
        return Ok(serde_json::Value::Array(a));
    }
    let Value::Enum(e) = value else {
        return Err(bad());
    };
    if let ValueType::Enum(id) = ty {
        let variant = enums
            .get(*id as usize)
            .and_then(|value| value.variants.get(e.variant as usize))
            .ok_or_else(bad)?;
        let mut object = serde_json::Map::new();
        object.insert("tag".into(), variant.name.clone().into());
        match (&variant.payload, &e.payload) {
            (allen_bytecode::EnumPayloadType::Unit, EnumPayload::Unit) => {}
            (allen_bytecode::EnumPayloadType::Tuple(types), EnumPayload::Tuple(values))
                if types.len() == values.len() =>
            {
                let _ = object.insert(
                    "value".into(),
                    serde_json::Value::Array(
                        values
                            .iter()
                            .zip(types)
                            .map(|(v, t)| value_to_json(v, t, enums))
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                );
            }
            (allen_bytecode::EnumPayloadType::Record(fields), EnumPayload::Record(values)) => {
                let _ = object.insert(
                    "value".into(),
                    value_to_json(
                        &Value::Record(values.clone()),
                        &ValueType::Record(fields.clone()),
                        enums,
                    )?,
                );
            }
            _ => return Err(bad()),
        };
        return Ok(serde_json::Value::Object(object));
    }
    let (tag, payload_ty, expected_identity) = match ty {
        ValueType::Option(t) => {
            if e.variant == 0 {
                ("None", None, EnumIdentity::Option)
            } else {
                ("Some", Some(t.as_ref()), EnumIdentity::Option)
            }
        }
        ValueType::Result(a, b) => {
            if e.variant == 0 {
                ("Ok", Some(a.as_ref()), EnumIdentity::Result)
            } else {
                ("Err", Some(b.as_ref()), EnumIdentity::Result)
            }
        }
        ValueType::Enum(id) => {
            let variant = enums
                .get(*id as usize)
                .and_then(|x| x.variants.get(e.variant as usize))
                .ok_or_else(bad)?;
            match &variant.payload {
                allen_bytecode::EnumPayloadType::Unit => {
                    (variant.name.as_str(), None, EnumIdentity::User(*id))
                }
                _ => return Err(bad()),
            }
        }
        _ => return Err(bad()),
    };
    if e.identity != expected_identity {
        return Err(bad());
    }
    let mut o = serde_json::Map::new();
    o.insert("tag".into(), tag.into());
    if let Some(t) = payload_ty {
        let EnumPayload::Tuple(v) = &e.payload else {
            return Err(bad());
        };
        if v.len() != 1 {
            return Err(bad());
        }
        if expected_identity == EnumIdentity::Option {
            o.insert("value".into(), value_to_json(&v[0], t, enums)?);
        } else if matches!(t, ValueType::Unit) {
            if !matches!(v[0], Value::Unit) {
                return Err(bad());
            }
        } else {
            o.insert("value".into(), value_to_json(&v[0], t, enums)?);
        }
    }
    Ok(serde_json::Value::Object(o))
}

fn compare_map_keys(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        (Value::Int(left), Value::Int(right)) => Some(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Some(left.as_bytes().cmp(right.as_bytes())),
        (Value::Bytes(left), Value::Bytes(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use allen_bytecode::{
        Artifact, ArtifactMetadata, BYTECODE_VERSION, CapabilityOperation, Constant, DecodeLimits,
        EntryContract, EnumPayloadType, EnumType, EnumVariant, Function, Instruction,
        ManifestContract, Module, RecordField, StrictSchema, ToolContract,
        compute_tool_contract_digest, decode_and_verify, encode,
    };
    use allen_http_get::{Clock, Resolver, Transport, TransportRequest, TransportResponse};
    use allen_schema::{CatalogLimits, Field, Idempotency, ToolDefinition, Variant};
    use std::collections::BTreeMap;
    use std::net::{Ipv4Addr, SocketAddr};

    fn verified_runtime_artifact(
        required: Vec<String>,
        optional: Vec<String>,
        limits: Vec<(String, u64)>,
    ) -> VerifiedArtifact {
        verified_runtime_artifact_with_effects(
            required,
            optional,
            limits,
            vec!["fs.read".to_owned()],
        )
    }

    fn verified_runtime_artifact_with_effects(
        required: Vec<String>,
        optional: Vec<String>,
        limits: Vec<(String, u64)>,
        effects: Vec<String>,
    ) -> VerifiedArtifact {
        verified_runtime_artifact_with_effects_and_origins(
            required,
            optional,
            limits,
            effects,
            vec![],
        )
    }

    fn verified_runtime_artifact_with_effects_and_origins(
        required: Vec<String>,
        optional: Vec<String>,
        limits: Vec<(String, u64)>,
        effects: Vec<String>,
        https_origins: Vec<String>,
    ) -> VerifiedArtifact {
        verified_runtime_artifact_for_version(
            BYTECODE_VERSION,
            required,
            optional,
            limits,
            effects,
            https_origins,
        )
    }

    fn verified_runtime_artifact_for_version(
        bytecode_version: u16,
        required: Vec<String>,
        optional: Vec<String>,
        limits: Vec<(String, u64)>,
        effects: Vec<String>,
        https_origins: Vec<String>,
    ) -> VerifiedArtifact {
        let artifact = Artifact {
            metadata: ArtifactMetadata {
                bytecode_version,
                ..ArtifactMetadata::default()
            },
            module: Module {
                constants: vec![Constant::Int(7)],
                enum_types: vec![],
                effect_sets: vec![effects],
                functions: vec![Function {
                    name: package_test_symbol("runtime-test"),
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
                }],
                async_functions: vec![],
                entry: 0,
            },
            debug: None,
            schemas: vec![
                StrictSchema {
                    value_type: ValueType::Unit,
                },
                StrictSchema {
                    value_type: ValueType::Int,
                },
            ],
            entries: vec![EntryContract {
                name: "main".to_owned(),
                function: 0,
                input_schema: 0,
                output_schema: 1,
            }],
            imports: vec![],
            manifest: Some(ManifestContract {
                package: "runtime-test".to_owned(),
                version: "0.1.0".to_owned(),
                language_requirement: "^0.1".to_owned(),
                required_capabilities: required,
                optional_capabilities: optional,
                https_origins,
                limits,
                required_tools: vec![],
                tool_contract_digest: compute_tool_contract_digest(&[]),
            }),
        };
        decode_and_verify(&encode(&artifact).unwrap(), &DecodeLimits::default()).unwrap()
    }

    fn verified_capability_inspection_artifact(
        required: Vec<String>,
        optional: Vec<String>,
    ) -> VerifiedArtifact {
        let output_type = ValueType::List(Box::new(ValueType::String));
        let artifact = Artifact {
            metadata: ArtifactMetadata {
                bytecode_version: BYTECODE_VERSION,
                ..ArtifactMetadata::default()
            },
            module: Module {
                constants: vec![],
                enum_types: vec![],
                effect_sets: vec![vec!["capability.inspect".to_owned()]],
                functions: vec![Function {
                    name: package_test_symbol("runtime-capability-inspection-test"),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![output_type.clone()],
                    return_type: output_type.clone(),
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
            },
            debug: None,
            schemas: vec![
                StrictSchema {
                    value_type: ValueType::Unit,
                },
                StrictSchema {
                    value_type: output_type,
                },
            ],
            entries: vec![EntryContract {
                name: "main".to_owned(),
                function: 0,
                input_schema: 0,
                output_schema: 1,
            }],
            imports: vec![],
            manifest: Some(ManifestContract {
                package: "runtime-capability-inspection-test".to_owned(),
                version: "0.1.0".to_owned(),
                language_requirement: "^0.1".to_owned(),
                required_capabilities: required,
                optional_capabilities: optional,
                https_origins: vec![],
                limits: vec![],
                required_tools: vec![],
                tool_contract_digest: compute_tool_contract_digest(&[]),
            }),
        };
        decode_and_verify(&encode(&artifact).unwrap(), &DecodeLimits::default()).unwrap()
    }

    fn verified_stopping_artifact() -> VerifiedArtifact {
        let artifact = Artifact {
            metadata: ArtifactMetadata {
                bytecode_version: BYTECODE_VERSION,
                ..ArtifactMetadata::default()
            },
            module: Module {
                constants: vec![Constant::String("finished".to_owned())],
                enum_types: vec![],
                effect_sets: vec![vec![]],
                functions: vec![Function {
                    name: package_test_symbol("runtime-stop-test"),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![ValueType::String],
                    return_type: ValueType::Int,
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
            },
            debug: None,
            schemas: vec![
                StrictSchema {
                    value_type: ValueType::Unit,
                },
                StrictSchema {
                    value_type: ValueType::Int,
                },
            ],
            entries: vec![EntryContract {
                name: "main".to_owned(),
                function: 0,
                input_schema: 0,
                output_schema: 1,
            }],
            imports: vec![],
            manifest: Some(ManifestContract {
                package: "runtime-stop-test".to_owned(),
                version: "0.1.0".to_owned(),
                language_requirement: "^0.1".to_owned(),
                required_capabilities: vec![],
                optional_capabilities: vec![],
                https_origins: vec![],
                limits: vec![],
                required_tools: vec![],
                tool_contract_digest: compute_tool_contract_digest(&[]),
            }),
        };
        decode_and_verify(&encode(&artifact).unwrap(), &DecodeLimits::default()).unwrap()
    }

    fn verified_filesystem_artifact(
        required: Vec<String>,
        optional: Vec<String>,
    ) -> VerifiedArtifact {
        let result_type = ValueType::Result(
            Box::new(ValueType::String),
            Box::new(allen_bytecode::file_error_type()),
        );
        let artifact = Artifact {
            metadata: ArtifactMetadata {
                bytecode_version: BYTECODE_VERSION,
                ..ArtifactMetadata::default()
            },
            module: Module {
                constants: vec![Constant::String("note.txt".to_owned())],
                enum_types: vec![],
                effect_sets: vec![vec!["fs.read".to_owned()]],
                functions: vec![Function {
                    name: package_test_symbol("runtime-fs-test"),
                    parameters: vec![],
                    captures: vec![],
                    registers: vec![
                        ValueType::Workspace,
                        ValueType::String,
                        ValueType::Future(Box::new(result_type.clone())),
                        result_type.clone(),
                    ],
                    return_type: result_type.clone(),
                    effects: 0,
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
            },
            debug: None,
            schemas: vec![
                StrictSchema {
                    value_type: ValueType::Unit,
                },
                StrictSchema {
                    value_type: result_type,
                },
            ],
            entries: vec![EntryContract {
                name: "main".to_owned(),
                function: 0,
                input_schema: 0,
                output_schema: 1,
            }],
            imports: vec![],
            manifest: Some(ManifestContract {
                package: "runtime-fs-test".to_owned(),
                version: "0.1.0".to_owned(),
                language_requirement: "^0.1".to_owned(),
                required_capabilities: required,
                optional_capabilities: optional,
                https_origins: vec![],
                limits: vec![],
                required_tools: vec![],
                tool_contract_digest: compute_tool_contract_digest(&[]),
            }),
        };
        decode_and_verify(&encode(&artifact).unwrap(), &DecodeLimits::default()).unwrap()
    }

    fn raw_digest(value: &str) -> [u8; 32] {
        let hex = value.strip_prefix("sha256:").unwrap();
        let mut output = [0; 32];
        for (index, byte) in output.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap();
        }
        output
    }

    #[allow(clippy::too_many_lines)]
    fn verified_tool_artifact() -> (VerifiedArtifact, FrozenCatalog) {
        let definition = ToolDefinition::parse(
            "example.echo",
            "1.2.3",
            r#"{"type":"string"}"#,
            r#"{"type":"string"}"#,
            r#"{"type":"string"}"#,
            vec![],
            Idempotency::Idempotent,
            &SchemaLimits::default(),
        )
        .unwrap();
        let catalog =
            FrozenCatalog::freeze(vec![definition.clone()], &CatalogLimits::default()).unwrap();
        let tool_error = EnumType {
            name: "pkg://runtime-tool-test@0.1.0/src/main.allen::_tool_tools_x2E_example_x2E_echo_x3A__x3A_Error"
                .to_owned(),
            variants: vec![
                EnumVariant {
                    name: "Declared".to_owned(),
                    payload: EnumPayloadType::Tuple(vec![ValueType::String]),
                },
                EnumVariant {
                    name: "Unavailable".to_owned(),
                    payload: EnumPayloadType::Record(match allen_bytecode::standard_error_type() {
                        ValueType::Record(fields) => fields,
                        _ => unreachable!(),
                    }),
                },
                EnumVariant {
                    name: "Schema".to_owned(),
                    payload: EnumPayloadType::Record(match allen_bytecode::standard_error_type() {
                        ValueType::Record(fields) => fields,
                        _ => unreachable!(),
                    }),
                },
            ],
        };
        let result = ValueType::Result(Box::new(ValueType::String), Box::new(ValueType::Enum(0)));
        let tool = ToolContract {
            name: "example.echo".to_owned(),
            version: "1.2.3".to_owned(),
            version_requirement: ">=1.0.0, <2.0.0".to_owned(),
            effect: "tool.example.echo@1".to_owned(),
            input_schema: 0,
            output_schema: 0,
            error_schema: 0,
            input_digest: raw_digest(definition.input_schema.digest()),
            output_digest: raw_digest(definition.output_schema.digest()),
            error_digest: raw_digest(definition.error_schema.digest()),
        };
        let artifact = Artifact {
            metadata: ArtifactMetadata {
                bytecode_version: BYTECODE_VERSION,
                ..ArtifactMetadata::default()
            },
            module: Module {
                constants: vec![Constant::Int(7)],
                enum_types: vec![tool_error],
                effect_sets: vec![vec![], vec![tool.effect.clone()]],
                functions: vec![
                    Function {
                        name: package_test_symbol("runtime-tool-test"),
                        parameters: vec![0],
                        captures: vec![],
                        registers: vec![
                            ValueType::String,
                            ValueType::Future(Box::new(result.clone())),
                            result.clone(),
                        ],
                        return_type: result.clone(),
                        effects: 1,
                        code: vec![
                            Instruction::ToolInvoke {
                                destination: 1,
                                tool: 0,
                                input: 0,
                            },
                            Instruction::Await {
                                destination: 2,
                                source: 1,
                            },
                            Instruction::Return { source: 2 },
                        ],
                    },
                    Function {
                        name: package_test_symbol("runtime-tool-test").replace("::main", "::pure"),
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
                async_functions: vec![0],
                entry: 0,
            },
            debug: None,
            schemas: vec![
                StrictSchema {
                    value_type: ValueType::String,
                },
                StrictSchema { value_type: result },
                StrictSchema {
                    value_type: ValueType::Unit,
                },
                StrictSchema {
                    value_type: ValueType::Int,
                },
            ],
            entries: vec![
                EntryContract {
                    name: "main".to_owned(),
                    function: 0,
                    input_schema: 0,
                    output_schema: 1,
                },
                EntryContract {
                    name: "pure".to_owned(),
                    function: 1,
                    input_schema: 2,
                    output_schema: 3,
                },
            ],
            imports: vec![],
            manifest: Some(ManifestContract {
                package: "runtime-tool-test".to_owned(),
                version: "0.1.0".to_owned(),
                language_requirement: "0.1".to_owned(),
                required_capabilities: vec![],
                optional_capabilities: vec![],
                limits: vec![],
                https_origins: vec![],
                required_tools: vec![tool.clone()],
                tool_contract_digest: compute_tool_contract_digest(&[tool]),
            }),
        };
        (
            decode_and_verify(&encode(&artifact).unwrap(), &DecodeLimits::default()).unwrap(),
            catalog,
        )
    }

    struct EchoTool {
        output: serde_json::Value,
        declared_error: bool,
        calls: usize,
    }
    struct AlwaysCancel;
    impl CancellationSource for AlwaysCancel {
        fn is_cancelled(&mut self) -> bool {
            true
        }
    }
    impl ToolProvider for EchoTool {
        fn invoke(
            &mut self,
            invocation: &ToolInvocation,
            input: serde_json::Value,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolOutcome, ToolHostError> {
            assert_eq!(invocation.name, "example.echo");
            assert_eq!(input, serde_json::json!("hello"));
            self.calls += 1;
            if self.declared_error {
                Ok(ToolOutcome::DeclaredError(self.output.clone()))
            } else {
                Ok(ToolOutcome::Output(self.output.clone()))
            }
        }
    }
    #[derive(Default)]
    struct UnavailableTool(usize);
    impl ToolProvider for UnavailableTool {
        fn invoke(
            &mut self,
            _invocation: &ToolInvocation,
            _input: serde_json::Value,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolOutcome, ToolHostError> {
            self.0 += 1;
            Err(ToolHostError::Unavailable)
        }
    }

    #[derive(Default)]
    struct DeadlineTool {
        deadline: Option<Duration>,
    }
    impl ToolProvider for DeadlineTool {
        fn invoke(
            &mut self,
            invocation: &ToolInvocation,
            _input: serde_json::Value,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolOutcome, ToolHostError> {
            self.deadline = Some(invocation.deadline);
            Ok(ToolOutcome::Output(serde_json::json!("done")))
        }
    }

    struct InvalidOutcomeTool;
    impl ToolProvider for InvalidOutcomeTool {
        fn invoke(
            &mut self,
            _invocation: &ToolInvocation,
            _input: serde_json::Value,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolOutcome, ToolHostError> {
            Err(ToolHostError::InvalidOutcome)
        }
    }

    #[derive(Default)]
    struct PanickingPendingTool {
        cancelled: usize,
    }

    impl ToolProvider for PanickingPendingTool {
        fn invoke(
            &mut self,
            _invocation: &ToolInvocation,
            _input: serde_json::Value,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolOutcome, ToolHostError> {
            unreachable!("the nonblocking path is used")
        }

        fn start_invoke(
            &mut self,
            _pending: PendingEffectId,
            _invocation: &ToolInvocation,
            _input: serde_json::Value,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolProviderPoll, ToolHostError> {
            Ok(ToolProviderPoll::Pending)
        }

        fn poll(
            &mut self,
            _pending: PendingEffectId,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolProviderPoll, ToolHostError> {
            panic!("provider-secret must never cross the supervisor boundary")
        }

        fn cancel_pending(
            &mut self,
            _pending: PendingEffectId,
            _execution_id: ExternalExecutionId,
            _operation_id: u64,
        ) {
            self.cancelled += 1;
        }
    }

    struct ForeverPendingTool;

    impl ToolProvider for ForeverPendingTool {
        fn invoke(
            &mut self,
            _invocation: &ToolInvocation,
            _input: serde_json::Value,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolOutcome, ToolHostError> {
            unreachable!("the nonblocking path is used")
        }

        fn start_invoke(
            &mut self,
            _pending: PendingEffectId,
            _invocation: &ToolInvocation,
            _input: serde_json::Value,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolProviderPoll, ToolHostError> {
            Ok(ToolProviderPoll::Pending)
        }

        fn poll(
            &mut self,
            _pending: PendingEffectId,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolProviderPoll, ToolHostError> {
            Ok(ToolProviderPoll::Pending)
        }
    }

    struct ReplayToolEffect {
        calls: usize,
        replayed: bool,
        finalization_error: bool,
    }

    struct RequestDigestMismatchReplay {
        lookups: usize,
        recorded_input: Value,
    }

    struct WrongValueReplay;

    enum PreVmFailure {
        IsReplayedPanic,
        BindPanic,
        BindMismatch,
    }

    struct PreVmReplay(PreVmFailure);

    #[derive(Default)]
    struct FinalOutcomeCapture(Vec<&'static str>);

    impl EffectProvider for FinalOutcomeCapture {
        fn is_replayed(&self) -> bool {
            true
        }

        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(
            &mut self,
            _operation: FsOperation,
            _arguments: &[Value],
        ) -> Result<Value, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn finish_execution(&mut self, outcome: EffectExecutionOutcome<'_>) -> Result<(), VmError> {
            self.0.push(match outcome {
                EffectExecutionOutcome::Completed => "completed",
                EffectExecutionOutcome::Stopped { .. } => "stopped",
                EffectExecutionOutcome::Terminal {
                    error: VmError::ResourceLimit { .. },
                } => "resource.limit",
                EffectExecutionOutcome::Terminal { .. } => "terminal",
                EffectExecutionOutcome::RuntimePanic => "runtime.panic",
            });
            Ok(())
        }
    }

    impl EffectProvider for PreVmReplay {
        fn is_replayed(&self) -> bool {
            if matches!(self.0, PreVmFailure::IsReplayedPanic) {
                panic!("CANARY replay provenance panic")
            }
            true
        }

        fn bind_execution(&mut self, _binding: &EffectExecutionBinding) -> Result<(), VmError> {
            match self.0 {
                PreVmFailure::BindPanic => panic!("CANARY replay binding panic"),
                PreVmFailure::BindMismatch => Err(VmError::ReplayDiverged),
                PreVmFailure::IsReplayedPanic => Ok(()),
            }
        }

        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            panic!("VM must not start")
        }

        fn call(
            &mut self,
            _operation: FsOperation,
            _arguments: &[Value],
        ) -> Result<Value, VmError> {
            panic!("VM must not start")
        }
    }

    impl EffectProvider for WrongValueReplay {
        fn is_replayed(&self) -> bool {
            true
        }

        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(&mut self, _operation: FsOperation, _args: &[Value]) -> Result<Value, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn start_tool(
            &mut self,
            _pending: PendingEffectId,
            _tool: u32,
            _input: &Value,
            _result_type: &ValueType,
            _cancellation: &mut dyn CancellationSource,
        ) -> Result<allen_vm::EffectPoll, VmError> {
            Ok(allen_vm::EffectPoll::Ready(Value::String(
                "provider-secret malformed value".into(),
            )))
        }
    }

    impl EffectProvider for RequestDigestMismatchReplay {
        fn is_replayed(&self) -> bool {
            true
        }

        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(&mut self, _operation: FsOperation, _args: &[Value]) -> Result<Value, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn start_tool(
            &mut self,
            _pending: PendingEffectId,
            _tool: u32,
            input: &Value,
            _result_type: &ValueType,
            _cancellation: &mut dyn CancellationSource,
        ) -> Result<allen_vm::EffectPoll, VmError> {
            self.lookups += 1;
            assert_ne!(input, &self.recorded_input);
            Err(VmError::ReplayDiverged)
        }
    }

    impl EffectProvider for ReplayToolEffect {
        fn is_replayed(&self) -> bool {
            self.replayed
        }

        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(&mut self, _operation: FsOperation, _args: &[Value]) -> Result<Value, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn start_tool(
            &mut self,
            _pending: PendingEffectId,
            tool: u32,
            input: &Value,
            _result_type: &ValueType,
            _cancellation: &mut dyn CancellationSource,
        ) -> Result<allen_vm::EffectPoll, VmError> {
            assert_eq!(tool, 0);
            assert_eq!(input, &Value::String("hello".into()));
            self.calls += 1;
            Ok(allen_vm::EffectPoll::Ready(result_value(
                0,
                Value::String("replayed".into()),
            )))
        }

        fn finish_execution(
            &mut self,
            _outcome: EffectExecutionOutcome<'_>,
        ) -> Result<(), VmError> {
            if self.finalization_error {
                Err(VmError::ReplayDiverged)
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct ProvenanceObserver(Vec<bool>);

    impl CheckpointObserver for ProvenanceObserver {
        fn checkpoint(&mut self, _checkpoint: Checkpoint) {}

        fn execution_effect_provenance(&mut self, replayed: bool) {
            self.0.push(replayed);
        }
    }

    struct TimeoutTool;
    impl ToolProvider for TimeoutTool {
        fn invoke(
            &mut self,
            _invocation: &ToolInvocation,
            _input: serde_json::Value,
            _cancellation: &mut dyn ToolCancellationSignal,
        ) -> Result<ToolOutcome, ToolHostError> {
            Err(ToolHostError::Timeout)
        }
    }

    struct DelayFirstCheckpoint(bool);
    impl CheckpointObserver for DelayFirstCheckpoint {
        fn checkpoint(&mut self, _checkpoint: Checkpoint) {
            if !self.0 {
                self.0 = true;
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    fn tool_policy(catalog: FrozenCatalog) -> HostPolicy {
        let mut policy = HostPolicy {
            tool_catalog: Some(catalog),
            ..HostPolicy::default()
        };
        policy.granted_tools.insert("example.echo".to_owned());
        policy
    }

    fn tool_request() -> LaunchRequest {
        LaunchRequest {
            entry: "main".to_owned(),
            input: serde_json::json!("hello"),
        }
    }

    fn launch_tool(
        artifact: &VerifiedArtifact,
        policy: &HostPolicy,
        tool: &mut dyn ToolProvider,
    ) -> Result<RuntimeOutcome, RuntimeError> {
        launch_with_providers(
            artifact,
            &tool_request(),
            policy,
            &mut RuntimeProviders {
                tools: Some(tool),
                ..RuntimeProviders::default()
            },
        )
    }

    #[test]
    fn typed_tool_preflight_and_cancellation_prevent_dispatch() {
        let (artifact, catalog) = verified_tool_artifact();
        let policy = tool_policy(catalog);
        let mut tool = EchoTool {
            output: serde_json::json!("done"),
            declared_error: false,
            calls: 0,
        };
        let mut denied = policy.clone();
        denied.granted_tools.clear();
        assert_eq!(
            launch_tool(&artifact, &denied, &mut tool).unwrap_err().code,
            RuntimeErrorCode::CapabilityDenied
        );
        assert_eq!(tool.calls, 0);
        let mut providers = RuntimeProviders {
            tools: Some(&mut tool),
            ..RuntimeProviders::default()
        };
        let mut cancel = AlwaysCancel;
        let mut observer = NoObserver;
        launch_with_context(
            &artifact,
            &tool_request(),
            &policy,
            &mut providers,
            &mut cancel,
            &mut observer,
        )
        .unwrap_err();
        assert_eq!(tool.calls, 0);
    }

    #[test]
    fn preparation_requires_tool_grants_only_for_the_selected_entry() {
        let (artifact, catalog) = verified_tool_artifact();
        let policy = HostPolicy {
            tool_catalog: Some(catalog),
            ..HostPolicy::default()
        };
        let prepared = prepare_launch(
            &artifact,
            &LaunchRequest {
                entry: "pure".to_owned(),
                input: serde_json::Value::Null,
            },
            &policy,
        )
        .unwrap();
        let mut providers = RuntimeProviders::default();
        let mut cancellation = NeverCancel;
        let mut observer = NoObserver;
        assert_eq!(
            execute_prepared_with_context(
                prepared,
                &mut providers,
                &mut cancellation,
                &mut observer,
            )
            .unwrap()
            .output,
            serde_json::json!(7)
        );
        let workspace = TestWorkspace::new();
        let extra_workdir = HostPolicy {
            workspace_root: Some(workspace.0.clone()),
            strict_preflight_authority: true,
            ..policy.clone()
        };
        assert_eq!(
            prepare_launch(
                &artifact,
                &LaunchRequest {
                    entry: "pure".to_owned(),
                    input: serde_json::Value::Null,
                },
                &extra_workdir,
            )
            .unwrap_err()
            .code,
            RuntimeErrorCode::ManifestInvalid
        );
        let mut undeclared_grant = HostPolicy {
            strict_preflight_authority: true,
            ..policy.clone()
        };
        undeclared_grant
            .granted_capabilities
            .insert("fs.read".to_owned());
        assert_eq!(
            prepare_launch(
                &artifact,
                &LaunchRequest {
                    entry: "pure".to_owned(),
                    input: serde_json::Value::Null,
                },
                &undeclared_grant,
            )
            .unwrap_err()
            .code,
            RuntimeErrorCode::CapabilityDenied
        );
        assert_eq!(
            prepare_launch(&artifact, &tool_request(), &policy)
                .unwrap_err()
                .code,
            RuntimeErrorCode::CapabilityDenied
        );
    }

    #[test]
    fn typed_tool_outputs_and_declared_errors_are_distinct() {
        let (artifact, catalog) = verified_tool_artifact();
        let policy = tool_policy(catalog);
        let mut tool = EchoTool {
            output: serde_json::json!("done"),
            declared_error: false,
            calls: 0,
        };
        let outcome = launch_tool(&artifact, &policy, &mut tool).unwrap();
        assert_eq!(
            outcome.output,
            serde_json::json!({"tag":"Ok","value":"done"})
        );
        assert_eq!(tool.calls, 1);

        let mut declared = EchoTool {
            output: serde_json::json!("missing"),
            declared_error: true,
            calls: 0,
        };
        let outcome = launch_tool(&artifact, &policy, &mut declared).unwrap();
        assert_eq!(
            outcome.output,
            serde_json::json!({"tag":"Err","value":{"tag":"Declared","value":["missing"]}})
        );
        assert_eq!(declared.calls, 1);
    }

    #[test]
    fn replay_effect_override_is_all_or_nothing_and_reports_provenance() {
        let (artifact, catalog) = verified_tool_artifact();
        let policy = tool_policy(catalog);
        let mut replay = ReplayToolEffect {
            calls: 0,
            replayed: true,
            finalization_error: false,
        };
        let mut live = EchoTool {
            output: serde_json::json!("live"),
            declared_error: false,
            calls: 0,
        };
        let mut providers = RuntimeProviders {
            effect_override: Some(&mut replay),
            tools: Some(&mut live),
            ..RuntimeProviders::default()
        };
        let mut cancellation = NeverCancel;
        let mut observer = ProvenanceObserver::default();
        let outcome = launch_with_context(
            &artifact,
            &tool_request(),
            &policy,
            &mut providers,
            &mut cancellation,
            &mut observer,
        )
        .unwrap();
        assert_eq!(
            outcome.output,
            serde_json::json!({"tag":"Ok","value":"replayed"})
        );
        assert_eq!(replay.calls, 1);
        assert_eq!(live.calls, 0);
        assert_eq!(observer.0, vec![true]);

        let mut non_replay = ReplayToolEffect {
            calls: 0,
            replayed: false,
            finalization_error: false,
        };
        let error = launch_with_providers(
            &artifact,
            &tool_request(),
            &policy,
            &mut RuntimeProviders {
                effect_override: Some(&mut non_replay),
                ..RuntimeProviders::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, RuntimeErrorCode::ReplayDiverged);
        assert_eq!(non_replay.calls, 0);
    }

    #[test]
    fn pre_vm_replay_callbacks_are_panic_contained_and_binding_errors_are_diagnostic() {
        let artifact = verified_runtime_artifact_with_effects(vec![], vec![], vec![], vec![]);
        for failure in [PreVmFailure::IsReplayedPanic, PreVmFailure::BindPanic] {
            let mut replay = PreVmReplay(failure);
            let error = launch_with_providers(
                &artifact,
                &launch_request(),
                &HostPolicy::default(),
                &mut RuntimeProviders {
                    effect_override: Some(&mut replay),
                    ..RuntimeProviders::default()
                },
            )
            .unwrap_err();
            assert_eq!(error.code, RuntimeErrorCode::Panic);
            assert_eq!(error.message, "runtime invariant failed");
            assert!(!error.message.contains("CANARY"));
        }

        let mut replay = PreVmReplay(PreVmFailure::BindMismatch);
        let error = launch_with_providers(
            &artifact,
            &launch_request(),
            &HostPolicy::default(),
            &mut RuntimeProviders {
                effect_override: Some(&mut replay),
                ..RuntimeProviders::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, RuntimeErrorCode::ReplayDiverged);
    }

    #[test]
    fn replay_request_digest_mismatch_is_a_runtime_trap_without_live_dispatch() {
        let (artifact, catalog) = verified_tool_artifact();
        assert_eq!(artifact.metadata().bytecode_version, BYTECODE_VERSION);
        let policy = tool_policy(catalog);
        let mut replay = RequestDigestMismatchReplay {
            lookups: 0,
            recorded_input: Value::String("different recorded request".into()),
        };
        let mut live = EchoTool {
            output: serde_json::json!("live side effect"),
            declared_error: false,
            calls: 0,
        };
        let error = launch_with_providers(
            &artifact,
            &tool_request(),
            &policy,
            &mut RuntimeProviders {
                effect_override: Some(&mut replay),
                tools: Some(&mut live),
                ..RuntimeProviders::default()
            },
        )
        .unwrap_err();

        assert_eq!(error.code, RuntimeErrorCode::ReplayRuntimeDiverged);
        assert_eq!(error.code.as_str(), "replay.runtime_diverged");
        assert_eq!(replay.lookups, 1);
        assert_eq!(live.calls, 0);
    }

    #[test]
    fn malformed_replayed_provider_value_is_a_safe_protocol_violation() {
        let (artifact, catalog) = verified_tool_artifact();
        let policy = tool_policy(catalog);
        let error = launch_with_providers(
            &artifact,
            &tool_request(),
            &policy,
            &mut RuntimeProviders {
                effect_override: Some(&mut WrongValueReplay),
                ..RuntimeProviders::default()
            },
        )
        .unwrap_err();

        assert_eq!(error.code, RuntimeErrorCode::ProtocolViolation);
        assert_eq!(error.message, "runtime protocol violation");
        assert!(!error.message.contains("provider-secret"));
    }

    #[test]
    fn invalid_tool_values_and_outcomes_are_schema_errors() {
        let (artifact, catalog) = verified_tool_artifact();
        let policy = tool_policy(catalog);
        let mut bad = EchoTool {
            output: serde_json::json!(7),
            declared_error: false,
            calls: 0,
        };
        let outcome = launch_tool(&artifact, &policy, &mut bad).unwrap();
        assert_eq!(outcome.output["tag"], "Err");
        assert_eq!(outcome.output["value"]["tag"], "Schema");
        assert_eq!(outcome.output["value"]["value"]["code"], "tool.schema");
        assert_eq!(bad.calls, 1);

        let mut unavailable = UnavailableTool::default();
        let outcome = launch_tool(&artifact, &policy, &mut unavailable).unwrap();
        assert_eq!(outcome.output["value"]["tag"], "Unavailable");
        assert_eq!(outcome.output["value"]["value"]["code"], "tool.unavailable");
        assert_eq!(unavailable.0, 1);

        let mut invalid = InvalidOutcomeTool;
        let outcome = launch_tool(&artifact, &policy, &mut invalid).unwrap();
        assert_eq!(outcome.output["value"]["tag"], "Schema");
    }

    #[test]
    fn tool_provider_timeout_is_a_recoverable_unavailable_result() {
        let (artifact, catalog) = verified_tool_artifact();
        let policy = tool_policy(catalog);
        let mut timed_out = TimeoutTool;
        let outcome = launch_tool(&artifact, &policy, &mut timed_out).unwrap();
        assert_eq!(outcome.output["value"]["tag"], "Unavailable");
        assert_eq!(outcome.output["value"]["value"]["code"], "tool.unavailable");
    }

    #[test]
    fn provider_panics_are_bounded_runtime_panics_and_cancel_pending_work() {
        let (artifact, catalog) = verified_tool_artifact();
        let policy = tool_policy(catalog);
        let mut provider = PanickingPendingTool::default();
        let error = launch_tool(&artifact, &policy, &mut provider).unwrap_err();

        assert_eq!(error.code, RuntimeErrorCode::Panic);
        assert_eq!(error.message, "runtime invariant failed");
        assert!(!error.message.contains("provider-secret"));
        assert!(error.message.len() <= 1_024);
        assert_eq!(provider.cancelled, 1);
    }

    #[test]
    fn forever_pending_provider_respects_the_execution_wall_timeout() {
        let (artifact, catalog) = verified_tool_artifact();
        let mut policy = tool_policy(catalog);
        policy.limits.wall_time = Duration::from_millis(1);
        let mut provider = ForeverPendingTool;
        let error = launch_tool(&artifact, &policy, &mut provider).unwrap_err();

        assert_eq!(error.code, RuntimeErrorCode::Timeout);
    }

    #[test]
    fn tool_invocation_deadline_is_the_actual_remaining_execution_time() {
        let (artifact, catalog) = verified_tool_artifact();
        let wall_time = Duration::from_millis(250);
        let mut policy = HostPolicy {
            tool_catalog: Some(catalog),
            limits: ExecutionLimits {
                wall_time,
                ..ExecutionLimits::default()
            },
            ..HostPolicy::default()
        };
        policy.granted_tools.insert("example.echo".to_owned());
        let mut tool = DeadlineTool::default();
        let mut providers = RuntimeProviders {
            tools: Some(&mut tool),
            ..RuntimeProviders::default()
        };
        let mut cancellation = NeverCancel;
        let mut observer = DelayFirstCheckpoint(false);
        launch_with_context(
            &artifact,
            &LaunchRequest {
                entry: "main".to_owned(),
                input: serde_json::json!("hello"),
            },
            &policy,
            &mut providers,
            &mut cancellation,
            &mut observer,
        )
        .unwrap();
        let remaining = tool.deadline.unwrap();
        assert!(remaining > Duration::ZERO);
        assert!(remaining <= wall_time.saturating_sub(Duration::from_millis(15)));
    }

    fn package_test_symbol(package: &str) -> String {
        fn escape(value: &str) -> String {
            let mut output = String::from('x');
            for byte in value.bytes() {
                use std::fmt::Write as _;
                write!(&mut output, "{byte:02x}").unwrap();
            }
            output
        }
        format!(
            "pkg/{}/{}/x737263/x6d61696e.allen::main",
            escape(package),
            escape("0.1.0")
        )
    }

    static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(1);

    struct TestWorkspace(PathBuf);

    impl TestWorkspace {
        fn new() -> Self {
            let sequence = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "allen-runtime-test-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("note.txt"), "hello").unwrap();
            Self(path)
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn launch_request() -> LaunchRequest {
        LaunchRequest {
            entry: "main".to_owned(),
            input: serde_json::Value::Null,
        }
    }

    #[test]
    fn replay_binding_changes_with_effect_limit_workspace_and_denied_addresses() {
        let artifact = verified_runtime_artifact_with_effects(vec![], vec![], vec![], vec![]);
        let binding = |policy: &HostPolicy| {
            prepare_launch(&artifact, &launch_request(), policy)
                .unwrap()
                .effect_execution_binding()
        };
        let baseline = HostPolicy::default();
        let baseline_binding = binding(&baseline);

        let mut effects = baseline.clone();
        effects.effects -= 1;
        assert_ne!(
            baseline_binding.policy_digest,
            binding(&effects).policy_digest
        );

        let first = TestWorkspace::new();
        let second = TestWorkspace::new();
        let mut first_root = baseline.clone();
        first_root.workspace_root = Some(first.0.clone());
        let mut second_root = baseline.clone();
        second_root.workspace_root = Some(second.0.clone());
        assert_ne!(
            binding(&first_root).policy_digest,
            binding(&second_root).policy_digest
        );

        let mut denied = baseline;
        denied
            .denied_net_addresses
            .insert(IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(
            baseline_binding.policy_digest,
            binding(&denied).policy_digest
        );
    }

    #[test]
    fn launch_preserves_the_host_neutral_stopped_outcome() {
        let outcome = launch(
            &verified_stopping_artifact(),
            &launch_request(),
            &HostPolicy::default(),
        )
        .unwrap();
        assert_eq!(outcome.output, serde_json::Value::Null);
        assert!(matches!(
            outcome.execution,
            ExecutionOutcome::Stopped { ref reason, .. } if reason == "finished"
        ));
    }

    #[test]
    fn bytes_and_structures_use_exact_json_shapes() {
        let ty = ValueType::Record(vec![allen_bytecode::RecordField {
            name: "data".into(),
            value_type: ValueType::List(Box::new(ValueType::Bytes)),
        }]);
        let json = serde_json::json!({"data":[{"$bytes":"AQI="}]});
        let value = json_to_value(&json, &ty, &[]).unwrap();
        assert_eq!(value_to_json(&value, &ty, &[]).unwrap(), json);
        assert!(json_to_value(&serde_json::json!({"data":["AQI="]}), &ty, &[]).is_err());
        assert!(json_to_value(&serde_json::json!({"data":[],"extra":true}), &ty, &[]).is_err());
        assert!(
            json_to_value(&serde_json::json!({"$bytes":"AQI"}), &ValueType::Bytes, &[]).is_err()
        );
    }

    #[test]
    fn tool_codec_uses_descriptor_maps_and_flat_tagged_unions_only_at_tool_boundaries() {
        let integer = Descriptor::Int { min: 0, max: 9 };
        let map_descriptor = Descriptor::StringMap {
            values: Box::new(integer.clone()),
        };
        let map_type = ValueType::Map(Box::new(ValueType::String), Box::new(ValueType::Int));
        let entry_json = serde_json::json!([["a", 1], ["b", 2]]);
        let map_value = json_to_value(&entry_json, &map_type, &[]).unwrap();
        assert_eq!(
            value_to_json(&map_value, &map_type, &[]).unwrap(),
            entry_json
        );
        assert_eq!(
            tool_value_to_json(&map_value, &map_descriptor, &map_type, &[]).unwrap(),
            serde_json::json!({"a":1,"b":2})
        );
        assert_eq!(
            tool_json_to_value(
                &serde_json::json!({"a":1,"b":2}),
                &map_descriptor,
                &map_type,
                &[],
            )
            .unwrap(),
            map_value
        );

        let union_descriptor = Descriptor::TaggedUnion {
            variants: vec![
                Variant {
                    tag: "empty".to_owned(),
                    fields: vec![],
                },
                Variant {
                    tag: "ready".to_owned(),
                    fields: vec![Field {
                        name: "values".to_owned(),
                        schema: map_descriptor,
                    }],
                },
            ],
        };
        let union_type = ValueType::Enum(0);
        let enums = [EnumType {
            name: "tools.example.Output_union_test".to_owned(),
            variants: vec![
                EnumVariant {
                    name: "_tag_empty".to_owned(),
                    payload: EnumPayloadType::Unit,
                },
                EnumVariant {
                    name: "_tag_ready".to_owned(),
                    payload: EnumPayloadType::Record(vec![RecordField {
                        name: "values".to_owned(),
                        value_type: map_type,
                    }]),
                },
            ],
        }];
        let output_json = serde_json::json!({"tag":"ready","values":{"a":1,"b":2}});
        let output =
            tool_json_to_value(&output_json, &union_descriptor, &union_type, &enums).unwrap();
        assert_eq!(
            tool_value_to_json(&output, &union_descriptor, &union_type, &enums).unwrap(),
            output_json
        );
        assert_eq!(
            value_to_json(&output, &union_type, &enums).unwrap(),
            serde_json::json!({
                "tag":"_tag_ready",
                "value":{"values":[["a",1],["b",2]]}
            })
        );

        let declared_error_json = serde_json::json!({"tag":"empty"});
        let declared_error =
            tool_json_to_value(&declared_error_json, &union_descriptor, &union_type, &enums)
                .unwrap();
        assert_eq!(
            tool_value_to_json(&declared_error, &union_descriptor, &union_type, &enums,).unwrap(),
            declared_error_json
        );
    }

    #[test]
    fn floats_have_canonical_nonfinite_spellings() {
        let value = json_to_value(&serde_json::json!("Infinity"), &ValueType::Float, &[]).unwrap();
        assert_eq!(
            value_to_json(&value, &ValueType::Float, &[]).unwrap(),
            serde_json::json!("Infinity")
        );
        assert!(json_to_value(&serde_json::json!("inf"), &ValueType::Float, &[]).is_err());
        assert!(json_to_value(&serde_json::json!(1), &ValueType::Float, &[]).is_err());
    }

    #[test]
    fn tagged_and_map_forms_are_exact() {
        let option = ValueType::Option(Box::new(ValueType::Int));
        assert!(json_to_value(&serde_json::json!({"tag":"Some","value":2}), &option, &[]).is_ok());
        assert!(json_to_value(&serde_json::json!({"tag":"None","value":2}), &option, &[]).is_err());
        let map = ValueType::Map(Box::new(ValueType::String), Box::new(ValueType::Int));
        assert!(json_to_value(&serde_json::json!([["a", 1], ["b", 2]]), &map, &[]).is_ok());
        assert!(json_to_value(&serde_json::json!([["b", 2], ["a", 1]]), &map, &[]).is_err());
        let integers = ValueType::Map(Box::new(ValueType::Int), Box::new(ValueType::String));
        assert!(
            json_to_value(
                &serde_json::json!([[2, "two"], [10, "ten"]]),
                &integers,
                &[]
            )
            .is_ok()
        );
        assert!(
            json_to_value(
                &serde_json::json!([[10, "ten"], [2, "two"]]),
                &integers,
                &[]
            )
            .is_err()
        );

        let unit_result = ValueType::Result(Box::new(ValueType::Unit), Box::new(ValueType::String));
        let json = serde_json::json!({"tag":"Ok"});
        let value = json_to_value(&json, &unit_result, &[]).unwrap();
        assert_eq!(value_to_json(&value, &unit_result, &[]).unwrap(), json);
        assert!(
            json_to_value(
                &serde_json::json!({"tag":"Ok","value":null}),
                &unit_result,
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn option_unit_requires_and_emits_an_explicit_null_value() {
        let option_unit = ValueType::Option(Box::new(ValueType::Unit));
        let canonical = serde_json::json!({"tag":"Some","value":null});
        let value = json_to_value(&canonical, &option_unit, &[]).unwrap();
        assert_eq!(value_to_json(&value, &option_unit, &[]).unwrap(), canonical);
        assert_eq!(unknown_value_to_json(&value).unwrap(), canonical);
        assert!(json_to_value(&serde_json::json!({"tag":"Some"}), &option_unit, &[]).is_err());
        assert_eq!(
            exact_validation_issues(&serde_json::json!({"tag":"Some"}), &option_unit, &[]),
            [ValidationIssue {
                path: "/value".to_owned(),
                code: "required".to_owned(),
            }]
        );
        assert_eq!(
            schema_descriptor(&option_unit, &[])["variants"][1]["fields"]["value"],
            serde_json::json!({"type":"null"})
        );
    }

    #[test]
    fn preflight_denies_missing_authority_and_intersects_host_limits() {
        let required = verified_runtime_artifact(vec!["fs.read".to_owned()], vec![], vec![]);
        let request = LaunchRequest {
            entry: "main".to_owned(),
            input: serde_json::Value::Null,
        };
        assert_eq!(
            launch(&required, &request, &HostPolicy::default())
                .unwrap_err()
                .code,
            RuntimeErrorCode::CapabilityDenied
        );

        let above_host = verified_runtime_artifact(
            vec![],
            vec!["fs.read".to_owned()],
            vec![("instructions".to_owned(), 101)],
        );
        let mut bounded_policy = HostPolicy::default();
        bounded_policy.limits.instructions = 100;
        assert_eq!(
            launch(&above_host, &request, &bounded_policy)
                .unwrap()
                .effective_limits
                .instructions,
            100
        );
        let wrong_entry = LaunchRequest {
            entry: "missing".to_owned(),
            input: serde_json::Value::Null,
        };
        assert_eq!(
            launch(&above_host, &wrong_entry, &HostPolicy::default())
                .unwrap_err()
                .code,
            RuntimeErrorCode::EntryNotFound
        );
    }

    #[test]
    fn preflight_records_effective_limits_and_optional_grants() {
        let workspace = TestWorkspace::new();
        let artifact = verified_runtime_artifact(
            vec![],
            vec!["fs.read".to_owned()],
            vec![("instructions".to_owned(), 100)],
        );
        let mut policy = HostPolicy::default();
        policy.granted_capabilities.insert("fs.read".to_owned());
        policy.workspace_root = Some(workspace.0.clone());
        policy.workspace_rights = Rights::READ_ONLY;
        let outcome = launch(
            &artifact,
            &LaunchRequest {
                entry: "main".to_owned(),
                input: serde_json::Value::Null,
            },
            &policy,
        )
        .unwrap();
        assert_eq!(outcome.output, serde_json::json!(7));
        assert_eq!(outcome.effective_limits.instructions, 100);
        assert_eq!(
            outcome.optional_grants,
            BTreeSet::from(["fs.read".to_owned()])
        );
        assert_eq!(
            outcome.effective_manifest_grants,
            BTreeSet::from(["fs.read".to_owned()])
        );
    }

    #[test]
    fn capability_inspection_uses_the_frozen_effective_manifest_grants() {
        let workspace = TestWorkspace::new();
        let artifact = verified_capability_inspection_artifact(
            vec!["fs.read".to_owned()],
            vec![
                "fs.write".to_owned(),
                "permission.request_external_fs".to_owned(),
            ],
        );
        let mut policy = HostPolicy::default();
        policy.granted_capabilities.extend([
            "agent.ask".to_owned(),
            "fs.read".to_owned(),
            "fs.write".to_owned(),
            "net.http_get".to_owned(),
            "permission.request_external_fs".to_owned(),
        ]);
        policy.workspace_root = Some(workspace.0.clone());
        policy.workspace_rights = Rights::READ_ONLY;

        let outcome = launch(&artifact, &launch_request(), &policy).unwrap();
        let expected = BTreeSet::from([
            "fs.read".to_owned(),
            "permission.request_external_fs".to_owned(),
        ]);
        assert_eq!(outcome.effective_manifest_grants, expected);
        assert_eq!(
            outcome.output,
            serde_json::json!(["fs.read", "permission.request_external_fs"])
        );
        assert_eq!(outcome.effects, 0);

        let mut replay = ReplayToolEffect {
            calls: 0,
            replayed: true,
            finalization_error: false,
        };
        let mut providers = RuntimeProviders {
            effect_override: Some(&mut replay),
            ..RuntimeProviders::default()
        };
        let mut cancellation = NeverCancel;
        let mut observer = NoObserver;
        let replayed = launch_with_context(
            &artifact,
            &launch_request(),
            &policy,
            &mut providers,
            &mut cancellation,
            &mut observer,
        )
        .unwrap();
        assert_eq!(replayed.effective_manifest_grants, expected);
        assert_eq!(replayed.output, outcome.output);
        assert_eq!(replay.calls, 0);
    }

    #[test]
    fn prepared_workspace_authority_is_not_reopened_at_execution() {
        let workspace = TestWorkspace::new();
        let artifact = verified_filesystem_artifact(vec!["fs.read".to_owned()], vec![]);
        let mut policy = HostPolicy::default();
        policy.granted_capabilities.insert("fs.read".to_owned());
        policy.workspace_root = Some(workspace.0.clone());
        policy.workspace_rights = Rights::READ_ONLY;
        let prepared = prepare_launch(&artifact, &launch_request(), &policy).unwrap();

        let moved = workspace.0.with_extension("prepared");
        std::fs::rename(&workspace.0, &moved).unwrap();
        std::fs::create_dir(&workspace.0).unwrap();
        std::fs::write(workspace.0.join("note.txt"), "replacement").unwrap();
        let mut providers = RuntimeProviders::default();
        let mut cancellation = NeverCancel;
        let mut observer = NoObserver;
        let result = execute_prepared_with_context(
            prepared,
            &mut providers,
            &mut cancellation,
            &mut observer,
        );
        std::fs::remove_dir_all(&workspace.0).unwrap();
        std::fs::rename(&moved, &workspace.0).unwrap();

        assert_eq!(
            result.unwrap().output,
            serde_json::json!({"tag":"Ok","value":"hello"})
        );
    }

    #[test]
    fn required_filesystem_capability_needs_grant_root_and_physical_right() {
        let workspace = TestWorkspace::new();
        let artifact = verified_runtime_artifact(vec!["fs.read".to_owned()], vec![], vec![]);
        let request = launch_request();

        let mut no_root = HostPolicy::default();
        no_root.granted_capabilities.insert("fs.read".to_owned());
        no_root.workspace_rights = Rights::READ_ONLY;
        assert_eq!(
            launch(&artifact, &request, &no_root).unwrap_err().code,
            RuntimeErrorCode::CapabilityDenied
        );
        let strict_no_root = HostPolicy {
            strict_preflight_authority: true,
            ..no_root.clone()
        };
        assert_eq!(
            prepare_launch(&artifact, &request, &strict_no_root)
                .unwrap_err()
                .code,
            RuntimeErrorCode::ManifestInvalid
        );

        let mut no_right = no_root.clone();
        no_right.workspace_root = Some(workspace.0.clone());
        no_right.workspace_rights = Rights::NONE;
        assert_eq!(
            launch(&artifact, &request, &no_right).unwrap_err().code,
            RuntimeErrorCode::CapabilityDenied
        );

        let mut available = no_right;
        available.workspace_rights = Rights::READ_ONLY;
        assert_eq!(
            launch(&artifact, &request, &available).unwrap().output,
            serde_json::json!(7)
        );
    }

    #[test]
    fn optional_filesystem_denial_is_a_typed_operation_result() {
        let workspace = TestWorkspace::new();
        let artifact = verified_filesystem_artifact(vec![], vec!["fs.read".to_owned()]);
        let mut missing_provider = HostPolicy::default();
        missing_provider
            .granted_capabilities
            .insert("fs.read".to_owned());
        missing_provider.workspace_root = Some(workspace.0.join("missing"));
        missing_provider.workspace_rights = Rights::READ_ONLY;
        let mut missing_right = missing_provider.clone();
        missing_right.workspace_root = Some(workspace.0.clone());
        missing_right.workspace_rights = Rights::NONE;
        for policy in [HostPolicy::default(), missing_provider, missing_right] {
            let outcome = launch(&artifact, &launch_request(), &policy).unwrap();
            assert_eq!(
                outcome.output,
                serde_json::json!({
                    "tag": "Err",
                    "value": {
                        "code": "fs.permission_denied",
                        "message": "the workspace capability was not granted"
                    }
                })
            );
            assert_eq!(outcome.effects, 1);
            assert!(outcome.optional_grants.is_empty());
        }
    }

    #[test]
    fn available_optional_filesystem_grant_executes_through_the_broker() {
        let workspace = TestWorkspace::new();
        let artifact = verified_filesystem_artifact(vec![], vec!["fs.read".to_owned()]);
        let mut policy = HostPolicy::default();
        policy.granted_capabilities.insert("fs.read".to_owned());
        policy.workspace_root = Some(workspace.0.clone());
        policy.workspace_rights = Rights::READ_ONLY;

        let outcome = launch(&artifact, &launch_request(), &policy).unwrap();
        assert_eq!(
            outcome.output,
            serde_json::json!({"tag": "Ok", "value": "hello"})
        );
        assert_eq!(
            outcome.optional_grants,
            BTreeSet::from(["fs.read".to_owned()])
        );
    }

    #[test]
    fn optional_grant_requires_every_availability_intersection() {
        let workspace = TestWorkspace::new();
        for (policy_grant, root, physical_right, entry_effect, expected) in [
            (true, true, true, true, true),
            (false, true, true, true, false),
            (true, false, true, true, false),
            (true, true, false, true, false),
            (true, true, true, false, false),
        ] {
            let effects = if entry_effect {
                vec!["fs.read".to_owned()]
            } else {
                vec![]
            };
            let artifact = verified_runtime_artifact_with_effects(
                vec![],
                vec!["fs.read".to_owned()],
                vec![],
                effects,
            );
            let mut policy = HostPolicy::default();
            if policy_grant {
                policy.granted_capabilities.insert("fs.read".to_owned());
            }
            if root {
                policy.workspace_root = Some(workspace.0.clone());
            }
            if physical_right {
                policy.workspace_rights = Rights::READ_ONLY;
            }
            let outcome = launch(&artifact, &launch_request(), &policy).unwrap();
            assert_eq!(outcome.optional_grants.contains("fs.read"), expected);
        }

        let artifact = verified_runtime_artifact(vec![], vec!["fs.read".to_owned()], vec![]);
        let mut unavailable_provider = HostPolicy::default();
        unavailable_provider
            .granted_capabilities
            .insert("fs.read".to_owned());
        unavailable_provider.workspace_rights = Rights::READ_ONLY;
        unavailable_provider.workspace_root = Some(workspace.0.join("missing"));
        assert!(
            launch(&artifact, &launch_request(), &unavailable_provider)
                .unwrap()
                .optional_grants
                .is_empty()
        );

        let read_and_write = verified_runtime_artifact_with_effects(
            vec![],
            vec!["fs.read".to_owned(), "fs.write".to_owned()],
            vec![],
            vec!["fs.read".to_owned(), "fs.write".to_owned()],
        );
        let mut partial_rights = HostPolicy::default();
        partial_rights
            .granted_capabilities
            .extend(["fs.read".to_owned(), "fs.write".to_owned()]);
        partial_rights.workspace_root = Some(workspace.0.clone());
        partial_rights.workspace_rights = Rights::READ_ONLY;
        assert_eq!(
            launch(&read_and_write, &launch_request(), &partial_rights)
                .unwrap()
                .optional_grants,
            BTreeSet::from(["fs.read".to_owned()])
        );
        partial_rights.workspace_rights = Rights::READ_WRITE;
        assert_eq!(
            launch(&read_and_write, &launch_request(), &partial_rights)
                .unwrap()
                .optional_grants,
            BTreeSet::from(["fs.read".to_owned(), "fs.write".to_owned()])
        );
    }

    #[test]
    fn outcome_records_all_effective_preflight_ceilings() {
        let artifact = verified_runtime_artifact_with_effects(
            vec![],
            vec![],
            vec![
                ("effects".to_owned(), 7),
                ("fs_entries".to_owned(), 2),
                ("fs_file_bytes".to_owned(), 3),
                ("fs_operations".to_owned(), 6),
                ("fs_read_bytes".to_owned(), 5),
                ("fs_write_bytes".to_owned(), 4),
                ("http_response_headers".to_owned(), 3),
                ("input_bytes".to_owned(), 8),
                ("instructions".to_owned(), 100),
                ("output_bytes".to_owned(), 9),
                ("response_attempts".to_owned(), 2),
            ],
            vec![],
        );
        let outcome = launch(&artifact, &launch_request(), &HostPolicy::default()).unwrap();
        assert_eq!(outcome.effective_limits.instructions, 100);
        assert_eq!(outcome.effective_input_bytes, 8);
        assert_eq!(outcome.effective_output_bytes, 9);
        assert_eq!(outcome.effective_effects, 7);
        assert_eq!(outcome.effective_response_attempts, 2);
        assert_eq!(outcome.effective_workspace_limits.max_entries, 2);
        assert_eq!(outcome.effective_workspace_limits.max_file_bytes, 3);
        assert_eq!(outcome.effective_workspace_limits.max_operations, 6);
        assert_eq!(outcome.effective_workspace_limits.max_read_bytes, 5);
        assert_eq!(outcome.effective_workspace_limits.max_write_bytes, 4);
        assert_eq!(outcome.effective_http_limits.max_response_headers, 3);
        assert_eq!(
            effective_http_limits(
                HttpLimits::default(),
                &[("http_response_headers".to_owned(), 3)],
            )
            .max_response_headers,
            3
        );
    }

    #[test]
    fn workspace_handles_reject_stale_and_cross_generation_use() {
        let mut first = BrokerEffects::new(
            41,
            None,
            ExecutionAccounting::new(WorkspaceLimits::default()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            BTreeMap::new(),
            None,
            vec![],
            vec![],
            vec![],
            Duration::MAX,
            1024 * 1024,
            3,
            EffectiveAuthority {
                filesystem: Rights::READ_ONLY,
                permission_request: false,
                http_get: false,
            },
            10,
        );
        let handle = first.workspace().unwrap();
        let arguments = [Value::Workspace(handle), Value::String("note.txt".into())];
        let denied = first
            .call(FsOperation::ReadText, &arguments)
            .expect("same-generation denied operation returns a typed result");
        assert_eq!(
            value_to_json(
                &denied,
                &ValueType::Result(
                    Box::new(ValueType::String),
                    Box::new(allen_bytecode::file_error_type()),
                ),
                &[],
            )
            .unwrap()["value"]["code"],
            "fs.permission_denied"
        );

        let mut second = BrokerEffects::new(
            42,
            None,
            ExecutionAccounting::new(WorkspaceLimits::default()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            BTreeMap::new(),
            None,
            vec![],
            vec![],
            vec![],
            Duration::MAX,
            1024 * 1024,
            3,
            EffectiveAuthority {
                filesystem: Rights::READ_ONLY,
                permission_request: false,
                http_get: false,
            },
            10,
        );
        assert_eq!(
            second.call(FsOperation::ReadText, &arguments),
            Err(VmError::CapabilityMissing)
        );
        let wrong_index = [
            Value::Workspace(WorkspaceValue::new(41, 1, 0)),
            Value::String("note.txt".into()),
        ];
        assert_eq!(
            first.call(FsOperation::ReadText, &wrong_index),
            Err(VmError::CapabilityMissing)
        );
        let forged_nonce = [
            Value::Workspace(WorkspaceValue::new(41, 0, handle.nonce().wrapping_add(1))),
            Value::String("note.txt".into()),
        ];
        assert_eq!(
            first.call(FsOperation::ReadText, &forged_nonce),
            Err(VmError::CapabilityMissing)
        );
        first.expire();
        assert_eq!(
            first.call(FsOperation::ReadText, &arguments),
            Err(VmError::CapabilityMissing)
        );
    }

    struct TestGrantProvider;

    impl ExternalGrantDecisionProvider for TestGrantProvider {
        fn decide(
            &mut self,
            _request: &ExternalGrantRequest,
        ) -> Result<ExternalGrantDecision, VmError> {
            Ok(ExternalGrantDecision::Deny)
        }
    }

    fn permission_request(path: &Path) -> Value {
        Value::Record(
            vec![
                (
                    "access".into(),
                    Value::ExternalFsAccess(ExternalFsAccess::Read),
                ),
                (
                    "path".into(),
                    Value::String(path.to_string_lossy().into_owned().into()),
                ),
                ("reason".into(), Value::String("read one test file".into())),
            ]
            .into(),
        )
    }

    fn grant_broker(
        generation: u64,
        provider: Option<&mut dyn ExternalGrantDecisionProvider>,
    ) -> BrokerEffects<'_> {
        BrokerEffects::new(
            generation,
            None,
            ExecutionAccounting::new(WorkspaceLimits::default()),
            None,
            provider,
            None,
            None,
            None,
            None,
            None,
            BTreeMap::new(),
            None,
            vec![],
            vec![],
            vec![],
            Duration::MAX,
            1024 * 1024,
            3,
            EffectiveAuthority {
                filesystem: Rights::READ_WRITE,
                permission_request: true,
                http_get: false,
            },
            20,
        )
    }

    #[test]
    fn external_provider_denial_returns_typed_permission_denied() {
        let workspace = TestWorkspace::new();
        let target = workspace.0.join("note.txt").canonicalize().unwrap();
        let mut provider = TestGrantProvider;
        let mut broker = grant_broker(73, Some(&mut provider));
        let response = broker
            .call(
                FsOperation::PermissionRequestFile,
                &[permission_request(&target)],
            )
            .unwrap();
        let Value::Enum(result) = response else {
            panic!("denial must return a typed result")
        };
        assert_eq!(result.variant, 1);
        assert_eq!(broker.capabilities.len(), 1);
    }

    #[test]
    fn singular_grant_byte_ceiling_uses_effective_limits_and_rights() {
        let limits = WorkspaceLimits {
            max_path_bytes: 101,
            max_path_depth: 11,
            max_file_bytes: 10,
            max_entries: 12,
            max_operations: 13,
            max_read_bytes: 7,
            max_write_bytes: 3,
        };
        assert_eq!(grant_max_bytes(limits, Rights::READ_ONLY), 7);
        assert_eq!(grant_max_bytes(limits, Rights::new(false, true)), 3);
        assert_eq!(grant_max_bytes(limits, Rights::READ_WRITE), 3);

        let narrowed = grant_limits(limits, 2);
        assert_eq!(narrowed.max_file_bytes, 2);
        assert_eq!(narrowed.max_read_bytes, 2);
        assert_eq!(narrowed.max_write_bytes, 2);
        assert_eq!(narrowed.max_path_bytes, limits.max_path_bytes);
        assert_eq!(narrowed.max_path_depth, limits.max_path_depth);
        assert_eq!(narrowed.max_entries, limits.max_entries);
        assert_eq!(narrowed.max_operations, limits.max_operations);
    }

    struct TestResolver;
    impl Resolver for TestResolver {
        fn resolve(
            &self,
            _host: &str,
            port: u16,
            _timeout: Duration,
        ) -> Result<Vec<SocketAddr>, HttpError> {
            Ok(vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                port,
            )])
        }
    }

    struct TestTransport;
    impl Transport for TestTransport {
        fn get(&self, request: &TransportRequest) -> Result<TransportResponse, HttpError> {
            Ok(TransportResponse {
                status: 200,
                headers: vec![("content-type".to_owned(), b"text/plain".to_vec())],
                header_bytes: 24,
                compressed_body: b"hello".to_vec(),
                peer_address: request.selected_address,
            })
        }
    }

    struct TestClock;
    impl Clock for TestClock {
        fn now(&self) -> Duration {
            Duration::ZERO
        }
    }

    #[derive(Default)]
    struct CapturingHttpFactory {
        origins: Vec<String>,
        denied_addresses: BTreeSet<IpAddr>,
        limits: Option<HttpLimits>,
    }

    impl HttpBrokerFactory for CapturingHttpFactory {
        fn create(
            &mut self,
            origins: &[String],
            denied_addresses: &BTreeSet<IpAddr>,
            limits: HttpLimits,
        ) -> Result<HttpBroker, HttpError> {
            self.origins = origins.to_vec();
            self.denied_addresses = denied_addresses.clone();
            self.limits = Some(limits);
            HttpBroker::new_with_denied_addresses(
                origins.to_vec(),
                denied_addresses.clone(),
                limits,
                Box::new(TestResolver),
                Box::new(TestTransport),
                Box::new(TestClock),
            )
        }
    }

    struct PanickingHttpFactory;

    impl HttpBrokerFactory for PanickingHttpFactory {
        fn create(
            &mut self,
            _origins: &[String],
            _denied_addresses: &BTreeSet<IpAddr>,
            _limits: HttpLimits,
        ) -> Result<HttpBroker, HttpError> {
            panic!("CANARY http factory panic")
        }
    }

    #[test]
    fn preflight_http_factory_panics_are_safe_runtime_panics() {
        let artifact = verified_runtime_artifact_with_effects_and_origins(
            vec![],
            vec!["net.http_get".to_owned()],
            vec![],
            vec!["net.http_get".to_owned()],
            vec!["https://example.com".to_owned()],
        );
        let mut policy = HostPolicy::default();
        policy
            .granted_capabilities
            .insert("net.http_get".to_owned());
        policy.http_origins.insert("https://example.com".to_owned());
        let error = prepare_launch_with_http_factory(
            &artifact,
            &launch_request(),
            &policy,
            &mut PanickingHttpFactory,
        )
        .unwrap_err();
        assert_eq!(error.code, RuntimeErrorCode::Panic);
        assert_eq!(error.message, "runtime invariant failed");
        assert!(!error.message.contains("CANARY"));
    }

    #[test]
    fn launch_intersects_http_origins_and_limits_before_broker_creation() {
        let artifact = verified_runtime_artifact_with_effects_and_origins(
            vec![],
            vec!["net.http_get".to_owned()],
            vec![("http_requests".to_owned(), 2)],
            vec!["net.http_get".to_owned()],
            vec![
                "https://denied.example".to_owned(),
                "https://example.com".to_owned(),
            ],
        );
        let mut policy = HostPolicy::default();
        policy
            .granted_capabilities
            .insert("net.http_get".to_owned());
        policy.http_origins.insert("https://example.com".to_owned());
        policy
            .denied_net_addresses
            .insert(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 4)));
        policy.http_limits.max_requests = 9;
        let mut factory = CapturingHttpFactory::default();
        let outcome = {
            let mut providers = RuntimeProviders {
                effect_override: None,
                external_grants: None,
                http_factory: Some(&mut factory),
                tools: None,
                invoking_agent: None,
                model: None,
                user: None,
                sub_agent: None,
            };
            launch_with_providers(&artifact, &launch_request(), &policy, &mut providers).unwrap()
        };
        assert_eq!(factory.origins, ["https://example.com"]);
        assert_eq!(factory.denied_addresses, policy.denied_net_addresses);
        assert_eq!(factory.limits.unwrap().max_requests, 2);
        assert_eq!(outcome.effective_http_origins, policy.http_origins);
        assert_eq!(outcome.effective_http_limits.max_requests, 2);
        assert_eq!(
            outcome.optional_grants,
            BTreeSet::from(["net.http_get".to_owned()])
        );

        let required = verified_runtime_artifact_with_effects_and_origins(
            vec!["net.http_get".to_owned()],
            vec![],
            vec![],
            vec!["net.http_get".to_owned()],
            vec!["https://example.com".to_owned()],
        );
        policy.http_origins.clear();
        assert_eq!(
            launch(&required, &launch_request(), &policy)
                .unwrap_err()
                .code,
            RuntimeErrorCode::CapabilityDenied
        );
    }

    #[test]
    fn http_get_routes_through_the_restricted_broker() {
        let http = HttpBroker::new(
            ["https://example.com".to_owned()],
            HttpLimits::default(),
            Box::new(TestResolver),
            Box::new(TestTransport),
            Box::new(TestClock),
        )
        .unwrap();
        let mut broker = BrokerEffects::new(
            80,
            None,
            ExecutionAccounting::new(WorkspaceLimits::default()),
            Some(http),
            None,
            None,
            None,
            None,
            None,
            None,
            BTreeMap::new(),
            None,
            vec![],
            vec![],
            vec![],
            Duration::MAX,
            1024 * 1024,
            3,
            EffectiveAuthority {
                filesystem: Rights::NONE,
                permission_request: false,
                http_get: true,
            },
            10,
        );
        let response = broker
            .call(
                FsOperation::HttpGet,
                &[Value::String("https://example.com/report".into())],
            )
            .unwrap();
        let json = value_to_json(
            &response,
            &ValueType::Result(
                Box::new(allen_bytecode::http_response_type()),
                Box::new(allen_bytecode::network_error_type()),
            ),
            &[],
        )
        .unwrap();
        assert_eq!(json["value"]["status"], 200);
        assert_eq!(
            json["value"]["body"],
            serde_json::json!({"$bytes":"aGVsbG8="})
        );
        assert_eq!(broker.http_usage().requests, 1);
        assert_eq!(broker.http_usage().response_headers, 1);
        assert_eq!(broker.effects, 1);
    }

    #[test]
    fn transcript_validation_rejects_bad_timestamp_and_oversized_projection() {
        let snapshot = TranscriptSnapshot {
            snapshot_id: "snapshot".to_owned(),
            session_id: "session".to_owned(),
            policy_version: "v1".to_owned(),
            captured_at: "2026-08-14T12:00:00+00:00".to_owned(),
            truncated: false,
            messages: Vec::new(),
        };
        assert_eq!(
            validate_transcript(&snapshot, 1, 1024),
            Err(VmError::AgentResponseSchema)
        );
        let mut valid = snapshot;
        valid.captured_at = "2026-08-14T12:00:00Z".to_owned();
        valid.messages.push(TranscriptMessage {
            id: Some("m1".to_owned()),
            role: TranscriptRole::Assistant,
            time: None,
            content: vec![TranscriptPart::Redacted {
                reason_code: "policy".to_owned(),
            }],
        });
        assert_eq!(
            validate_transcript(&valid, 1, 1),
            Err(VmError::ResourceLimit {
                resource: "transcript_bytes"
            })
        );
        let mut unordered = valid;
        unordered.messages[0].time = Some("2026-08-14T12:00:01Z".to_owned());
        unordered.messages.push(TranscriptMessage {
            id: Some("m2".to_owned()),
            role: TranscriptRole::User,
            time: Some("2026-08-14T12:00:00.999Z".to_owned()),
            content: vec![TranscriptPart::Omitted {
                content_kind: "text".to_owned(),
                count: 1,
            }],
        });
        assert_eq!(
            validate_transcript(&unordered, 2, 1024 * 1024),
            Err(VmError::AgentResponseSchema)
        );
    }

    #[test]
    fn canonical_text_prompt_distinguishes_absence_from_json_null() {
        let absent = canonical_text_prompt(&StructuredPrompt {
            system: "Review.".to_owned(),
            context: None,
            data: None,
            max_attempts: 3,
        });
        let present_null = canonical_text_prompt(&StructuredPrompt {
            system: "Review.".to_owned(),
            context: Some(serde_json::Value::Null),
            data: Some(serde_json::Value::Null),
            max_attempts: 3,
        });
        assert_ne!(absent, present_null);
        assert!(absent.contains(r#"{"tag":"None"}"#));
        assert!(present_null.contains(r#"{"tag":"Some","value":null}"#));
        assert!(absent.contains("POLICY "));
    }

    #[test]
    fn replay_finalization_observes_post_output_boundary_terminal_channel() {
        let artifact = verified_runtime_artifact_for_version(
            BYTECODE_VERSION,
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        let mut oversized = prepare_launch(
            &artifact,
            &launch_request(),
            &HostPolicy {
                output_bytes: 0,
                ..HostPolicy::default()
            },
        )
        .unwrap();
        oversized.effective_output_bytes = 0;
        let mut capture = FinalOutcomeCapture::default();
        let error = execute_prepared_with_context(
            oversized,
            &mut RuntimeProviders {
                effect_override: Some(&mut capture),
                ..RuntimeProviders::default()
            },
            &mut NeverCancel,
            &mut NoObserver,
        )
        .unwrap_err();
        assert_eq!(error.code, RuntimeErrorCode::ResourceLimit);
        assert_eq!(capture.0, ["resource.limit"]);

        let mut invalid =
            prepare_launch(&artifact, &launch_request(), &HostPolicy::default()).unwrap();
        invalid.output_type = ValueType::String;
        let mut capture = FinalOutcomeCapture::default();
        let error = execute_prepared_with_context(
            invalid,
            &mut RuntimeProviders {
                effect_override: Some(&mut capture),
                ..RuntimeProviders::default()
            },
            &mut NeverCancel,
            &mut NoObserver,
        )
        .unwrap_err();
        assert_eq!(error.code, RuntimeErrorCode::Panic);
        assert_eq!(capture.0, ["terminal"]);

        let mut invalid =
            prepare_launch(&artifact, &launch_request(), &HostPolicy::default()).unwrap();
        invalid.output_type = ValueType::String;
        let mut replay = ReplayToolEffect {
            calls: 0,
            replayed: true,
            finalization_error: true,
        };
        let error = execute_prepared_with_context(
            invalid,
            &mut RuntimeProviders {
                effect_override: Some(&mut replay),
                ..RuntimeProviders::default()
            },
            &mut NeverCancel,
            &mut NoObserver,
        )
        .unwrap_err();
        assert_eq!(error.code, RuntimeErrorCode::ReplayRuntimeDiverged);
        assert_eq!(replay.calls, 0);
    }

    #[test]
    fn terminal_messages_are_fixed_bounded_and_content_free() {
        let private = "provider-secret".repeat(2_048);
        let private: &'static str = Box::leak(private.into_boxed_str());
        let cases = [
            (
                VmError::ArithmeticOverflow,
                RuntimeErrorCode::ArithmeticOverflow,
            ),
            (VmError::DivisionByZero, RuntimeErrorCode::DivisionByZero),
            (
                VmError::IndexOutOfBounds,
                RuntimeErrorCode::IndexOutOfBounds,
            ),
            (VmError::MapKeyNotFound, RuntimeErrorCode::MapKeyNotFound),
            (VmError::DuplicateMapKey, RuntimeErrorCode::DuplicateMapKey),
            (
                VmError::ResourceLimit { resource: private },
                RuntimeErrorCode::ResourceLimit,
            ),
            (
                VmError::Timeout { resource: private },
                RuntimeErrorCode::Timeout,
            ),
            (VmError::Cancelled, RuntimeErrorCode::Cancelled),
            (VmError::Invariant(private), RuntimeErrorCode::Panic),
            (
                VmError::ProtocolViolation,
                RuntimeErrorCode::ProtocolViolation,
            ),
            (VmError::ReplayDiverged, RuntimeErrorCode::ReplayDiverged),
            (
                VmError::AgentUnavailable,
                RuntimeErrorCode::ProtocolViolation,
            ),
        ];
        for (error, expected) in cases {
            let code = runtime_vm_error_code(&error);
            assert_eq!(code, expected);
            let message = safe_terminal_message(code);
            assert!(message.len() <= 1_024);
            assert!(!message.contains("provider-secret"));
        }
        assert_eq!(
            runtime_vm_error_code(&VmError::Invariant(private)),
            RuntimeErrorCode::Panic
        );
    }
}
