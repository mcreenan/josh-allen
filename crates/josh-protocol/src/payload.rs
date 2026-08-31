use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use semver::Version;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::invalid;
use crate::{PeerInfo, ProtocolError, WireMessage};

pub const SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";
pub const SCHEMA_PROFILE: &str = "allen.tool-schema/0.1";
pub const HOST_PROJECTION_PROFILE: &str = "josh.host-projection/0.1";
pub trait Validate {
    /// Validates semantic constraints not represented by the Serde shape.
    ///
    /// # Errors
    ///
    /// Returns an error when a value violates the current protocol contract.
    fn validate(&self) -> Result<(), ProtocolError>;
}

/// Extracts and validates typed params from an already-decoded request.
///
/// # Errors
///
/// Returns an error for the wrong envelope, method, shape, or semantic value.
pub fn request_params<T>(message: &WireMessage, expected_method: &str) -> Result<T, ProtocolError>
where
    T: DeserializeOwned + Validate,
{
    let WireMessage::Request { method, params, .. } = message else {
        return Err(invalid("expected a request"));
    };
    if method != expected_method {
        return Err(invalid(format!("expected method '{expected_method}'")));
    }
    let params =
        serde_json::from_value::<T>(params.clone()).map_err(|error| invalid(error.to_string()))?;
    params.validate()?;
    Ok(params)
}

/// Extracts and validates typed params from an already-decoded notification.
///
/// # Errors
///
/// Returns an error for the wrong envelope, method, shape, or semantic value.
pub fn notification_params<T>(
    message: &WireMessage,
    expected_method: &str,
) -> Result<T, ProtocolError>
where
    T: DeserializeOwned + Validate,
{
    let WireMessage::Notification { method, params } = message else {
        return Err(invalid("expected a notification"));
    };
    if method != expected_method {
        return Err(invalid(format!("expected method '{expected_method}'")));
    }
    let params =
        serde_json::from_value::<T>(params.clone()).map_err(|error| invalid(error.to_string()))?;
    params.validate()?;
    Ok(params)
}

