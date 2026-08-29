//! Deterministic effect mocks and canonical `ALLEN-REPLAY/3` journals.
//!
//! The testkit intentionally records only canonical bytes plus schema digests.
//! It never serializes VM capability, future, task, closure, workspace, or
//! sub-agent values.  It is execution-local: every [`ReplaySession`] owns its
//! cursor and pending table.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::rc::Rc;

use allen_bytecode::{
    BYTECODE_VERSION, EffectOperation, EnumPayloadType, EnumType, MAX_VALUE_NESTING, ValueType,
    VerifiedArtifact, canonical_value_type_bytes, effect_result_type,
};
use allen_exec::CommandPattern;
use allen_schema::{FrozenCatalog, ToolSchema};
use allen_vm::{
    CancellationSource, EffectExecutionBinding, EffectExecutionOutcome, EffectPoll, EffectProvider,
    EnumIdentity, EnumPayload, EnumValue, ExecutionCapabilities, PendingEffectId, SubAgentValue,
    Value, VmError, WorkspaceValue, decode_canonical_with_limit, encode_canonical_with_limit,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Canonical replay document identifier.
pub const REPLAY_FORMAT: &str = "ALLEN-REPLAY/3";

/// Conservative resource limits for a replay document and its payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplayLimits {
    /// Maximum number of entries in one execution-local journal.
    pub entries: usize,
    /// Maximum request or result payload bytes in one entry.
    pub payload_bytes: usize,
    /// Maximum canonical JSON bytes for the whole document.
    pub document_bytes: usize,
}

impl Default for ReplayLimits {
    fn default() -> Self {
        Self {
            entries: 4_096,
            payload_bytes: 1 << 20,
            document_bytes: 8 << 20,
        }
    }
}

/// Stable error category returned when recording or replay cannot proceed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayError {
    /// The journal would exceed one of its configured bounds.
    LimitExceeded,
    /// An opaque, non-serializable, or caller-labelled sensitive value was refused.
    RefusedValue,
    /// The JSON document is malformed, noncanonical, or has a wrong format tag.
    InvalidJournal,
    /// A recorded provider error was malformed.
    InvalidOutcome,
    /// Request, schema, order, pending state, or leftovers differ from the journal.
    ReplayDiverged,
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::LimitExceeded => "replay limit exceeded",
            Self::RefusedValue => "replay recording refused this value",
            Self::InvalidJournal => "replay journal is invalid or noncanonical",
            Self::InvalidOutcome => "replay outcome is invalid",
            Self::ReplayDiverged => "effect replay diverged",
        })
    }
}

impl std::error::Error for ReplayError {}

/// One effect family as observed at the low-level provider boundary.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum EffectKind {
    /// An ordinary bytecode effect operation, such as `fs.read`.
    Call(String),
    /// A typed tool contract index.
    Tool(u32),
    /// An invoking agent, model, or user operation.
    Agent(String),
    /// One of the four child-agent operations.
    SubAgent(String),
}

/// A fully deterministic effect request fingerprint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectRequest {
    /// Provider effect family and stable operation identity.
    pub kind: EffectKind,
    /// Canonical input bytes, never debug/display output.
    pub request: Vec<u8>,
    /// SHA-256 of `canonical_value_type_bytes(result_type)`.
    pub schema_digest: [u8; 32],
}

impl EffectRequest {
    /// Construct a request from already canonical, bounded input bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::LimitExceeded`] when the payload exceeds `limits`.
    pub fn new(
        kind: EffectKind,
        request: Vec<u8>,
        result_type: &ValueType,
        limits: ReplayLimits,
    ) -> Result<Self, ReplayError> {
        if request.len() > limits.payload_bytes {
            return Err(ReplayError::LimitExceeded);
        }
        decode_canonical_with_limit(
            &request,
            u64::try_from(limits.payload_bytes).map_err(|_| ReplayError::LimitExceeded)?,
        )
        .map_err(|_| ReplayError::RefusedValue)?;
        Ok(Self {
            kind,
            request,
            schema_digest: schema_digest(result_type),
        })
    }

    /// Construct a request from a serializable VM value.
    ///
    /// Canonical VM encoding refuses opaque and affine values. Sensitive values
    /// are refused by the default recorder policy and must use a redaction hook.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::RefusedValue`] for opaque or noncanonical values.
    pub fn from_value(
        kind: EffectKind,
        request: &Value,
        result_type: &ValueType,
        limits: ReplayLimits,
    ) -> Result<Self, ReplayError> {
        let payload = encode_canonical_with_limit(
            request,
            u64::try_from(limits.payload_bytes).map_err(|_| ReplayError::LimitExceeded)?,
        )
        .map_err(|_| ReplayError::RefusedValue)?;
        Self::new(kind, payload, result_type, limits)
    }
}

/// A validated provider completion captured without decoding it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EffectOutcome {
    /// Canonical provider result bytes.
    Ok(Vec<u8>),
    /// A closed, context-free provider error. Free-form messages are intentionally excluded.
    Err { error: RecordedVmError },
}

/// The replay-safe subset of [`VmError`] values that can cross a provider boundary.
///
/// Free-form context is excluded; invariant and protocol failures use only
/// registered content-free terminal identities.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RecordedVmError {
    ArithmeticOverflow,
    DivisionByZero,
    IndexOutOfBounds,
    MapKeyNotFound,
    ListLengthMismatch,
    DuplicateMapKey,
    ResourceLimit { resource: ReplayResource },
    Timeout { resource: ReplayResource },
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
    RuntimePanic,
    ProtocolViolation,
}

/// Exact terminal state recorded after the final effect completion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
pub enum ReplayExecutionOutcome {
    Completed,
    Stopped { reason: String },
    Failed { reason: String },
    Terminal { error: RecordedVmError },
}

impl ReplayExecutionOutcome {
    /// Convert one VM terminal error to its stable replay representation.
    ///
    /// # Errors
    ///
    /// Returns an error when a dynamic resource name is not replay-safe.
    pub fn terminal(error: &VmError) -> Result<Self, ReplayError> {
        if !matches!(
            error,
            VmError::ArithmeticOverflow
                | VmError::DivisionByZero
                | VmError::IndexOutOfBounds
                | VmError::MapKeyNotFound
                | VmError::DuplicateMapKey
                | VmError::ResourceLimit { .. }
                | VmError::Timeout { .. }
                | VmError::Cancelled
                | VmError::Invariant(_)
                | VmError::ProtocolViolation
                | VmError::ReplayRuntimeDiverged
        ) {
            return Err(ReplayError::RefusedValue);
        }
        Ok(Self::Terminal {
            error: recorded_vm_error(error)?,
        })
    }
}

/// Fixed resource names used in replay-safe runtime errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReplayResource {
    AgentInteractions,
    AgentOperations,
    AllocationBytes,
    CallDepth,
    Effects,
    Handles,
    Instructions,
    MaximumAllocationBytes,
    FailureReasonBytes,
    PendingEffects,
    PermissionOperations,
    SubAgentContextBytes,
    SubAgents,
    ToolOperations,
    TranscriptBytes,
    WallTime,
    FsFileTooLarge,
    FsTooManyEntries,
    FsOperations,
    FsReadBytes,
    FsWriteBytes,
    HttpRequests,
    HttpRedirects,
    HttpHeaderBytes,
    HttpCompressedBytes,
    HttpDecodedBytes,
    HttpDecompressionRatio,
}

impl ReplayResource {
    fn from_static(value: &'static str) -> Option<Self> {
        Some(match value {
            "agent_interactions" => Self::AgentInteractions,
            "agent_operations" => Self::AgentOperations,
            "allocation_bytes" => Self::AllocationBytes,
            "call_depth" => Self::CallDepth,
            "effects" => Self::Effects,
            "handles" => Self::Handles,
            "instructions" => Self::Instructions,
            "maximum_allocation_bytes" => Self::MaximumAllocationBytes,
            "failure_reason_bytes" => Self::FailureReasonBytes,
            "pending_effects" => Self::PendingEffects,
            "permission_operations" => Self::PermissionOperations,
            "sub_agent_context_bytes" => Self::SubAgentContextBytes,
            "sub_agents" => Self::SubAgents,
            "tool_operations" => Self::ToolOperations,
            "transcript_bytes" => Self::TranscriptBytes,
            "wall_time" => Self::WallTime,
            "fs.file_too_large" => Self::FsFileTooLarge,
            "fs.too_many_entries" => Self::FsTooManyEntries,
            "resource.fs_operations" => Self::FsOperations,
            "resource.fs_read_bytes" => Self::FsReadBytes,
            "resource.fs_write_bytes" => Self::FsWriteBytes,
            "resource.http_requests" => Self::HttpRequests,
            "resource.http_redirects" => Self::HttpRedirects,
            "resource.http_header_bytes" => Self::HttpHeaderBytes,
            "resource.http_compressed_bytes" => Self::HttpCompressedBytes,
            "resource.http_decoded_bytes" => Self::HttpDecodedBytes,
            "resource.http_decompression_ratio" => Self::HttpDecompressionRatio,
            _ => return None,
        })
    }

    const fn as_static(self) -> &'static str {
        match self {
            Self::AgentInteractions => "agent_interactions",
            Self::AgentOperations => "agent_operations",
            Self::AllocationBytes => "allocation_bytes",
            Self::CallDepth => "call_depth",
            Self::Effects => "effects",
            Self::Handles => "handles",
            Self::Instructions => "instructions",
            Self::MaximumAllocationBytes => "maximum_allocation_bytes",
            Self::FailureReasonBytes => "failure_reason_bytes",
            Self::PendingEffects => "pending_effects",
            Self::PermissionOperations => "permission_operations",
            Self::SubAgentContextBytes => "sub_agent_context_bytes",
            Self::SubAgents => "sub_agents",
            Self::ToolOperations => "tool_operations",
            Self::TranscriptBytes => "transcript_bytes",
            Self::WallTime => "wall_time",
            Self::FsFileTooLarge => "fs.file_too_large",
            Self::FsTooManyEntries => "fs.too_many_entries",
            Self::FsOperations => "resource.fs_operations",
            Self::FsReadBytes => "resource.fs_read_bytes",
            Self::FsWriteBytes => "resource.fs_write_bytes",
            Self::HttpRequests => "resource.http_requests",
            Self::HttpRedirects => "resource.http_redirects",
            Self::HttpHeaderBytes => "resource.http_header_bytes",
            Self::HttpCompressedBytes => "resource.http_compressed_bytes",
            Self::HttpDecodedBytes => "resource.http_decoded_bytes",
            Self::HttpDecompressionRatio => "resource.http_decompression_ratio",
        }
    }
}

impl EffectOutcome {
    /// Validate the wire-safe, bounded representation.
    ///
    /// # Errors
    ///
    /// Returns an error for oversized payloads or malformed stable error codes.
    pub fn validate(&self, limits: ReplayLimits) -> Result<(), ReplayError> {
        match self {
            Self::Ok(bytes) if bytes.len() <= limits.payload_bytes => decode_canonical_with_limit(
                bytes,
                u64::try_from(limits.payload_bytes).map_err(|_| ReplayError::LimitExceeded)?,
            )
            .map(|_| ())
            .map_err(|_| ReplayError::InvalidOutcome),
            Self::Ok(_) => Err(ReplayError::LimitExceeded),
            Self::Err { .. } => Ok(()),
        }
    }
}

/// Canonical journal header bound to one artifact, policy, and scheduler trace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplayHeader {
    /// Bytecode format whose exact provider result values are recorded.
    pub bytecode_version: u16,
    /// Digest of the verified artifact bytes.
    pub artifact_digest: [u8; 32],
    /// Digest of the verified entry/effect/tool contracts.
    #[serde(default, skip_serializing_if = "is_zero_digest")]
    pub contract_digest: [u8; 32],
    /// Digest of the language profile and source contract.
    pub language_digest: [u8; 32],
    /// Digest of the runtime build/profile.
    pub runtime_digest: [u8; 32],
    /// Digest of the effective execution policy.
    pub policy_digest: [u8; 32],
    /// Digest of the frozen tool catalog.
    pub catalog_digest: [u8; 32],
    /// Digest of the frozen capability registry and grant semantics.
    #[serde(default, skip_serializing_if = "is_zero_digest")]
    pub capability_digest: [u8; 32],
    /// Digest of the canonical stable error registry.
    #[serde(default, skip_serializing_if = "is_zero_digest")]
    pub error_registry_digest: [u8; 32],
    /// Canonical sorted unique effective manifest grants frozen before execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effective_manifest_grants: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_exec_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_exec_environment: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effective_exec_grants: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effective_exec_environment: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero_digest")]
    pub effective_exec_environment_digest: [u8; 32],
    #[serde(default, skip_serializing_if = "is_zero_digest")]
    pub pinned_exec_identity_digest: [u8; 32],
    /// Deterministic completion sequence for this execution.
    pub scheduler_completion_order: Vec<u64>,
}

impl ReplayHeader {
    /// Recreate the VM-local immutable inspection snapshot bound by this header.
    #[must_use]
    pub fn execution_capabilities(&self) -> ExecutionCapabilities {
        ExecutionCapabilities::new(self.effective_manifest_grants.iter().cloned())
    }
}

/// One ordered, complete effect observation with no raw input payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplayEntry {
    /// Zero-based canonical execution order.
    pub sequence: u64,
    /// Provider effect family and stable operation identity.
    pub effect: EffectKind,
    /// SHA-256 of `effect`, NUL, schema digest, NUL, and canonical arguments.
    pub request_digest: [u8; 32],
    /// SHA-256 of the strict result schema.
    pub schema_digest: [u8; 32],
    /// The validated completion.
    pub outcome: EffectOutcome,
}

/// Immutable deterministic replay document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReplayLog {
    format: String,
    header: ReplayHeader,
    entries: Vec<ReplayEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_outcome: Option<ReplayExecutionOutcome>,
}

impl ReplayLog {
    /// Build a canonical replay document after checking order and bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid ordering or configured size limits.
    pub fn new(
        header: ReplayHeader,
        entries: Vec<ReplayEntry>,
        execution_outcome: ReplayExecutionOutcome,
        limits: ReplayLimits,
    ) -> Result<Self, ReplayError> {
        validate_header(&header)?;
        validate_entries(&header, &entries, limits)?;
        validate_execution_outcome(Some(&execution_outcome), limits)?;
        let log = Self {
            format: REPLAY_FORMAT.to_owned(),
            header,
            entries,
            execution_outcome: Some(execution_outcome),
        };
        if log.to_json()?.len() > limits.document_bytes {
            return Err(ReplayError::LimitExceeded);
        }
        Ok(log)
    }

    /// Parse only exact canonical JSON generated by [`Self::to_json`].
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, noncanonical, or oversized JSON.
    pub fn from_json(json: &str, limits: ReplayLimits) -> Result<Self, ReplayError> {
        if json.len() > limits.document_bytes {
            return Err(ReplayError::LimitExceeded);
        }
        let log: Self = serde_json::from_str(json).map_err(|_| ReplayError::InvalidJournal)?;
        if log.format != REPLAY_FORMAT || log.execution_outcome.is_none() {
            return Err(ReplayError::InvalidJournal);
        }
        validate_header(&log.header)?;
        validate_entries(&log.header, &log.entries, limits)?;
        validate_execution_outcome(log.execution_outcome.as_ref(), limits)?;
        if log.to_json()? != json {
            return Err(ReplayError::InvalidJournal);
        }
        Ok(log)
    }

    /// Produce deterministic compact canonical JSON.
    ///
    /// # Errors
    ///
    /// Returns an error only if serialization cannot produce a valid journal.
    pub fn to_json(&self) -> Result<String, ReplayError> {
        serde_json::to_string(self).map_err(|_| ReplayError::InvalidJournal)
    }

    /// Ordered entries for test assertions and later host adapters.
    #[must_use]
    pub fn entries(&self) -> &[ReplayEntry] {
        &self.entries
    }

    /// Header bindings and deterministic scheduler completion order.
    #[must_use]
    pub fn header(&self) -> &ReplayHeader {
        &self.header
    }

    /// Exact final execution channel, when the recording owner supplied one.
    #[must_use]
    pub const fn execution_outcome(&self) -> Option<&ReplayExecutionOutcome> {
        self.execution_outcome.as_ref()
    }

    /// Attach the exact final execution channel before serialization.
    ///
    /// # Errors
    ///
    /// Returns an error if the resulting canonical document exceeds its bound.
    pub fn with_execution_outcome(
        mut self,
        outcome: ReplayExecutionOutcome,
        limits: ReplayLimits,
    ) -> Result<Self, ReplayError> {
        if self.format != REPLAY_FORMAT {
            return Err(ReplayError::InvalidJournal);
        }
        validate_header(&self.header)?;
        validate_execution_outcome(Some(&outcome), limits)?;
        self.execution_outcome = Some(outcome);
        if self.to_json()?.len() > limits.document_bytes {
            return Err(ReplayError::LimitExceeded);
        }
        Ok(self)
    }
}

/// Caller-supplied redaction policy for explicitly sensitive payloads.
pub trait Redactor {
    /// Return a safe replacement request and outcome, or refuse recording.
    ///
    /// # Errors
    ///
    /// Returns an error when safe redaction is not possible.
    fn redact(
        &self,
        request: &EffectRequest,
        outcome: &EffectOutcome,
        limits: ReplayLimits,
    ) -> Result<(EffectRequest, EffectOutcome), ReplayError>;

    /// Replace one caller-labelled sensitive stopped reason before it enters
    /// an unencrypted replay document.
    ///
    /// The fail-closed default prevents existing effect-only redactors from
    /// accidentally authorizing terminal text.
    ///
    /// # Errors
    ///
    /// Returns an error when this redactor cannot safely replace the reason.
    fn redact_stopped_reason(
        &self,
        _reason: &str,
        _limits: ReplayLimits,
    ) -> Result<String, ReplayError> {
        Err(ReplayError::RefusedValue)
    }
}

/// The secure default: sensitive values are never written to a replay log.
#[derive(Clone, Copy, Debug, Default)]
pub struct RefuseSensitive;

impl Redactor for RefuseSensitive {
    fn redact(
        &self,
        _request: &EffectRequest,
        _outcome: &EffectOutcome,
        _limits: ReplayLimits,
    ) -> Result<(EffectRequest, EffectOutcome), ReplayError> {
        Err(ReplayError::RefusedValue)
    }
}

/// A digest redactor that fails closed for executable replay data.
///
/// A generic redactor does not know the frozen request and result schemas. Any
/// digest-shaped replacement could therefore change the value type while
/// retaining the original schema digest, and low-entropy digests can disclose
/// their inputs by enumeration. Use an encrypted fixture or a
/// schema-aware redactor instead.
#[derive(Clone, Copy, Debug, Default)]
pub struct DigestRedactor;

impl Redactor for DigestRedactor {
    fn redact(
        &self,
        _request: &EffectRequest,
        _outcome: &EffectOutcome,
        _limits: ReplayLimits,
    ) -> Result<(EffectRequest, EffectOutcome), ReplayError> {
        Err(ReplayError::RefusedValue)
    }

    fn redact_stopped_reason(
        &self,
        _reason: &str,
        _limits: ReplayLimits,
    ) -> Result<String, ReplayError> {
        Err(ReplayError::RefusedValue)
    }
}

/// Recording side of an execution-local effect journal.
#[derive(Debug)]
pub struct Recorder {
    limits: ReplayLimits,
    header: ReplayHeader,
    next_sequence: u64,
    poisoned: bool,
    pending: BTreeMap<u64, (EffectRequest, bool)>,
    entries: BTreeMap<u64, ReplayEntry>,
}

#[cfg(test)]
fn test_replay_header() -> ReplayHeader {
    ReplayHeader {
        bytecode_version: BYTECODE_VERSION,
        artifact_digest: [1; 32],
        contract_digest: [2; 32],
        language_digest: [3; 32],
        runtime_digest: [4; 32],
        policy_digest: [5; 32],
        catalog_digest: [6; 32],
        capability_digest: [7; 32],
        error_registry_digest: [8; 32],
        effective_manifest_grants: Vec::new(),
        requested_exec_commands: Vec::new(),
        requested_exec_environment: Vec::new(),
        effective_exec_grants: Vec::new(),
        effective_exec_environment: Vec::new(),
        effective_exec_environment_digest: [0; 32],
        pinned_exec_identity_digest: [0; 32],
        scheduler_completion_order: Vec::new(),
    }
}

