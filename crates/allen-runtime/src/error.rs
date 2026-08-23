use allen_vm::VmError;

use crate::ResponseAuditRecord;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeErrorCode {
    ArithmeticOverflow,
    DivisionByZero,
    IndexOutOfBounds,
    MapKeyNotFound,
    DuplicateMapKey,
    ResourceLimit,
    Cancelled,
    Timeout,
    Panic,
    ProtocolViolation,
    ReplayDiverged,
    ReplayRuntimeDiverged,
    EntryNotFound,
    CapabilityDenied,
    InputTooLarge,
    InvalidInput,
    ManifestInvalid,
    CatalogMismatch,
}

impl RuntimeErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ArithmeticOverflow => "arithmetic.overflow",
            Self::DivisionByZero => "arithmetic.division_by_zero",
            Self::IndexOutOfBounds => "index.out_of_bounds",
            Self::MapKeyNotFound => "map.key_not_found",
            Self::DuplicateMapKey => "map.duplicate_key",
            Self::ResourceLimit => "resource.limit",
            Self::Cancelled => "runtime.cancelled",
            Self::Timeout => "runtime.timeout",
            Self::Panic => "runtime.panic",
            Self::ProtocolViolation => "protocol.violation",
            Self::ReplayDiverged => "replay.diverged",
            Self::ReplayRuntimeDiverged => "replay.runtime_diverged",
            Self::EntryNotFound => "runtime.entry_not_found",
            Self::CapabilityDenied => "runtime.capability_denied",
            Self::InputTooLarge => "resource.input_bytes",
            Self::InvalidInput => "runtime.invalid_input",
            Self::ManifestInvalid => "runtime.manifest_invalid",
            Self::CatalogMismatch => "tool.catalog_mismatch",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeError {
    pub code: RuntimeErrorCode,
    pub message: String,
    /// Content-free response audit entries collected before this failure.
    pub response_audit: Vec<ResponseAuditRecord>,
}

impl RuntimeError {
    pub(crate) fn new(code: RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            response_audit: Vec::new(),
        }
    }

    pub(crate) fn with_response_audit(mut self, response_audit: Vec<ResponseAuditRecord>) -> Self {
        self.response_audit = response_audit;
        self
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}

pub(crate) fn runtime_vm_error_code(error: &VmError) -> RuntimeErrorCode {
    match error {
        VmError::AgentUnavailable
        | VmError::AgentResponseSchema
        | VmError::ModelUnavailable
        | VmError::ModelValidationError
        | VmError::UserUnavailable
        | VmError::ResponseValidationError
        | VmError::SubAgentUnavailable
        | VmError::SubAgentResponseSchema
        | VmError::ToolUnavailable
        | VmError::ToolSchemaError
        | VmError::CapabilityMissing
        | VmError::ProtocolViolation => RuntimeErrorCode::ProtocolViolation,
        VmError::ReplayDiverged => RuntimeErrorCode::ReplayDiverged,
        VmError::ReplayRuntimeDiverged => RuntimeErrorCode::ReplayRuntimeDiverged,
        VmError::ArithmeticOverflow => RuntimeErrorCode::ArithmeticOverflow,
        VmError::DivisionByZero => RuntimeErrorCode::DivisionByZero,
        VmError::IndexOutOfBounds => RuntimeErrorCode::IndexOutOfBounds,
        VmError::MapKeyNotFound => RuntimeErrorCode::MapKeyNotFound,
        VmError::DuplicateMapKey => RuntimeErrorCode::DuplicateMapKey,
        VmError::ResourceLimit { .. } => RuntimeErrorCode::ResourceLimit,
        VmError::Cancelled => RuntimeErrorCode::Cancelled,
        VmError::Timeout { .. } => RuntimeErrorCode::Timeout,
        VmError::Invariant(_) | VmError::Stopped { .. } => RuntimeErrorCode::Panic,
    }
}

pub(crate) fn post_start_runtime_vm_error_code(error: &VmError) -> RuntimeErrorCode {
    if matches!(error, VmError::ReplayDiverged) {
        RuntimeErrorCode::ReplayRuntimeDiverged
    } else {
        runtime_vm_error_code(error)
    }
}

pub(crate) const fn safe_terminal_message(code: RuntimeErrorCode) -> &'static str {
    match code {
        RuntimeErrorCode::ArithmeticOverflow => "arithmetic overflow",
        RuntimeErrorCode::DivisionByZero => "division by zero",
        RuntimeErrorCode::IndexOutOfBounds => "index out of bounds",
        RuntimeErrorCode::MapKeyNotFound => "map key not found",
        RuntimeErrorCode::DuplicateMapKey => "duplicate map key",
        RuntimeErrorCode::ResourceLimit => "execution resource limit exceeded",
        RuntimeErrorCode::Cancelled => "execution was cancelled",
        RuntimeErrorCode::Timeout => "execution timed out",
        RuntimeErrorCode::Panic => "runtime invariant failed",
        RuntimeErrorCode::ProtocolViolation => "runtime protocol violation",
        RuntimeErrorCode::ReplayRuntimeDiverged => "runtime replay diverged",
        _ => "runtime execution failed",
    }
}
