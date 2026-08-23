use std::time::Duration;

use allen_vm::PendingEffectId;

use crate::{AgentCancellationSignal, ExternalExecutionId};

/// One host-neutral request for an exact response from the attached agent.
///
/// `interaction_id` is stable for this logical interaction. The host adapter
/// maps it to the immutable session that it bound at initialization.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentAskCall {
    pub execution_id: ExternalExecutionId,
    pub operation_id: u64,
    pub interaction_id: u64,
    pub prompt: PromptPayload,
    pub response_schema: ResponseSchema,
    pub attempt: u32,
    pub validation_issues: Vec<ValidationIssue>,
    pub deadline: Duration,
}

/// One canonical strict response schema supplied to a typed response provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseSchema {
    pub digest: String,
    pub descriptor: serde_json::Value,
}

/// One safe, bounded exact-validation issue. Values are never included.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationIssue {
    pub path: String,
    pub code: String,
}

/// Structured prompt segments remain distinct through provider dispatch.
#[derive(Clone, Debug, PartialEq)]
pub struct StructuredPrompt {
    pub system: String,
    pub context: Option<serde_json::Value>,
    pub data: Option<serde_json::Value>,
    pub max_attempts: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PromptPayload {
    Text(String),
    Structured(StructuredPrompt),
}

impl PromptPayload {
    /// Render this prompt for a provider that accepts only one text prompt.
    /// Structured data remains in escaped, length-prefixed segments.
    #[must_use]
    pub fn canonical_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Structured(prompt) => canonical_text_prompt(prompt),
        }
    }
}