impl Recorder {
    #[cfg(test)]
    fn new(limits: ReplayLimits) -> Self {
        Self::with_header(limits, test_replay_header())
    }

    /// Begin a recording bound to explicit verified execution digests.
    #[must_use]
    pub const fn with_header(limits: ReplayLimits, header: ReplayHeader) -> Self {
        Self {
            limits,
            header,
            next_sequence: 0,
            poisoned: false,
            pending: BTreeMap::new(),
            entries: BTreeMap::new(),
        }
    }

    /// Assign a request sequence at dispatch time, before a live provider is called.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured entry bound is exhausted.
    pub fn start(&mut self, request: EffectRequest, sensitive: bool) -> Result<u64, ReplayError> {
        if self.poisoned {
            return Err(ReplayError::ReplayDiverged);
        }
        if usize::try_from(self.next_sequence).map_or(true, |value| value >= self.limits.entries) {
            return Err(ReplayError::LimitExceeded);
        }
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(ReplayError::LimitExceeded)?;
        self.pending.insert(sequence, (request, sensitive));
        Ok(sequence)
    }

    /// Record the completion of an already-dispatched request.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown requests, unsafe values, or configured bounds.
    pub fn complete(
        &mut self,
        sequence: u64,
        outcome: EffectOutcome,
        redactor: &dyn Redactor,
    ) -> Result<(), ReplayError> {
        if self.poisoned {
            return Err(ReplayError::ReplayDiverged);
        }
        let Some((request, sensitive)) = self.pending.get(&sequence).cloned() else {
            self.poisoned = true;
            return Err(ReplayError::ReplayDiverged);
        };
        let result = (|| {
            let (request, outcome) = if sensitive {
                let replacement = redactor.redact(&request, &outcome, self.limits)?;
                if replacement.0 != request || replacement.1 != outcome {
                    return Err(ReplayError::RefusedValue);
                }
                replacement
            } else {
                (request, outcome)
            };
            if request.request.len() > self.limits.payload_bytes {
                return Err(ReplayError::LimitExceeded);
            }
            outcome.validate(self.limits)?;
            self.entries.insert(
                sequence,
                ReplayEntry {
                    sequence,
                    effect: request.kind.clone(),
                    request_digest: request_digest(&request),
                    schema_digest: request.schema_digest,
                    outcome,
                },
            );
            self.header.scheduler_completion_order.push(sequence);
            self.pending.remove(&sequence);
            Ok(())
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    /// Record one completed effect. Sensitive values require a redactor.
    ///
    /// # Errors
    ///
    /// Returns an error for bounds, unsafe values, invalid outcomes, or failed redaction.
    pub fn record(
        &mut self,
        request: EffectRequest,
        outcome: EffectOutcome,
        sensitive: bool,
        redactor: &dyn Redactor,
    ) -> Result<(), ReplayError> {
        let sequence = self.start(request, sensitive)?;
        match self.complete(sequence, outcome, redactor) {
            Ok(()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Finalize the journal with the exact completed, stopped, or terminal channel.
    ///
    /// # Errors
    ///
    /// Returns an error for pending effects or a noncanonical/oversized outcome.
    pub fn finish_with_execution_outcome(
        self,
        outcome: ReplayExecutionOutcome,
    ) -> Result<ReplayLog, ReplayError> {
        self.finish_with_execution_outcome_policy(outcome, &RefuseAll, &RefuseSensitive)
    }

    #[cfg(test)]
    fn finish(self) -> Result<ReplayLog, ReplayError> {
        self.finish_with_execution_outcome(ReplayExecutionOutcome::Completed)
    }

    /// Finalize with an explicit policy for any untrusted stopped reason.
    ///
    /// # Errors
    ///
    /// Returns an error when pending effects remain, the policy refuses the
    /// reason, redaction fails, or the canonical document is invalid.
    pub fn finish_with_execution_outcome_policy(
        self,
        outcome: ReplayExecutionOutcome,
        policy: &dyn ReplayRecordingPolicy,
        redactor: &dyn Redactor,
    ) -> Result<ReplayLog, ReplayError> {
        if self.poisoned || !self.pending.is_empty() {
            return Err(ReplayError::ReplayDiverged);
        }
        let outcome = protect_execution_outcome(outcome, policy, redactor, self.limits)?;
        ReplayLog::new(
            self.header,
            self.entries.into_values().collect(),
            outcome,
            self.limits,
        )
    }
}

/// VM-independent pending token for deterministic mock scheduling.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PendingEffect(pub u64);

/// Execution-local replay cursor and deterministic pending-effect scheduler.
#[derive(Debug)]
pub struct ReplaySession {
    entries: Vec<ReplayEntry>,
    cursor: usize,
    completion_order: Vec<u64>,
    completion_cursor: usize,
    pending: BTreeMap<u64, (PendingEffect, EffectOutcome)>,
    next_pending: u64,
    execution_outcome: Option<ReplayExecutionOutcome>,
}

impl ReplaySession {
    /// Start a replay session. No live provider is accepted or stored.
    #[must_use]
    pub fn new(log: &ReplayLog) -> Self {
        Self {
            entries: log.entries.clone(),
            cursor: 0,
            completion_order: log.header.scheduler_completion_order.clone(),
            completion_cursor: 0,
            pending: BTreeMap::new(),
            next_pending: 0,
            execution_outcome: log.execution_outcome.clone(),
        }
    }

    /// Match and issue the next effect. Any drift is `ReplayDiverged`.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::ReplayDiverged`] when request or order differs.
    pub fn start(&mut self, request: &EffectRequest) -> Result<PendingEffect, ReplayError> {
        let entry = self
            .entries
            .get(self.cursor)
            .ok_or(ReplayError::ReplayDiverged)?;
        if entry.effect != request.kind
            || entry.schema_digest != request.schema_digest
            || entry.request_digest != request_digest(request)
        {
            return Err(ReplayError::ReplayDiverged);
        }
        let pending = PendingEffect(self.next_pending);
        self.next_pending = self
            .next_pending
            .checked_add(1)
            .ok_or(ReplayError::LimitExceeded)?;
        self.cursor += 1;
        self.pending
            .insert(entry.sequence, (pending, entry.outcome.clone()));
        Ok(pending)
    }

    /// Complete the next request in the recorded scheduler completion order.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::ReplayDiverged`] when no effect is pending.
    pub fn complete_next(&mut self) -> Result<(PendingEffect, EffectOutcome), ReplayError> {
        let sequence = *self
            .completion_order
            .get(self.completion_cursor)
            .ok_or(ReplayError::ReplayDiverged)?;
        let (pending, outcome) = self
            .pending
            .remove(&sequence)
            .ok_or(ReplayError::ReplayDiverged)?;
        self.completion_cursor = self
            .completion_cursor
            .checked_add(1)
            .ok_or(ReplayError::LimitExceeded)?;
        Ok((pending, outcome))
    }

    /// Complete `pending` only when it is next in the recorded completion order.
    ///
    /// A different next token returns `Ok(None)` without advancing the global
    /// cursor, allowing the VM's deterministic task polling order to differ
    /// from the recorded provider completion order.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::ReplayDiverged`] for an unknown token or an
    /// exhausted completion trace.
    pub fn complete_if_next(
        &mut self,
        pending: PendingEffect,
    ) -> Result<Option<EffectOutcome>, ReplayError> {
        if !self
            .pending
            .values()
            .any(|(candidate, _)| *candidate == pending)
        {
            return Err(ReplayError::ReplayDiverged);
        }
        let sequence = *self
            .completion_order
            .get(self.completion_cursor)
            .ok_or(ReplayError::ReplayDiverged)?;
        let Some((next, _)) = self.pending.get(&sequence) else {
            return Ok(None);
        };
        if *next != pending {
            return Ok(None);
        }
        let (_, outcome) = self
            .pending
            .remove(&sequence)
            .ok_or(ReplayError::ReplayDiverged)?;
        self.completion_cursor = self
            .completion_cursor
            .checked_add(1)
            .ok_or(ReplayError::LimitExceeded)?;
        Ok(Some(outcome))
    }

    /// Cancel one issued pending effect; late provider output cannot be observed.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::ReplayDiverged`] for an unknown pending token.
    pub fn cancel(&mut self, pending: PendingEffect) -> Result<(), ReplayError> {
        let sequence = self
            .pending
            .iter()
            .find_map(|(sequence, (token, _))| (*token == pending).then_some(*sequence))
            .ok_or(ReplayError::ReplayDiverged)?;
        if self.completion_order.get(self.completion_cursor) != Some(&sequence)
            || !matches!(
                self.pending.get(&sequence),
                Some((
                    _,
                    EffectOutcome::Err {
                        error: RecordedVmError::Cancelled
                    }
                ))
            )
        {
            return Err(ReplayError::ReplayDiverged);
        }
        self.pending.remove(&sequence);
        self.completion_cursor = self
            .completion_cursor
            .checked_add(1)
            .ok_or(ReplayError::LimitExceeded)?;
        Ok(())
    }

    /// Number of issued effects that have not been completed or cancelled.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Require no pending work and no unconsumed journal entries.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::ReplayDiverged`] if work or journal entries remain.
    fn finish_effects(&self) -> Result<(), ReplayError> {
        if self.pending.is_empty()
            && self.cursor == self.entries.len()
            && self.completion_cursor == self.completion_order.len()
        {
            Ok(())
        } else {
            Err(ReplayError::ReplayDiverged)
        }
    }

    /// Verify replay ended on the exact recorded execution channel.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::ReplayDiverged`] for a mismatched or absent final outcome.
    pub fn finish_with_execution_outcome(
        self,
        actual: &ReplayExecutionOutcome,
    ) -> Result<(), ReplayError> {
        self.validate_execution_outcome(actual)
    }

    #[cfg(test)]
    fn finish(self) -> Result<(), ReplayError> {
        self.finish_with_execution_outcome(&ReplayExecutionOutcome::Completed)
    }

    fn validate_execution_outcome(
        &self,
        actual: &ReplayExecutionOutcome,
    ) -> Result<(), ReplayError> {
        self.finish_effects()?;
        if self.execution_outcome.as_ref() == Some(actual) {
            Ok(())
        } else {
            Err(ReplayError::ReplayDiverged)
        }
    }
}

/// A small adapter seam for a future VM/provider bridge.
///
/// In recording mode the supplied live closure is invoked exactly once and its
/// completion is appended to the journal. In replay mode it is deliberately
/// never invoked: only the already-recorded ordered completion is returned.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum EffectHarness {
    /// An execution recording live provider completions.
    Recording(Recorder),
    /// An execution replaying a previously captured journal.
    Replay(ReplaySession),
}

impl EffectHarness {
    /// Execute or replay one already-encoded effect completion.
    ///
    /// # Errors
    ///
    /// Returns live, recording, or replay divergence failures.
    pub fn execute(
        &mut self,
        request: EffectRequest,
        sensitive: bool,
        redactor: &dyn Redactor,
        live: impl FnOnce() -> Result<EffectOutcome, ReplayError>,
    ) -> Result<EffectOutcome, ReplayError> {
        match self {
            Self::Recording(recorder) => {
                let outcome = live()?;
                recorder.record(request, outcome.clone(), sensitive, redactor)?;
                Ok(outcome)
            }
            Self::Replay(session) => {
                let pending = session.start(&request)?;
                let (completed, outcome) = session.complete_next()?;
                if completed != pending {
                    return Err(ReplayError::ReplayDiverged);
                }
                Ok(outcome)
            }
        }
    }

    #[cfg(test)]
    fn finish(self) -> Result<Option<ReplayLog>, ReplayError> {
        match self {
            Self::Recording(recorder) => recorder.finish().map(Some),
            Self::Replay(session) => session.finish().map(|()| None),
        }
    }
}

/// Resolves the strict response schema for a typed tool contract index.
///
/// The VM provider trait intentionally exposes only the verified tool index,
/// so a host adapter supplies this immutable artifact-derived mapping.
pub trait ToolResultSchema {
    /// Return the exact output schema for `tool`, if it is known.
    fn result_type(&self, tool: u32) -> Option<ValueType>;

    /// Validate the full program-visible tool result against the frozen strict
    /// output and declared-error schemas, including bounds and formats.
    fn validate_result(&self, _tool: u32, _value: &Value) -> bool {
        false
    }
}

/// Mandatory decision for each provider request before its live dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingDisposition {
    /// The request and completion may be recorded as-is.
    Record,
    /// The supplied [`Redactor`] must replace request and completion before writing.
    Redact,
    /// This request must not be dispatched while recording.
    Refuse,
}

/// Classifies provider requests before their live dispatch.
pub trait ReplayRecordingPolicy {
    /// Classify one request. Hosts must supply this policy explicitly.
    fn classify(&self, request: &EffectRequest) -> RecordingDisposition;

    /// Classify terminal stopped text separately from effect payloads.
    fn classify_stopped_reason(&self, _reason: &str) -> RecordingDisposition {
        RecordingDisposition::Refuse
    }
}

/// An explicit policy for fixtures with known-safe values.
#[derive(Clone, Copy, Debug, Default)]
pub struct RecordAll;

impl ReplayRecordingPolicy for RecordAll {
    fn classify(&self, _request: &EffectRequest) -> RecordingDisposition {
        RecordingDisposition::Record
    }

    fn classify_stopped_reason(&self, _reason: &str) -> RecordingDisposition {
        RecordingDisposition::Record
    }
}

/// The fail-closed policy used by the generic default type parameter.
#[derive(Clone, Copy, Debug, Default)]
pub struct RefuseAll;

impl ReplayRecordingPolicy for RefuseAll {
    fn classify(&self, _request: &EffectRequest) -> RecordingDisposition {
        RecordingDisposition::Refuse
    }
}

const SUB_AGENT_TOKEN_FIELD: &str = "$replay_sub_agent";
const WORKSPACE_TOKEN_FIELD: &str = "$replay_workspace";

#[derive(Default)]
struct RecordingSubAgentTokens {
    by_handle: HashMap<(u64, u32, u64), u64>,
    next: u64,
}

impl RecordingSubAgentTokens {
    fn token_for(&mut self, handle: SubAgentValue) -> Result<u64, ReplayError> {
        let key = (handle.generation(), handle.index(), handle.nonce());
        if let Some(token) = self.by_handle.get(&key) {
            return Ok(*token);
        }
        let token = self.next;
        self.next = self.next.checked_add(1).ok_or(ReplayError::LimitExceeded)?;
        self.by_handle.insert(key, token);
        Ok(token)
    }
}

#[derive(Default)]
struct ReplaySubAgentTokens {
    by_token: HashMap<u64, SubAgentValue>,
    by_handle: HashMap<(u64, u32, u64), u64>,
}

impl ReplaySubAgentTokens {
    fn handle_for(&mut self, token: u64) -> Result<SubAgentValue, ReplayError> {
        if let Some(handle) = self.by_token.get(&token) {
            return Ok(*handle);
        }
        let index = u32::try_from(token).map_err(|_| ReplayError::LimitExceeded)?;
        let handle = SubAgentValue::new(0xA11E_0000_0000_0001, index, token);
        self.by_token.insert(token, handle);
        self.by_handle
            .insert((handle.generation(), handle.index(), handle.nonce()), token);
        Ok(handle)
    }

    fn token_for(&self, handle: SubAgentValue) -> Option<u64> {
        self.by_handle
            .get(&(handle.generation(), handle.index(), handle.nonce()))
            .copied()
    }
}

struct RecordingWorkspaceTokens {
    by_handle: HashMap<(u64, u32, u64), u64>,
    next: u64,
}

impl Default for RecordingWorkspaceTokens {
    fn default() -> Self {
        Self {
            by_handle: HashMap::new(),
            next: 1,
        }
    }
}

impl RecordingWorkspaceTokens {
    fn bind_primary(&mut self, handle: WorkspaceValue) -> Result<(), ReplayError> {
        let key = (handle.generation(), handle.index(), handle.nonce());
        match self.by_handle.get(&key) {
            Some(token) if *token == 0 => Ok(()),
            Some(_) => Err(ReplayError::RefusedValue),
            None if self.by_handle.values().any(|token| *token == 0) => {
                Err(ReplayError::RefusedValue)
            }
            None => {
                self.by_handle.insert(key, 0);
                Ok(())
            }
        }
    }

    fn token_for(&mut self, handle: WorkspaceValue) -> Result<u64, ReplayError> {
        let key = (handle.generation(), handle.index(), handle.nonce());
        if let Some(token) = self.by_handle.get(&key) {
            return Ok(*token);
        }
        let token = self.next;
        self.next = self.next.checked_add(1).ok_or(ReplayError::LimitExceeded)?;
        self.by_handle.insert(key, token);
        Ok(token)
    }

    fn existing_token_for(&self, handle: WorkspaceValue) -> Option<u64> {
        self.by_handle
            .get(&(handle.generation(), handle.index(), handle.nonce()))
            .copied()
    }
}

#[derive(Default)]
struct ReplayWorkspaceTokens {
    by_token: HashMap<u64, WorkspaceValue>,
    by_handle: HashMap<(u64, u32, u64), u64>,
}

impl ReplayWorkspaceTokens {
    fn handle_for(&mut self, token: u64) -> Result<WorkspaceValue, ReplayError> {
        if let Some(handle) = self.by_token.get(&token) {
            return Ok(*handle);
        }
        let index = u32::try_from(token).map_err(|_| ReplayError::LimitExceeded)?;
        let handle = WorkspaceValue::new(0xA11E_0000_0000_0002, index, token);
        self.by_token.insert(token, handle);
        self.by_handle
            .insert((handle.generation(), handle.index(), handle.nonce()), token);
        Ok(handle)
    }

    fn token_for(&self, handle: WorkspaceValue) -> Option<u64> {
        self.by_handle
            .get(&(handle.generation(), handle.index(), handle.nonce()))
            .copied()
    }
}

fn replay_sub_agent_token_value(token: u64) -> Value {
    Value::Record(Rc::from([(
        Rc::<str>::from(SUB_AGENT_TOKEN_FIELD),
        Value::Int(i64::try_from(token).expect("bounded replay token")),
    )]))
}

fn replay_sub_agent_token(value: &Value) -> Option<u64> {
    let Value::Record(fields) = value else {
        return None;
    };
    let [(name, Value::Int(token))] = fields.as_ref() else {
        return None;
    };
    (name.as_ref() == SUB_AGENT_TOKEN_FIELD)
        .then(|| u64::try_from(*token).ok())
        .flatten()
}

fn replay_workspace_token_value(token: u64) -> Value {
    Value::Record(Rc::from([(
        Rc::<str>::from(WORKSPACE_TOKEN_FIELD),
        Value::Int(i64::try_from(token).expect("bounded replay token")),
    )]))
}

fn replay_workspace_token(value: &Value) -> Option<u64> {
    let Value::Record(fields) = value else {
        return None;
    };
    let [(name, Value::Int(token))] = fields.as_ref() else {
        return None;
    };
    (name.as_ref() == WORKSPACE_TOKEN_FIELD)
        .then(|| u64::try_from(*token).ok())
        .flatten()
}

fn recordable_permission_arguments(
    operation: EffectOperation,
    arguments: &[Value],
) -> Result<Vec<Value>, ReplayError> {
    let [Value::Record(fields)] = arguments else {
        return Err(ReplayError::RefusedValue);
    };
    let expected_fields = match operation {
        EffectOperation::PermissionRequestFile => ["access", "path", "reason"].as_slice(),
        EffectOperation::PermissionRequestDirectory => {
            ["access", "path", "reason", "recursive"].as_slice()
        }
        _ => return Err(ReplayError::RefusedValue),
    };
    if fields.len() != expected_fields.len()
        || !fields
            .iter()
            .zip(expected_fields)
            .all(|((name, _), expected)| name.as_ref() == *expected)
    {
        return Err(ReplayError::RefusedValue);
    }
    let mut transformed = Vec::with_capacity(fields.len());
    for (name, value) in fields.iter() {
        let value = match (name.as_ref(), value) {
            ("access", Value::ExternalFsAccess(access)) => Value::String(Rc::from(match access {
                allen_bytecode::ExternalFsAccess::Read => "read",
                allen_bytecode::ExternalFsAccess::Write => "write",
                allen_bytecode::ExternalFsAccess::ReadWrite => "read_write",
            })),
            ("path" | "reason", Value::String(_)) | ("recursive", Value::Bool(_)) => value.clone(),
            _ => return Err(ReplayError::RefusedValue),
        };
        transformed.push((Rc::clone(name), value));
    }
    Ok(vec![Value::Record(Rc::from(transformed))])
}

fn record_permission_value(
    value: &Value,
    tokens: &mut RecordingWorkspaceTokens,
) -> Result<Value, ReplayError> {
    let Value::Enum(result) = value else {
        return Err(ReplayError::RefusedValue);
    };
    if result.identity != EnumIdentity::Result {
        return Err(ReplayError::RefusedValue);
    }
    if result.variant == 1 {
        return Ok(value.clone());
    }
    let (0, EnumPayload::Tuple(payload)) = (result.variant, &result.payload) else {
        return Err(ReplayError::RefusedValue);
    };
    let [Value::Workspace(handle)] = payload.as_ref() else {
        return Err(ReplayError::RefusedValue);
    };
    let token = tokens.token_for(*handle)?;
    Ok(Value::Enum(Rc::new(EnumValue {
        identity: result.identity,
        type_name: Rc::clone(&result.type_name),
        variant: result.variant,
        variant_name: Rc::clone(&result.variant_name),
        payload: EnumPayload::Tuple(Rc::from([replay_workspace_token_value(token)])),
    })))
}

fn replay_permission_value(
    value: Value,
    tokens: &mut ReplayWorkspaceTokens,
) -> Result<Value, ReplayError> {
    let Value::Enum(result) = &value else {
        return Err(ReplayError::RefusedValue);
    };
    if result.identity != EnumIdentity::Result {
        return Err(ReplayError::RefusedValue);
    }
    if result.variant == 1 {
        return Ok(value);
    }
    let (0, EnumPayload::Tuple(payload)) = (result.variant, &result.payload) else {
        return Err(ReplayError::RefusedValue);
    };
    let [token_value] = payload.as_ref() else {
        return Err(ReplayError::RefusedValue);
    };
    let token = replay_workspace_token(token_value).ok_or(ReplayError::RefusedValue)?;
    let handle = tokens.handle_for(token)?;
    Ok(Value::Enum(Rc::new(EnumValue {
        identity: result.identity,
        type_name: Rc::clone(&result.type_name),
        variant: result.variant,
        variant_name: Rc::clone(&result.variant_name),
        payload: EnumPayload::Tuple(Rc::from([Value::Workspace(handle)])),
    })))
}

fn record_sub_agent_create_value(
    value: &Value,
    tokens: &mut RecordingSubAgentTokens,
) -> Result<Value, ReplayError> {
    let Value::Enum(result) = value else {
        return Err(ReplayError::RefusedValue);
    };
    if result.identity != EnumIdentity::Result {
        return Err(ReplayError::RefusedValue);
    }
    if result.variant == 1 {
        return Ok(value.clone());
    }
    let (0, EnumPayload::Tuple(payload)) = (result.variant, &result.payload) else {
        return Err(ReplayError::RefusedValue);
    };
    let [Value::SubAgent(handle)] = payload.as_ref() else {
        return Err(ReplayError::RefusedValue);
    };
    let token = tokens.token_for(*handle)?;
    Ok(Value::Enum(Rc::new(EnumValue {
        identity: result.identity,
        type_name: Rc::clone(&result.type_name),
        variant: result.variant,
        variant_name: Rc::clone(&result.variant_name),
        payload: EnumPayload::Tuple(Rc::from([replay_sub_agent_token_value(token)])),
    })))
}

fn replay_sub_agent_create_value(
    value: Value,
    tokens: &mut ReplaySubAgentTokens,
) -> Result<Value, ReplayError> {
    let Value::Enum(result) = &value else {
        return Err(ReplayError::RefusedValue);
    };
    if result.identity != EnumIdentity::Result {
        return Err(ReplayError::RefusedValue);
    }
    if result.variant == 1 {
        return Ok(value);
    }
    let (0, EnumPayload::Tuple(payload)) = (result.variant, &result.payload) else {
        return Err(ReplayError::RefusedValue);
    };
    let [token_value] = payload.as_ref() else {
        return Err(ReplayError::RefusedValue);
    };
    let token = replay_sub_agent_token(token_value).ok_or(ReplayError::RefusedValue)?;
    let handle = tokens.handle_for(token)?;
    Ok(Value::Enum(Rc::new(EnumValue {
        identity: result.identity,
        type_name: Rc::clone(&result.type_name),
        variant: result.variant,
        variant_name: Rc::clone(&result.variant_name),
        payload: EnumPayload::Tuple(Rc::from([Value::SubAgent(handle)])),
    })))
}

/// A conservative schema resolver for tests that do not issue typed tools.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoToolSchemas;

impl ToolResultSchema for NoToolSchemas {
    fn result_type(&self, _tool: u32) -> Option<ValueType> {
        None
    }
}

/// Exact replay schema resolver bound to one verified artifact and the frozen
/// catalog that supplied its manifest-selected tool contracts.
#[derive(Clone, Debug)]
pub struct ArtifactToolSchemas {
    tools: Vec<ArtifactToolSchema>,
    enum_types: Vec<EnumType>,
}

#[derive(Clone, Debug)]
struct ArtifactToolSchema {
    result_type: ValueType,
    output: ToolSchema,
    error: ToolSchema,
}

impl ArtifactToolSchemas {
    /// Bind the artifact tool indices to exact catalog schemas.
    ///
    /// # Errors
    ///
    /// Rejects missing tools or any version/schema digest mismatch.
    pub fn new(artifact: &VerifiedArtifact, catalog: &FrozenCatalog) -> Result<Self, ReplayError> {
        let contracts = artifact
            .manifest()
            .map(|manifest| manifest.required_tools.as_slice())
            .unwrap_or_default();
        let enum_types = artifact.verified_module().module().enum_types.clone();
        let wrapper_ids = enum_types
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                candidate.name.contains("::_tool_tools_")
                    && candidate.name.ends_with("_x3A__x3A_Error")
                    && candidate.variants.len() == 3
                    && candidate.variants[0].name == "Declared"
                    && candidate.variants[1].name == "Unavailable"
                    && candidate.variants[2].name == "Schema"
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if wrapper_ids.len() != contracts.len() {
            return Err(ReplayError::ReplayDiverged);
        }
        let mut tools = Vec::with_capacity(contracts.len());
        for (contract, error) in contracts.iter().zip(wrapper_ids) {
            let name = allen_schema::ToolName::parse(&contract.name)
                .map_err(|_| ReplayError::ReplayDiverged)?;
            let definition = catalog.get(&name).ok_or(ReplayError::ReplayDiverged)?;
            if definition.version.to_string() != contract.version
                || definition.output_schema.digest() != digest_text(&contract.output_digest)
                || definition.error_schema.digest() != digest_text(&contract.error_digest)
            {
                return Err(ReplayError::ReplayDiverged);
            }
            let output = artifact
                .schemas()
                .get(contract.output_schema as usize)
                .ok_or(ReplayError::ReplayDiverged)?
                .value_type
                .clone();
            let error = u32::try_from(error).map_err(|_| ReplayError::ReplayDiverged)?;
            tools.push(ArtifactToolSchema {
                result_type: ValueType::Result(Box::new(output), Box::new(ValueType::Enum(error))),
                output: definition.output_schema.clone(),
                error: definition.error_schema.clone(),
            });
        }
        Ok(Self { tools, enum_types })
    }
}

impl ToolResultSchema for ArtifactToolSchemas {
    fn result_type(&self, tool: u32) -> Option<ValueType> {
        self.tools
            .get(tool as usize)
            .map(|schema| schema.result_type.clone())
    }

    fn validate_result(&self, tool: u32, value: &Value) -> bool {
        self.tools.get(tool as usize).is_some_and(|schema| {
            allen_runtime::validate_replayed_tool_result(
                value,
                &schema.result_type,
                &schema.output,
                &schema.error,
                &self.enum_types,
            )
        })
    }
}

fn digest_text(digest: &[u8; 32]) -> String {
    let mut text = String::from("sha256:");
    for byte in digest {
        write!(&mut text, "{byte:02x}").expect("writing into String cannot fail");
    }
    text
}

/// Record a real public VM provider using canonical byte outcomes.
pub struct RecordingEffectProvider<P, S, R, C = RefuseAll> {
    live: P,
    schemas: S,
    redactor: R,
    policy: C,
    recorder: Recorder,
    execution_outcome: Option<ReplayExecutionOutcome>,
    pending: HashMap<PendingEffectId, RecordingPending>,
    sub_agents: RecordingSubAgentTokens,
    workspaces: RecordingWorkspaceTokens,
}

#[derive(Clone, Copy)]
struct RecordingPending {
    sequence: u64,
    operation: Option<EffectOperation>,
}

impl<P, S, R: Redactor, C: ReplayRecordingPolicy> RecordingEffectProvider<P, S, R, C> {
    /// Wrap one execution-local live provider and journal recorder.
    #[must_use]
    pub fn new(live: P, schemas: S, redactor: R, policy: C, recorder: Recorder) -> Self {
        Self {
            live,
            schemas,
            redactor,
            policy,
            recorder,
            execution_outcome: None,
            pending: HashMap::new(),
            sub_agents: RecordingSubAgentTokens::default(),
            workspaces: RecordingWorkspaceTokens::default(),
        }
    }

    /// Finish recording with the execution's exact final channel.
    ///
    /// # Errors
    ///
    /// Returns an error when effects remain pending or the outcome is not bounded.
    pub fn finish_with_execution_outcome(
        self,
        outcome: ReplayExecutionOutcome,
    ) -> Result<ReplayLog, ReplayError> {
        if !self.pending.is_empty() {
            return Err(ReplayError::ReplayDiverged);
        }
        if self
            .execution_outcome
            .as_ref()
            .is_some_and(|actual| actual != &outcome)
        {
            return Err(ReplayError::ReplayDiverged);
        }
        self.recorder
            .finish_with_execution_outcome_policy(outcome, &self.policy, &self.redactor)
    }

    #[cfg(test)]
    fn finish(self) -> Result<ReplayLog, ReplayError> {
        self.finish_with_execution_outcome(ReplayExecutionOutcome::Completed)
    }
}

fn protect_execution_outcome(
    outcome: ReplayExecutionOutcome,
    policy: &dyn ReplayRecordingPolicy,
    redactor: &dyn Redactor,
    limits: ReplayLimits,
) -> Result<ReplayExecutionOutcome, ReplayError> {
    let (reason, failed) = match outcome {
        ReplayExecutionOutcome::Stopped { reason } => (reason, false),
        ReplayExecutionOutcome::Failed { reason } => (reason, true),
        outcome => return Ok(outcome),
    };
    let reason = match policy.classify_stopped_reason(&reason) {
        RecordingDisposition::Record => reason,
        RecordingDisposition::Redact => redactor.redact_stopped_reason(&reason, limits)?,
        RecordingDisposition::Refuse => return Err(ReplayError::RefusedValue),
    };
    Ok(if failed {
        ReplayExecutionOutcome::Failed { reason }
    } else {
        ReplayExecutionOutcome::Stopped { reason }
    })
}

impl<P: EffectProvider, S: ToolResultSchema, R: Redactor, C: ReplayRecordingPolicy>
    RecordingEffectProvider<P, S, R, C>
{
    fn request(
        &mut self,
        kind: EffectKind,
        value: &Value,
        result_type: &ValueType,
    ) -> Result<EffectRequest, VmError> {
        EffectRequest::from_value(kind, value, result_type, self.recorder.limits)
            .map_err(|_| VmError::ResponseValidationError)
    }

    fn sub_agent_request(
        &mut self,
        operation: EffectOperation,
        arguments: &[Value],
        result_type: &ValueType,
    ) -> Result<EffectRequest, VmError> {
        let mut arguments = arguments.to_vec();
        if matches!(
            operation,
            EffectOperation::SubAgentMessage | EffectOperation::SubAgentAsk
        ) {
            let Some(Value::SubAgent(handle)) = arguments.first() else {
                return Err(VmError::ResponseValidationError);
            };
            let token = self
                .sub_agents
                .token_for(*handle)
                .map_err(|_| VmError::ResponseValidationError)?;
            arguments[0] = replay_sub_agent_token_value(token);
        }
        self.request(
            EffectKind::SubAgent(operation.required_effect().to_owned()),
            &Value::Tuple(Rc::from(arguments)),
            result_type,
        )
    }

    fn call_request(
        &mut self,
        operation: EffectOperation,
        arguments: &[Value],
        result_type: &ValueType,
    ) -> Result<EffectRequest, VmError> {
        let mut arguments = if matches!(
            operation,
            EffectOperation::PermissionRequestFile | EffectOperation::PermissionRequestDirectory
        ) {
            recordable_permission_arguments(operation, arguments)
                .map_err(|_| VmError::ResponseValidationError)?
        } else {
            arguments.to_vec()
        };
        if matches!(
            operation,
            EffectOperation::ReadText
                | EffectOperation::ReadBytes
                | EffectOperation::WriteText
                | EffectOperation::WriteBytes
                | EffectOperation::List
                | EffectOperation::Search
        ) {
            let Some(Value::Workspace(handle)) = arguments.first() else {
                return Err(VmError::ResponseValidationError);
            };
            let token = self
                .workspaces
                .existing_token_for(*handle)
                .ok_or(VmError::ResponseValidationError)?;
            arguments[0] = replay_workspace_token_value(token);
        }
        self.request(
            EffectKind::Call(operation.required_effect().to_owned()),
            &Value::Tuple(Rc::from(arguments)),
            result_type,
        )
    }

    fn record_result(
        &mut self,
        sequence: u64,
        operation: Option<EffectOperation>,
        result: &Result<Value, VmError>,
    ) -> Result<(), VmError> {
        let outcome = match result {
            Ok(value) => {
                let value = if operation == Some(EffectOperation::SubAgentCreate) {
                    record_sub_agent_create_value(value, &mut self.sub_agents)
                        .map_err(|_| VmError::ResponseValidationError)?
                } else if matches!(
                    operation,
                    Some(
                        EffectOperation::PermissionRequestFile
                            | EffectOperation::PermissionRequestDirectory
                    )
                ) {
                    record_permission_value(value, &mut self.workspaces)
                        .map_err(|_| VmError::ResponseValidationError)?
                } else {
                    value.clone()
                };
                EffectOutcome::Ok(
                    encode_canonical_with_limit(
                        &value,
                        u64::try_from(self.recorder.limits.payload_bytes)
                            .map_err(|_| VmError::ResponseValidationError)?,
                    )
                    .map_err(|_| VmError::ResponseValidationError)?,
                )
            }
            Err(error) => EffectOutcome::Err {
                error: recorded_vm_error(error).map_err(|_| VmError::ResponseValidationError)?,
            },
        };
        self.recorder
            .complete(sequence, outcome, &self.redactor)
            .map_err(|_| VmError::ResponseValidationError)
    }

    fn begin(&mut self, request: EffectRequest) -> Result<u64, VmError> {
        let sensitive = match self.policy.classify(&request) {
            RecordingDisposition::Record => false,
            RecordingDisposition::Redact => true,
            RecordingDisposition::Refuse => return Err(VmError::ResponseValidationError),
        };
        self.recorder
            .start(request, sensitive)
            .map_err(|_| VmError::ResponseValidationError)
    }

    fn start(
        &mut self,
        pending: PendingEffectId,
        sequence: u64,
        operation: Option<EffectOperation>,
        result: Result<EffectPoll, VmError>,
    ) -> Result<EffectPoll, VmError> {
        match result {
            Ok(EffectPoll::Ready(value)) => {
                let completed = Ok(value.clone());
                self.record_result(sequence, operation, &completed)?;
                Ok(EffectPoll::Ready(value))
            }
            Ok(EffectPoll::Pending) => {
                self.pending.insert(
                    pending,
                    RecordingPending {
                        sequence,
                        operation,
                    },
                );
                Ok(EffectPoll::Pending)
            }
            Err(error) => {
                self.record_result(sequence, operation, &Err(error.clone()))?;
                Err(error)
            }
        }
    }
}

impl<P: EffectProvider, S: ToolResultSchema, R: Redactor, C: ReplayRecordingPolicy> EffectProvider
    for RecordingEffectProvider<P, S, R, C>
{
    fn bind_execution(&mut self, binding: &EffectExecutionBinding) -> Result<(), VmError> {
        if !header_matches_binding(&self.recorder.header, binding) {
            return Err(VmError::ReplayDiverged);
        }
        self.live.bind_execution(binding)
    }

    fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
        let workspace = self.live.workspace()?;
        self.workspaces
            .bind_primary(workspace)
            .map_err(|_| VmError::ResponseValidationError)?;
        Ok(workspace)
    }

    fn call(&mut self, operation: EffectOperation, arguments: &[Value]) -> Result<Value, VmError> {
        self.live.call(operation, arguments)
    }

    fn start_call(
        &mut self,
        pending: PendingEffectId,
        operation: EffectOperation,
        arguments: &[Value],
        cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        let result_type =
            effect_result_type(operation, None).ok_or(VmError::ResponseValidationError)?;
        let request = self.call_request(operation, arguments, &result_type)?;
        let sequence = self.begin(request)?;
        let result = self
            .live
            .start_call(pending, operation, arguments, cancellation);
        self.start(pending, sequence, Some(operation), result)
    }

    fn start_tool(
        &mut self,
        pending: PendingEffectId,
        tool: u32,
        input: &Value,
        result_type: &ValueType,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        let schema_result_type = self
            .schemas
            .result_type(tool)
            .ok_or(VmError::ToolSchemaError)?;
        if &schema_result_type != result_type {
            return Err(VmError::ToolSchemaError);
        }
        let request = self.request(EffectKind::Tool(tool), input, result_type)?;
        let sequence = self.begin(request)?;
        let result = self
            .live
            .start_tool(pending, tool, input, result_type, cancellation);
        self.start(pending, sequence, None, result)
    }

    fn agent(
        &mut self,
        operation: EffectOperation,
        arguments: &[Value],
        result_type: &ValueType,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<Value, VmError> {
        self.live
            .agent(operation, arguments, result_type, cancellation)
    }

    fn start_agent(
        &mut self,
        pending: PendingEffectId,
        operation: EffectOperation,
        arguments: &[Value],
        result_type: &ValueType,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        let request_value = Value::Tuple(Rc::from(arguments));
        let request = self.request(
            EffectKind::Agent(operation.required_effect().to_owned()),
            &request_value,
            result_type,
        )?;
        let sequence = self.begin(request)?;
        let result =
            self.live
                .start_agent(pending, operation, arguments, result_type, cancellation);
        self.start(pending, sequence, None, result)
    }

    fn start_sub_agent(
        &mut self,
        pending: PendingEffectId,
        operation: EffectOperation,
        arguments: &[Value],
        result_type: &ValueType,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        let request = self.sub_agent_request(operation, arguments, result_type)?;
        let sequence = self.begin(request)?;
        let result =
            self.live
                .start_sub_agent(pending, operation, arguments, result_type, cancellation);
        self.start(pending, sequence, Some(operation), result)
    }

    fn poll_effect(
        &mut self,
        pending: PendingEffectId,
        cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        let result = self.live.poll_effect(pending, cancellation);
        if let Ok(EffectPoll::Ready(value)) = &result {
            let pending = self
                .pending
                .remove(&pending)
                .ok_or(VmError::ReplayDiverged)?;
            self.record_result(pending.sequence, pending.operation, &Ok(value.clone()))?;
        } else if let Err(error) = &result {
            let pending = self
                .pending
                .remove(&pending)
                .ok_or(VmError::ReplayDiverged)?;
            self.record_result(pending.sequence, pending.operation, &Err(error.clone()))?;
        }
        result
    }

    fn cancel_effect(&mut self, pending: PendingEffectId) {
        if let Some(pending) = self.pending.remove(&pending) {
            let _ = self.record_result(
                pending.sequence,
                pending.operation,
                &Err(VmError::Cancelled),
            );
        }
        self.live.cancel_effect(pending);
    }

    fn cancel_pending(&mut self) {
        let mut pending: Vec<_> = std::mem::take(&mut self.pending).into_values().collect();
        pending.sort_by_key(|pending| pending.sequence);
        for pending in pending {
            let _ = self.record_result(
                pending.sequence,
                pending.operation,
                &Err(VmError::Cancelled),
            );
        }
        self.live.cancel_pending();
    }

    fn finish_execution(&mut self, outcome: EffectExecutionOutcome<'_>) -> Result<(), VmError> {
        if self.execution_outcome.is_some() {
            return Err(VmError::ReplayRuntimeDiverged);
        }
        let recorded = recorded_execution_outcome(outcome)?;
        self.live.finish_execution(outcome)?;
        self.execution_outcome = Some(recorded);
        Ok(())
    }
}

fn recorded_execution_outcome(
    outcome: EffectExecutionOutcome<'_>,
) -> Result<ReplayExecutionOutcome, VmError> {
    Ok(match outcome {
        EffectExecutionOutcome::Completed => ReplayExecutionOutcome::Completed,
        EffectExecutionOutcome::Stopped { reason } => ReplayExecutionOutcome::Stopped {
            reason: reason.to_owned(),
        },
        EffectExecutionOutcome::Failed { reason } => ReplayExecutionOutcome::Failed {
            reason: reason.to_owned(),
        },
        EffectExecutionOutcome::Terminal { error } => ReplayExecutionOutcome::Terminal {
            error: recorded_vm_error(error).map_err(|_| VmError::ReplayRuntimeDiverged)?,
        },
        EffectExecutionOutcome::RuntimePanic => ReplayExecutionOutcome::Terminal {
            error: RecordedVmError::RuntimePanic,
        },
    })
}

/// Replay-only public VM provider. It has no live provider field by design.
pub struct ReplayingEffectProvider<S = NoToolSchemas> {
    session: ReplaySession,
    header: ReplayHeader,
    enum_types: Vec<EnumType>,
    limits: ReplayLimits,
    schemas: S,
    pending: HashMap<PendingEffectId, ReplayPending>,
    sub_agents: ReplaySubAgentTokens,
    workspaces: ReplayWorkspaceTokens,
}

#[derive(Clone)]
struct ReplayPending {
    token: PendingEffect,
    operation: Option<EffectOperation>,
    tool: Option<u32>,
    result_type: ValueType,
}

impl<S: ToolResultSchema> ReplayingEffectProvider<S> {
    /// Bind replay to one immutable execution-local journal.
    ///
    /// The caller must provide the current execution binding and the exact enum
    /// table from the verified module. Binding digests are checked before the
    /// journal can consume an effect entry, and nominal results are validated
    /// recursively against that enum table before release.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::ReplayDiverged`] when the execution binding differs.
    pub fn new(
        log: &ReplayLog,
        expected_header: &ReplayHeader,
        limits: ReplayLimits,
        schemas: S,
        enum_types: &[EnumType],
    ) -> Result<Self, ReplayError> {
        validate_header(log.header())?;
        if log.execution_outcome().is_none() {
            return Err(ReplayError::InvalidJournal);
        }
        if !same_execution_binding(log.header(), expected_header) {
            return Err(ReplayError::ReplayDiverged);
        }
        Ok(Self {
            session: ReplaySession::new(log),
            header: log.header().clone(),
            enum_types: enum_types.to_vec(),
            limits,
            schemas,
            pending: HashMap::new(),
            sub_agents: ReplaySubAgentTokens::default(),
            workspaces: ReplayWorkspaceTokens::default(),
        })
    }

    /// Expose the replayed-state signal for host diagnostics.
    #[must_use]
    pub const fn is_replaying(&self) -> bool {
        true
    }

    /// Validate exhaustion and the exact recorded final execution channel.
    ///
    /// # Errors
    ///
    /// Returns an in-execution replay-divergence trap when effects or the final channel differ.
    pub fn finish_with_execution_outcome(
        self,
        actual: &ReplayExecutionOutcome,
    ) -> Result<(), VmError> {
        self.session
            .finish_with_execution_outcome(actual)
            .map_err(|_| VmError::ReplayRuntimeDiverged)
    }

    fn start(
        &mut self,
        pending: PendingEffectId,
        request: &EffectRequest,
        operation: Option<EffectOperation>,
        tool: Option<u32>,
        result_type: ValueType,
    ) -> Result<EffectPoll, VmError> {
        let token = self
            .session
            .start(request)
            .map_err(|_| VmError::ReplayRuntimeDiverged)?;
        self.pending.insert(
            pending,
            ReplayPending {
                token,
                operation,
                tool,
                result_type,
            },
        );
        Ok(EffectPoll::Pending)
    }

    fn poll(&mut self, pending: PendingEffectId) -> Result<EffectPoll, VmError> {
        let replay_pending = self
            .pending
            .get(&pending)
            .cloned()
            .ok_or(VmError::ReplayRuntimeDiverged)?;
        let Some(outcome) = self
            .session
            .complete_if_next(replay_pending.token)
            .map_err(|_| VmError::ReplayRuntimeDiverged)?
        else {
            return Ok(EffectPoll::Pending);
        };
        self.pending.remove(&pending);
        match outcome {
            EffectOutcome::Ok(bytes) => {
                let value = decode_canonical_with_limit(
                    &bytes,
                    u64::try_from(self.limits.payload_bytes)
                        .map_err(|_| VmError::ReplayRuntimeDiverged)?,
                )
                .map_err(|_| VmError::ReplayRuntimeDiverged)?;
                if let Some(tool) = replay_pending.tool {
                    if !replay_value_matches_type(
                        &value,
                        &replay_pending.result_type,
                        &self.enum_types,
                        0,
                    ) || !validate_current_tool_completion(&value)
                    {
                        return Err(VmError::ReplayRuntimeDiverged);
                    }
                    if !self.schemas.validate_result(tool, &value) {
                        return Err(VmError::ReplayRuntimeDiverged);
                    }
                    return Ok(EffectPoll::Ready(value));
                }
                let value = if replay_pending.operation == Some(EffectOperation::SubAgentCreate) {
                    replay_sub_agent_create_value(value, &mut self.sub_agents)
                        .map_err(|_| VmError::ReplayRuntimeDiverged)?
                } else if matches!(
                    replay_pending.operation,
                    Some(
                        EffectOperation::PermissionRequestFile
                            | EffectOperation::PermissionRequestDirectory
                    )
                ) {
                    replay_permission_value(value, &mut self.workspaces)
                        .map_err(|_| VmError::ReplayRuntimeDiverged)?
                } else {
                    value
                };
                let operation = replay_pending
                    .operation
                    .ok_or(VmError::ReplayRuntimeDiverged)?;
                validate_current_standard_completion(
                    operation,
                    &value,
                    &replay_pending.result_type,
                    &self.enum_types,
                )
                .map_err(|_| VmError::ReplayRuntimeDiverged)?;
                Ok(EffectPoll::Ready(value))
            }
            EffectOutcome::Err { error } => {
                let valid = if replay_pending.tool.is_some() {
                    validate_current_tool_raw_error(&error)
                } else {
                    replay_pending.operation.is_some_and(|operation| {
                        validate_current_operation_raw_error(operation, &error)
                    })
                };
                if !valid {
                    return Err(VmError::ReplayRuntimeDiverged);
                }
                Err(vm_error_from_recorded(&error))
            }
        }
    }
}

fn same_execution_binding(left: &ReplayHeader, right: &ReplayHeader) -> bool {
    left.bytecode_version == right.bytecode_version
        && left.artifact_digest == right.artifact_digest
        && left.contract_digest == right.contract_digest
        && left.language_digest == right.language_digest
        && left.runtime_digest == right.runtime_digest
        && left.policy_digest == right.policy_digest
        && left.catalog_digest == right.catalog_digest
        && left.capability_digest == right.capability_digest
        && left.error_registry_digest == right.error_registry_digest
        && left.effective_manifest_grants == right.effective_manifest_grants
        && left.requested_exec_commands == right.requested_exec_commands
        && left.requested_exec_environment == right.requested_exec_environment
        && left.effective_exec_grants == right.effective_exec_grants
        && left.effective_exec_environment == right.effective_exec_environment
        && left.effective_exec_environment_digest == right.effective_exec_environment_digest
        && left.pinned_exec_identity_digest == right.pinned_exec_identity_digest
}

fn header_matches_binding(header: &ReplayHeader, binding: &EffectExecutionBinding) -> bool {
    header.bytecode_version == binding.bytecode_version
        && header.artifact_digest == binding.artifact_digest
        && header.contract_digest == binding.contract_digest
        && header.language_digest == binding.language_digest
        && header.runtime_digest == binding.runtime_digest
        && header.policy_digest == binding.policy_digest
        && header.catalog_digest == binding.catalog_digest
        && header.capability_digest == binding.capability_digest
        && header.error_registry_digest == binding.error_registry_digest
        && header.effective_manifest_grants == binding.effective_manifest_grants
        && header.requested_exec_commands == binding.requested_exec_commands
        && header.requested_exec_environment == binding.requested_exec_environment
        && header.effective_exec_grants == binding.effective_exec_grants
        && header.effective_exec_environment == binding.effective_exec_environment
        && header.effective_exec_environment_digest == binding.effective_exec_environment_digest
        && header.pinned_exec_identity_digest == binding.pinned_exec_identity_digest
}

fn is_zero_digest(value: &[u8; 32]) -> bool {
    value.iter().all(|byte| *byte == 0)
}

impl<S: ToolResultSchema> EffectProvider for ReplayingEffectProvider<S> {
    fn is_replayed(&self) -> bool {
        true
    }

    fn bind_execution(&mut self, binding: &EffectExecutionBinding) -> Result<(), VmError> {
        if header_matches_binding(&self.header, binding) {
            Ok(())
        } else {
            Err(VmError::ReplayDiverged)
        }
    }

    fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
        self.workspaces
            .handle_for(0)
            .map_err(|_| VmError::ReplayRuntimeDiverged)
    }

    fn call(
        &mut self,
        _operation: EffectOperation,
        _arguments: &[Value],
    ) -> Result<Value, VmError> {
        Err(VmError::ReplayRuntimeDiverged)
    }

    fn start_call(
        &mut self,
        pending: PendingEffectId,
        operation: EffectOperation,
        arguments: &[Value],
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        let result_type =
            effect_result_type(operation, None).ok_or(VmError::ReplayRuntimeDiverged)?;
        let mut arguments = if matches!(
            operation,
            EffectOperation::PermissionRequestFile | EffectOperation::PermissionRequestDirectory
        ) {
            recordable_permission_arguments(operation, arguments)
                .map_err(|_| VmError::ReplayRuntimeDiverged)?
        } else {
            arguments.to_vec()
        };
        if matches!(
            operation,
            EffectOperation::ReadText
                | EffectOperation::ReadBytes
                | EffectOperation::WriteText
                | EffectOperation::WriteBytes
                | EffectOperation::List
                | EffectOperation::Search
        ) {
            let Some(Value::Workspace(handle)) = arguments.first() else {
                return Err(VmError::ReplayRuntimeDiverged);
            };
            let token = self
                .workspaces
                .token_for(*handle)
                .ok_or(VmError::ReplayRuntimeDiverged)?;
            arguments[0] = replay_workspace_token_value(token);
        }
        let arguments = Value::Tuple(Rc::from(arguments));
        let request = EffectRequest::from_value(
            EffectKind::Call(operation.required_effect().to_owned()),
            &arguments,
            &result_type,
            self.limits,
        )
        .map_err(|_| VmError::ReplayRuntimeDiverged)?;
        self.start(pending, &request, Some(operation), None, result_type)
    }

    fn start_tool(
        &mut self,
        pending: PendingEffectId,
        tool: u32,
        input: &Value,
        result_type: &ValueType,
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        let schema_result_type = self
            .schemas
            .result_type(tool)
            .ok_or(VmError::ToolSchemaError)?;
        if &schema_result_type != result_type {
            return Err(VmError::ReplayRuntimeDiverged);
        }
        let request =
            EffectRequest::from_value(EffectKind::Tool(tool), input, result_type, self.limits)
                .map_err(|_| VmError::ReplayRuntimeDiverged)?;
        self.start(pending, &request, None, Some(tool), result_type.clone())
    }

    fn start_agent(
        &mut self,
        pending: PendingEffectId,
        operation: EffectOperation,
        arguments: &[Value],
        result_type: &ValueType,
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        let arguments = Value::Tuple(Rc::from(arguments));
        let request = EffectRequest::from_value(
            EffectKind::Agent(operation.required_effect().to_owned()),
            &arguments,
            result_type,
            self.limits,
        )
        .map_err(|_| VmError::ReplayRuntimeDiverged)?;
        self.start(
            pending,
            &request,
            Some(operation),
            None,
            result_type.clone(),
        )
    }

    fn start_sub_agent(
        &mut self,
        pending: PendingEffectId,
        operation: EffectOperation,
        arguments: &[Value],
        result_type: &ValueType,
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        let mut arguments = arguments.to_vec();
        if matches!(
            operation,
            EffectOperation::SubAgentMessage | EffectOperation::SubAgentAsk
        ) {
            let Some(Value::SubAgent(handle)) = arguments.first() else {
                return Err(VmError::ReplayRuntimeDiverged);
            };
            let token = self
                .sub_agents
                .token_for(*handle)
                .ok_or(VmError::ReplayRuntimeDiverged)?;
            arguments[0] = replay_sub_agent_token_value(token);
        }
        let arguments = Value::Tuple(Rc::from(arguments));
        let request = EffectRequest::from_value(
            EffectKind::SubAgent(operation.required_effect().to_owned()),
            &arguments,
            result_type,
            self.limits,
        )
        .map_err(|_| VmError::ReplayRuntimeDiverged)?;
        self.start(
            pending,
            &request,
            Some(operation),
            None,
            result_type.clone(),
        )
    }

    fn poll_effect(
        &mut self,
        pending: PendingEffectId,
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        self.poll(pending)
    }

    fn cancel_effect(&mut self, pending: PendingEffectId) {
        if let Some(pending) = self.pending.remove(&pending) {
            let _ = self.session.cancel(pending.token);
        }
    }
    fn cancel_pending(&mut self) {
        let mut pending: Vec<_> = std::mem::take(&mut self.pending).into_values().collect();
        pending.sort_by_key(|pending| pending.token.0);
        for pending in pending {
            let _ = self.session.cancel(pending.token);
        }
    }

    fn finish_execution(&mut self, outcome: EffectExecutionOutcome<'_>) -> Result<(), VmError> {
        let outcome = recorded_execution_outcome(outcome)?;
        if !self.pending.is_empty() {
            return Err(VmError::ReplayRuntimeDiverged);
        }
        self.session
            .validate_execution_outcome(&outcome)
            .map_err(|_| VmError::ReplayRuntimeDiverged)
    }
}

fn recorded_vm_error(error: &VmError) -> Result<RecordedVmError, ReplayError> {
    Ok(match error {
        VmError::ArithmeticOverflow => RecordedVmError::ArithmeticOverflow,
        VmError::DivisionByZero => RecordedVmError::DivisionByZero,
        VmError::IndexOutOfBounds => RecordedVmError::IndexOutOfBounds,
        VmError::ListLengthMismatch => RecordedVmError::ListLengthMismatch,
        VmError::MapKeyNotFound => RecordedVmError::MapKeyNotFound,
        VmError::DuplicateMapKey => RecordedVmError::DuplicateMapKey,
        VmError::ResourceLimit { resource } => RecordedVmError::ResourceLimit {
            resource: ReplayResource::from_static(resource).ok_or(ReplayError::RefusedValue)?,
        },
        VmError::Timeout { resource } => RecordedVmError::Timeout {
            resource: ReplayResource::from_static(resource).ok_or(ReplayError::RefusedValue)?,
        },
        VmError::Cancelled => RecordedVmError::Cancelled,
        VmError::CapabilityMissing => RecordedVmError::CapabilityMissing,
        VmError::ProtocolViolation => RecordedVmError::ProtocolViolation,
        VmError::AgentUnavailable => RecordedVmError::AgentUnavailable,
        VmError::AgentResponseSchema => RecordedVmError::AgentResponseSchema,
        VmError::ModelUnavailable => RecordedVmError::ModelUnavailable,
        VmError::ModelValidationError => RecordedVmError::ModelValidationError,
        VmError::UserUnavailable => RecordedVmError::UserUnavailable,
        VmError::SubAgentUnavailable => RecordedVmError::SubAgentUnavailable,
        VmError::SubAgentResponseSchema => RecordedVmError::SubAgentResponseSchema,
        VmError::ReplayDiverged => RecordedVmError::ReplayDiverged,
        VmError::ReplayRuntimeDiverged => RecordedVmError::ReplayRuntimeDiverged,
        VmError::ResponseValidationError => RecordedVmError::ResponseValidationError,
        VmError::ToolUnavailable => RecordedVmError::ToolUnavailable,
        VmError::ToolSchemaError => RecordedVmError::ToolSchemaError,
        VmError::Invariant(_) => RecordedVmError::RuntimePanic,
        VmError::Stopped { .. } | VmError::ProgramFailed { .. } => {
            return Err(ReplayError::RefusedValue);
        }
    })
}

fn vm_error_from_recorded(error: &RecordedVmError) -> VmError {
    match error {
        RecordedVmError::ArithmeticOverflow => VmError::ArithmeticOverflow,
        RecordedVmError::DivisionByZero => VmError::DivisionByZero,
        RecordedVmError::IndexOutOfBounds => VmError::IndexOutOfBounds,
        RecordedVmError::MapKeyNotFound => VmError::MapKeyNotFound,
        RecordedVmError::ListLengthMismatch => VmError::ListLengthMismatch,
        RecordedVmError::DuplicateMapKey => VmError::DuplicateMapKey,
        RecordedVmError::ResourceLimit { resource } => VmError::ResourceLimit {
            resource: resource.as_static(),
        },
        RecordedVmError::Timeout { resource } => VmError::Timeout {
            resource: resource.as_static(),
        },
        RecordedVmError::Cancelled => VmError::Cancelled,
        RecordedVmError::CapabilityMissing => VmError::CapabilityMissing,
        RecordedVmError::ProtocolViolation => VmError::ProtocolViolation,
        RecordedVmError::AgentUnavailable => VmError::AgentUnavailable,
        RecordedVmError::AgentResponseSchema => VmError::AgentResponseSchema,
        RecordedVmError::ModelUnavailable => VmError::ModelUnavailable,
        RecordedVmError::ModelValidationError => VmError::ModelValidationError,
        RecordedVmError::UserUnavailable => VmError::UserUnavailable,
        RecordedVmError::SubAgentUnavailable => VmError::SubAgentUnavailable,
        RecordedVmError::SubAgentResponseSchema => VmError::SubAgentResponseSchema,
        RecordedVmError::ReplayDiverged => VmError::ReplayDiverged,
        RecordedVmError::ReplayRuntimeDiverged => VmError::ReplayRuntimeDiverged,
        RecordedVmError::ResponseValidationError => VmError::ResponseValidationError,
        RecordedVmError::ToolUnavailable => VmError::ToolUnavailable,
        RecordedVmError::ToolSchemaError => VmError::ToolSchemaError,
        RecordedVmError::RuntimePanic => VmError::Invariant("replayed runtime panic"),
    }
}

fn validate_current_standard_completion(
    operation: EffectOperation,
    value: &Value,
    result_type: &ValueType,
    enum_types: &[EnumType],
) -> Result<(), ReplayError> {
    let (Value::Enum(result), ValueType::Result(ok_type, error_type)) = (value, result_type) else {
        return Err(ReplayError::InvalidOutcome);
    };
    if result.identity != EnumIdentity::Result {
        return Err(ReplayError::InvalidOutcome);
    }
    let EnumPayload::Tuple(payload) = &result.payload else {
        return Err(ReplayError::InvalidOutcome);
    };
    let [payload] = payload.as_ref() else {
        return Err(ReplayError::InvalidOutcome);
    };
    match result.variant {
        0 if replay_value_matches_type(payload, ok_type, enum_types, 0) => Ok(()),
        1 if replay_value_matches_type(payload, error_type, enum_types, 0) => {
            let Value::Record(fields) = payload else {
                return Err(ReplayError::InvalidOutcome);
            };
            let (code, message) = replay_standard_error_fields(fields)?;
            if message.len() <= 1_024 && operation_allows_replayed_error_code(operation, code) {
                Ok(())
            } else {
                Err(ReplayError::InvalidOutcome)
            }
        }
        _ => Err(ReplayError::InvalidOutcome),
    }
}

fn validate_current_tool_completion(value: &Value) -> bool {
    let Value::Enum(result) = value else {
        return false;
    };
    if result.identity != EnumIdentity::Result {
        return false;
    }
    let EnumPayload::Tuple(payload) = &result.payload else {
        return false;
    };
    let [payload] = payload.as_ref() else {
        return false;
    };
    match result.variant {
        0 => true,
        1 => validate_current_generated_tool_error(payload),
        _ => false,
    }
}

fn validate_current_generated_tool_error(value: &Value) -> bool {
    let Value::Enum(error) = value else {
        return false;
    };
    match (error.variant, &error.payload) {
        (0, EnumPayload::Tuple(payload)) => payload.len() == 1,
        (1 | 2, EnumPayload::Record(fields)) => {
            let Ok((code, message)) = replay_standard_error_fields(fields) else {
                return false;
            };
            message.len() <= 1_024
                && match error.variant {
                    1 => matches!(code, "tool.unavailable" | "tool.denied"),
                    2 => code == "tool.schema",
                    _ => false,
                }
        }
        _ => false,
    }
}

fn replay_standard_error_fields(fields: &[(Rc<str>, Value)]) -> Result<(&str, &str), ReplayError> {
    let [code, message] = fields else {
        return Err(ReplayError::InvalidOutcome);
    };
    let ("code", Value::String(code)) = (code.0.as_ref(), &code.1) else {
        return Err(ReplayError::InvalidOutcome);
    };
    let ("message", Value::String(message)) = (message.0.as_ref(), &message.1) else {
        return Err(ReplayError::InvalidOutcome);
    };
    Ok((code, message))
}

#[allow(clippy::too_many_lines)]
fn operation_allows_replayed_error_code(operation: EffectOperation, code: &str) -> bool {
    match operation {
        EffectOperation::ReadText => matches!(
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
        EffectOperation::ReadBytes => matches!(
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
        EffectOperation::WriteText | EffectOperation::WriteBytes => matches!(
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
        EffectOperation::List | EffectOperation::Search => matches!(
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
        EffectOperation::HttpGet => matches!(
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
        EffectOperation::ExecRun => matches!(
            code,
            "exec.denied"
                | "exec.invalid_argv"
                | "exec.stdin_limit"
                | "exec.stdout_limit"
                | "exec.stderr_limit"
                | "exec.timeout"
                | "exec.unavailable"
                | "exec.limit"
        ),
        EffectOperation::PermissionRequestFile | EffectOperation::PermissionRequestDirectory => {
            matches!(code, "permission.denied" | "permission.unavailable")
        }
        EffectOperation::AgentMessage | EffectOperation::AgentTranscript => {
            matches!(code, "agent.denied" | "agent.unavailable")
        }
        EffectOperation::AgentAsk => matches!(
            code,
            "agent.denied" | "agent.unavailable" | "agent.validation_failed"
        ),
        EffectOperation::ModelRequest => matches!(
            code,
            "model.denied" | "model.unavailable" | "model.validation_failed"
        ),
        EffectOperation::UserAsk => matches!(
            code,
            "user.denied" | "user.unavailable" | "user.validation_failed"
        ),
        EffectOperation::SubAgentCreate | EffectOperation::SubAgentMessage => {
            matches!(code, "sub_agent.denied" | "sub_agent.unavailable")
        }
        EffectOperation::SubAgentRun | EffectOperation::SubAgentAsk => matches!(
            code,
            "sub_agent.denied" | "sub_agent.unavailable" | "sub_agent.validation_failed"
        ),
    }
}

fn replay_value_matches_type(
    value: &Value,
    value_type: &ValueType,
    enum_types: &[EnumType],
    depth: usize,
) -> bool {
    if depth > MAX_VALUE_NESTING {
        return false;
    }
    match (value, value_type) {
        (Value::Int(_), ValueType::Int)
        | (Value::Bool(_), ValueType::Bool)
        | (Value::Float(_), ValueType::Float)
        | (Value::String(_), ValueType::String)
        | (Value::Bytes(_), ValueType::Bytes)
        | (Value::ExternalFsAccess(_), ValueType::ExternalFsAccess)
        | (Value::Unit, ValueType::Unit)
        | (Value::Workspace(_), ValueType::Workspace)
        | (Value::SubAgent(_), ValueType::SubAgent) => true,
        (Value::Unknown(value), ValueType::Unknown) => {
            replay_value_is_canonical_data(value, depth + 1)
        }
        (Value::List(values), ValueType::List(element)) => values
            .iter()
            .all(|value| replay_value_matches_type(value, element, enum_types, depth + 1)),
        (Value::Map(entries), ValueType::Map(key, item)) => {
            entries.iter().all(|(key_value, value)| {
                replay_value_matches_type(key_value, key, enum_types, depth + 1)
                    && replay_value_matches_type(value, item, enum_types, depth + 1)
            })
        }
        (Value::Tuple(values), ValueType::Tuple(types)) => {
            values.len() == types.len()
                && values.iter().zip(types).all(|(value, value_type)| {
                    replay_value_matches_type(value, value_type, enum_types, depth + 1)
                })
        }
        (Value::Record(values), ValueType::Record(fields)) => {
            values.len() == fields.len()
                && values.iter().zip(fields).all(|((name, value), field)| {
                    name.as_ref() == field.name
                        && replay_value_matches_type(
                            value,
                            &field.value_type,
                            enum_types,
                            depth + 1,
                        )
                })
        }
        (Value::Newtype(value), ValueType::Newtype { name, underlying }) => {
            value.identity() == name
                && replay_value_matches_type(value.value(), underlying, enum_types, depth + 1)
        }
        (Value::Enum(value), ValueType::Enum(type_id)) => {
            value.identity == EnumIdentity::User(*type_id)
                && enum_types
                    .get(*type_id as usize)
                    .and_then(|enum_type| enum_type.variants.get(value.variant as usize))
                    .is_some_and(|variant| {
                        replay_enum_payload_matches_type(
                            &value.payload,
                            &variant.payload,
                            enum_types,
                            depth,
                        )
                    })
        }
        (Value::Enum(value), ValueType::Option(element)) => match (value.variant, &value.payload) {
            (0, EnumPayload::Unit) => value.identity == EnumIdentity::Option,
            (1, EnumPayload::Tuple(payload)) if payload.len() == 1 => {
                value.identity == EnumIdentity::Option
                    && replay_value_matches_type(&payload[0], element, enum_types, depth + 1)
            }
            _ => false,
        },
        (Value::Enum(value), ValueType::Result(ok, error)) => {
            if value.identity != EnumIdentity::Result {
                return false;
            }
            match (value.variant, &value.payload) {
                (0, EnumPayload::Tuple(payload)) if payload.len() == 1 => {
                    replay_value_matches_type(&payload[0], ok, enum_types, depth + 1)
                }
                (1, EnumPayload::Tuple(payload)) if payload.len() == 1 => {
                    replay_value_matches_type(&payload[0], error, enum_types, depth + 1)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn replay_enum_payload_matches_type(
    payload: &EnumPayload,
    payload_type: &EnumPayloadType,
    enum_types: &[EnumType],
    depth: usize,
) -> bool {
    match (payload, payload_type) {
        (EnumPayload::Unit, EnumPayloadType::Unit) => true,
        (EnumPayload::Tuple(values), EnumPayloadType::Tuple(types)) => {
            values.len() == types.len()
                && values.iter().zip(types).all(|(value, value_type)| {
                    replay_value_matches_type(value, value_type, enum_types, depth + 1)
                })
        }
        (EnumPayload::Record(values), EnumPayloadType::Record(fields)) => {
            values.len() == fields.len()
                && values.iter().zip(fields).all(|((name, value), field)| {
                    name.as_ref() == field.name
                        && replay_value_matches_type(
                            value,
                            &field.value_type,
                            enum_types,
                            depth + 1,
                        )
                })
        }
        _ => false,
    }
}

fn replay_value_is_canonical_data(value: &Value, depth: usize) -> bool {
    if depth > MAX_VALUE_NESTING {
        return false;
    }
    match value {
        Value::Int(_)
        | Value::Bool(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bytes(_)
        | Value::ExternalFsAccess(_)
        | Value::Unit => true,
        Value::List(values) | Value::Tuple(values) => values
            .iter()
            .all(|value| replay_value_is_canonical_data(value, depth + 1)),
        Value::Map(entries) => entries.iter().all(|(key, value)| {
            replay_value_is_canonical_data(key, depth + 1)
                && replay_value_is_canonical_data(value, depth + 1)
        }),
        Value::Record(fields) => fields
            .iter()
            .all(|(_, value)| replay_value_is_canonical_data(value, depth + 1)),
        Value::Enum(value) => match &value.payload {
            EnumPayload::Unit => true,
            EnumPayload::Tuple(values) => values
                .iter()
                .all(|value| replay_value_is_canonical_data(value, depth + 1)),
            EnumPayload::Record(fields) => fields
                .iter()
                .all(|(_, value)| replay_value_is_canonical_data(value, depth + 1)),
        },
        Value::Newtype(value) => replay_value_is_canonical_data(value.value(), depth + 1),
        Value::Unknown(value) => replay_value_is_canonical_data(value, depth + 1),
        Value::Range(_)
        | Value::Closure(_)
        | Value::Future(_)
        | Value::Task(_)
        | Value::Workspace(_)
        | Value::SubAgent(_)
        | Value::Sequence(_) => false,
    }
}

fn validate_entries(
    header: &ReplayHeader,
    entries: &[ReplayEntry],
    limits: ReplayLimits,
) -> Result<(), ReplayError> {
    if !header
        .effective_manifest_grants
        .windows(2)
        .all(|pair| pair[0] < pair[1])
        || header
            .effective_manifest_grants
            .iter()
            .any(|grant| !is_manifest_grantable_capability(grant))
    {
        return Err(ReplayError::InvalidJournal);
    }
    if entries.len() > limits.entries {
        return Err(ReplayError::LimitExceeded);
    }
    for (index, entry) in entries.iter().enumerate() {
        if entry.sequence != u64::try_from(index).map_err(|_| ReplayError::LimitExceeded)? {
            return Err(ReplayError::InvalidJournal);
        }
        entry.outcome.validate(limits)?;
        validate_current_effect_kind(&entry.effect)?;
        if let EffectOutcome::Err { error } = &entry.outcome {
            validate_current_entry_error(&entry.effect, error)?;
        }
    }
    let expected: BTreeSet<u64> = entries.iter().map(|entry| entry.sequence).collect();
    let actual: BTreeSet<u64> = header.scheduler_completion_order.iter().copied().collect();
    if header.scheduler_completion_order.len() != entries.len() || actual != expected {
        return Err(ReplayError::InvalidJournal);
    }
    Ok(())
}

fn validate_current_entry_error(
    effect: &EffectKind,
    error: &RecordedVmError,
) -> Result<(), ReplayError> {
    let valid = match effect {
        EffectKind::Tool(_) => validate_current_tool_raw_error(error),
        EffectKind::Call(name) | EffectKind::Agent(name) | EffectKind::SubAgent(name) => {
            operation_for_effect_kind(effect, name)
                .is_some_and(|operation| validate_current_operation_raw_error(operation, error))
        }
    };
    valid.then_some(()).ok_or(ReplayError::InvalidJournal)
}

fn validate_current_effect_kind(effect: &EffectKind) -> Result<(), ReplayError> {
    let valid = match effect {
        EffectKind::Tool(_) => true,
        EffectKind::Call(name) | EffectKind::Agent(name) | EffectKind::SubAgent(name) => {
            operation_for_effect_kind(effect, name).is_some()
        }
    };
    valid.then_some(()).ok_or(ReplayError::InvalidJournal)
}

fn operation_for_effect_kind(effect: &EffectKind, name: &str) -> Option<EffectOperation> {
    match (effect, name) {
        (EffectKind::Call(_), "fs.read" | "fs.read_text") => Some(EffectOperation::ReadText),
        (EffectKind::Call(_), "fs.read_bytes") => Some(EffectOperation::ReadBytes),
        (EffectKind::Call(_), "fs.write" | "fs.write_text") => Some(EffectOperation::WriteText),
        (EffectKind::Call(_), "fs.write_bytes") => Some(EffectOperation::WriteBytes),
        (EffectKind::Call(_), "fs.list") => Some(EffectOperation::List),
        (EffectKind::Call(_), "fs.search") => Some(EffectOperation::Search),
        (EffectKind::Call(_), "net.http_get" | "http.get") => Some(EffectOperation::HttpGet),
        (EffectKind::Call(_), "exec.run") => Some(EffectOperation::ExecRun),
        (EffectKind::Call(_), "permission.request_external_fs" | "permission.request_file") => {
            Some(EffectOperation::PermissionRequestFile)
        }
        (EffectKind::Call(_), "permission.request_directory") => {
            Some(EffectOperation::PermissionRequestDirectory)
        }
        (EffectKind::Agent(_), "agent.message") => Some(EffectOperation::AgentMessage),
        (EffectKind::Agent(_), "agent.ask") => Some(EffectOperation::AgentAsk),
        (EffectKind::Agent(_), "agent.transcript") => Some(EffectOperation::AgentTranscript),
        (EffectKind::Agent(_), "model.request") => Some(EffectOperation::ModelRequest),
        (EffectKind::Agent(_), "user.ask") => Some(EffectOperation::UserAsk),
        (EffectKind::SubAgent(_), "sub_agent.create") => Some(EffectOperation::SubAgentCreate),
        (EffectKind::SubAgent(_), "sub_agent.run") => Some(EffectOperation::SubAgentRun),
        (EffectKind::SubAgent(_), "sub_agent.message") => Some(EffectOperation::SubAgentMessage),
        (EffectKind::SubAgent(_), "sub_agent.ask") => Some(EffectOperation::SubAgentAsk),
        _ => None,
    }
}

fn validate_current_common_raw_error(error: &RecordedVmError) -> bool {
    matches!(
        error,
        RecordedVmError::Cancelled
            | RecordedVmError::Timeout { .. }
            | RecordedVmError::ResourceLimit { .. }
            | RecordedVmError::ProtocolViolation
            | RecordedVmError::ReplayRuntimeDiverged
    )
}

fn validate_current_tool_raw_error(error: &RecordedVmError) -> bool {
    validate_current_common_raw_error(error)
        || matches!(
            error,
            RecordedVmError::CapabilityMissing
                | RecordedVmError::ToolUnavailable
                | RecordedVmError::ToolSchemaError
        )
}

fn validate_current_operation_raw_error(
    operation: EffectOperation,
    error: &RecordedVmError,
) -> bool {
    if validate_current_common_raw_error(error) {
        return true;
    }
    match operation {
        EffectOperation::ReadText
        | EffectOperation::ReadBytes
        | EffectOperation::WriteText
        | EffectOperation::WriteBytes
        | EffectOperation::List
        | EffectOperation::Search
        | EffectOperation::HttpGet
        | EffectOperation::ExecRun => matches!(error, RecordedVmError::CapabilityMissing),
        EffectOperation::PermissionRequestFile | EffectOperation::PermissionRequestDirectory => {
            matches!(
                error,
                RecordedVmError::CapabilityMissing | RecordedVmError::AgentUnavailable
            )
        }
        EffectOperation::AgentMessage | EffectOperation::AgentTranscript => matches!(
            error,
            RecordedVmError::CapabilityMissing | RecordedVmError::AgentUnavailable
        ),
        EffectOperation::AgentAsk => matches!(
            error,
            RecordedVmError::CapabilityMissing
                | RecordedVmError::AgentUnavailable
                | RecordedVmError::AgentResponseSchema
        ),
        EffectOperation::ModelRequest => matches!(
            error,
            RecordedVmError::CapabilityMissing
                | RecordedVmError::ModelUnavailable
                | RecordedVmError::ModelValidationError
        ),
        EffectOperation::UserAsk => matches!(
            error,
            RecordedVmError::CapabilityMissing
                | RecordedVmError::UserUnavailable
                | RecordedVmError::ResponseValidationError
        ),
        EffectOperation::SubAgentCreate | EffectOperation::SubAgentMessage => matches!(
            error,
            RecordedVmError::CapabilityMissing | RecordedVmError::SubAgentUnavailable
        ),
        EffectOperation::SubAgentRun | EffectOperation::SubAgentAsk => matches!(
            error,
            RecordedVmError::CapabilityMissing
                | RecordedVmError::SubAgentUnavailable
                | RecordedVmError::SubAgentResponseSchema
        ),
    }
}

fn validate_header(header: &ReplayHeader) -> Result<(), ReplayError> {
    if header.bytecode_version != BYTECODE_VERSION
        || is_zero_digest(&header.artifact_digest)
        || is_zero_digest(&header.contract_digest)
        || is_zero_digest(&header.language_digest)
        || is_zero_digest(&header.runtime_digest)
        || is_zero_digest(&header.policy_digest)
        || is_zero_digest(&header.catalog_digest)
        || is_zero_digest(&header.capability_digest)
        || is_zero_digest(&header.error_registry_digest)
    {
        return Err(ReplayError::InvalidJournal);
    }
    for values in [
        &header.effective_manifest_grants,
        &header.requested_exec_commands,
        &header.requested_exec_environment,
        &header.effective_exec_grants,
        &header.effective_exec_environment,
    ] {
        if !values.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(ReplayError::InvalidJournal);
        }
    }
    if header
        .requested_exec_commands
        .iter()
        .chain(&header.effective_exec_grants)
        .any(|pattern| CommandPattern::parse(pattern).is_err())
        || header
            .requested_exec_environment
            .iter()
            .chain(&header.effective_exec_environment)
            .any(|name| !is_exec_environment_name(name))
    {
        return Err(ReplayError::InvalidJournal);
    }
    let environment_digest_is_zero = is_zero_digest(&header.effective_exec_environment_digest);
    let pinned_digest_is_zero = is_zero_digest(&header.pinned_exec_identity_digest);
    if (header.effective_exec_grants.is_empty()
        && (!environment_digest_is_zero || !pinned_digest_is_zero))
        || (!header.effective_exec_grants.is_empty()
            && (environment_digest_is_zero || pinned_digest_is_zero))
    {
        return Err(ReplayError::InvalidJournal);
    }
    Ok(())
}

fn is_exec_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|first| {
        (first.is_ascii_alphabetic() || first == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && !name.eq_ignore_ascii_case("LC_ALL")
            && !name.eq_ignore_ascii_case("TZ")
    })
}

fn validate_execution_outcome(
    outcome: Option<&ReplayExecutionOutcome>,
    limits: ReplayLimits,
) -> Result<(), ReplayError> {
    if let Some(ReplayExecutionOutcome::Failed { reason }) = outcome {
        if reason.len() > 2_048 {
            return Err(ReplayError::LimitExceeded);
        }
    }
    if let Some(
        ReplayExecutionOutcome::Stopped { reason } | ReplayExecutionOutcome::Failed { reason },
    ) = outcome
    {
        if reason.len() > limits.payload_bytes {
            return Err(ReplayError::LimitExceeded);
        }
    }
    let Some(outcome) = outcome else {
        return Err(ReplayError::InvalidJournal);
    };
    if let ReplayExecutionOutcome::Terminal { error } = outcome {
        validate_current_terminal_error(error)?;
    }
    Ok(())
}

fn validate_current_terminal_error(error: &RecordedVmError) -> Result<(), ReplayError> {
    if matches!(
        error,
        RecordedVmError::ArithmeticOverflow
            | RecordedVmError::DivisionByZero
            | RecordedVmError::IndexOutOfBounds
            | RecordedVmError::MapKeyNotFound
            | RecordedVmError::DuplicateMapKey
            | RecordedVmError::ResourceLimit { .. }
            | RecordedVmError::Timeout { .. }
            | RecordedVmError::Cancelled
            | RecordedVmError::RuntimePanic
            | RecordedVmError::ProtocolViolation
            | RecordedVmError::ReplayRuntimeDiverged
    ) {
        Ok(())
    } else {
        Err(ReplayError::InvalidJournal)
    }
}

fn is_manifest_grantable_capability(capability: &str) -> bool {
    matches!(
        capability,
        "fs.read" | "fs.write" | "net.http_get" | "permission.request_external_fs" | "exec.run"
    )
}

fn schema_digest(value_type: &ValueType) -> [u8; 32] {
    digest_bytes(&canonical_value_type_bytes(value_type))
}

fn digest_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn request_digest(request: &EffectRequest) -> [u8; 32] {
    let effect = match &request.kind {
        EffectKind::Call(value) => format!("call:{value}"),
        EffectKind::Tool(value) => format!("tool:{value}"),
        EffectKind::Agent(value) => format!("agent:{value}"),
        EffectKind::SubAgent(value) => format!("sub_agent:{value}"),
    };
    let mut digest = Sha256::new();
    digest.update(effect.as_bytes());
    digest.update([0]);
    digest.update(request.schema_digest);
    digest.update([0]);
    digest.update(&request.request);
    digest.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::needless_pass_by_value)]
    fn canonical(value: Value) -> Vec<u8> {
        encode_canonical_with_limit(&value, 1 << 20).unwrap()
    }

    fn outcome(value: Value) -> EffectOutcome {
        EffectOutcome::Ok(canonical(value))
    }

    fn result_value(ok: bool, payload: Value) -> Value {
        Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::Result,
            type_name: Rc::from("Result"),
            variant: u32::from(!ok),
            variant_name: Rc::from(if ok { "Ok" } else { "Err" }),
            payload: EnumPayload::Tuple(Rc::from([payload])),
        }))
    }

    fn standard_error(code: &str, message: &str) -> Value {
        Value::Record(Rc::from([
            (Rc::from("code"), Value::String(Rc::from(code))),
            (Rc::from("message"), Value::String(Rc::from(message))),
        ]))
    }

    fn nominal_enum_types() -> Vec<EnumType> {
        vec![
            EnumType {
                name: "pkg://test/root.allen::Inner".to_owned(),
                variants: vec![
                    allen_bytecode::EnumVariant {
                        name: "Empty".to_owned(),
                        payload: EnumPayloadType::Unit,
                    },
                    allen_bytecode::EnumVariant {
                        name: "Text".to_owned(),
                        payload: EnumPayloadType::Tuple(vec![ValueType::String]),
                    },
                    allen_bytecode::EnumVariant {
                        name: "Details".to_owned(),
                        payload: EnumPayloadType::Record(vec![
                            allen_bytecode::RecordField {
                                name: "count".to_owned(),
                                value_type: ValueType::Int,
                            },
                            allen_bytecode::RecordField {
                                name: "label".to_owned(),
                                value_type: ValueType::String,
                            },
                        ]),
                    },
                ],
            },
            EnumType {
                name: "pkg://test/root.allen::Outer".to_owned(),
                variants: vec![
                    allen_bytecode::EnumVariant {
                        name: "Wrapped".to_owned(),
                        payload: EnumPayloadType::Tuple(vec![ValueType::Enum(0)]),
                    },
                    allen_bytecode::EnumVariant {
                        name: "Batch".to_owned(),
                        payload: EnumPayloadType::Record(vec![allen_bytecode::RecordField {
                            name: "items".to_owned(),
                            value_type: ValueType::List(Box::new(ValueType::Enum(0))),
                        }]),
                    },
                ],
            },
        ]
    }

    fn nominal_enum(type_id: u32, variant: u32, payload: EnumPayload) -> Value {
        Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::User(type_id),
            type_name: Rc::from("<canonical>"),
            variant,
            variant_name: Rc::from("<canonical>"),
            payload,
        }))
    }

    fn request(name: &str) -> EffectRequest {
        EffectRequest::new(
            EffectKind::Call(name.to_owned()),
            canonical(Value::String(Rc::from("request"))),
            &ValueType::String,
            ReplayLimits::default(),
        )
        .unwrap()
    }

    fn bound_header() -> ReplayHeader {
        ReplayHeader {
            bytecode_version: BYTECODE_VERSION,
            artifact_digest: [1; 32],
            contract_digest: [2; 32],
            language_digest: [3; 32],
            runtime_digest: [4; 32],
            policy_digest: [5; 32],
            catalog_digest: [6; 32],
            capability_digest: [7; 32],
            error_registry_digest: [8; 32],
            effective_manifest_grants: Vec::new(),
            requested_exec_commands: Vec::new(),
            requested_exec_environment: Vec::new(),
            effective_exec_grants: Vec::new(),
            effective_exec_environment: Vec::new(),
            effective_exec_environment_digest: [0; 32],
            pinned_exec_identity_digest: [0; 32],
            scheduler_completion_order: Vec::new(),
        }
    }

    fn binding_from_header(header: &ReplayHeader) -> EffectExecutionBinding {
        EffectExecutionBinding {
            bytecode_version: header.bytecode_version,
            artifact_digest: header.artifact_digest,
            contract_digest: header.contract_digest,
            language_digest: header.language_digest,
            runtime_digest: header.runtime_digest,
            policy_digest: header.policy_digest,
            catalog_digest: header.catalog_digest,
            capability_digest: header.capability_digest,
            error_registry_digest: header.error_registry_digest,
            effective_manifest_grants: header.effective_manifest_grants.clone(),
            requested_exec_commands: header.requested_exec_commands.clone(),
            requested_exec_environment: header.requested_exec_environment.clone(),
            effective_exec_grants: header.effective_exec_grants.clone(),
            effective_exec_environment: header.effective_exec_environment.clone(),
            effective_exec_environment_digest: header.effective_exec_environment_digest,
            pinned_exec_identity_digest: header.pinned_exec_identity_digest,
        }
    }

    fn log() -> ReplayLog {
        let mut recorder = Recorder::new(ReplayLimits::default());
        recorder
            .record(
                request("fs.read"),
                outcome(Value::String(Rc::from("result"))),
                false,
                &RefuseSensitive,
            )
            .unwrap();
        recorder.finish().unwrap()
    }

    #[test]
    fn canonical_json_round_trips_exactly() {
        let log = log();
        let json = log.to_json().unwrap();
        assert!(json.starts_with(r#"{"format":"ALLEN-REPLAY/3","header":{"#));
        assert_eq!(
            ReplayLog::from_json(&json, ReplayLimits::default()).unwrap(),
            log
        );
        assert_eq!(
            ReplayLog::from_json(
                " {\"format\":\"ALLEN-REPLAY/2\",\"entries\":[]}",
                ReplayLimits::default()
            ),
            Err(ReplayError::InvalidJournal)
        );
    }

    #[test]
    fn replay_detects_request_schema_and_order_drift_without_live_work() {
        let mut session = ReplaySession::new(&log());
        let mut wrong = request("fs.write");
        assert_eq!(session.start(&wrong), Err(ReplayError::ReplayDiverged));
        wrong = request("fs.read");
        wrong.schema_digest[0] ^= 1;
        assert_eq!(session.start(&wrong), Err(ReplayError::ReplayDiverged));
        let pending = session.start(&request("fs.read")).unwrap();
        assert_eq!(session.complete_next().unwrap().0, pending);
        assert_eq!(
            session.start(&request("fs.read")),
            Err(ReplayError::ReplayDiverged)
        );
        assert!(session.finish().is_ok());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn replay_provider_refuses_mismatched_execution_binding_before_dispatch() {
        let header = bound_header();
        let mut recorder = Recorder::with_header(ReplayLimits::default(), header.clone());
        recorder
            .record(
                request("fs.read"),
                outcome(Value::Unit),
                false,
                &RefuseSensitive,
            )
            .unwrap();
        let log = recorder
            .finish_with_execution_outcome(ReplayExecutionOutcome::Completed)
            .unwrap();
        let mut mismatches = Vec::new();
        let mut wrong = header.clone();
        wrong.bytecode_version = 12;
        mismatches.push(wrong);
        for mutate in [
            |value: &mut ReplayHeader| value.artifact_digest[0] ^= 1,
            |value: &mut ReplayHeader| value.contract_digest[0] ^= 1,
            |value: &mut ReplayHeader| value.language_digest[0] ^= 1,
            |value: &mut ReplayHeader| value.runtime_digest[0] ^= 1,
            |value: &mut ReplayHeader| value.policy_digest[0] ^= 1,
            |value: &mut ReplayHeader| value.catalog_digest[0] ^= 1,
            |value: &mut ReplayHeader| value.capability_digest[0] ^= 1,
            |value: &mut ReplayHeader| value.error_registry_digest[0] ^= 1,
        ] {
            let mut wrong = header.clone();
            mutate(&mut wrong);
            mismatches.push(wrong);
        }
        let mut wrong_grants = header;
        wrong_grants.effective_manifest_grants = vec!["fs.write".to_owned()];
        mismatches.push(wrong_grants);
        for mutate in [
            |value: &mut ReplayHeader| value.requested_exec_commands.push("env *".to_owned()),
            |value: &mut ReplayHeader| value.requested_exec_environment.push("HOME".to_owned()),
            |value: &mut ReplayHeader| value.effective_exec_grants.push("env".to_owned()),
            |value: &mut ReplayHeader| value.effective_exec_environment.push("HOME".to_owned()),
            |value: &mut ReplayHeader| value.effective_exec_environment_digest[0] ^= 1,
            |value: &mut ReplayHeader| value.pinned_exec_identity_digest[0] ^= 1,
        ] {
            let mut wrong = bound_header();
            mutate(&mut wrong);
            mismatches.push(wrong);
        }

        for wrong in mismatches {
            assert!(matches!(
                ReplayingEffectProvider::new(
                    &log,
                    &wrong,
                    ReplayLimits::default(),
                    NoToolSchemas,
                    &[],
                ),
                Err(ReplayError::ReplayDiverged)
            ));
        }

        let mut replay = ReplayingEffectProvider::new(
            &log,
            log.header(),
            ReplayLimits::default(),
            NoToolSchemas,
            &[],
        )
        .unwrap();
        replay
            .bind_execution(&binding_from_header(log.header()))
            .unwrap();
        for mutate in [
            |value: &mut EffectExecutionBinding| value.artifact_digest[0] ^= 1,
            |value: &mut EffectExecutionBinding| value.contract_digest[0] ^= 1,
            |value: &mut EffectExecutionBinding| value.language_digest[0] ^= 1,
            |value: &mut EffectExecutionBinding| value.runtime_digest[0] ^= 1,
            |value: &mut EffectExecutionBinding| value.policy_digest[0] ^= 1,
            |value: &mut EffectExecutionBinding| value.catalog_digest[0] ^= 1,
            |value: &mut EffectExecutionBinding| value.capability_digest[0] ^= 1,
            |value: &mut EffectExecutionBinding| value.error_registry_digest[0] ^= 1,
        ] {
            let mut binding = binding_from_header(log.header());
            mutate(&mut binding);
            let mut replay = ReplayingEffectProvider::new(
                &log,
                log.header(),
                ReplayLimits::default(),
                NoToolSchemas,
                &[],
            )
            .unwrap();
            assert_eq!(
                replay.bind_execution(&binding),
                Err(VmError::ReplayDiverged)
            );
        }
        for binding in [
            {
                let mut binding = binding_from_header(log.header());
                binding.bytecode_version -= 1;
                binding
            },
            {
                let mut binding = binding_from_header(log.header());
                binding.effective_manifest_grants.push("fs.read".to_owned());
                binding
            },
            {
                let mut binding = binding_from_header(log.header());
                binding.requested_exec_commands.push("env *".to_owned());
                binding
            },
            {
                let mut binding = binding_from_header(log.header());
                binding.requested_exec_environment.push("HOME".to_owned());
                binding
            },
            {
                let mut binding = binding_from_header(log.header());
                binding.effective_exec_grants.push("env".to_owned());
                binding
            },
            {
                let mut binding = binding_from_header(log.header());
                binding.effective_exec_environment.push("HOME".to_owned());
                binding
            },
            {
                let mut binding = binding_from_header(log.header());
                binding.effective_exec_environment_digest[0] ^= 1;
                binding
            },
            {
                let mut binding = binding_from_header(log.header());
                binding.pinned_exec_identity_digest[0] ^= 1;
                binding
            },
        ] {
            let mut replay = ReplayingEffectProvider::new(
                &log,
                log.header(),
                ReplayLimits::default(),
                NoToolSchemas,
                &[],
            )
            .unwrap();
            assert_eq!(
                replay.bind_execution(&binding),
                Err(VmError::ReplayDiverged)
            );
        }
    }

    #[test]
    fn replay_requires_every_current_binding() {
        let limits = ReplayLimits::default();
        let mut header = bound_header();
        header.effective_manifest_grants = vec!["fs.read".to_owned(), "net.http_get".to_owned()];
        let log = ReplayLog::new(
            header.clone(),
            vec![],
            ReplayExecutionOutcome::Completed,
            limits,
        )
        .unwrap();
        let json = log.to_json().unwrap();
        assert!(json.contains("ALLEN-REPLAY/3"));
        assert_eq!(
            ReplayLog::from_json(&json, limits).unwrap().header(),
            &header
        );
        assert_eq!(
            header.execution_capabilities().iter().collect::<Vec<_>>(),
            ["fs.read", "net.http_get"]
        );

        for grants in [
            vec!["fs.read".to_owned(), "fs.read".to_owned()],
            vec!["net.http_get".to_owned(), "fs.read".to_owned()],
            vec!["agent.ask".to_owned()],
        ] {
            let mut invalid = bound_header();
            invalid.effective_manifest_grants = grants;
            assert_eq!(
                ReplayLog::new(invalid, vec![], ReplayExecutionOutcome::Completed, limits),
                Err(ReplayError::InvalidJournal)
            );
        }

        for mutate in [
            |value: &mut ReplayHeader| {
                value.requested_exec_commands = vec!["sh -c 'echo escaped'".to_owned()];
            },
            |value: &mut ReplayHeader| {
                value.requested_exec_environment = vec!["LC_ALL".to_owned()];
            },
            |value: &mut ReplayHeader| {
                value.effective_exec_grants = vec!["env".to_owned()];
                value.effective_exec_environment_digest = [1; 32];
            },
            |value: &mut ReplayHeader| {
                value.effective_exec_grants = vec!["env".to_owned()];
                value.pinned_exec_identity_digest = [1; 32];
            },
        ] {
            let mut invalid = bound_header();
            mutate(&mut invalid);
            assert_eq!(
                ReplayLog::new(invalid, vec![], ReplayExecutionOutcome::Completed, limits),
                Err(ReplayError::InvalidJournal)
            );
        }

        let mut missing_bindings = Vec::new();
        let mut wrong_version = bound_header();
        wrong_version.bytecode_version = 12;
        missing_bindings.push(wrong_version);
        for clear in [
            |value: &mut ReplayHeader| value.artifact_digest = [0; 32],
            |value: &mut ReplayHeader| value.contract_digest = [0; 32],
            |value: &mut ReplayHeader| value.language_digest = [0; 32],
            |value: &mut ReplayHeader| value.runtime_digest = [0; 32],
            |value: &mut ReplayHeader| value.policy_digest = [0; 32],
            |value: &mut ReplayHeader| value.catalog_digest = [0; 32],
            |value: &mut ReplayHeader| value.capability_digest = [0; 32],
            |value: &mut ReplayHeader| value.error_registry_digest = [0; 32],
        ] {
            let mut invalid = bound_header();
            clear(&mut invalid);
            missing_bindings.push(invalid);
        }
        for invalid in missing_bindings {
            assert_eq!(
                ReplayLog::new(invalid, vec![], ReplayExecutionOutcome::Completed, limits),
                Err(ReplayError::InvalidJournal)
            );
        }

        let mut missing_final = log.clone();
        missing_final.execution_outcome = None;
        let missing_final_json = missing_final.to_json().unwrap();
        assert_eq!(
            ReplayLog::from_json(&missing_final_json, limits),
            Err(ReplayError::InvalidJournal)
        );
        assert!(matches!(
            ReplayingEffectProvider::new(&missing_final, &header, limits, NoToolSchemas, &[],),
            Err(ReplayError::InvalidJournal)
        ));
    }

    struct RedactStopped;

    impl ReplayRecordingPolicy for RedactStopped {
        fn classify(&self, _request: &EffectRequest) -> RecordingDisposition {
            RecordingDisposition::Record
        }

        fn classify_stopped_reason(&self, _reason: &str) -> RecordingDisposition {
            RecordingDisposition::Redact
        }
    }

    #[test]
    fn stopped_reason_is_refused_by_default_and_by_schema_agnostic_redaction() {
        const CANARY: &str = "CANARY-SECRET stop detail";
        let limits = ReplayLimits::default();
        assert_eq!(
            Recorder::with_header(limits, bound_header()).finish_with_execution_outcome(
                ReplayExecutionOutcome::Stopped {
                    reason: CANARY.to_owned(),
                }
            ),
            Err(ReplayError::RefusedValue)
        );
        assert_eq!(
            Recorder::with_header(limits, bound_header()).finish_with_execution_outcome(
                ReplayExecutionOutcome::Failed {
                    reason: CANARY.to_owned(),
                }
            ),
            Err(ReplayError::RefusedValue)
        );

        assert_eq!(
            Recorder::with_header(limits, bound_header()).finish_with_execution_outcome_policy(
                ReplayExecutionOutcome::Stopped {
                    reason: CANARY.to_owned(),
                },
                &RedactStopped,
                &DigestRedactor,
            ),
            Err(ReplayError::RefusedValue)
        );
    }

    #[test]
    fn recorder_completion_failure_poisoning_cannot_emit_a_truncated_log() {
        let mut recorder = Recorder::new(ReplayLimits::default());
        let sequence = recorder.start(request("secret"), true).unwrap();
        assert_eq!(
            recorder.complete(
                sequence,
                outcome(Value::String("CANARY-low-entropy".into())),
                &DigestRedactor,
            ),
            Err(ReplayError::RefusedValue)
        );
        assert_eq!(recorder.finish(), Err(ReplayError::ReplayDiverged));
    }

    #[test]
    fn runtime_replay_finalization_requires_exhaustion_and_exact_channel() {
        let limits = ReplayLimits::default();
        let completed = ReplayLog::new(
            bound_header(),
            vec![],
            ReplayExecutionOutcome::Completed,
            limits,
        )
        .unwrap();
        let mut replay = ReplayingEffectProvider::new(
            &completed,
            completed.header(),
            limits,
            NoToolSchemas,
            &[],
        )
        .unwrap();
        replay
            .finish_execution(EffectExecutionOutcome::Completed)
            .unwrap();

        let mut mismatch = ReplayingEffectProvider::new(
            &completed,
            completed.header(),
            limits,
            NoToolSchemas,
            &[],
        )
        .unwrap();
        assert_eq!(
            mismatch.finish_execution(EffectExecutionOutcome::Stopped { reason: "other" }),
            Err(VmError::ReplayRuntimeDiverged)
        );

        let request = request("fs.read");
        let mut header = bound_header();
        header.scheduler_completion_order = vec![0];
        let with_leftover = ReplayLog::new(
            header,
            vec![ReplayEntry {
                sequence: 0,
                effect: request.kind.clone(),
                request_digest: request_digest(&request),
                schema_digest: request.schema_digest,
                outcome: outcome(Value::Unit),
            }],
            ReplayExecutionOutcome::Completed,
            limits,
        )
        .unwrap();
        let mut leftover = ReplayingEffectProvider::new(
            &with_leftover,
            with_leftover.header(),
            limits,
            NoToolSchemas,
            &[],
        )
        .unwrap();
        assert_eq!(
            leftover.finish_execution(EffectExecutionOutcome::Completed),
            Err(VmError::ReplayRuntimeDiverged)
        );

        let terminal = ReplayLog::new(
            bound_header(),
            vec![],
            ReplayExecutionOutcome::terminal(&VmError::ArithmeticOverflow).unwrap(),
            limits,
        )
        .unwrap();
        let mut trap_mismatch =
            ReplayingEffectProvider::new(&terminal, terminal.header(), limits, NoToolSchemas, &[])
                .unwrap();
        assert_eq!(
            trap_mismatch.finish_execution(EffectExecutionOutcome::Terminal {
                error: &VmError::DivisionByZero,
            }),
            Err(VmError::ReplayRuntimeDiverged)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn current_replay_round_trips_and_checks_exact_final_execution_channels() {
        let limits = ReplayLimits::default();
        let outcomes = [
            ReplayExecutionOutcome::Completed,
            ReplayExecutionOutcome::Stopped {
                reason: "finished by program".to_owned(),
            },
            ReplayExecutionOutcome::Failed {
                reason: "failed by program".to_owned(),
            },
            ReplayExecutionOutcome::terminal(&VmError::ArithmeticOverflow).unwrap(),
            ReplayExecutionOutcome::terminal(&VmError::DivisionByZero).unwrap(),
            ReplayExecutionOutcome::terminal(&VmError::IndexOutOfBounds).unwrap(),
            ReplayExecutionOutcome::terminal(&VmError::MapKeyNotFound).unwrap(),
            ReplayExecutionOutcome::terminal(&VmError::DuplicateMapKey).unwrap(),
            ReplayExecutionOutcome::terminal(&VmError::Cancelled).unwrap(),
            ReplayExecutionOutcome::terminal(&VmError::Timeout {
                resource: "wall_time",
            })
            .unwrap(),
            ReplayExecutionOutcome::terminal(&VmError::ResourceLimit {
                resource: "effects",
            })
            .unwrap(),
            ReplayExecutionOutcome::terminal(&VmError::Invariant("secret internal detail"))
                .unwrap(),
            ReplayExecutionOutcome::terminal(&VmError::ProtocolViolation).unwrap(),
        ];
        for outcome in outcomes {
            let log = ReplayLog::new(bound_header(), vec![], outcome.clone(), limits).unwrap();
            let json = log.to_json().unwrap();
            assert!(!json.contains("secret internal detail"));
            let parsed = ReplayLog::from_json(&json, limits).unwrap();
            assert_eq!(parsed.execution_outcome(), Some(&outcome));
            ReplaySession::new(&parsed)
                .finish_with_execution_outcome(&outcome)
                .unwrap();
            assert_eq!(
                ReplaySession::new(&parsed)
                    .finish_with_execution_outcome(&ReplayExecutionOutcome::Completed),
                if outcome == ReplayExecutionOutcome::Completed {
                    Ok(())
                } else {
                    Err(ReplayError::ReplayDiverged)
                }
            );
        }

        let oversized = ReplayExecutionOutcome::Stopped {
            reason: "x".repeat(limits.payload_bytes + 1),
        };
        assert_eq!(
            ReplayLog::new(bound_header(), vec![], oversized, limits),
            Err(ReplayError::LimitExceeded)
        );

        for nonterminal in [
            VmError::AgentUnavailable,
            VmError::ReplayDiverged,
            VmError::CapabilityMissing,
            VmError::ToolUnavailable,
            VmError::AgentResponseSchema,
        ] {
            assert_eq!(
                ReplayExecutionOutcome::terminal(&nonterminal),
                Err(ReplayError::RefusedValue)
            );
        }
        for invalid in [
            RecordedVmError::AgentUnavailable,
            RecordedVmError::ReplayDiverged,
            RecordedVmError::CapabilityMissing,
            RecordedVmError::ToolSchemaError,
        ] {
            assert_eq!(
                ReplayLog::new(
                    bound_header(),
                    vec![],
                    ReplayExecutionOutcome::Terminal { error: invalid },
                    limits,
                ),
                Err(ReplayError::InvalidJournal)
            );
        }

        let request = EffectRequest::new(
            EffectKind::Agent("agent.ask".to_owned()),
            canonical(Value::String(Rc::from("request"))),
            &ValueType::String,
            limits,
        )
        .unwrap();
        let mut header = bound_header();
        header.scheduler_completion_order = vec![0];
        let provider_error = ReplayEntry {
            sequence: 0,
            effect: request.kind.clone(),
            request_digest: request_digest(&request),
            schema_digest: request.schema_digest,
            outcome: EffectOutcome::Err {
                error: RecordedVmError::AgentUnavailable,
            },
        };
        assert!(
            ReplayLog::new(
                header,
                vec![provider_error],
                ReplayExecutionOutcome::Completed,
                limits,
            )
            .is_ok()
        );
    }

    #[test]
    fn bounds_and_sensitive_canaries_are_refused_without_schema_aware_redaction() {
        let limits = ReplayLimits {
            payload_bytes: 4,
            ..ReplayLimits::default()
        };
        assert_eq!(
            EffectRequest::new(
                EffectKind::Call("x".into()),
                vec![0; 5],
                &ValueType::Unit,
                limits
            ),
            Err(ReplayError::LimitExceeded)
        );
        let canary = b"CANARY-SECRET".to_vec();
        let mut recorder = Recorder::new(ReplayLimits::default());
        assert_eq!(
            recorder.record(
                request("secret"),
                outcome(Value::Bytes(Rc::from(canary.clone()))),
                true,
                &RefuseSensitive
            ),
            Err(ReplayError::RefusedValue)
        );
        let mut recorder = Recorder::new(ReplayLimits::default());
        assert_eq!(
            recorder.record(
                request("secret"),
                outcome(Value::Bytes(Rc::from(canary))),
                true,
                &DigestRedactor,
            ),
            Err(ReplayError::RefusedValue)
        );
    }

    struct ChangingRedactor;

    impl Redactor for ChangingRedactor {
        fn redact(
            &self,
            request: &EffectRequest,
            outcome: &EffectOutcome,
            _limits: ReplayLimits,
        ) -> Result<(EffectRequest, EffectOutcome), ReplayError> {
            let mut request = request.clone();
            request.request.push(0);
            Ok((request, outcome.clone()))
        }
    }

    #[test]
    fn executable_recording_refuses_semantic_redactor_replacements() {
        let mut recorder = Recorder::new(ReplayLimits::default());
        assert_eq!(
            recorder.record(
                request("secret"),
                outcome(Value::Unit),
                true,
                &ChangingRedactor,
            ),
            Err(ReplayError::RefusedValue)
        );
        assert_eq!(recorder.finish(), Err(ReplayError::ReplayDiverged));
    }

    #[test]
    fn opaque_vm_values_are_refused_before_recording() {
        let value = Value::SubAgent(allen_vm::SubAgentValue::new(1, 2, 3));
        assert_eq!(
            EffectRequest::from_value(
                EffectKind::SubAgent("sub_agent.ask".to_owned()),
                &value,
                &ValueType::String,
                ReplayLimits::default(),
            ),
            Err(ReplayError::RefusedValue)
        );
    }

    #[test]
    fn replay_safe_errors_preserve_supported_variants_and_redact_invariants() {
        for error in [
            VmError::Cancelled,
            VmError::Timeout {
                resource: "wall_time",
            },
            VmError::ResourceLimit {
                resource: "effects",
            },
            VmError::ArithmeticOverflow,
        ] {
            let recorded = recorded_vm_error(&error).unwrap();
            assert_eq!(vm_error_from_recorded(&recorded), error);
        }
        assert_eq!(
            recorded_vm_error(&VmError::Stopped {
                reason: "not execution-local".to_owned(),
            }),
            Err(ReplayError::RefusedValue)
        );
        assert_eq!(
            recorded_vm_error(&VmError::Invariant("contextual internal failure")),
            Ok(RecordedVmError::RuntimePanic)
        );
        assert_eq!(
            recorded_vm_error(&VmError::ProtocolViolation),
            Ok(RecordedVmError::ProtocolViolation)
        );
    }

    #[test]
    fn replay_never_invokes_the_live_provider_closure() {
        let mut harness = EffectHarness::Replay(ReplaySession::new(&log()));
        let calls = std::cell::Cell::new(0_u32);
        assert_eq!(
            harness
                .execute(request("fs.read"), false, &RefuseSensitive, || {
                    calls.set(calls.get() + 1);
                    Ok(outcome(Value::String(Rc::from("live"))))
                })
                .unwrap(),
            outcome(Value::String(Rc::from("result")))
        );
        assert_eq!(calls.get(), 0);
        assert_eq!(harness.finish(), Ok(None));
    }

    #[test]
    fn replay_exec_run_never_invokes_the_spawn_canary() {
        let stdin = Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::Option,
            type_name: "Option".into(),
            variant: 1,
            variant_name: "Some".into(),
            payload: EnumPayload::Tuple(vec![Value::Bytes(b"input".to_vec().into())].into()),
        }));
        let arguments = Value::Tuple(
            vec![
                Value::List(
                    vec![
                        Value::String("printf".into()),
                        Value::String("%s; touch /tmp/replay-must-not-spawn".into()),
                    ]
                    .into(),
                ),
                stdin,
            ]
            .into(),
        );
        let result_type = allen_bytecode::effect_result_type(EffectOperation::ExecRun, None)
            .expect("exec.run has a closed result type");
        let request = EffectRequest::from_value(
            EffectKind::Call("exec.run".to_owned()),
            &arguments,
            &result_type,
            ReplayLimits::default(),
        )
        .unwrap();
        let response = Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::Result,
            type_name: "Result".into(),
            variant: 0,
            variant_name: "Ok".into(),
            payload: EnumPayload::Tuple(
                vec![Value::Record(
                    vec![
                        ("status".into(), Value::Int(7)),
                        ("stderr".into(), Value::Bytes(Vec::new().into())),
                        ("stdout".into(), Value::Bytes(b"data".to_vec().into())),
                    ]
                    .into(),
                )]
                .into(),
            ),
        }));
        let mut recorder = Recorder::new(ReplayLimits::default());
        recorder
            .record(
                request.clone(),
                outcome(response.clone()),
                false,
                &RefuseSensitive,
            )
            .unwrap();
        let mut harness = EffectHarness::Replay(ReplaySession::new(&recorder.finish().unwrap()));
        let spawn_attempts = std::cell::Cell::new(0_u32);
        assert_eq!(
            harness
                .execute(request, false, &RefuseSensitive, || {
                    spawn_attempts.set(spawn_attempts.get() + 1);
                    Ok(outcome(Value::Unit))
                })
                .unwrap(),
            outcome(response)
        );
        assert_eq!(spawn_attempts.get(), 0);
        assert_eq!(harness.finish(), Ok(None));
    }

    struct NeverCancelled;

    impl CancellationSource for NeverCancelled {
        fn is_cancelled(&mut self) -> bool {
            false
        }
    }

    struct FixtureSchemas;

    struct StrictFixtureSchemas;

    impl ToolResultSchema for FixtureSchemas {
        fn result_type(&self, tool: u32) -> Option<ValueType> {
            (tool == 3).then_some(ValueType::String)
        }

        fn validate_result(&self, tool: u32, value: &Value) -> bool {
            tool == 3 && matches!(value, Value::String(value) if value.len() <= 16)
        }
    }

    impl ToolResultSchema for StrictFixtureSchemas {
        fn result_type(&self, tool: u32) -> Option<ValueType> {
            (tool == 3).then_some(ValueType::String)
        }

        fn validate_result(&self, tool: u32, value: &Value) -> bool {
            if tool != 3 {
                return false;
            }
            let Value::Enum(result) = value else {
                return false;
            };
            let EnumPayload::Tuple(payload) = &result.payload else {
                return false;
            };
            match (result.variant, payload.first()) {
                (0, Some(Value::String(value))) => value.len() <= 3,
                (1, Some(Value::Enum(error))) if error.variant == 0 => {
                    matches!(&error.payload, EnumPayload::Tuple(payload) if matches!(payload.first(), Some(Value::String(value)) if value.len() <= 3))
                }
                _ => false,
            }
        }
    }

    struct RedactAll;

    impl ReplayRecordingPolicy for RedactAll {
        fn classify(&self, _request: &EffectRequest) -> RecordingDisposition {
            RecordingDisposition::Redact
        }
    }

    struct SecretFixture;

    impl EffectProvider for SecretFixture {
        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(
            &mut self,
            _operation: EffectOperation,
            _arguments: &[Value],
        ) -> Result<Value, VmError> {
            Ok(Value::String(Rc::from("REPLAY-PROVIDER-CANARY-94a2")))
        }
    }

    struct ErrorFixture(VmError);

    impl EffectProvider for ErrorFixture {
        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }

        fn call(
            &mut self,
            _operation: EffectOperation,
            _arguments: &[Value],
        ) -> Result<Value, VmError> {
            Err(self.0.clone())
        }

        fn start_agent(
            &mut self,
            _pending: PendingEffectId,
            _operation: EffectOperation,
            _arguments: &[Value],
            _result_type: &ValueType,
            _cancellation: &mut dyn CancellationSource,
        ) -> Result<EffectPoll, VmError> {
            Err(self.0.clone())
        }

        fn start_tool(
            &mut self,
            _pending: PendingEffectId,
            _tool: u32,
            _input: &Value,
            _result_type: &ValueType,
            _cancellation: &mut dyn CancellationSource,
        ) -> Result<EffectPoll, VmError> {
            Err(self.0.clone())
        }

        fn start_sub_agent(
            &mut self,
            _pending: PendingEffectId,
            _operation: EffectOperation,
            _arguments: &[Value],
            _result_type: &ValueType,
            _cancellation: &mut dyn CancellationSource,
        ) -> Result<EffectPoll, VmError> {
            Err(self.0.clone())
        }
    }

    #[test]
    fn provider_policy_refuses_schema_agnostic_sensitive_redaction() {
        let mut recording = RecordingEffectProvider::new(
            SecretFixture,
            NoToolSchemas,
            DigestRedactor,
            RedactAll,
            Recorder::new(ReplayLimits::default()),
        );
        let mut cancellation = NeverCancelled;
        assert_eq!(
            recording.start_call(
                PendingEffectId(7),
                EffectOperation::ReadText,
                &[Value::String(Rc::from("safe-request"))],
                &mut cancellation,
            ),
            Err(VmError::ResponseValidationError)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn current_recording_preserves_expected_provider_errors_for_vm_closure() {
        let cases = [
            (EffectOperation::AgentAsk, VmError::AgentUnavailable),
            (EffectOperation::AgentAsk, VmError::CapabilityMissing),
            (EffectOperation::AgentAsk, VmError::AgentResponseSchema),
            (EffectOperation::ModelRequest, VmError::ModelUnavailable),
            (EffectOperation::ModelRequest, VmError::ModelValidationError),
            (EffectOperation::UserAsk, VmError::UserUnavailable),
            (EffectOperation::UserAsk, VmError::ResponseValidationError),
            (EffectOperation::SubAgentRun, VmError::SubAgentUnavailable),
            (
                EffectOperation::SubAgentRun,
                VmError::SubAgentResponseSchema,
            ),
        ];
        for (offset, (operation, error)) in cases.into_iter().enumerate() {
            let result_type = ValueType::Result(
                Box::new(ValueType::String),
                Box::new(
                    if matches!(
                        operation,
                        EffectOperation::SubAgentCreate
                            | EffectOperation::SubAgentRun
                            | EffectOperation::SubAgentMessage
                            | EffectOperation::SubAgentAsk
                    ) {
                        allen_bytecode::sub_agent_error_type()
                    } else {
                        allen_bytecode::standard_error_type()
                    },
                ),
            );
            let mut recording = RecordingEffectProvider::new(
                ErrorFixture(error.clone()),
                NoToolSchemas,
                RefuseSensitive,
                RecordAll,
                Recorder::with_header(ReplayLimits::default(), bound_header()),
            );
            let recorded = if matches!(
                operation,
                EffectOperation::SubAgentCreate
                    | EffectOperation::SubAgentRun
                    | EffectOperation::SubAgentMessage
                    | EffectOperation::SubAgentAsk
            ) {
                recording.start_sub_agent(
                    PendingEffectId(offset as u64),
                    operation,
                    &[Value::String("request".into())],
                    &result_type,
                    &mut NeverCancelled,
                )
            } else {
                recording.start_agent(
                    PendingEffectId(offset as u64),
                    operation,
                    &[Value::String("request".into())],
                    &result_type,
                    &mut NeverCancelled,
                )
            };
            assert_eq!(recorded, Err(error.clone()));
            recording
                .finish_execution(EffectExecutionOutcome::Completed)
                .unwrap();
            let log = recording.finish().unwrap();
            assert_eq!(
                log.entries()[0].outcome,
                EffectOutcome::Err {
                    error: recorded_vm_error(&error).unwrap()
                }
            );
        }

        for error in [
            VmError::ToolUnavailable,
            VmError::CapabilityMissing,
            VmError::ToolSchemaError,
        ] {
            let mut recording = RecordingEffectProvider::new(
                ErrorFixture(error.clone()),
                FixtureSchemas,
                RefuseSensitive,
                RecordAll,
                Recorder::with_header(ReplayLimits::default(), bound_header()),
            );
            assert_eq!(
                recording.start_tool(
                    PendingEffectId(1),
                    3,
                    &Value::Unit,
                    &ValueType::String,
                    &mut NeverCancelled,
                ),
                Err(error.clone())
            );
            recording
                .finish_execution(EffectExecutionOutcome::Completed)
                .unwrap();
            assert!(recording.finish().is_ok());
        }
    }

    #[test]
    fn replay_tool_results_retain_strict_output_and_declared_error_constraints() {
        let result_type = ValueType::String;
        let input = Value::Unit;
        for forged in [
            Value::Enum(Rc::new(allen_vm::EnumValue {
                identity: EnumIdentity::Result,
                type_name: "Result".into(),
                variant: 0,
                variant_name: "Ok".into(),
                payload: EnumPayload::Tuple(vec![Value::String("toolong".into())].into()),
            })),
            Value::Enum(Rc::new(allen_vm::EnumValue {
                identity: EnumIdentity::Result,
                type_name: "Result".into(),
                variant: 1,
                variant_name: "Err".into(),
                payload: EnumPayload::Tuple(
                    vec![Value::Enum(Rc::new(allen_vm::EnumValue {
                        identity: EnumIdentity::User(0),
                        type_name: "ToolError".into(),
                        variant: 0,
                        variant_name: "Declared".into(),
                        payload: EnumPayload::Tuple(vec![Value::String("toolong".into())].into()),
                    }))]
                    .into(),
                ),
            })),
        ] {
            let mut recorder = Recorder::with_header(ReplayLimits::default(), bound_header());
            recorder
                .record(
                    EffectRequest::from_value(
                        EffectKind::Tool(3),
                        &input,
                        &result_type,
                        ReplayLimits::default(),
                    )
                    .unwrap(),
                    outcome(forged),
                    false,
                    &RefuseSensitive,
                )
                .unwrap();
            let log = recorder
                .finish_with_execution_outcome_policy(
                    ReplayExecutionOutcome::Completed,
                    &RecordAll,
                    &RefuseSensitive,
                )
                .unwrap();
            let mut replay = ReplayingEffectProvider::new(
                &log,
                log.header(),
                ReplayLimits::default(),
                StrictFixtureSchemas,
                &[],
            )
            .unwrap();
            replay
                .start_tool(
                    PendingEffectId(1),
                    3,
                    &input,
                    &result_type,
                    &mut NeverCancelled,
                )
                .unwrap();
            assert_eq!(
                replay.poll_effect(PendingEffectId(1), &mut NeverCancelled),
                Err(VmError::ReplayRuntimeDiverged)
            );
        }
    }

    #[test]
    fn current_protocol_violation_replays_as_terminal_protocol_violation() {
        let limits = ReplayLimits::default();
        let header = bound_header();
        let mut recording = RecordingEffectProvider::new(
            ErrorFixture(VmError::ProtocolViolation),
            NoToolSchemas,
            RefuseSensitive,
            RecordAll,
            Recorder::with_header(limits, header.clone()),
        );
        let arguments = [Value::String(Rc::from("current request"))];
        let result_type = ValueType::Result(
            Box::new(ValueType::Unit),
            Box::new(allen_bytecode::standard_error_type()),
        );
        let mut cancellation = NeverCancelled;
        assert_eq!(
            recording.start_agent(
                PendingEffectId(41),
                EffectOperation::AgentMessage,
                &arguments,
                &result_type,
                &mut cancellation,
            ),
            Err(VmError::ProtocolViolation)
        );
        let final_outcome = ReplayExecutionOutcome::terminal(&VmError::ProtocolViolation).unwrap();
        let log = recording
            .finish_with_execution_outcome(final_outcome.clone())
            .unwrap();
        assert!(matches!(
            log.entries()[0].outcome,
            EffectOutcome::Err {
                error: RecordedVmError::ProtocolViolation
            }
        ));

        let mut replay =
            ReplayingEffectProvider::new(&log, &header, limits, NoToolSchemas, &[]).unwrap();
        assert_eq!(
            replay
                .start_agent(
                    PendingEffectId(41),
                    EffectOperation::AgentMessage,
                    &arguments,
                    &result_type,
                    &mut cancellation,
                )
                .unwrap(),
            EffectPoll::Pending
        );
        assert_eq!(
            replay.poll_effect(PendingEffectId(41), &mut cancellation),
            Err(VmError::ProtocolViolation)
        );
        replay
            .finish_with_execution_outcome(&final_outcome)
            .unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn current_raw_provider_errors_are_exactly_operation_scoped() {
        let common = [
            RecordedVmError::Cancelled,
            RecordedVmError::Timeout {
                resource: ReplayResource::WallTime,
            },
            RecordedVmError::ResourceLimit {
                resource: ReplayResource::Effects,
            },
            RecordedVmError::ProtocolViolation,
            RecordedVmError::ReplayRuntimeDiverged,
        ];
        let domain_errors = [
            RecordedVmError::CapabilityMissing,
            RecordedVmError::AgentUnavailable,
            RecordedVmError::AgentResponseSchema,
            RecordedVmError::ModelUnavailable,
            RecordedVmError::ModelValidationError,
            RecordedVmError::UserUnavailable,
            RecordedVmError::SubAgentUnavailable,
            RecordedVmError::SubAgentResponseSchema,
            RecordedVmError::ResponseValidationError,
            RecordedVmError::ToolUnavailable,
            RecordedVmError::ToolSchemaError,
        ];
        let cases: &[(EffectOperation, &[RecordedVmError])] = &[
            (
                EffectOperation::ReadText,
                &[RecordedVmError::CapabilityMissing],
            ),
            (
                EffectOperation::ReadBytes,
                &[RecordedVmError::CapabilityMissing],
            ),
            (
                EffectOperation::WriteText,
                &[RecordedVmError::CapabilityMissing],
            ),
            (
                EffectOperation::WriteBytes,
                &[RecordedVmError::CapabilityMissing],
            ),
            (EffectOperation::List, &[RecordedVmError::CapabilityMissing]),
            (
                EffectOperation::Search,
                &[RecordedVmError::CapabilityMissing],
            ),
            (
                EffectOperation::HttpGet,
                &[RecordedVmError::CapabilityMissing],
            ),
            (
                EffectOperation::PermissionRequestFile,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::AgentUnavailable,
                ],
            ),
            (
                EffectOperation::PermissionRequestDirectory,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::AgentUnavailable,
                ],
            ),
            (
                EffectOperation::AgentMessage,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::AgentUnavailable,
                ],
            ),
            (
                EffectOperation::AgentAsk,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::AgentUnavailable,
                    RecordedVmError::AgentResponseSchema,
                ],
            ),
            (
                EffectOperation::AgentTranscript,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::AgentUnavailable,
                ],
            ),
            (
                EffectOperation::ModelRequest,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::ModelUnavailable,
                    RecordedVmError::ModelValidationError,
                ],
            ),
            (
                EffectOperation::UserAsk,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::UserUnavailable,
                    RecordedVmError::ResponseValidationError,
                ],
            ),
            (
                EffectOperation::SubAgentCreate,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::SubAgentUnavailable,
                ],
            ),
            (
                EffectOperation::SubAgentRun,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::SubAgentUnavailable,
                    RecordedVmError::SubAgentResponseSchema,
                ],
            ),
            (
                EffectOperation::SubAgentMessage,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::SubAgentUnavailable,
                ],
            ),
            (
                EffectOperation::SubAgentAsk,
                &[
                    RecordedVmError::CapabilityMissing,
                    RecordedVmError::SubAgentUnavailable,
                    RecordedVmError::SubAgentResponseSchema,
                ],
            ),
        ];
        for (operation, allowed) in cases {
            for error in &common {
                assert!(
                    validate_current_operation_raw_error(*operation, error),
                    "{operation:?} rejected common terminal {error:?}"
                );
            }
            for error in &domain_errors {
                assert_eq!(
                    validate_current_operation_raw_error(*operation, error),
                    allowed.contains(error),
                    "{operation:?} classified {error:?} in the wrong domain"
                );
            }
        }
        for error in &domain_errors {
            assert_eq!(
                validate_current_tool_raw_error(error),
                matches!(
                    error,
                    RecordedVmError::CapabilityMissing
                        | RecordedVmError::ToolUnavailable
                        | RecordedVmError::ToolSchemaError
                ),
                "tool raw error classification drifted for {error:?}"
            );
        }

        let limits = ReplayLimits::default();
        let request = EffectRequest::from_value(
            EffectKind::Call("permission.request_file".to_owned()),
            &Value::Unit,
            &ValueType::Unit,
            limits,
        )
        .unwrap();
        let mut header = bound_header();
        header.scheduler_completion_order.push(0);
        assert_eq!(
            ReplayLog::new(
                header,
                vec![ReplayEntry {
                    sequence: 0,
                    effect: request.kind.clone(),
                    request_digest: request_digest(&request),
                    schema_digest: request.schema_digest,
                    outcome: EffectOutcome::Err {
                        error: RecordedVmError::ToolUnavailable,
                    },
                }],
                ReplayExecutionOutcome::Completed,
                limits,
            ),
            Err(ReplayError::InvalidJournal),
            "permission.request_file must reject a tool-domain raw error"
        );
    }

    #[test]
    fn current_replay_rechecks_raw_error_against_the_started_operation() {
        let limits = ReplayLimits::default();
        let header = bound_header();
        let result_type = ValueType::Result(
            Box::new(ValueType::String),
            Box::new(allen_bytecode::agent_error_type()),
        );
        let arguments = [Value::String(Rc::from("question"))];
        let request = EffectRequest::from_value(
            EffectKind::Agent("agent.ask".to_owned()),
            &Value::Tuple(Rc::from(arguments.clone())),
            &result_type,
            limits,
        )
        .unwrap();
        let mut recorder = Recorder::with_header(limits, header.clone());
        recorder
            .record(
                request,
                EffectOutcome::Err {
                    error: RecordedVmError::AgentUnavailable,
                },
                false,
                &RefuseSensitive,
            )
            .unwrap();
        let mut log = recorder
            .finish_with_execution_outcome(ReplayExecutionOutcome::Completed)
            .unwrap();
        log.entries[0].outcome = EffectOutcome::Err {
            error: RecordedVmError::ToolUnavailable,
        };

        let mut replay =
            ReplayingEffectProvider::new(&log, &header, limits, NoToolSchemas, &[]).unwrap();
        replay
            .start_agent(
                PendingEffectId(1),
                EffectOperation::AgentAsk,
                &arguments,
                &result_type,
                &mut NeverCancelled,
            )
            .unwrap();
        assert_eq!(
            replay.poll_effect(PendingEffectId(1), &mut NeverCancelled),
            Err(VmError::ReplayRuntimeDiverged)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn current_standard_result_errors_use_the_exact_operation_registry() {
        let result_type = ValueType::Result(
            Box::new(ValueType::Unit),
            Box::new(allen_bytecode::standard_error_type()),
        );
        let cases = [
            (EffectOperation::ReadText, "fs.invalid_utf8"),
            (EffectOperation::ReadBytes, "fs.not_found"),
            (EffectOperation::WriteText, "fs.io"),
            (EffectOperation::WriteBytes, "fs.permission_denied"),
            (EffectOperation::List, "fs.invalid_utf8"),
            (EffectOperation::Search, "fs.invalid_utf8"),
            (EffectOperation::HttpGet, "network.unavailable"),
            (
                EffectOperation::PermissionRequestFile,
                "permission.unavailable",
            ),
            (
                EffectOperation::PermissionRequestDirectory,
                "permission.denied",
            ),
            (EffectOperation::AgentMessage, "agent.unavailable"),
            (EffectOperation::AgentAsk, "agent.validation_failed"),
            (EffectOperation::AgentTranscript, "agent.denied"),
            (EffectOperation::ModelRequest, "model.validation_failed"),
            (EffectOperation::UserAsk, "user.validation_failed"),
            (EffectOperation::SubAgentCreate, "sub_agent.unavailable"),
            (EffectOperation::SubAgentRun, "sub_agent.validation_failed"),
            (EffectOperation::SubAgentMessage, "sub_agent.denied"),
            (EffectOperation::SubAgentAsk, "sub_agent.validation_failed"),
        ];
        let domain_mutations = [
            "fs.not_found",
            "network.unavailable",
            "permission.unavailable",
            "agent.unavailable",
            "model.unavailable",
            "user.unavailable",
            "sub_agent.unavailable",
            "tool.unavailable",
            "resource.limit",
            "unknown.code",
        ];
        for (operation, valid_code) in cases {
            let valid = result_value(false, standard_error(valid_code, "safe"));
            assert_eq!(
                validate_current_standard_completion(operation, &valid, &result_type, &[]),
                Ok(()),
                "{operation:?} rejected its registered code {valid_code}"
            );
            for mutation in domain_mutations {
                let mutated = result_value(false, standard_error(mutation, "safe"));
                assert_eq!(
                    validate_current_standard_completion(operation, &mutated, &result_type, &[])
                        .is_ok(),
                    operation_allows_replayed_error_code(operation, mutation),
                    "{operation:?} misclassified mutated code {mutation}"
                );
            }
        }

        let oversized = result_value(
            false,
            standard_error("agent.unavailable", &"x".repeat(1_025)),
        );
        assert_eq!(
            validate_current_standard_completion(
                EffectOperation::AgentAsk,
                &oversized,
                &result_type,
                &[],
            ),
            Err(ReplayError::InvalidOutcome)
        );
        let ok_type = ValueType::Result(
            Box::new(ValueType::String),
            Box::new(allen_bytecode::standard_error_type()),
        );
        assert_eq!(
            validate_current_standard_completion(
                EffectOperation::AgentAsk,
                &result_value(true, Value::String("answer".into())),
                &ok_type,
                &[],
            ),
            Ok(())
        );
        assert_eq!(
            validate_current_standard_completion(
                EffectOperation::AgentAsk,
                &result_value(true, Value::Bytes(Rc::from(&b"answer"[..]))),
                &ok_type,
                &[],
            ),
            Err(ReplayError::InvalidOutcome)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn current_nominal_enum_replay_validation_checks_the_complete_schema() {
        let enum_types = nominal_enum_types();
        let result_type = ValueType::Result(
            Box::new(ValueType::Enum(1)),
            Box::new(allen_bytecode::agent_error_type()),
        );
        let valid_inner = nominal_enum(
            0,
            2,
            EnumPayload::Record(Rc::from([
                (Rc::from("count"), Value::Int(3)),
                (Rc::from("label"), Value::String(Rc::from("safe"))),
            ])),
        );
        let valid = result_value(
            true,
            nominal_enum(1, 0, EnumPayload::Tuple(Rc::from([valid_inner.clone()]))),
        );
        assert_eq!(
            validate_current_standard_completion(
                EffectOperation::AgentAsk,
                &valid,
                &result_type,
                &enum_types,
            ),
            Ok(())
        );
        assert_eq!(
            validate_current_standard_completion(
                EffectOperation::AgentAsk,
                &valid,
                &result_type,
                &[],
            ),
            Err(ReplayError::InvalidOutcome),
            "a nominal output requires the exact module enum table"
        );

        let malformed = [
            nominal_enum(0, 0, EnumPayload::Tuple(Rc::from([valid_inner.clone()]))),
            nominal_enum(1, 99, EnumPayload::Unit),
            nominal_enum(1, 0, EnumPayload::Unit),
            nominal_enum(1, 0, EnumPayload::Tuple(Rc::from([]))),
            nominal_enum(
                1,
                0,
                EnumPayload::Tuple(Rc::from([nominal_enum(0, 99, EnumPayload::Unit)])),
            ),
            nominal_enum(
                1,
                0,
                EnumPayload::Tuple(Rc::from([nominal_enum(
                    0,
                    2,
                    EnumPayload::Record(Rc::from([
                        (Rc::from("label"), Value::String(Rc::from("safe"))),
                        (Rc::from("count"), Value::Int(3)),
                    ])),
                )])),
            ),
            nominal_enum(
                1,
                0,
                EnumPayload::Tuple(Rc::from([nominal_enum(
                    0,
                    2,
                    EnumPayload::Record(Rc::from([
                        (Rc::from("count"), Value::String(Rc::from("three"))),
                        (Rc::from("label"), Value::String(Rc::from("safe"))),
                    ])),
                )])),
            ),
            nominal_enum(
                1,
                1,
                EnumPayload::Record(Rc::from([(
                    Rc::from("items"),
                    Value::List(Rc::from([nominal_enum(
                        0,
                        1,
                        EnumPayload::Tuple(Rc::from([Value::Int(7)])),
                    )])),
                )])),
            ),
        ];
        for value in malformed {
            assert_eq!(
                validate_current_standard_completion(
                    EffectOperation::AgentAsk,
                    &result_value(true, value),
                    &result_type,
                    &enum_types,
                ),
                Err(ReplayError::InvalidOutcome)
            );
        }

        let mut deep_type = ValueType::String;
        let mut deep_value = Value::String(Rc::from("bounded"));
        for _ in 0..=MAX_VALUE_NESTING {
            deep_type = ValueType::List(Box::new(deep_type));
            deep_value = Value::List(Rc::from([deep_value]));
        }
        assert!(!replay_value_matches_type(
            &deep_value,
            &deep_type,
            &enum_types,
            0,
        ));
    }

    #[test]
    fn current_generated_tool_wrapper_codes_are_checked_before_schema_release() {
        let generated = |variant, code: &str| {
            result_value(
                false,
                Value::Enum(Rc::new(EnumValue {
                    identity: EnumIdentity::User(0),
                    type_name: Rc::from("ToolError"),
                    variant,
                    variant_name: Rc::from(if variant == 1 {
                        "Unavailable"
                    } else {
                        "Schema"
                    }),
                    payload: EnumPayload::Record(Rc::from([
                        (Rc::from("code"), Value::String(Rc::from(code))),
                        (Rc::from("message"), Value::String(Rc::from("safe"))),
                    ])),
                })),
            )
        };
        for (variant, code, expected) in [
            (1, "tool.unavailable", true),
            (1, "tool.denied", true),
            (1, "agent.unavailable", false),
            (1, "tool.schema", false),
            (2, "tool.schema", true),
            (2, "tool.unavailable", false),
            (2, "unknown.code", false),
        ] {
            assert_eq!(
                validate_current_tool_completion(&generated(variant, code)),
                expected,
                "generated tool variant {variant} misclassified {code}"
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn replayed_terminal_code_inside_result_is_runtime_divergence() {
        struct IgnoreCheckpoints;

        impl allen_vm::CheckpointObserver for IgnoreCheckpoints {
            fn checkpoint(&mut self, _checkpoint: allen_vm::Checkpoint) {}
        }

        let limits = ReplayLimits::default();
        let header = bound_header();
        let result_type = effect_result_type(EffectOperation::HttpGet, None).unwrap();
        let url = Value::String("https://example.test/data".into());
        let arguments = Value::Tuple(vec![url.clone()].into());
        let request = EffectRequest::from_value(
            EffectKind::Call("net.http_get".to_owned()),
            &arguments,
            &result_type,
            limits,
        )
        .unwrap();
        let forged = Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::Result,
            type_name: "Result".into(),
            variant: 1,
            variant_name: "Err".into(),
            payload: EnumPayload::Tuple(
                vec![Value::Record(
                    vec![
                        ("code".into(), Value::String("resource.limit".into())),
                        ("message".into(), Value::String("resource exhausted".into())),
                    ]
                    .into(),
                )]
                .into(),
            ),
        }));
        let mut recorder = Recorder::with_header(limits, header.clone());
        recorder
            .record(request, outcome(forged), false, &RefuseSensitive)
            .unwrap();
        let final_outcome =
            ReplayExecutionOutcome::terminal(&VmError::ReplayRuntimeDiverged).unwrap();
        let log = recorder
            .finish_with_execution_outcome(final_outcome.clone())
            .unwrap();
        let mut replay =
            ReplayingEffectProvider::new(&log, &header, limits, NoToolSchemas, &[]).unwrap();

        let verified = allen_bytecode::verify(allen_bytecode::Module {
            constants: vec![allen_bytecode::Constant::String(
                "https://example.test/data".to_owned(),
            )],
            enum_types: vec![],
            effect_sets: vec![vec!["net.http_get".to_owned()]],
            functions: vec![allen_bytecode::Function {
                name: "main.allen::main".to_owned(),
                parameters: vec![],
                parameter_names: Vec::new(),
                parameter_default_digests: Vec::new(),
                captures: vec![],
                registers: vec![
                    ValueType::String,
                    ValueType::Future(Box::new(result_type.clone())),
                    result_type.clone(),
                ],
                return_type: result_type,
                effects: 0,
                code: vec![
                    allen_bytecode::Instruction::Const {
                        destination: 0,
                        constant: 0,
                    },
                    allen_bytecode::Instruction::EffectCall {
                        destination: 1,
                        operation: EffectOperation::HttpGet,
                        arguments: vec![0],
                    },
                    allen_bytecode::Instruction::Await {
                        destination: 2,
                        source: 1,
                    },
                    allen_bytecode::Instruction::Return { source: 2 },
                ],
            }],
            async_functions: vec![0],
            entry: 0,
        })
        .unwrap();
        let failure = allen_vm::execute_entry_with_capabilities_and_runtime_context(
            &verified,
            None,
            0,
            &[],
            allen_vm::ExecutionLimits::default(),
            &mut allen_vm::SystemMonotonicClock::new(),
            &mut IgnoreCheckpoints,
            &mut NeverCancelled,
            &mut replay,
            &allen_vm::ExecutionCapabilities::default(),
        )
        .expect_err("replay cannot smuggle a terminal code into Result::Err");
        assert_eq!(failure.error, VmError::ReplayRuntimeDiverged);
        replay
            .finish_with_execution_outcome(&final_outcome)
            .unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn malformed_current_nominal_replay_is_runtime_divergence_before_vm_validation() {
        struct IgnoreCheckpoints;

        impl allen_vm::CheckpointObserver for IgnoreCheckpoints {
            fn checkpoint(&mut self, _checkpoint: allen_vm::Checkpoint) {}
        }

        let limits = ReplayLimits::default();
        let header = bound_header();
        let enum_types = vec![EnumType {
            name: "pkg://test/root.allen::Answer".to_owned(),
            variants: vec![allen_bytecode::EnumVariant {
                name: "Text".to_owned(),
                payload: EnumPayloadType::Tuple(vec![ValueType::String]),
            }],
        }];
        let answer_type = ValueType::Enum(0);
        let result_type = ValueType::Result(
            Box::new(answer_type.clone()),
            Box::new(allen_bytecode::agent_error_type()),
        );
        let prompt_type = allen_bytecode::prompt_type(answer_type);
        let none = || {
            Value::Enum(Rc::new(EnumValue {
                identity: EnumIdentity::Option,
                type_name: Rc::from("Option"),
                variant: 0,
                variant_name: Rc::from("None"),
                payload: EnumPayload::Unit,
            }))
        };
        let prompt = Value::Record(Rc::from([
            (
                Rc::from("$prompt_0_system"),
                Value::String(Rc::from("answer safely")),
            ),
            (Rc::from("$prompt_1_context"), none()),
            (Rc::from("$prompt_2_data"), none()),
            (Rc::from("$prompt_3_output"), none()),
            (Rc::from("$prompt_4_max_attempts"), Value::Int(1)),
        ]));
        let request = EffectRequest::from_value(
            EffectKind::Agent("agent.ask".to_owned()),
            &Value::Tuple(Rc::from([prompt.clone()])),
            &result_type,
            limits,
        )
        .unwrap();
        let malformed = result_value(true, nominal_enum(0, 0, EnumPayload::Unit));
        let mut recorder = Recorder::with_header(limits, header.clone());
        recorder
            .record(request, outcome(malformed), false, &RefuseSensitive)
            .unwrap();
        let final_outcome =
            ReplayExecutionOutcome::terminal(&VmError::ReplayRuntimeDiverged).unwrap();
        let log = recorder
            .finish_with_execution_outcome(final_outcome.clone())
            .unwrap();

        let verified = allen_bytecode::verify(allen_bytecode::Module {
            constants: vec![],
            enum_types,
            effect_sets: vec![vec!["agent.ask".to_owned()]],
            functions: vec![allen_bytecode::Function {
                name: "main.allen::main".to_owned(),
                parameters: vec![0],
                parameter_names: vec!["_arg0".to_owned()],
                parameter_default_digests: vec![None],
                captures: vec![],
                registers: vec![
                    prompt_type,
                    ValueType::Future(Box::new(result_type.clone())),
                    result_type.clone(),
                ],
                return_type: result_type,
                effects: 0,
                code: vec![
                    allen_bytecode::Instruction::EffectCall {
                        destination: 1,
                        operation: EffectOperation::AgentAsk,
                        arguments: vec![0],
                    },
                    allen_bytecode::Instruction::Await {
                        destination: 2,
                        source: 1,
                    },
                    allen_bytecode::Instruction::Return { source: 2 },
                ],
            }],
            async_functions: vec![0],
            entry: 0,
        })
        .unwrap();
        let mut replay = ReplayingEffectProvider::new(
            &log,
            &header,
            limits,
            NoToolSchemas,
            &verified.module().enum_types,
        )
        .unwrap();
        let failure = allen_vm::execute_entry_with_capabilities_and_runtime_context(
            &verified,
            None,
            0,
            &[prompt],
            allen_vm::ExecutionLimits::default(),
            &mut allen_vm::SystemMonotonicClock::new(),
            &mut IgnoreCheckpoints,
            &mut NeverCancelled,
            &mut replay,
            &allen_vm::ExecutionCapabilities::default(),
        )
        .expect_err("malformed nominal replay must fail before VM result validation");
        assert_eq!(failure.error, VmError::ReplayRuntimeDiverged);
        replay
            .finish_with_execution_outcome(&final_outcome)
            .unwrap();
    }
}