/// Extracts and validates a typed successful response result.
///
/// # Errors
///
/// Returns an error for a failed response or an invalid result shape or value.
pub fn response_result<T>(message: &WireMessage) -> Result<T, ProtocolError>
where
    T: DeserializeOwned + Validate,
{
    let WireMessage::Response {
        result: Some(result),
        error: None,
        ..
    } = message
    else {
        return Err(invalid("expected a successful response"));
    };
    let result =
        serde_json::from_value::<T>(result.clone()).map_err(|error| invalid(error.to_string()))?;
    result.validate()?;
    Ok(result)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Idempotency {
    Unknown,
    Idempotent,
    NonIdempotent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogFreshness {
    Current,
    Cached,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionSectionKind {
    Tools,
    Resources,
    Attachments,
    Transcript,
    Models,
    UserInteraction,
    Agents,
    Roots,
    Permissions,
    Telemetry,
}

impl ProjectionSectionKind {
    pub const ALL: [Self; 10] = [
        Self::Tools,
        Self::Resources,
        Self::Attachments,
        Self::Transcript,
        Self::Models,
        Self::UserInteraction,
        Self::Agents,
        Self::Roots,
        Self::Permissions,
        Self::Telemetry,
    ];
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionBindingLevel {
    None,
    PromptAssisted,
    Authenticated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionSection {
    pub kind: ProjectionSectionKind,
    pub source: String,
    pub source_revision: String,
    pub observed_at_unix_ms: u64,
    pub freshness: CatalogFreshness,
    pub complete: bool,
    pub item_count: u64,
}

impl Validate for ProjectionSection {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_opaque(&self.source, "projection section source")?;
        validate_opaque(&self.source_revision, "projection section source revision")?;
        if self.observed_at_unix_ms == 0 {
            return Err(invalid("projection section observation time is zero"));
        }
        if self.item_count > 1_048_576 {
            return Err(invalid("projection section item count exceeds the limit"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostProjectionSetParams {
    pub profile: String,
    pub projection_id: String,
    pub host: PeerInfo,
    pub session_binding: SessionBindingLevel,
    pub sections: Vec<ProjectionSection>,
}

impl Validate for HostProjectionSetParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.profile != HOST_PROJECTION_PROFILE {
            return Err(invalid("host projection profile is invalid"));
        }
        validate_opaque(&self.projection_id, "projection ID")?;
        self.host.validate()?;
        if self.sections.len() != ProjectionSectionKind::ALL.len() {
            return Err(invalid(
                "host projection must contain every canonical section",
            ));
        }
        for (section, expected) in self.sections.iter().zip(ProjectionSectionKind::ALL) {
            if section.kind != expected {
                return Err(invalid(
                    "host projection sections are not in canonical order",
                ));
            }
            section.validate()?;
            if !section.complete {
                return Err(invalid("host projection section is incomplete"));
            }
        }
        Ok(())
    }
}

impl HostProjectionSetParams {
    #[must_use]
    pub fn section(&self, kind: ProjectionSectionKind) -> &ProjectionSection {
        self.sections
            .iter()
            .find(|section| section.kind == kind)
            .expect("validated host projection contains every canonical section")
    }

    #[must_use]
    pub fn complete_for_catalog(
        projection_id: &str,
        host: PeerInfo,
        session_binding: SessionBindingLevel,
        catalog: &CatalogSetParams,
    ) -> Self {
        let sections = ProjectionSectionKind::ALL
            .into_iter()
            .map(|kind| ProjectionSection {
                kind,
                source: catalog.metadata.source.clone(),
                source_revision: catalog.metadata.source_revision.clone(),
                observed_at_unix_ms: catalog.metadata.observed_at_unix_ms,
                freshness: catalog.metadata.freshness,
                complete: true,
                item_count: if kind == ProjectionSectionKind::Tools {
                    u64::try_from(catalog.tools.len()).unwrap_or(u64::MAX)
                } else {
                    0
                },
            })
            .collect();
        Self {
            profile: HOST_PROJECTION_PROFILE.to_owned(),
            projection_id: projection_id.to_owned(),
            host,
            session_binding,
            sections,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostProjectionSetResult {
    pub projection_digest: String,
    pub projection: HostProjectionSetParams,
}

impl Validate for HostProjectionSetResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_digest(&self.projection_digest)?;
        self.projection.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogMetadata {
    pub source: String,
    pub source_revision: String,
    pub observed_at_unix_ms: u64,
    pub freshness: CatalogFreshness,
    pub complete: bool,
}

impl Validate for CatalogMetadata {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_opaque(&self.source, "catalog source")?;
        validate_opaque(&self.source_revision, "catalog source revision")?;
        if self.observed_at_unix_ms == 0 {
            return Err(invalid("catalog observation time is zero"));
        }
        Ok(())
    }
}

impl CatalogMetadata {
    #[must_use]
    pub fn complete(source: &str, source_revision: &str, observed_at_unix_ms: u64) -> Self {
        Self {
            source: source.to_owned(),
            source_revision: source_revision.to_owned(),
            observed_at_unix_ms,
            freshness: CatalogFreshness::Current,
            complete: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogTool {
    pub name: String,
    pub version: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub error_schema: Value,
    pub effects: Vec<String>,
    pub idempotency: Idempotency,
}

impl Validate for CatalogTool {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_tool_name(&self.name)?;
        validate_semver(&self.version)?;
        validate_description(&self.description)?;
        if self.effects.len() > 32 {
            return Err(invalid("tool has more than 32 effects"));
        }
        validate_sorted_unique(&self.effects, "tool effects")?;
        for effect in &self.effects {
            validate_effect(effect)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSetParams {
    pub schema_dialect: String,
    pub metadata: CatalogMetadata,
    pub tools: Vec<CatalogTool>,
}

impl Validate for CatalogSetParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema_dialect != SCHEMA_DIALECT {
            return Err(invalid("catalog schema dialect is invalid"));
        }
        self.metadata.validate()?;
        let names: Vec<_> = self.tools.iter().map(|tool| tool.name.clone()).collect();
        validate_sorted_unique(&names, "catalog tools")?;
        for tool in &self.tools {
            tool.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogToolSummary {
    pub name: String,
    pub version: String,
    pub description: String,
}

impl Validate for CatalogToolSummary {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_tool_name(&self.name)?;
        validate_semver(&self.version)?;
        validate_description(&self.description)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSetResult {
    pub catalog_digest: String,
    pub schema_profile: String,
    pub tool_count: u64,
    pub metadata: CatalogMetadata,
    pub tools: Vec<CatalogToolSummary>,
}

impl Validate for CatalogSetResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_digest(&self.catalog_digest)?;
        if self.schema_profile != SCHEMA_PROFILE {
            return Err(invalid("catalog result schema profile is invalid"));
        }
        self.metadata.validate()?;
        if !self.metadata.complete {
            return Err(invalid("successful catalog result is incomplete"));
        }
        if usize::try_from(self.tool_count).ok() != Some(self.tools.len()) {
            return Err(invalid("catalog result tool count does not match tools"));
        }
        let names: Vec<_> = self.tools.iter().map(|tool| tool.name.clone()).collect();
        validate_sorted_unique(&names, "catalog result tools")?;
        for tool in &self.tools {
            tool.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "format", deny_unknown_fields)]
pub enum ProgramLoadParams {
    #[serde(rename = "source_bundle")]
    SourceBundle { files: Vec<SourceFile> },
    #[serde(rename = "bytecode")]
    Bytecode { artifact: String },
}

impl Validate for ProgramLoadParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::SourceBundle { files } => validate_source_files(files),
            Self::Bytecode { artifact } => validate_base64(artifact),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileEncoding {
    Utf8,
    Base64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: String,
    pub encoding: FileEncoding,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryContract {
    pub name: String,
    pub input_schema: String,
    pub output_schema: String,
    pub input_contract_digest: String,
    pub output_contract_digest: String,
}

impl Validate for EntryContract {
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.name.is_empty() {
            return Err(invalid("entry name is empty"));
        }
        validate_digest(&self.input_schema)?;
        validate_digest(&self.output_schema)?;
        validate_digest(&self.input_contract_digest)?;
        validate_digest(&self.output_contract_digest)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticSpan {
    pub uri: String,
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticLabelSpan {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticLabel {
    pub span: DiagnosticLabelSpan,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerDiagnostic {
    pub code: String,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub primary: DiagnosticSpan,
    pub labels: Vec<DiagnosticLabel>,
    pub notes: Vec<String>,
    pub help: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramLoadResult {
    pub program_id: String,
    pub artifact_digest: String,
    pub tool_contract_digest: String,
    pub diagnostics: Vec<CompilerDiagnostic>,
    pub entries: Vec<EntryContract>,
    pub required_tools: Vec<String>,
    pub exec_commands: Vec<String>,
    pub exec_environment: Vec<String>,
}

impl Validate for ProgramLoadResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.program_id.is_empty() || self.program_id.len() > 64 {
            return Err(invalid("program_id is invalid"));
        }
        validate_digest(&self.artifact_digest)?;
        validate_digest(&self.tool_contract_digest)?;
        if !self.diagnostics.is_empty() {
            return Err(invalid("successful load diagnostics must be empty"));
        }
        let names: Vec<_> = self
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        validate_sorted_unique(&names, "entries")?;
        for entry in &self.entries {
            entry.validate()?;
        }
        validate_sorted_unique(&self.required_tools, "required_tools")?;
        for tool in &self.required_tools {
            validate_tool_name(tool)?;
        }
        validate_sorted_unique(&self.exec_commands, "exec_commands")?;
        for pattern in &self.exec_commands {
            validate_exec_pattern(pattern)?;
        }
        validate_sorted_unique(&self.exec_environment, "exec_environment")?;
        for name in &self.exec_environment {
            validate_exec_environment_name(name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStartParams {
    pub execution_id: String,
    pub program_id: String,
    pub artifact_digest: String,
    pub entry: String,
    pub input: Value,
    pub working_directory: Option<String>,
    pub granted_capabilities: Vec<String>,
    pub granted_tools: Vec<String>,
    pub allowed_http_origins: Vec<String>,
    pub granted_exec: Vec<String>,
    pub granted_exec_environment: Vec<String>,
    pub limits: BTreeMap<String, u64>,
}

impl Validate for ExecutionStartParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_opaque(&self.execution_id, "execution_id")?;
        validate_opaque(&self.program_id, "program_id")?;
        validate_digest(&self.artifact_digest)?;
        if self.entry.is_empty() {
            return Err(invalid("execution entry is empty"));
        }
        if self
            .working_directory
            .as_ref()
            .is_some_and(String::is_empty)
        {
            return Err(invalid("working_directory cannot be empty"));
        }
        validate_sorted_unique(&self.granted_capabilities, "granted_capabilities")?;
        for capability in &self.granted_capabilities {
            if !matches!(
                capability.as_str(),
                "fs.read" | "fs.write" | "net.http_get" | "permission.request_external_fs"
            ) {
                return Err(invalid("granted capability is not implemented"));
            }
        }
        validate_sorted_unique(&self.granted_tools, "granted_tools")?;
        for tool in &self.granted_tools {
            validate_tool_name(tool)?;
        }
        validate_sorted_unique(&self.allowed_http_origins, "allowed_http_origins")?;
        for origin in &self.allowed_http_origins {
            validate_https_origin(origin)?;
        }
        validate_sorted_unique(&self.granted_exec, "granted_exec")?;
        for pattern in &self.granted_exec {
            validate_exec_pattern(pattern)?;
        }
        validate_sorted_unique(&self.granted_exec_environment, "granted_exec_environment")?;
        for name in &self.granted_exec_environment {
            validate_exec_environment_name(name)?;
        }
        for (name, value) in &self.limits {
            if !EXECUTION_LIMITS.contains(&name.as_str()) || *value == 0 {
                return Err(invalid("execution limit is unknown or zero"));
            }
        }
        Ok(())
    }
}

fn validate_exec_pattern(pattern: &str) -> Result<(), ProtocolError> {
    if pattern.is_empty()
        || pattern.starts_with(' ')
        || pattern.ends_with(' ')
        || pattern.contains("  ")
    {
        return Err(invalid("exec command pattern is not canonical"));
    }
    let tokens = pattern.split(' ').collect::<Vec<_>>();
    let Some(executable) = tokens.first() else {
        return Err(invalid("exec command pattern is not canonical"));
    };
    if executable.contains('/')
        || *executable == "*"
        || tokens.iter().enumerate().any(|(index, token)| {
            let final_wildcard = *token == "*" && index + 1 == tokens.len();
            token.is_empty()
                || token.contains('*') && !final_wildcard
                || !final_wildcard
                    && token.bytes().any(|byte| {
                        !byte.is_ascii_graphic() || matches!(byte, b'\'' | b'"' | b'\\')
                    })
        })
    {
        return Err(invalid("exec command pattern is not canonical"));
    }
    Ok(())
}

fn validate_exec_environment_name(name: &str) -> Result<(), ProtocolError> {
    let mut bytes = name.bytes();
    if !bytes.next().is_some_and(|first| {
        (first.is_ascii_alphabetic() || first == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && !name.eq_ignore_ascii_case("LC_ALL")
            && !name.eq_ignore_ascii_case("TZ")
    }) {
        return Err(invalid("exec environment name is not canonical"));
    }
    Ok(())
}

const EXECUTION_LIMITS: &[&str] = &[
    "call_depth",
    "cleanup_instructions",
    "concurrent_effects",
    "effects",
    "fs_entries",
    "fs_file_bytes",
    "fs_operations",
    "fs_read_bytes",
    "fs_write_bytes",
    "heap_bytes",
    "http_compressed_bytes",
    "http_connect_ms",
    "http_decoded_bytes",
    "http_decompression_ratio",
    "http_dns_addresses",
    "http_first_byte_ms",
    "http_idle_ms",
    "http_redirects",
    "http_requests",
    "http_response_header_bytes",
    "http_response_headers",
    "http_total_ms",
    "input_bytes",
    "instructions",
    "maximum_allocation_bytes",
    "output_bytes",
    "tasks",
    "wall_ms",
];

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase", deny_unknown_fields)]
pub enum ExecutionResult {
    Completed { output: Value },
    Stopped { reason: Option<String> },
    Failed { error: ProtocolRuntimeError },
    Cancelled { reason: Option<String> },
}

impl Validate for ExecutionResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Stopped { reason } | Self::Cancelled { reason } => {
                if let Some(reason) = reason {
                    crate::validate_reason(reason)?;
                }
            }
            Self::Failed { error } => error.validate()?,
            Self::Completed { .. } => {}
        }
        Ok(())
    }
}

pub type ExecutionStartResult = ExecutionResult;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolRuntimeError {
    pub code: String,
    pub message: String,
    pub category: String,
    pub retryable: bool,
    pub span: Option<Value>,
    pub operation_id: Option<String>,
    pub metadata: BTreeMap<String, Value>,
    pub causes: Vec<Value>,
}

impl Validate for ProtocolRuntimeError {
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.code.is_empty() || self.message.len() > 1_024 {
            return Err(invalid("runtime error has invalid safe text"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInvokeParams {
    pub execution_id: String,
    pub operation_id: String,
    pub tool: String,
    pub tool_version: String,
    pub catalog_digest: String,
    pub input_schema: String,
    pub output_schema: String,
    pub error_schema: String,
    pub input: Value,
    pub deadline_ms: u64,
}

impl Validate for ToolInvokeParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_opaque(&self.execution_id, "execution_id")?;
        validate_opaque(&self.operation_id, "operation_id")?;
        validate_tool_name(&self.tool)?;
        validate_semver(&self.tool_version)?;
        validate_digest(&self.catalog_digest)?;
        validate_digest(&self.input_schema)?;
        validate_digest(&self.output_schema)?;
        validate_digest(&self.error_schema)?;
        if self.deadline_ms == 0 {
            return Err(invalid("tool deadline must be positive"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase", deny_unknown_fields)]
pub enum ToolInvokeResult {
    Ok { value: Value },
    Error { error: Value },
}

impl Validate for ToolInvokeResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessageParams {
    pub execution_id: String,
    pub operation_id: String,
    pub session_id: String,
    pub message: String,
    pub deadline_ms: u64,
}

impl Validate for AgentMessageParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_bound_operation(
            &self.execution_id,
            &self.operation_id,
            &self.session_id,
            self.deadline_ms,
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessageResult {
    pub accepted: bool,
}

impl Validate for AgentMessageResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        if !self.accepted {
            return Err(invalid("agent message result must acknowledge acceptance"));
        }
        Ok(())
    }
}

/// The structured prompt wire representation.
///
/// Nullable context and data fields are always present, preserving the
/// distinction between an absent segment and an omitted wire field.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredPromptPayload {
    pub system: String,
    pub context: PromptSegmentPayload,
    pub data: PromptSegmentPayload,
    pub policy: PromptPolicy,
}

impl Validate for StructuredPromptPayload {
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.system.is_empty() || self.system.len() > 65_536 {
            return Err(invalid("prompt system must be 1 through 65536 UTF-8 bytes"));
        }
        self.policy.validate()
    }
}

/// Presence-preserving prompt segment encoding aligned with `Option<T>`.
/// A present JSON null remains distinct from an absent segment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "tag", deny_unknown_fields)]
pub enum PromptSegmentPayload {
    None,
    Some { value: Value },
}

impl<'de> Deserialize<'de> for PromptSegmentPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        enum NoneTag {
            #[serde(rename = "None")]
            None,
        }
        #[derive(Deserialize)]
        enum SomeTag {
            #[serde(rename = "Some")]
            Some,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NoneShape {
            tag: NoneTag,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SomeShape {
            tag: SomeTag,
            value: Value,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            None(NoneShape),
            Some(SomeShape),
        }
        match Shape::deserialize(deserializer)? {
            Shape::None(NoneShape { tag: NoneTag::None }) => Ok(Self::None),
            Shape::Some(SomeShape {
                tag: SomeTag::Some,
                value,
            }) => Ok(Self::Some { value }),
        }
    }
}

impl PromptSegmentPayload {
    #[must_use]
    pub fn from_option(value: Option<Value>) -> Self {
        value.map_or(Self::None, |value| Self::Some { value })
    }

    #[must_use]
    pub fn as_option(&self) -> Option<&Value> {
        match self {
            Self::None => None,
            Self::Some { value } => Some(value),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromptPolicy {
    pub max_attempts: u32,
}

impl Validate for PromptPolicy {
    fn validate(&self) -> Result<(), ProtocolError> {
        if !(1..=3).contains(&self.max_attempts) {
            return Err(invalid("prompt max_attempts must be 1 through 3"));
        }
        Ok(())
    }
}

/// One exact response schema and its stable bytecode-profile digest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseSchemaPayload {
    pub digest: String,
    pub descriptor: Value,
}

impl Validate for ResponseSchemaPayload {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_digest(&self.digest)?;
        if !self.descriptor.is_object() {
            return Err(invalid("response schema descriptor must be an object"));
        }
        Ok(())
    }
}

/// One safe response-validation issue. It intentionally carries no value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationIssuePayload {
    pub path: String,
    pub code: String,
}

impl Validate for ValidationIssuePayload {
    fn validate(&self) -> Result<(), ProtocolError> {
        if self.path.len() > 1_024 || !self.path.starts_with('/') && !self.path.is_empty() {
            return Err(invalid("validation issue path is invalid"));
        }
        if self.code.is_empty() || self.code.len() > 128 || self.code.chars().any(char::is_control)
        {
            return Err(invalid("validation issue code is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAskParams {
    pub execution_id: String,
    pub operation_id: String,
    pub session_id: String,
    pub interaction_id: String,
    pub prompt: StructuredPromptPayload,
    pub response_schema: ResponseSchemaPayload,
    pub attempt: u32,
    pub validation_issues: Vec<ValidationIssuePayload>,
    pub deadline_ms: u64,
}

impl Validate for AgentAskParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_bound_operation(
            &self.execution_id,
            &self.operation_id,
            &self.session_id,
            self.deadline_ms,
        )?;
        validate_typed_response_request(
            &self.interaction_id,
            &self.prompt,
            &self.response_schema,
            self.attempt,
            &self.validation_issues,
        )
    }
}

impl AgentAskParams {
    #[must_use]
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestParams {
    pub execution_id: String,
    pub operation_id: String,
    pub interaction_id: String,
    pub prompt: StructuredPromptPayload,
    pub response_schema: ResponseSchemaPayload,
    pub attempt: u32,
    pub validation_issues: Vec<ValidationIssuePayload>,
    pub deadline_ms: u64,
}

impl Validate for ModelRequestParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_unbound_operation(&self.execution_id, &self.operation_id, self.deadline_ms)?;
        validate_typed_response_request(
            &self.interaction_id,
            &self.prompt,
            &self.response_schema,
            self.attempt,
            &self.validation_issues,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserAskParams {
    pub execution_id: String,
    pub operation_id: String,
    pub interaction_id: String,
    pub prompt: StructuredPromptPayload,
    pub response_schema: ResponseSchemaPayload,
    pub attempt: u32,
    pub validation_issues: Vec<ValidationIssuePayload>,
    pub deadline_ms: u64,
}

impl Validate for UserAskParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_unbound_operation(&self.execution_id, &self.operation_id, self.deadline_ms)?;
        validate_typed_response_request(
            &self.interaction_id,
            &self.prompt,
            &self.response_schema,
            self.attempt,
            &self.validation_issues,
        )
    }
}

/// The complete child authority projection. Prompt context is carried in the
/// adjacent structured prompt and no ambient context is implied.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentProjectionPayload {
    pub capabilities: Vec<String>,
    pub limits: BTreeMap<String, u64>,
    pub tools: Vec<String>,
}

impl Validate for SubAgentProjectionPayload {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_sorted_unique(&self.capabilities, "sub-agent capabilities")?;
        for capability in &self.capabilities {
            if !matches!(
                capability.as_str(),
                "fs.read" | "fs.write" | "net.http_get" | "permission.request_external_fs"
            ) {
                return Err(invalid("sub-agent capability is not implemented"));
            }
        }
        for (name, value) in &self.limits {
            if !EXECUTION_LIMITS.contains(&name.as_str()) || *value == 0 {
                return Err(invalid("sub-agent limit is unknown or zero"));
            }
        }
        validate_sorted_unique(&self.tools, "sub-agent tools")?;
        for tool in &self.tools {
            validate_tool_name(tool)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentCreateParams {
    pub execution_id: String,
    pub operation_id: String,
    pub prompt: StructuredPromptPayload,
    pub projection: SubAgentProjectionPayload,
    pub deadline_ms: u64,
}

impl Validate for SubAgentCreateParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_unbound_operation(&self.execution_id, &self.operation_id, self.deadline_ms)?;
        self.prompt.validate()?;
        self.projection.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentRunParams {
    pub execution_id: String,
    pub operation_id: String,
    pub interaction_id: String,
    pub prompt: StructuredPromptPayload,
    pub projection: SubAgentProjectionPayload,
    pub response_schema: ResponseSchemaPayload,
    pub attempt: u32,
    pub validation_issues: Vec<ValidationIssuePayload>,
    pub deadline_ms: u64,
}

impl Validate for SubAgentRunParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_unbound_operation(&self.execution_id, &self.operation_id, self.deadline_ms)?;
        self.projection.validate()?;
        validate_typed_response_request(
            &self.interaction_id,
            &self.prompt,
            &self.response_schema,
            self.attempt,
            &self.validation_issues,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentMessageParams {
    pub execution_id: String,
    pub operation_id: String,
    pub sub_agent_id: String,
    pub message: String,
    pub deadline_ms: u64,
}

impl Validate for SubAgentMessageParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_unbound_operation(&self.execution_id, &self.operation_id, self.deadline_ms)?;
        validate_ascii_id(&self.sub_agent_id, "sub_agent_id", 128)?;
        if self.message.len() > 1_048_576 {
            return Err(invalid("sub-agent message exceeds its byte limit"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentAskParams {
    pub execution_id: String,
    pub operation_id: String,
    pub sub_agent_id: String,
    pub interaction_id: String,
    pub prompt: StructuredPromptPayload,
    pub response_schema: ResponseSchemaPayload,
    pub attempt: u32,
    pub validation_issues: Vec<ValidationIssuePayload>,
    pub deadline_ms: u64,
}

impl Validate for SubAgentAskParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_unbound_operation(&self.execution_id, &self.operation_id, self.deadline_ms)?;
        validate_ascii_id(&self.sub_agent_id, "sub_agent_id", 128)?;
        validate_typed_response_request(
            &self.interaction_id,
            &self.prompt,
            &self.response_schema,
            self.attempt,
            &self.validation_issues,
        )
    }
}

fn validate_typed_response_request(
    interaction_id: &str,
    prompt: &StructuredPromptPayload,
    response_schema: &ResponseSchemaPayload,
    attempt: u32,
    validation_issues: &[ValidationIssuePayload],
) -> Result<(), ProtocolError> {
    validate_opaque(interaction_id, "interaction_id")?;
    prompt.validate()?;
    response_schema.validate()?;
    if attempt == 0 || attempt > prompt.policy.max_attempts {
        return Err(invalid("response attempt exceeds prompt policy"));
    }
    if validation_issues.len() > 16 || attempt == 1 && !validation_issues.is_empty() {
        return Err(invalid("response validation issues do not match attempt"));
    }
    for issue in validation_issues {
        issue.validate()?;
    }
    Ok(())
}

fn validate_unbound_operation(
    execution_id: &str,
    operation_id: &str,
    deadline_ms: u64,
) -> Result<(), ProtocolError> {
    validate_opaque(execution_id, "execution_id")?;
    validate_opaque(operation_id, "operation_id")?;
    if deadline_ms == 0 {
        return Err(invalid("operation deadline must be positive"));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TypedResponseResult {
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentCreateResult {
    pub sub_agent_id: String,
}

impl Validate for SubAgentCreateResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_ascii_id(&self.sub_agent_id, "sub_agent_id", 128)
    }
}

impl Validate for TypedResponseResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTranscriptParams {
    pub execution_id: String,
    pub operation_id: String,
    pub session_id: String,
    pub limit: u32,
    pub deadline_ms: u64,
}

impl Validate for AgentTranscriptParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_bound_operation(
            &self.execution_id,
            &self.operation_id,
            &self.session_id,
            self.deadline_ms,
        )?;
        if !(1..=100).contains(&self.limit) {
            return Err(invalid("transcript limit must be 1 through 100"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTranscriptResult {
    pub snapshot: TranscriptSnapshot,
}

impl Validate for AgentTranscriptResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        self.snapshot.validate()
    }
}

impl AgentTranscriptResult {
    /// Validates the snapshot against the request-scoped binding and message limit.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed snapshot, wrong session, or excess messages.
    pub fn validate_for(&self, session_id: &str, limit: u32) -> Result<(), ProtocolError> {
        self.validate()?;
        if self.snapshot.session_id != session_id || self.snapshot.messages.len() > limit as usize {
            return Err(invalid(
                "transcript snapshot does not match its bound request",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptSnapshot {
    pub snapshot_id: String,
    pub session_id: String,
    pub policy_version: String,
    pub captured_at: String,
    pub truncated: bool,
    pub messages: Vec<TranscriptMessage>,
}

impl Validate for TranscriptSnapshot {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_transcript_text(&self.snapshot_id, "snapshot_id", false)?;
        validate_session_id(&self.session_id)?;
        validate_transcript_text(&self.policy_version, "policy_version", false)?;
        validate_timestamp(&self.captured_at)?;
        if self.messages.len() > 100 {
            return Err(invalid("transcript has more than 100 messages"));
        }
        let mut previous_time = None;
        for message in &self.messages {
            message.validate()?;
            if let Some(time) = &message.time {
                let key = timestamp_key(time)?;
                if previous_time
                    .as_ref()
                    .is_some_and(|previous| previous > &key)
                {
                    return Err(invalid("transcript messages are not oldest first"));
                }
                previous_time = Some(key);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptMessage {
    #[serde(deserialize_with = "required_nullable")]
    pub id: Option<String>,
    pub role: TranscriptRole,
    #[serde(deserialize_with = "required_nullable")]
    pub time: Option<String>,
    pub content: Vec<TranscriptPart>,
}

impl Validate for TranscriptMessage {
    fn validate(&self) -> Result<(), ProtocolError> {
        if let Some(id) = &self.id {
            validate_transcript_text(id, "transcript message id", false)?;
        }
        if let Some(time) = &self.time {
            validate_timestamp(time)?;
        }
        for part in &self.content {
            part.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRole {
    User,
    Assistant,
    SystemVisible,
    Tool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TranscriptPart {
    Text {
        text: String,
    },
    Json {
        value: Value,
    },
    ToolCall {
        name: String,
        call_id: String,
        #[serde(deserialize_with = "required_nullable")]
        input: Option<Value>,
    },
    ToolResult {
        call_id: String,
        #[serde(deserialize_with = "required_nullable")]
        output: Option<Value>,
        is_error: bool,
    },
    Attachment {
        media_type: String,
        #[serde(deserialize_with = "required_nullable")]
        name: Option<String>,
        #[serde(deserialize_with = "required_nullable")]
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

impl Validate for TranscriptPart {
    fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Text { .. } | Self::Json { .. } => {}
            Self::ToolCall { name, call_id, .. } => {
                validate_transcript_text(name, "tool name", false)?;
                validate_transcript_text(call_id, "tool call ID", false)?;
            }
            Self::ToolResult { call_id, .. } => {
                validate_transcript_text(call_id, "tool call ID", false)?;
            }
            Self::Attachment {
                media_type,
                name,
                content_ref,
            } => {
                validate_transcript_text(media_type, "attachment media type", false)?;
                if let Some(name) = name {
                    validate_transcript_text(name, "attachment name", true)?;
                }
                if let Some(content_ref) = content_ref {
                    validate_transcript_text(content_ref, "attachment content reference", true)?;
                }
            }
            Self::Redacted { reason_code } => {
                validate_safe_code(reason_code, "redaction reason code")?;
            }
            Self::Omitted {
                content_kind,
                count,
            } => {
                validate_safe_code(content_kind, "omitted content kind")?;
                if *count == 0 {
                    return Err(invalid("omitted content count must be positive"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequestParams {
    pub execution_id: String,
    pub operation_id: String,
    pub session_id: String,
    pub pending_target_id: String,
    pub kind: PermissionTargetKind,
    pub path: String,
    pub rights: Vec<PermissionRight>,
    pub recursive: bool,
    pub max_bytes: u64,
    pub duration: GrantDuration,
    pub reason: String,
}

impl Validate for PermissionRequestParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_opaque(&self.execution_id, "execution_id")?;
        validate_opaque(&self.operation_id, "operation_id")?;
        validate_session_id(&self.session_id)?;
        validate_opaque(&self.pending_target_id, "pending_target_id")?;
        validate_permission_scope(
            &self.path,
            &self.rights,
            self.recursive,
            self.max_bytes,
            self.kind,
        )?;
        if self.reason.len() > 1_024 || self.reason.chars().any(char::is_control) {
            return Err(invalid("permission reason is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionRight {
    Read,
    Write,
    List,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionTargetKind {
    File,
    Directory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantDuration {
    Execution,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "lowercase", deny_unknown_fields)]
pub enum PermissionRequestResult {
    Allow {
        grant_id: String,
        path: String,
        rights: Vec<PermissionRight>,
        recursive: bool,
        max_bytes: u64,
        duration: GrantDuration,
    },
    Deny {
        reason_code: String,
    },
}

impl Validate for PermissionRequestResult {
    fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Allow {
                grant_id,
                path,
                rights,
                recursive: _,
                max_bytes,
                ..
            } => {
                validate_ascii_id(grant_id, "grant_id", 128)?;
                validate_permission_values(path, rights, *max_bytes)?;
            }
            Self::Deny { reason_code } => {
                validate_safe_code(reason_code, "permission denial reason code")?;
            }
        }
        Ok(())
    }
}

impl PermissionRequestResult {
    /// Validates an allow decision against the retained request.
    ///
    /// # Errors
    ///
    /// Returns an error when a decision changes kind or broadens path, rights,
    /// recursion, byte limit, or duration.
    pub fn validate_for(&self, request: &PermissionRequestParams) -> Result<(), ProtocolError> {
        self.validate()?;
        let Self::Allow {
            path,
            rights,
            recursive,
            max_bytes,
            duration,
            ..
        } = self
        else {
            return Ok(());
        };
        let path_is_allowed = match request.kind {
            PermissionTargetKind::File => path == &request.path,
            PermissionTargetKind::Directory => Path::new(path).starts_with(&request.path),
        };
        if !path_is_allowed
            || rights.iter().any(|right| !request.rights.contains(right))
            || (*recursive && !request.recursive)
            || *max_bytes > request.max_bytes
            || *duration != request.duration
            || (request.kind == PermissionTargetKind::File
                && (*recursive || rights.contains(&PermissionRight::List)))
        {
            return Err(invalid("permission decision broadens the retained request"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRevokeParams {
    pub execution_id: String,
    pub session_id: String,
    pub grant_id: String,
}

impl Validate for PermissionRevokeParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_opaque(&self.execution_id, "execution_id")?;
        validate_session_id(&self.session_id)?;
        validate_ascii_id(&self.grant_id, "grant_id", 128)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEventParams {
    pub execution_id: String,
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub kind: EventKind,
    pub replayed: bool,
    pub fields: BTreeMap<String, Value>,
}

impl Validate for ExecutionEventParams {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_opaque(&self.execution_id, "execution_id")?;
        if self.sequence == 0 {
            return Err(invalid("event sequence must be positive"));
        }
        let expected = self.kind.field_names();
        if self.fields.len() != expected.len()
            || !self
                .fields
                .keys()
                .map(String::as_str)
                .eq(expected.iter().copied())
        {
            return Err(invalid("event fields do not match the event kind"));
        }
        for field in expected {
            let value = &self.fields[*field];
            let valid = match *field {
                "task_id" | "owner_task_id" | "used" | "limit" => value.as_u64().is_some(),
                _ => value.as_str().is_some(),
            };
            if !valid {
                return Err(invalid("event field has the wrong type"));
            }
            if matches!(
                *field,
                "artifact_digest" | "catalog_digest" | "schema_digest"
            ) {
                validate_digest(value.as_str().expect("field type was checked"))?;
            }
            if *field == "decision" && !matches!(value.as_str(), Some("allow" | "deny")) {
                return Err(invalid("permission decision is invalid"));
            }
            if *field == "reason_code" {
                validate_safe_code(
                    value.as_str().expect("field type was checked"),
                    "permission decision reason code",
                )?;
            }
        }
        Ok(())
    }
}

/// Stateful ordering validator for one execution event stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventSequenceTracker {
    execution_id: String,
    next_sequence: u64,
    last_elapsed_ms: u64,
    terminal: bool,
}

impl EventSequenceTracker {
    #[must_use]
    pub fn new(execution_id: String) -> Self {
        Self {
            execution_id,
            next_sequence: 1,
            last_elapsed_ms: 0,
            terminal: false,
        }
    }

    /// Validates and records the next event in the stream.
    ///
    /// # Errors
    ///
    /// Returns an error for the wrong execution, a sequence gap, decreasing elapsed
    /// time, or an event after the terminal event.
    pub fn record(&mut self, event: &ExecutionEventParams) -> Result<(), ProtocolError> {
        event.validate()?;
        if self.terminal
            || event.execution_id != self.execution_id
            || event.sequence != self.next_sequence
            || event.elapsed_ms < self.last_elapsed_ms
        {
            return Err(invalid("event stream ordering is invalid"));
        }
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| invalid("event sequence overflowed"))?;
        self.last_elapsed_ms = event.elapsed_ms;
        self.terminal = event.kind.is_terminal();
        Ok(())
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Accepted,
    Started,
    EffectStarted,
    EffectCompleted,
    EffectFailed,
    TaskStarted,
    TaskCancelled,
    BudgetWarning,
    PermissionDecision,
    Stopped,
    Completed,
    Failed,
    Cancelled,
}

impl EventKind {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Stopped | Self::Completed | Self::Failed | Self::Cancelled
        )
    }

    const fn field_names(self) -> &'static [&'static str] {
        match self {
            Self::Accepted => &["artifact_digest", "catalog_digest", "entry", "program_id"],
            Self::EffectStarted | Self::EffectCompleted | Self::EffectFailed => {
                &["effect", "operation_id", "schema_digest"]
            }
            Self::TaskStarted | Self::TaskCancelled => &["owner_task_id", "task_id"],
            Self::BudgetWarning => &["limit", "resource", "used"],
            Self::PermissionDecision => &["decision", "operation_id", "reason_code"],
            Self::Started | Self::Stopped | Self::Completed | Self::Failed | Self::Cancelled => &[],
        }
    }
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn validate_bound_operation(
    execution_id: &str,
    operation_id: &str,
    session_id: &str,
    deadline_ms: u64,
) -> Result<(), ProtocolError> {
    validate_opaque(execution_id, "execution_id")?;
    validate_opaque(operation_id, "operation_id")?;
    validate_session_id(session_id)?;
    if deadline_ms == 0 {
        return Err(invalid("operation deadline must be positive"));
    }
    Ok(())
}

pub(crate) fn validate_session_id(value: &str) -> Result<(), ProtocolError> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(invalid("invoking session ID is invalid"));
    }
    Ok(())
}

fn validate_ascii_id(value: &str, name: &str, maximum: usize) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > maximum
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(invalid(format!("{name} is invalid")));
    }
    Ok(())
}

fn validate_transcript_text(
    value: &str,
    name: &str,
    allow_empty: bool,
) -> Result<(), ProtocolError> {
    if (!allow_empty && value.is_empty())
        || value.len() > 1_024
        || value.chars().any(char::is_control)
    {
        return Err(invalid(format!("{name} is invalid")));
    }
    Ok(())
}

fn validate_safe_code(value: &str, name: &str) -> Result<(), ProtocolError> {
    validate_transcript_text(value, name, false)
}

fn validate_permission_values(
    path: &str,
    rights: &[PermissionRight],
    max_bytes: u64,
) -> Result<(), ProtocolError> {
    if path.is_empty()
        || path.len() > 4_096
        || path.chars().any(char::is_control)
        || !Path::new(path).is_absolute()
        || path.contains("//")
        || (path.len() > 1 && path.ends_with('/'))
        || Path::new(path).components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(invalid(
            "permission path is not an absolute canonical target",
        ));
    }
    if rights.is_empty() || rights.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid("permission rights must be sorted and unique"));
    }
    if max_bytes == 0 {
        return Err(invalid("permission byte limit must be positive"));
    }
    Ok(())
}

fn validate_permission_scope(
    path: &str,
    rights: &[PermissionRight],
    recursive: bool,
    max_bytes: u64,
    kind: PermissionTargetKind,
) -> Result<(), ProtocolError> {
    validate_permission_values(path, rights, max_bytes)?;
    if kind == PermissionTargetKind::File && (recursive || rights.contains(&PermissionRight::List))
    {
        return Err(invalid("file permission request has directory authority"));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TimestampKey {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    nanos: u32,
}

fn validate_timestamp(value: &str) -> Result<(), ProtocolError> {
    timestamp_key(value).map(|_| ())
}

fn timestamp_key(value: &str) -> Result<TimestampKey, ProtocolError> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || *bytes.last().expect("minimum timestamp length was checked") != b'Z'
    {
        return Err(invalid("timestamp is not canonical RFC 3339 UTC text"));
    }
    let year = parse_decimal::<u16>(&bytes[0..4])?;
    let month = parse_decimal::<u8>(&bytes[5..7])?;
    let day = parse_decimal::<u8>(&bytes[8..10])?;
    let hour = parse_decimal::<u8>(&bytes[11..13])?;
    let minute = parse_decimal::<u8>(&bytes[14..16])?;
    let second = parse_decimal::<u8>(&bytes[17..19])?;
    let nanos = match bytes.len() {
        20 => 0,
        22..=30 if bytes[19] == b'.' => {
            let fraction = &bytes[20..bytes.len() - 1];
            let value = parse_decimal::<u32>(fraction)?;
            value * 10_u32.pow(u32::try_from(9 - fraction.len()).expect("fraction is bounded"))
        }
        _ => return Err(invalid("timestamp is not canonical RFC 3339 UTC text")),
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let maximum_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if year == 0 || day == 0 || day > maximum_day || hour > 23 || minute > 59 || second > 59 {
        return Err(invalid("timestamp has an invalid calendar value"));
    }
    Ok(TimestampKey {
        year,
        month,
        day,
        hour,
        minute,
        second,
        nanos,
    })
}

fn parse_decimal<T>(bytes: &[u8]) -> Result<T, ProtocolError>
where
    T: std::str::FromStr,
{
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(invalid("timestamp contains invalid decimal text"));
    }
    std::str::from_utf8(bytes)
        .map_err(|_| invalid("timestamp is not UTF-8"))?
        .parse()
        .map_err(|_| invalid("timestamp decimal value is out of range"))
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn validate_source_files(files: &[SourceFile]) -> Result<(), ProtocolError> {
    if !(1..=1_024).contains(&files.len()) {
        return Err(invalid("source bundle file count is invalid"));
    }
    let paths: Vec<_> = files.iter().map(|file| file.path.clone()).collect();
    validate_sorted_unique(&paths, "source bundle paths")?;
    let loose_source = paths.as_slice() == ["src/main.allen"];
    let manifest_count = paths
        .iter()
        .filter(|path| path.as_str() == "allen.toml")
        .count();
    if !loose_source && manifest_count != 1 {
        return Err(invalid(
            "source bundle must contain one root allen.toml or one loose src/main.allen",
        ));
    }
    for file in files {
        validate_path(&file.path)?;
        let must_be_utf8 =
            file.path == "allen.toml" || file.path == "allen.lock" || file.path.ends_with(".allen");
        match file.encoding {
            FileEncoding::Utf8 if must_be_utf8 => {}
            FileEncoding::Base64 if !must_be_utf8 => validate_base64(&file.content)?,
            _ => return Err(invalid("source file encoding does not match its path")),
        }
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), ProtocolError> {
    if path.is_empty()
        || path.len() > 4_096
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .next()
            .is_some_and(|part| part.ends_with(':'))
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid(
            "source path is not normalized and package-relative",
        ));
    }
    Ok(())
}

fn validate_base64(value: &str) -> Result<(), ProtocolError> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| invalid("base64 is not canonical RFC 4648 standard base64"))?;
    if base64::engine::general_purpose::STANDARD.encode(decoded) != value {
        return Err(invalid("base64 is not canonical RFC 4648 standard base64"));
    }
    Ok(())
}

/// Validates canonical lower-case `sha256:` digest text.
///
/// # Errors
///
/// Returns an error when the prefix, length, or hexadecimal text is invalid.
pub fn validate_digest(digest: &str) -> Result<(), ProtocolError> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(invalid("digest must use sha256 text"));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(
            "digest must contain 64 lower-case hexadecimal digits",
        ));
    }
    Ok(())
}

pub(crate) fn validate_semver(version: &str) -> Result<(), ProtocolError> {
    let parsed =
        Version::parse(version).map_err(|_| invalid("version is not semantic version text"))?;
    if !parsed.pre.is_empty() || !parsed.build.is_empty() || parsed.to_string() != version {
        return Err(invalid("version is not a canonical exact semantic version"));
    }
    Ok(())
}

pub(crate) fn validate_language_range(range: &str) -> Result<(), ProtocolError> {
    let Some((lower, upper)) = range.split_once(", ") else {
        return Err(invalid(
            "language range must have one lower and one upper bound",
        ));
    };
    let Some(lower) = lower.strip_prefix(">=") else {
        return Err(invalid("language range lower bound is invalid"));
    };
    let Some(upper) = upper.strip_prefix('<') else {
        return Err(invalid("language range upper bound is invalid"));
    };
    validate_semver(lower)?;
    validate_semver(upper)?;
    let lower = Version::parse(lower).map_err(|_| invalid("language range is invalid"))?;
    let upper = Version::parse(upper).map_err(|_| invalid("language range is invalid"))?;
    if lower >= upper {
        return Err(invalid("language range is empty"));
    }
    Ok(())
}

pub(crate) fn range_contains_language(range: &str) -> bool {
    let Some((lower, upper)) = range.split_once(", ") else {
        return false;
    };
    let (Some(lower), Some(upper)) = (lower.strip_prefix(">="), upper.strip_prefix('<')) else {
        return false;
    };
    let Ok(lower) = Version::parse(lower) else {
        return false;
    };
    let Ok(upper) = Version::parse(upper) else {
        return false;
    };
    let current = Version::new(0, 1, 0);
    lower <= current && current < upper
}

pub(crate) fn validate_sorted_unique(values: &[String], name: &str) -> Result<(), ProtocolError> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid(format!("{name} must be sorted and unique")));
    }
    Ok(())
}

fn validate_tool_name(name: &str) -> Result<(), ProtocolError> {
    if name.is_empty()
        || name.len() > 255
        || name.split('.').any(str::is_empty)
        || name.chars().any(char::is_whitespace)
        || name.chars().any(char::is_control)
    {
        return Err(invalid("tool name is not canonical"));
    }
    Ok(())
}

fn validate_effect(effect: &str) -> Result<(), ProtocolError> {
    if effect.is_empty()
        || effect.len() > 128
        || effect.split('.').any(str::is_empty)
        || !effect.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_')
        })
    {
        return Err(invalid("tool effect ID is not canonical"));
    }
    Ok(())
}

fn validate_description(description: &str) -> Result<(), ProtocolError> {
    if description.len() > 4_096 || description.contains('\0') {
        return Err(invalid("tool description is invalid"));
    }
    Ok(())
}

fn validate_opaque(value: &str, name: &str) -> Result<(), ProtocolError> {
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return Err(invalid(format!("{name} is invalid")));
    }
    Ok(())
}

fn validate_https_origin(origin: &str) -> Result<(), ProtocolError> {
    let parsed = url::Url::parse(origin).map_err(|_| invalid("HTTP origin is invalid"))?;
    let canonical = parsed.origin().ascii_serialization();
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || canonical != origin
    {
        return Err(invalid("HTTP origin is not canonical HTTPS origin text"));
    }
    Ok(())
}