/// Render a structured prompt for a text-only provider with canonical,
/// length-prefixed JSON segments. Untrusted values cannot create a delimiter.
#[must_use]
pub fn canonical_text_prompt(prompt: &StructuredPrompt) -> String {
    fn segment(name: &str, value: &serde_json::Value, output: &mut String) {
        let encoded = serde_json::to_string(value).expect("JSON values serialize");
        output.push_str(name);
        output.push(' ');
        output.push_str(&encoded.len().to_string());
        output.push('\n');
        output.push_str(&encoded);
        output.push('\n');
    }

    fn optional_segment(value: Option<&serde_json::Value>) -> serde_json::Value {
        match value {
            Some(value) => serde_json::json!({"tag":"Some", "value":value}),
            None => serde_json::json!({"tag":"None"}),
        }
    }

    let mut output = "ALLEN-PROMPT/1\n".to_owned();
    segment(
        "SYSTEM",
        &serde_json::Value::String(prompt.system.clone()),
        &mut output,
    );
    segment(
        "CONTEXT",
        &optional_segment(prompt.context.as_ref()),
        &mut output,
    );
    segment("DATA", &optional_segment(prompt.data.as_ref()), &mut output);
    segment(
        "POLICY",
        &serde_json::json!({"max_attempts":prompt.max_attempts}),
        &mut output,
    );
    output.push_str("END\n");
    output
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseHostError {
    Unavailable,
    Cancelled,
    Timeout,
    Transport,
    Rejected,
    InvalidOutcome,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResponseProviderPoll {
    Response(serde_json::Value),
    Pending,
}

/// A typed-response call adapted for a provider that only accepts text prompts.
#[derive(Clone, Debug, PartialEq)]
pub struct TextPromptCall {
    pub execution_id: ExternalExecutionId,
    pub operation_id: u64,
    pub interaction_id: u64,
    pub prompt: String,
    pub response_schema: ResponseSchema,
    pub attempt: u32,
    pub validation_issues: Vec<ValidationIssue>,
    pub deadline: Duration,
}

impl From<&AgentAskCall> for TextPromptCall {
    fn from(call: &AgentAskCall) -> Self {
        Self {
            execution_id: call.execution_id,
            operation_id: call.operation_id,
            interaction_id: call.interaction_id,
            prompt: call.prompt.canonical_text(),
            response_schema: call.response_schema.clone(),
            attempt: call.attempt,
            validation_issues: call.validation_issues.clone(),
            deadline: call.deadline,
        }
    }
}

/// Provider contract for transports that can accept only a canonical text prompt.
pub trait TextPromptProvider {
    fn identity(&self) -> &str;

    /// # Errors
    ///
    /// Returns a stable provider error without converting it to a response value.
    fn request(
        &mut self,
        call: &TextPromptCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<serde_json::Value, ResponseHostError>;

    /// # Errors
    ///
    /// Returns a stable provider error if the request cannot be started.
    fn start_request(
        &mut self,
        _pending: PendingEffectId,
        call: &TextPromptCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<ResponseProviderPoll, ResponseHostError> {
        self.request(call, cancellation)
            .map(ResponseProviderPoll::Response)
    }

    /// # Errors
    ///
    /// Returns a stable provider error if the pending request cannot continue.
    fn poll(
        &mut self,
        _pending: PendingEffectId,
        _cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<ResponseProviderPoll, ResponseHostError> {
        Err(ResponseHostError::InvalidOutcome)
    }

    fn cancel(
        &mut self,
        _pending: PendingEffectId,
        _execution_id: ExternalExecutionId,
        _operation_id: u64,
    ) {
    }
}

/// Adapts a text-prompt-only provider to the structured response-provider API.
pub struct TextPromptProviderAdapter<P> {
    provider: P,
}

impl<P> TextPromptProviderAdapter<P> {
    #[must_use]
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    #[must_use]
    pub const fn inner(&self) -> &P {
        &self.provider
    }

    pub fn inner_mut(&mut self) -> &mut P {
        &mut self.provider
    }

    #[must_use]
    pub fn into_inner(self) -> P {
        self.provider
    }
}

impl<P: TextPromptProvider> ResponseProvider for TextPromptProviderAdapter<P> {
    fn identity(&self) -> &str {
        self.provider.identity()
    }

    fn request(
        &mut self,
        call: &AgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<serde_json::Value, ResponseHostError> {
        self.provider.request(&call.into(), cancellation)
    }

    fn start_request(
        &mut self,
        pending: PendingEffectId,
        call: &AgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<ResponseProviderPoll, ResponseHostError> {
        self.provider
            .start_request(pending, &call.into(), cancellation)
    }

    fn poll(
        &mut self,
        pending: PendingEffectId,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<ResponseProviderPoll, ResponseHostError> {
        self.provider.poll(pending, cancellation)
    }

    fn cancel(
        &mut self,
        pending: PendingEffectId,
        execution_id: ExternalExecutionId,
        operation_id: u64,
    ) {
        self.provider.cancel(pending, execution_id, operation_id);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseProviderKind {
    InvokingAgent,
    Model,
    User,
    SubAgent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseAuditOutcome {
    Valid,
    ValidationFailed,
    ProviderFailed,
    Cancelled,
}

/// Content-free audit metadata for one completed typed-response interaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseAuditRecord {
    pub provider_kind: ResponseProviderKind,
    pub provider_identity: String,
    pub schema_digest: String,
    pub attempts: u32,
    pub outcome: ResponseAuditOutcome,
}

/// Independent provider for `model.request` or `user.ask`. Provider identity
/// is safe audit metadata and must not contain prompt or response content.
pub trait ResponseProvider {
    fn identity(&self) -> &str;

    /// # Errors
    ///
    /// Returns a stable provider error without converting it to a response value.
    fn request(
        &mut self,
        call: &AgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<serde_json::Value, ResponseHostError>;

    /// # Errors
    ///
    /// Returns a stable provider error if the request cannot be started.
    fn start_request(
        &mut self,
        _pending: PendingEffectId,
        call: &AgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<ResponseProviderPoll, ResponseHostError> {
        self.request(call, cancellation)
            .map(ResponseProviderPoll::Response)
    }

    /// # Errors
    ///
    /// Returns a stable provider error if the pending request cannot continue.
    fn poll(
        &mut self,
        _pending: PendingEffectId,
        _cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<ResponseProviderPoll, ResponseHostError> {
        Err(ResponseHostError::InvalidOutcome)
    }

    fn cancel(
        &mut self,
        _pending: PendingEffectId,
        _execution_id: ExternalExecutionId,
        _operation_id: u64,
    ) {
    }
}
