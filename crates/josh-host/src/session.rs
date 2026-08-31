use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use allen_bytecode::{
    DecodeLimits, VerifiedArtifact, compute_strict_schema_digest, decode_and_verify, encode,
};
use allen_compiler::{assemble_inline_source, assemble_root_source_package_with_resources};
use allen_package::LoadLimits;
use allen_runtime::{
    HostPolicy, LaunchRequest, PreparedLaunch, RuntimeError, RuntimeProviders, ToolProvider,
    execute_prepared_with_context, prepare_launch,
};
use allen_sandbox_fs::Rights;
use allen_schema::{
    CatalogLimits, ExactVersion, FrozenCatalog, Idempotency as SchemaIdempotency, SchemaLimits,
    ToolDefinition, ToolName, ToolRequirement, VersionRange, generated_tool_effect,
    selected_tool_contract_digest,
};
use allen_vm::{CancellationSource, Checkpoint, CheckpointObserver, ExecutionOutcome};
use base64::Engine as _;
use josh_protocol::{
    CatalogSetParams, CatalogSetResult, CatalogToolSummary, EntryContract, ExecutionMode,
    ExecutionResult, ExecutionStartParams, HostProjectionSetParams, HostProjectionSetResult,
    InitializeParams, InitializeResult, PeerInfo, ProgramLoadParams, ProgramLoadResult,
    ProjectionSectionKind, ProtocolLimits, SessionBindingLevel, Validate, WireError, WireErrorCode,
};
use sha2::{Digest as _, Sha256};

const RUNTIME_NAME: &str = "allen-reference";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_LOADED_PROGRAMS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostError {
    pub code: WireErrorCode,
    pub message: &'static str,
}

impl HostError {
    const fn new(code: WireErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    #[must_use]
    pub fn wire(&self) -> WireError {
        WireError {
            code: self.code,
            message: self.message.to_owned(),
            data: None,
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for HostError {}

#[derive(Debug)]
struct LoadedProgram {
    artifact: Arc<VerifiedArtifact>,
    artifact_digest: String,
}

#[derive(Debug)]
pub struct PreparedExecution {
    request_id: String,
    params: ExecutionStartParams,
    program: Arc<LoadedProgram>,
    catalog: Arc<FrozenCatalog>,
    prepared: Mutex<Option<PreparedLaunch>>,
    wall_time: Duration,
    cancelled: Arc<AtomicBool>,
    invoking_session_id: Option<String>,
}

impl PreparedExecution {
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    #[must_use]
    pub fn execution_id(&self) -> &str {
        &self.params.execution_id
    }

    #[must_use]
    pub fn program_id(&self) -> &str {
        &self.params.program_id
    }

    #[must_use]
    pub fn entry(&self) -> &str {
        &self.params.entry
    }

    #[must_use]
    pub fn artifact_digest(&self) -> &str {
        &self.program.artifact_digest
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn cancellation_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    pub(crate) fn wall_time(&self) -> Duration {
        self.wall_time
    }

    #[must_use]
    pub(crate) fn invoking_session_id(&self) -> Option<&str> {
        self.invoking_session_id.as_deref()
    }

    #[must_use]
    pub fn run(&self, tools: Option<&mut dyn ToolProvider>) -> ExecutionResult {
        let mut observer = NullObserver;
        let mut providers = RuntimeProviders {
            tools,
            ..RuntimeProviders::default()
        };
        self.run_with_observer(&mut providers, &mut observer)
    }

    pub(crate) fn run_with_observer(
        &self,
        providers: &mut RuntimeProviders<'_>,
        observer: &mut dyn CheckpointObserver,
    ) -> ExecutionResult {
        if self.is_cancelled() {
            return ExecutionResult::Cancelled { reason: None };
        }
        let mut cancellation = ExecutionCancellation(Arc::clone(&self.cancelled));
        let Some(prepared) = self.prepared.lock().ok().and_then(|mut value| value.take()) else {
            return ExecutionResult::Failed {
                error: runtime_panic(),
            };
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            execute_prepared_with_context(prepared, providers, &mut cancellation, observer)
        }));
        let Ok(result) = result else {
            return ExecutionResult::Failed {
                error: runtime_panic(),
            };
        };
        if self.is_cancelled() {
            return ExecutionResult::Cancelled { reason: None };
        }
        match result {
            Ok(outcome) => match outcome.execution {
                ExecutionOutcome::Completed(_) => ExecutionResult::Completed {
                    output: outcome.output,
                },
                ExecutionOutcome::Stopped { reason, .. } => stopped_result(reason),
            },
            Err(error) => ExecutionResult::Failed {
                error: runtime_error(&error),
            },
        }
    }

    pub(crate) fn catalog(&self) -> &Arc<FrozenCatalog> {
        &self.catalog
    }
}

fn stopped_result(reason: String) -> ExecutionResult {
    let result = ExecutionResult::Stopped {
        reason: Some(reason),
    };
    if result.validate().is_ok() {
        result
    } else {
        ExecutionResult::Stopped { reason: None }
    }
}

struct ExecutionCancellation(Arc<AtomicBool>);

impl CancellationSource for ExecutionCancellation {
    fn is_cancelled(&mut self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

struct NullObserver;

impl CheckpointObserver for NullObserver {
    fn checkpoint(&mut self, _checkpoint: Checkpoint) {}
}

struct ActiveExecution {
    request_id: String,
    execution_id: String,
    cancelled: Arc<AtomicBool>,
}

pub struct Session {
    initialized: bool,
    initialized_host: Option<PeerInfo>,
    execution_mode: Option<ExecutionMode>,
    invoking_session_id: Option<String>,
    standard_capabilities: BTreeSet<String>,
    effective_limits: ProtocolLimits,
    projection: Option<HostProjectionSetResult>,
    catalog: Option<Arc<FrozenCatalog>>,
    programs: BTreeMap<String, Arc<LoadedProgram>>,
    next_program_id: u64,
    execution_ids: BTreeSet<String>,
    active_execution: Option<ActiveExecution>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self {
            initialized: false,
            initialized_host: None,
            execution_mode: None,
            invoking_session_id: None,
            standard_capabilities: BTreeSet::new(),
            effective_limits: runtime_limits(),
            projection: None,
            catalog: None,
            programs: BTreeMap::new(),
            next_program_id: 1,
            execution_ids: BTreeSet::new(),
            active_execution: None,
        }
    }

    /// Negotiates the one supported protocol and language version.
    ///
    /// # Errors
    ///
    /// Returns `request.invalid` for invalid parameters or a repeated call.
    pub fn initialize(&mut self, params: &InitializeParams) -> Result<InitializeResult, HostError> {
        if self.initialized || params.validate().is_err() {
            return Err(HostError::new(
                WireErrorCode::RequestInvalid,
                "initialize parameters are invalid",
            ));
        }
        self.effective_limits = lower_limits(runtime_limits(), params.limits);
        self.standard_capabilities = params.standard_capabilities.iter().cloned().collect();
        self.initialized_host = Some(params.host.clone());
        self.execution_mode = Some(params.execution_mode);
        self.invoking_session_id = params.bound_session_id().map(str::to_owned);
        self.initialized = true;
        Ok(InitializeResult {
            protocol_version: josh_protocol::PROTOCOL_VERSION.to_owned(),
            runtime: PeerInfo {
                name: RUNTIME_NAME.to_owned(),
                version: VERSION.to_owned(),
            },
            language_version: josh_protocol::LANGUAGE_VERSION.to_owned(),
            features: josh_protocol::FEATURES
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
            limits: self.effective_limits,
        })
    }

    /// Validates and freezes the complete host projection for this connection.
    ///
    /// # Errors
    ///
    /// Returns a stable projection error without storing partial state.
    pub fn set_projection(
        &mut self,
        params: &HostProjectionSetParams,
    ) -> Result<HostProjectionSetResult, HostError> {
        if !self.initialized || self.projection.is_some() || params.validate().is_err() {
            return Err(HostError::new(
                WireErrorCode::ProjectionInvalid,
                "host projection is invalid",
            ));
        }
        if self.initialized_host.as_ref() != Some(&params.host) {
            return Err(HostError::new(
                WireErrorCode::ProjectionMismatch,
                "host projection identity does not match initialization",
            ));
        }
        let binding_matches = matches!(
            (self.execution_mode, params.session_binding),
            (Some(ExecutionMode::Unattended), SessionBindingLevel::None)
                | (
                    Some(ExecutionMode::Attached),
                    SessionBindingLevel::PromptAssisted | SessionBindingLevel::Authenticated
                )
        );
        if !binding_matches {
            return Err(HostError::new(
                WireErrorCode::ProjectionMismatch,
                "host projection session binding does not match initialization",
            ));
        }
        let encoded = serde_json::to_vec(params).map_err(|_| {
            HostError::new(
                WireErrorCode::ProjectionInvalid,
                "host projection is invalid",
            )
        })?;
        let digest: [u8; 32] = Sha256::digest(encoded).into();
        let result = HostProjectionSetResult {
            projection_digest: digest_text(&digest),
            projection: params.clone(),
        };
        self.projection = Some(result.clone());
        Ok(result)
    }

    /// Validates and freezes the complete connection tool catalog.
    ///
    /// # Errors
    ///
    /// Returns a stable catalog error without freezing partial state.
    pub fn set_catalog(
        &mut self,
        params: &CatalogSetParams,
    ) -> Result<CatalogSetResult, HostError> {
        if !self.initialized
            || self.projection.is_none()
            || self.catalog.is_some()
            || params.validate().is_err()
        {
            return Err(HostError::new(
                WireErrorCode::CatalogInvalid,
                "tool catalog is invalid",
            ));
        }
        if !params.metadata.complete {
            return Err(HostError::new(
                WireErrorCode::CatalogInvalid,
                "tool catalog is incomplete",
            ));
        }
        let tools_projection = self
            .projection
            .as_ref()
            .expect("projection presence checked above")
            .projection
            .section(ProjectionSectionKind::Tools);
        let metadata_matches = tools_projection.source == params.metadata.source
            && tools_projection.source_revision == params.metadata.source_revision
            && tools_projection.observed_at_unix_ms == params.metadata.observed_at_unix_ms
            && tools_projection.freshness == params.metadata.freshness
            && tools_projection.complete == params.metadata.complete
            && usize::try_from(tools_projection.item_count).ok() == Some(params.tools.len());
        if !metadata_matches {
            return Err(HostError::new(
                WireErrorCode::ProjectionMismatch,
                "tool catalog does not match the host projection",
            ));
        }
        let schema_limits = SchemaLimits::default();
        let tools = params
            .tools
            .iter()
            .map(|tool| {
                ToolDefinition::parse(
                    &tool.name,
                    &tool.version,
                    &serde_json::to_string(&tool.input_schema).map_err(|_| ())?,
                    &serde_json::to_string(&tool.output_schema).map_err(|_| ())?,
                    &serde_json::to_string(&tool.error_schema).map_err(|_| ())?,
                    tool.effects.clone(),
                    match tool.idempotency {
                        josh_protocol::Idempotency::Unknown => SchemaIdempotency::Unknown,
                        josh_protocol::Idempotency::Idempotent => SchemaIdempotency::Idempotent,
                        josh_protocol::Idempotency::NonIdempotent => {
                            SchemaIdempotency::NonIdempotent
                        }
                    },
                    &schema_limits,
                )
                .map_err(|_| ())
            })
            .collect::<Result<Vec<_>, ()>>()
            .map_err(|()| {
                HostError::new(WireErrorCode::CatalogInvalid, "tool catalog is invalid")
            })?;
        let limits = CatalogLimits {
            tools: usize::try_from(self.effective_limits.max_catalog_tools)
                .unwrap_or(MAX_LOADED_PROGRAMS),
            decoded_schema_bytes: usize::try_from(self.effective_limits.max_catalog_bytes)
                .unwrap_or(usize::MAX),
            schema: schema_limits,
        };
        let catalog = FrozenCatalog::freeze_with_dialect(&params.schema_dialect, tools, &limits)
            .map_err(|_| {
                HostError::new(WireErrorCode::CatalogInvalid, "tool catalog is invalid")
            })?;
        let result = CatalogSetResult {
            catalog_digest: catalog.digest().to_owned(),
            schema_profile: catalog.schema_profile().to_owned(),
            tool_count: u64::try_from(catalog.tools().len()).unwrap_or(u64::MAX),
            metadata: params.metadata.clone(),
            tools: catalog
                .tools()
                .iter()
                .zip(&params.tools)
                .map(|(tool, supplied)| CatalogToolSummary {
                    name: tool.name.as_str().to_owned(),
                    version: tool.version.to_string(),
                    description: supplied.description.clone(),
                })
                .collect(),
        };
        self.catalog = Some(Arc::new(catalog));
        Ok(result)
    }

    /// Decodes, verifies, and stores one complete bytecode artifact.
    ///
    /// # Errors
    ///
    /// Returns a stable program error and creates no program ID on failure.
    pub fn load_program(
        &mut self,
        params: &ProgramLoadParams,
    ) -> Result<ProgramLoadResult, HostError> {
        params.validate().map_err(|_| {
            HostError::new(WireErrorCode::ProgramInvalid, "program load is invalid")
        })?;
        let catalog = self.catalog.as_ref().ok_or_else(|| {
            HostError::new(WireErrorCode::RequestInvalidState, "catalog is not frozen")
        })?;
        if self.programs.len()
            >= usize::try_from(self.effective_limits.max_loaded_programs)
                .unwrap_or(MAX_LOADED_PROGRAMS)
                .min(MAX_LOADED_PROGRAMS)
        {
            return Err(HostError::new(
                WireErrorCode::RequestLimit,
                "loaded program limit reached",
            ));
        }
        let bytes = match params {
            ProgramLoadParams::Bytecode { artifact } => base64::engine::general_purpose::STANDARD
                .decode(artifact)
                .map_err(|_| {
                    HostError::new(WireErrorCode::ProgramInvalid, "program artifact is invalid")
                })?,
            ProgramLoadParams::SourceBundle { files } => assemble_source_bundle(
                files,
                catalog,
                self.effective_limits
                    .max_frame_bytes
                    .min(self.effective_limits.max_catalog_bytes),
            )?,
        };
        let mut limits = DecodeLimits::default();
        limits.artifact_bytes = limits
            .artifact_bytes
            .min(usize::try_from(self.effective_limits.max_frame_bytes).unwrap_or(usize::MAX));
        let verified = decode_and_verify(&bytes, &limits).map_err(|_| {
            HostError::new(WireErrorCode::ProgramInvalid, "program artifact is invalid")
        })?;
        validate_catalog_contracts(&verified, catalog)?;
        let artifact_digest = digest_text(verified.content_digest());
        let manifest = verified.manifest().ok_or_else(|| {
            HostError::new(WireErrorCode::ProgramInvalid, "program manifest is missing")
        })?;
        let tool_contract_digest = digest_text(&manifest.tool_contract_digest);
        let required_tools = manifest
            .required_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        let exec_commands = manifest.exec_commands.clone();
        let exec_environment = manifest.exec_environment.clone();
        let entries = verified
            .entries()
            .iter()
            .map(|entry| EntryContract {
                name: entry.name.clone(),
                input_schema: digest_text(&compute_strict_schema_digest(
                    &verified.schemas()[entry.input_schema as usize],
                )),
                output_schema: digest_text(&compute_strict_schema_digest(
                    &verified.schemas()[entry.output_schema as usize],
                )),
                input_contract_digest: digest_text(&entry.input_contract_digest),
                output_contract_digest: digest_text(&entry.output_contract_digest),
            })
            .collect();
        let program_id = format!("program-{}", self.next_program_id);
        self.next_program_id = self.next_program_id.saturating_add(1);
        self.programs.insert(
            program_id.clone(),
            Arc::new(LoadedProgram {
                artifact: Arc::new(verified),
                artifact_digest: artifact_digest.clone(),
            }),
        );
        Ok(ProgramLoadResult {
            program_id,
            artifact_digest,
            tool_contract_digest,
            diagnostics: Vec::new(),
            entries,
            required_tools,
            exec_commands,
            exec_environment,
        })
    }

    /// Performs start preflight and reserves the one active execution slot.
    ///
    /// # Errors
    ///
    /// Returns a wire error before an accepted event for invalid input or authority.
    pub fn prepare_execution(
        &mut self,
        request_id: String,
        params: ExecutionStartParams,
    ) -> Result<PreparedExecution, HostError> {
        params.validate().map_err(|_| {
            HostError::new(WireErrorCode::RequestInvalid, "execution start is invalid")
        })?;
        if self.active_execution.is_some() {
            return Err(HostError::new(
                WireErrorCode::RequestInvalidState,
                "an execution is already active",
            ));
        }
        if params
            .granted_capabilities
            .iter()
            .any(|capability| !self.standard_capabilities.contains(capability))
        {
            return Err(HostError::new(
                WireErrorCode::RequestInvalid,
                "execution capability was not negotiated",
            ));
        }
        if self.execution_ids.contains(&params.execution_id) {
            return Err(HostError::new(
                WireErrorCode::ExecutionDuplicate,
                "execution ID was already used",
            ));
        }
        if self.execution_ids.len()
            >= usize::try_from(self.effective_limits.max_total_executions).unwrap_or(usize::MAX)
        {
            return Err(HostError::new(
                WireErrorCode::RequestLimit,
                "total execution limit reached",
            ));
        }
        let program = self
            .programs
            .get(&params.program_id)
            .cloned()
            .ok_or_else(|| {
                HostError::new(WireErrorCode::RequestInvalid, "program ID is unknown")
            })?;
        if params.artifact_digest != program.artifact_digest {
            return Err(HostError::new(
                WireErrorCode::CatalogMismatch,
                "artifact digest does not match the loaded program",
            ));
        }
        let catalog = self.catalog.clone().ok_or_else(|| {
            HostError::new(WireErrorCode::RequestInvalidState, "catalog is not frozen")
        })?;
        let policy = host_policy(&params, &catalog);
        let wall_time = policy.limits.wall_time;
        let prepared = prepare_launch(
            &program.artifact,
            &LaunchRequest {
                entry: params.entry.clone(),
                input: params.input.clone(),
            },
            &policy,
        )
        .map_err(|_| HostError::new(WireErrorCode::RequestInvalid, "execution preflight failed"))?;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.execution_ids.insert(params.execution_id.clone());
        self.active_execution = Some(ActiveExecution {
            request_id: request_id.clone(),
            execution_id: params.execution_id.clone(),
            cancelled: Arc::clone(&cancelled),
        });
        Ok(PreparedExecution {
            request_id,
            params,
            program,
            catalog,
            prepared: Mutex::new(Some(prepared)),
            wall_time,
            cancelled,
            invoking_session_id: self.invoking_session_id.clone(),
        })
    }

    #[must_use]
    pub fn cancel(&self, request_id: &str) -> bool {
        self.active_execution.as_ref().is_some_and(|active| {
            if active.request_id == request_id {
                active.cancelled.store(true, Ordering::Release);
                true
            } else {
                false
            }
        })
    }

    pub fn cancel_active(&self) {
        if let Some(active) = &self.active_execution {
            active.cancelled.store(true, Ordering::Release);
        }
    }

    pub fn finish_execution(&mut self, request_id: &str) {
        if self
            .active_execution
            .as_ref()
            .is_some_and(|active| active.request_id == request_id)
        {
            self.active_execution = None;
        }
    }

    #[must_use]
    pub fn matches_active_binding(&self, execution_id: &str, session_id: &str) -> bool {
        self.invoking_session_id.as_deref() == Some(session_id)
            && self
                .active_execution
                .as_ref()
                .is_some_and(|active| active.execution_id == execution_id)
    }

    #[must_use]
    pub fn catalog_digest(&self) -> Option<&str> {
        self.catalog.as_deref().map(FrozenCatalog::digest)
    }

    #[must_use]
    pub const fn effective_limits(&self) -> ProtocolLimits {
        self.effective_limits
    }
}

fn runtime_limits() -> ProtocolLimits {
    ProtocolLimits {
        max_frame_bytes: josh_protocol::DEFAULT_MAX_FRAME_BYTES as u64,
        max_active_requests: 64,
        max_loaded_programs: MAX_LOADED_PROGRAMS as u64,
        max_total_executions: 1_024,
        max_catalog_tools: 256,
        max_catalog_bytes: 3_145_728,
    }
}

fn lower_limits(left: ProtocolLimits, right: ProtocolLimits) -> ProtocolLimits {
    ProtocolLimits {
        max_frame_bytes: left.max_frame_bytes.min(right.max_frame_bytes),
        max_active_requests: left.max_active_requests.min(right.max_active_requests),
        max_loaded_programs: left.max_loaded_programs.min(right.max_loaded_programs),
        max_total_executions: left.max_total_executions.min(right.max_total_executions),
        max_catalog_tools: left.max_catalog_tools.min(right.max_catalog_tools),
        max_catalog_bytes: left.max_catalog_bytes.min(right.max_catalog_bytes),
    }
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn assemble_source_bundle(
    files: &[josh_protocol::SourceFile],
    catalog: &FrozenCatalog,
    max_bytes: u64,
) -> Result<Vec<u8>, HostError> {
    let mut manifest = None;
    let mut lock = None;
    let mut sources = BTreeMap::new();
    let mut resources = BTreeMap::new();
    let mut decoded_bytes = 0_u64;
    for file in files {
        let decoded = match file.encoding {
            josh_protocol::FileEncoding::Utf8 => file.content.as_bytes().to_vec(),
            josh_protocol::FileEncoding::Base64 => base64::engine::general_purpose::STANDARD
                .decode(&file.content)
                .map_err(|_| {
                    HostError::new(WireErrorCode::ProgramInvalid, "source data is invalid")
                })?,
        };
        let file_bytes = u64::try_from(decoded.len()).unwrap_or(u64::MAX);
        decoded_bytes = decoded_bytes.checked_add(file_bytes).ok_or_else(|| {
            HostError::new(WireErrorCode::ProgramInvalid, "source program is too large")
        })?;
        if decoded_bytes > max_bytes {
            return Err(HostError::new(
                WireErrorCode::ProgramInvalid,
                "source program is too large",
            ));
        }
        match file.path.as_str() {
            "allen.toml" => manifest = Some(file.content.as_str()),
            "allen.lock" => lock = Some(file.content.as_str()),
            path if path.starts_with("src/") && path.ends_with(".allen") => {
                sources.insert(path.to_owned(), file.content.clone());
            }
            path => {
                resources.insert(path.to_owned(), decoded);
            }
        }
    }
    let mut limits = LoadLimits::default();
    limits.source_bytes = limits.source_bytes.min(max_bytes);
    limits.manifest_bytes = limits.manifest_bytes.min(max_bytes);
    limits.modules = limits.modules.min(files.len());
    limits.filesystem_entries = limits.filesystem_entries.min(files.len());
    limits.path_bytes = limits.path_bytes.min(4_096);
    let compiled = if let Some(manifest) = manifest {
        assemble_root_source_package_with_resources(
            manifest,
            &sources,
            &resources,
            lock,
            Some(catalog),
            &limits,
        )
    } else if lock.is_none() && sources.len() == 1 {
        let source = sources.get("src/main.allen").ok_or_else(|| {
            HostError::new(
                WireErrorCode::ProgramInvalid,
                "loose source must be named src/main.allen",
            )
        })?;
        assemble_inline_source(source, catalog)
    } else {
        return Err(HostError::new(
            WireErrorCode::ProgramInvalid,
            "source manifest is missing",
        ));
    }
    .map_err(|_| HostError::new(WireErrorCode::ProgramInvalid, "source program is invalid"))?;
    encode(&compiled.artifact)
        .map_err(|_| HostError::new(WireErrorCode::ProgramInvalid, "source program is invalid"))
}

fn validate_catalog_contracts(
    artifact: &VerifiedArtifact,
    catalog: &FrozenCatalog,
) -> Result<(), HostError> {
    let manifest = artifact.manifest().ok_or_else(|| {
        HostError::new(WireErrorCode::ProgramInvalid, "program manifest is missing")
    })?;
    let requirements = manifest
        .required_tools
        .iter()
        .map(|contract| ToolRequirement::parse(&contract.name, &contract.version_requirement))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| unsatisfied())?;
    let selected = catalog.select(&requirements).map_err(|_| unsatisfied())?;
    if selected_tool_contract_digest(&selected).map_err(|_| unsatisfied())?
        != digest_text(&manifest.tool_contract_digest)
    {
        return Err(unsatisfied());
    }
    for contract in &manifest.required_tools {
        let name = ToolName::parse(&contract.name).map_err(|_| unsatisfied())?;
        let definition = catalog.get(&name).ok_or_else(unsatisfied)?;
        let version = ExactVersion::parse(&contract.version).map_err(|_| unsatisfied())?;
        let range =
            VersionRange::parse(&contract.version_requirement).map_err(|_| unsatisfied())?;
        if definition.version != version
            || !range.contains(version)
            || contract.effect
                != generated_tool_effect(&name, version).map_err(|_| unsatisfied())?
            || definition.input_schema.digest() != digest_text(&contract.input_digest)
            || definition.output_schema.digest() != digest_text(&contract.output_digest)
            || definition.error_schema.digest() != digest_text(&contract.error_digest)
        {
            return Err(unsatisfied());
        }
    }
    Ok(())
}

const fn unsatisfied() -> HostError {
    HostError::new(
        WireErrorCode::ProgramUnsatisfied,
        "program tool contract is unsatisfied",
    )
}

fn host_policy(params: &ExecutionStartParams, catalog: &FrozenCatalog) -> HostPolicy {
    let mut policy = HostPolicy {
        granted_exec: params.granted_exec.iter().cloned().collect(),
        granted_exec_environment: params.granted_exec_environment.iter().cloned().collect(),
        ..HostPolicy::default()
    };
    for (name, value) in &params.limits {
        match name.as_str() {
            "instructions" => policy.limits.instructions = *value,
            "heap_bytes" => policy.limits.allocation_bytes = *value,
            "maximum_allocation_bytes" => policy.limits.maximum_allocation_bytes = *value,
            "call_depth" => policy.limits.call_depth = u32::try_from(*value).unwrap_or(u32::MAX),
            "wall_ms" => policy.limits.wall_time = Duration::from_millis(*value),
            "tasks" => policy.limits.tasks = u32::try_from(*value).unwrap_or(u32::MAX),
            "concurrent_effects" => {
                policy.limits.concurrent_effects = u32::try_from(*value).unwrap_or(u32::MAX);
            }
            "cleanup_instructions" => policy.limits.cleanup_instructions = *value,
            "effects" => policy.effects = *value,
            "input_bytes" => policy.input_bytes = usize::try_from(*value).unwrap_or(usize::MAX),
            "output_bytes" => policy.output_bytes = usize::try_from(*value).unwrap_or(usize::MAX),
            "fs_entries" => {
                policy.workspace_limits.max_entries = usize::try_from(*value).unwrap_or(usize::MAX);
            }
            "fs_file_bytes" => policy.workspace_limits.max_file_bytes = *value,
            "fs_operations" => policy.workspace_limits.max_operations = *value,
            "fs_read_bytes" => policy.workspace_limits.max_read_bytes = *value,
            "fs_write_bytes" => policy.workspace_limits.max_write_bytes = *value,
            "http_requests" => {
                policy.http_limits.max_requests = u32::try_from(*value).unwrap_or(u32::MAX);
            }
            "http_redirects" => {
                policy.http_limits.max_redirects = u32::try_from(*value).unwrap_or(u32::MAX);
            }
            "http_dns_addresses" => {
                policy.http_limits.max_dns_candidates =
                    usize::try_from(*value).unwrap_or(usize::MAX);
            }
            "http_response_headers" => {
                policy.http_limits.max_response_headers =
                    usize::try_from(*value).unwrap_or(usize::MAX);
            }
            "http_response_header_bytes" => {
                policy.http_limits.max_header_bytes = usize::try_from(*value).unwrap_or(usize::MAX);
            }
            "http_compressed_bytes" => policy.http_limits.max_compressed_bytes = *value,
            "http_decoded_bytes" => policy.http_limits.max_decoded_bytes = *value,
            "http_decompression_ratio" => {
                policy.http_limits.max_decompression_ratio =
                    u32::try_from(*value).unwrap_or(u32::MAX);
            }
            "http_connect_ms" => policy.http_limits.connect_timeout = Duration::from_millis(*value),
            "http_first_byte_ms" => {
                policy.http_limits.first_byte_timeout = Duration::from_millis(*value);
            }
            "http_idle_ms" => policy.http_limits.idle_timeout = Duration::from_millis(*value),
            "http_total_ms" => policy.http_limits.total_timeout = Duration::from_millis(*value),
            _ => {}
        }
    }
    let capabilities = params
        .granted_capabilities
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    HostPolicy {
        granted_capabilities: capabilities.clone(),
        workspace_root: params.working_directory.as_ref().map(PathBuf::from),
        workspace_rights: Rights::new(
            capabilities.contains("fs.read"),
            capabilities.contains("fs.write"),
        ),
        http_origins: params.allowed_http_origins.iter().cloned().collect(),
        granted_tools: params.granted_tools.iter().cloned().collect(),
        tool_catalog: Some(catalog.clone()),
        strict_preflight_authority: true,
        ..policy
    }
}

fn runtime_error(error: &RuntimeError) -> josh_protocol::ProtocolRuntimeError {
    let mut result = josh_protocol::ProtocolRuntimeError {
        code: error.code.as_str().to_owned(),
        message: error.message.clone(),
        category: "runtime".to_owned(),
        retryable: false,
        span: None,
        operation_id: None,
        metadata: BTreeMap::new(),
        causes: Vec::new(),
    };
    if result.code == "program.failed" && result.validate().is_err() {
        "program failed".clone_into(&mut result.message);
    }
    result
}

fn runtime_panic() -> josh_protocol::ProtocolRuntimeError {
    josh_protocol::ProtocolRuntimeError {
        code: "runtime.panic".to_owned(),
        message: "runtime invariant failed".to_owned(),
        category: "runtime".to_owned(),
        retryable: false,
        span: None,
        operation_id: None,
        metadata: BTreeMap::new(),
        causes: Vec::new(),
    }
}

fn digest_text(digest: &[u8; 32]) -> String {
    let mut text = String::with_capacity(71);
    text.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(text, "{byte:02x}").expect("writing to String cannot fail");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_reason_is_omitted_when_it_exceeds_the_protocol_bound() {
        assert_eq!(
            stopped_result("é".repeat(513)),
            ExecutionResult::Stopped { reason: None }
        );
        let control = "line\n\u{0000}tab\t".to_owned();
        assert_eq!(
            stopped_result(control.clone()),
            ExecutionResult::Stopped {
                reason: Some(control)
            }
        );
    }
    use allen_bytecode::{
        Artifact, ArtifactMetadata, Constant, EntryContract as ArtifactEntry, Function,
        Instruction, ManifestContract, Module, StrictSchema, ValueType,
        compute_entry_contract_digest, compute_tool_contract_digest, encode,
    };
    use josh_protocol::{ExecutionMode, InvokingSessionId};
    use serde_json::Value;

    fn initialize_params() -> InitializeParams {
        InitializeParams {
            host: PeerInfo {
                name: "test-host".to_owned(),
                version: "1.0.0".to_owned(),
            },
            protocol_versions: vec![josh_protocol::PROTOCOL_VERSION.to_owned()],
            language_versions: vec![">=0.1.0, <0.2.0".to_owned()],
            execution_mode: ExecutionMode::Unattended,
            invoking_session_id: InvokingSessionId::Null,
            standard_capabilities: Vec::new(),
            limits: runtime_limits(),
            extensions: Vec::new(),
        }
    }

    fn empty_catalog() -> CatalogSetParams {
        CatalogSetParams {
            schema_dialect: josh_protocol::SCHEMA_DIALECT.to_owned(),
            metadata: josh_protocol::CatalogMetadata::complete("test-host", "1", 1),
            tools: Vec::new(),
        }
    }

    fn set_projection(
        session: &mut Session,
        binding: SessionBindingLevel,
        catalog: &CatalogSetParams,
    ) {
        session
            .set_projection(&HostProjectionSetParams::complete_for_catalog(
                "test-projection",
                initialize_params().host,
                binding,
                catalog,
            ))
            .unwrap();
    }

    fn artifact() -> Artifact {
        Artifact {
            metadata: ArtifactMetadata::default(),
            module: Module {
                constants: vec![Constant::Unit],
                enum_types: Vec::new(),
                effect_sets: vec![Vec::new()],
                functions: vec![Function {
                    name: "pkg/x74657374/x302e312e30/x737263/x6d61696e.allen::main".to_owned(),
                    parameters: vec![0],
                    parameter_names: vec!["_arg0".to_owned()],
                    parameter_default_digests: vec![None],
                    captures: Vec::new(),
                    registers: vec![ValueType::Unit],
                    return_type: ValueType::Unit,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                }],
                async_functions: Vec::new(),
                entry: 0,
            },
            debug: None,
            schemas: vec![StrictSchema {
                value_type: ValueType::Unit,
            }],
            entries: vec![ArtifactEntry {
                name: "main".to_owned(),
                function: 0,
                input_schema: 0,
                output_schema: 0,
                input_validators: Vec::new(),
                output_validators: Vec::new(),
                input_record_provenance: Vec::new(),
                output_record_provenance: Vec::new(),
                input_contract_digest: compute_entry_contract_digest(
                    &StrictSchema {
                        value_type: ValueType::Unit,
                    },
                    &[],
                    &[],
                ),
                output_contract_digest: compute_entry_contract_digest(
                    &StrictSchema {
                        value_type: ValueType::Unit,
                    },
                    &[],
                    &[],
                ),
            }],
            imports: Vec::new(),
            manifest: Some(ManifestContract {
                package: "test".to_owned(),
                version: "0.1.0".to_owned(),
                language_requirement: "0.1".to_owned(),
                required_capabilities: Vec::new(),
                optional_capabilities: Vec::new(),
                limits: Vec::new(),
                https_origins: Vec::new(),
                exec_commands: Vec::new(),
                exec_environment: Vec::new(),
                required_tools: Vec::new(),
                tool_contract_digest: compute_tool_contract_digest(&[]),
            }),
            templates: Vec::new(),
            record_invariants: Vec::new(),
        }
    }

    fn artifact_bytes() -> Vec<u8> {
        encode(&artifact()).unwrap()
    }

    fn loaded_session() -> (Session, ProgramLoadResult) {
        let mut session = Session::new();
        session.initialize(&initialize_params()).unwrap();
        let catalog = empty_catalog();
        set_projection(&mut session, SessionBindingLevel::None, &catalog);
        session.set_catalog(&catalog).unwrap();
        let loaded = session
            .load_program(&ProgramLoadParams::Bytecode {
                artifact: base64::engine::general_purpose::STANDARD.encode(artifact_bytes()),
            })
            .unwrap();
        (session, loaded)
    }

    struct PanickingObserver;

    impl CheckpointObserver for PanickingObserver {
        fn checkpoint(&mut self, _checkpoint: Checkpoint) {
            panic!("provider-secret observer detail")
        }
    }

    #[test]
    fn initialization_and_catalog_are_single_assignment() {
        let mut session = Session::new();
        let mut requested = initialize_params();
        requested.limits.max_frame_bytes = 1_024;
        let initialized = session.initialize(&requested).unwrap();
        assert_eq!(
            initialized.protocol_version,
            josh_protocol::PROTOCOL_VERSION
        );
        assert_eq!(initialized.limits.max_frame_bytes, 1_024);
        assert_eq!(
            session.initialize(&initialize_params()).unwrap_err().code,
            WireErrorCode::RequestInvalid
        );
        let empty = empty_catalog();
        set_projection(&mut session, SessionBindingLevel::None, &empty);
        let catalog = session.set_catalog(&empty).unwrap();
        assert_eq!(catalog.tool_count, 0);
        assert_eq!(catalog.metadata.source, "test-host");
        assert!(catalog.metadata.complete);
        assert!(catalog.tools.is_empty());
        assert_eq!(
            session.set_catalog(&empty_catalog()).unwrap_err().code,
            WireErrorCode::CatalogInvalid
        );
    }

    #[test]
    fn projection_binding_digest_and_single_assignment_are_strict() {
        let catalog = empty_catalog();
        let valid = HostProjectionSetParams::complete_for_catalog(
            "projection-stable",
            initialize_params().host,
            SessionBindingLevel::None,
            &catalog,
        );

        let mut uninitialized = Session::new();
        assert_eq!(
            uninitialized.set_projection(&valid).unwrap_err().code,
            WireErrorCode::ProjectionInvalid
        );

        let mut first = Session::new();
        first.initialize(&initialize_params()).unwrap();
        let mut wrong_host = valid.clone();
        wrong_host.host.name = "another-host".to_owned();
        assert_eq!(
            first.set_projection(&wrong_host).unwrap_err().code,
            WireErrorCode::ProjectionMismatch
        );
        let mut wrong_binding = valid.clone();
        wrong_binding.session_binding = SessionBindingLevel::PromptAssisted;
        assert_eq!(
            first.set_projection(&wrong_binding).unwrap_err().code,
            WireErrorCode::ProjectionMismatch
        );
        let first_result = first.set_projection(&valid).unwrap();
        assert!(first_result.projection_digest.starts_with("sha256:"));
        assert_eq!(
            first.set_projection(&valid).unwrap_err().code,
            WireErrorCode::ProjectionInvalid
        );

        let mut second = Session::new();
        second.initialize(&initialize_params()).unwrap();
        let second_result = second.set_projection(&valid).unwrap();
        assert_eq!(
            first_result.projection_digest,
            second_result.projection_digest
        );

        let mut attached = initialize_params();
        attached.execution_mode = ExecutionMode::Attached;
        attached.invoking_session_id = InvokingSessionId::Id("session-1".to_owned());
        let mut attached_session = Session::new();
        attached_session.initialize(&attached).unwrap();
        assert_eq!(
            attached_session.set_projection(&valid).unwrap_err().code,
            WireErrorCode::ProjectionMismatch
        );
        let attached_projection = HostProjectionSetParams::complete_for_catalog(
            "projection-attached",
            attached.host,
            SessionBindingLevel::Authenticated,
            &catalog,
        );
        attached_session
            .set_projection(&attached_projection)
            .unwrap();
    }

    #[test]
    fn every_execution_mode_and_projection_binding_combination_is_enforced() {
        let catalog = empty_catalog();
        for (mode, binding, accepted) in [
            (ExecutionMode::Unattended, SessionBindingLevel::None, true),
            (
                ExecutionMode::Unattended,
                SessionBindingLevel::PromptAssisted,
                false,
            ),
            (
                ExecutionMode::Unattended,
                SessionBindingLevel::Authenticated,
                false,
            ),
            (ExecutionMode::Attached, SessionBindingLevel::None, false),
            (
                ExecutionMode::Attached,
                SessionBindingLevel::PromptAssisted,
                true,
            ),
            (
                ExecutionMode::Attached,
                SessionBindingLevel::Authenticated,
                true,
            ),
        ] {
            let mut initialize = initialize_params();
            initialize.execution_mode = mode;
            initialize.invoking_session_id = match mode {
                ExecutionMode::Unattended => InvokingSessionId::Null,
                ExecutionMode::Attached => InvokingSessionId::Id("session-1".to_owned()),
            };
            let projection = HostProjectionSetParams::complete_for_catalog(
                "projection-bindings",
                initialize.host.clone(),
                binding,
                &catalog,
            );
            let mut session = Session::new();
            session.initialize(&initialize).unwrap();
            let result = session.set_projection(&projection);
            if accepted {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().code, WireErrorCode::ProjectionMismatch);
            }
        }
    }

    #[test]
    fn host_projection_conformance_fixture_matches_runtime_digest_and_catalog() {
        let report: Value =
            serde_json::from_str(include_str!("../../../docs/conformance/host-0.1.json")).unwrap();
        assert_eq!(
            report["josh_protocol"]["lifecycle"],
            serde_json::json!([
                "initialize",
                "host/project",
                "catalog/set",
                "program/load",
                "execution/start"
            ])
        );
        assert_eq!(
            report["josh_protocol"]["program_load_required_field"],
            "required_tools"
        );
        let fixture = &report["josh_protocol"]["host_projection"];
        let projection: HostProjectionSetParams =
            serde_json::from_value(fixture["request"].clone()).unwrap();
        let catalog: CatalogSetParams = serde_json::from_value(fixture["catalog"].clone()).unwrap();
        let expected_digest = fixture["expected_projection_digest"].as_str().unwrap();

        let mut initialize = initialize_params();
        initialize.host = projection.host.clone();
        initialize.execution_mode = ExecutionMode::Attached;
        initialize.invoking_session_id = InvokingSessionId::Id("session-1".to_owned());
        let mut session = Session::new();
        session.initialize(&initialize).unwrap();
        let projected = session.set_projection(&projection).unwrap();
        assert_eq!(projected.projection_digest, expected_digest);
        assert_eq!(session.set_catalog(&catalog).unwrap().tool_count, 0);
    }

    #[test]
    fn program_load_reports_exact_sorted_verified_tools_for_source_and_bytecode() {
        let catalog: CatalogSetParams = serde_json::from_value(serde_json::json!({
            "schema_dialect": josh_protocol::SCHEMA_DIALECT,
            "metadata": {
                "source": "test-host",
                "source_revision": "tools-1",
                "observed_at_unix_ms": 1,
                "freshness": "current",
                "complete": true
            },
            "tools": [
                {
                    "name": "alpha.echo",
                    "version": "1.0.0",
                    "description": "Alpha echo.",
                    "input_schema": {"type": "string"},
                    "output_schema": {"type": "string"},
                    "error_schema": {"type": "string"},
                    "effects": [],
                    "idempotency": "idempotent"
                },
                {
                    "name": "zeta.echo",
                    "version": "1.0.0",
                    "description": "Zeta echo.",
                    "input_schema": {"type": "string"},
                    "output_schema": {"type": "string"},
                    "error_schema": {"type": "string"},
                    "effects": [],
                    "idempotency": "idempotent"
                }
            ]
        }))
        .unwrap();
        let initialize = initialize_params();
        let mut session = Session::new();
        session.initialize(&initialize).unwrap();
        session
            .set_projection(&HostProjectionSetParams::complete_for_catalog(
                "projection-tools",
                initialize.host,
                SessionBindingLevel::None,
                &catalog,
            ))
            .unwrap();
        session.set_catalog(&catalog).unwrap();

        let source = r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
  tools: { required: [
    { name: "zeta.echo", version: ">=1.0.0, <2.0.0" },
    { name: "alpha.echo", version: ">=1.0.0, <2.0.0" }
  ] }
}
export fn main() returns Void { () }
"#;
        let expected = vec!["alpha.echo".to_owned(), "zeta.echo".to_owned()];
        let source_result = session
            .load_program(&ProgramLoadParams::SourceBundle {
                files: vec![josh_protocol::SourceFile {
                    path: "src/main.allen".to_owned(),
                    encoding: josh_protocol::FileEncoding::Utf8,
                    content: source.to_owned(),
                }],
            })
            .unwrap();
        assert_eq!(source_result.required_tools, expected);

        let compiled = assemble_inline_source(
            source,
            session
                .catalog
                .as_ref()
                .expect("test session has a frozen catalog"),
        )
        .unwrap();
        let bytecode_result = session
            .load_program(&ProgramLoadParams::Bytecode {
                artifact: base64::engine::general_purpose::STANDARD
                    .encode(encode(&compiled.artifact).unwrap()),
            })
            .unwrap();
        assert_eq!(bytecode_result.required_tools, expected);
    }

    #[test]
    fn catalog_must_match_the_frozen_tools_projection() {
        let catalog = empty_catalog();
        let mut session = Session::new();
        session.initialize(&initialize_params()).unwrap();
        set_projection(&mut session, SessionBindingLevel::None, &catalog);

        let mut wrong_source = catalog.clone();
        wrong_source.metadata.source = "other-source".to_owned();
        assert_eq!(
            session.set_catalog(&wrong_source).unwrap_err().code,
            WireErrorCode::ProjectionMismatch
        );
        let mut wrong_revision = catalog.clone();
        wrong_revision.metadata.source_revision = "2".to_owned();
        assert_eq!(
            session.set_catalog(&wrong_revision).unwrap_err().code,
            WireErrorCode::ProjectionMismatch
        );
        let mut wrong_observation = catalog.clone();
        wrong_observation.metadata.observed_at_unix_ms = 2;
        assert_eq!(
            session.set_catalog(&wrong_observation).unwrap_err().code,
            WireErrorCode::ProjectionMismatch
        );
        let mut wrong_freshness = catalog.clone();
        wrong_freshness.metadata.freshness = josh_protocol::CatalogFreshness::Cached;
        assert_eq!(
            session.set_catalog(&wrong_freshness).unwrap_err().code,
            WireErrorCode::ProjectionMismatch
        );
        let mut incomplete = catalog.clone();
        incomplete.metadata.complete = false;
        assert_eq!(
            session.set_catalog(&incomplete).unwrap_err().code,
            WireErrorCode::CatalogInvalid
        );
        assert_eq!(session.set_catalog(&catalog).unwrap().tool_count, 0);

        let mut count_session = Session::new();
        count_session.initialize(&initialize_params()).unwrap();
        let mut wrong_count = HostProjectionSetParams::complete_for_catalog(
            "projection-count",
            initialize_params().host,
            SessionBindingLevel::None,
            &catalog,
        );
        wrong_count.sections[ProjectionSectionKind::Tools as usize].item_count = 1;
        count_session.set_projection(&wrong_count).unwrap();
        assert_eq!(
            count_session.set_catalog(&catalog).unwrap_err().code,
            WireErrorCode::ProjectionMismatch
        );
    }

    #[test]
    fn incomplete_catalog_is_rejected_without_freezing_partial_state() {
        let mut session = Session::new();
        session.initialize(&initialize_params()).unwrap();
        let complete = empty_catalog();
        set_projection(&mut session, SessionBindingLevel::None, &complete);
        let mut incomplete = empty_catalog();
        incomplete.metadata.complete = false;
        let error = session.set_catalog(&incomplete).unwrap_err();
        assert_eq!(error.code, WireErrorCode::CatalogInvalid);
        assert_eq!(error.message, "tool catalog is incomplete");
        assert_eq!(session.set_catalog(&complete).unwrap().tool_count, 0);
    }

    #[test]
    fn attached_initialization_freezes_the_invoking_session_for_execution() {
        let mut requested = initialize_params();
        requested.execution_mode = ExecutionMode::Attached;
        requested.invoking_session_id = InvokingSessionId::Id("session-10".to_owned());
        let mut session = Session::new();
        let initialized = session.initialize(&requested).unwrap();
        assert_eq!(
            initialized.protocol_version,
            josh_protocol::PROTOCOL_VERSION
        );
        assert_eq!(initialized.features, josh_protocol::FEATURES);
        let catalog = empty_catalog();
        set_projection(&mut session, SessionBindingLevel::PromptAssisted, &catalog);
        session.set_catalog(&catalog).unwrap();
        let loaded = session
            .load_program(&ProgramLoadParams::Bytecode {
                artifact: base64::engine::general_purpose::STANDARD.encode(artifact_bytes()),
            })
            .unwrap();
        let prepared = session
            .prepare_execution(
                "attached".to_owned(),
                ExecutionStartParams {
                    execution_id: "exec-attached".to_owned(),
                    program_id: loaded.program_id,
                    artifact_digest: loaded.artifact_digest,
                    entry: "main".to_owned(),
                    input: Value::Null,
                    working_directory: None,
                    granted_capabilities: Vec::new(),
                    granted_tools: Vec::new(),
                    allowed_http_origins: Vec::new(),
                    granted_exec: Vec::new(),
                    granted_exec_environment: Vec::new(),
                    limits: BTreeMap::new(),
                },
            )
            .unwrap();
        assert_eq!(prepared.invoking_session_id(), Some("session-10"));
    }

    #[test]
    fn every_wire_execution_limit_maps_to_host_policy() {
        let mut session = Session::new();
        session.initialize(&initialize_params()).unwrap();
        let catalog = empty_catalog();
        set_projection(&mut session, SessionBindingLevel::None, &catalog);
        session.set_catalog(&catalog).unwrap();
        let params = ExecutionStartParams {
            execution_id: "limits".to_owned(),
            program_id: "program".to_owned(),
            artifact_digest: format!("sha256:{}", "0".repeat(64)),
            entry: "main".to_owned(),
            input: Value::Null,
            working_directory: None,
            granted_capabilities: Vec::new(),
            granted_tools: Vec::new(),
            allowed_http_origins: Vec::new(),
            granted_exec: Vec::new(),
            granted_exec_environment: Vec::new(),
            limits: [
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
            ]
            .iter()
            .map(|name| ((*name).to_owned(), 7))
            .collect(),
        };
        let policy = host_policy(&params, session.catalog.as_ref().unwrap());
        assert_eq!(
            [
                policy.limits.instructions,
                policy.limits.allocation_bytes,
                policy.limits.maximum_allocation_bytes,
                policy.limits.cleanup_instructions,
                policy.effects,
                policy.workspace_limits.max_file_bytes,
                policy.workspace_limits.max_operations,
                policy.workspace_limits.max_read_bytes,
                policy.workspace_limits.max_write_bytes,
                policy.http_limits.max_compressed_bytes,
                policy.http_limits.max_decoded_bytes,
            ],
            [7; 11]
        );
        assert_eq!(policy.input_bytes, 7);
        assert_eq!(policy.output_bytes, 7);
        assert_eq!(policy.limits.call_depth, 7);
        assert_eq!(policy.limits.tasks, 7);
        assert_eq!(policy.limits.concurrent_effects, 7);
        assert_eq!(policy.workspace_limits.max_entries, 7);
        assert_eq!(policy.http_limits.max_requests, 7);
        assert_eq!(policy.http_limits.max_redirects, 7);
        assert_eq!(policy.http_limits.max_dns_candidates, 7);
        assert_eq!(policy.http_limits.max_response_headers, 7);
        assert_eq!(policy.http_limits.max_header_bytes, 7);
        assert_eq!(policy.http_limits.max_decompression_ratio, 7);
        assert_eq!(policy.limits.wall_time, Duration::from_millis(7));
        assert_eq!(policy.http_limits.connect_timeout, Duration::from_millis(7));
        assert_eq!(
            policy.http_limits.first_byte_timeout,
            Duration::from_millis(7)
        );
        assert_eq!(policy.http_limits.idle_timeout, Duration::from_millis(7));
        assert_eq!(policy.http_limits.total_timeout, Duration::from_millis(7));
        assert!(policy.strict_preflight_authority);
    }

    #[test]
    fn bytecode_load_and_execution_are_stable() {
        let (mut session, loaded) = loaded_session();
        assert_eq!(loaded.program_id, "program-1");
        let prepared = session
            .prepare_execution(
                "h-4".to_owned(),
                ExecutionStartParams {
                    execution_id: "exec-1".to_owned(),
                    program_id: loaded.program_id,
                    artifact_digest: loaded.artifact_digest,
                    entry: "main".to_owned(),
                    input: Value::Null,
                    working_directory: None,
                    granted_capabilities: Vec::new(),
                    granted_tools: Vec::new(),
                    allowed_http_origins: Vec::new(),
                    granted_exec: Vec::new(),
                    granted_exec_environment: Vec::new(),
                    limits: BTreeMap::from([("wall_ms".to_owned(), 1_000)]),
                },
            )
            .unwrap();
        assert_eq!(
            prepared.run(None),
            ExecutionResult::Completed {
                output: Value::Null
            }
        );
        session.finish_execution("h-4");
    }

    #[test]
    fn consumed_prepared_execution_reports_a_safe_registered_runtime_panic() {
        let (mut session, loaded) = loaded_session();
        let prepared = session
            .prepare_execution(
                "h-repeat".to_owned(),
                ExecutionStartParams {
                    execution_id: "exec-repeat".to_owned(),
                    program_id: loaded.program_id,
                    artifact_digest: loaded.artifact_digest,
                    entry: "main".to_owned(),
                    input: Value::Null,
                    working_directory: None,
                    granted_capabilities: Vec::new(),
                    granted_tools: Vec::new(),
                    allowed_http_origins: Vec::new(),
                    granted_exec: Vec::new(),
                    granted_exec_environment: Vec::new(),
                    limits: BTreeMap::new(),
                },
            )
            .unwrap();
        assert!(matches!(
            prepared.run(None),
            ExecutionResult::Completed { .. }
        ));
        assert_eq!(
            prepared.run(None),
            ExecutionResult::Failed {
                error: josh_protocol::ProtocolRuntimeError {
                    code: "runtime.panic".to_owned(),
                    message: "runtime invariant failed".to_owned(),
                    category: "runtime".to_owned(),
                    retryable: false,
                    span: None,
                    operation_id: None,
                    metadata: BTreeMap::new(),
                    causes: Vec::new(),
                }
            }
        );
    }

    #[test]
    fn rust_panics_cannot_escape_the_josh_execution_boundary() {
        let (mut session, loaded) = loaded_session();
        let prepared = session
            .prepare_execution(
                "h-panic".to_owned(),
                ExecutionStartParams {
                    execution_id: "exec-panic".to_owned(),
                    program_id: loaded.program_id,
                    artifact_digest: loaded.artifact_digest,
                    entry: "main".to_owned(),
                    input: Value::Null,
                    working_directory: None,
                    granted_capabilities: Vec::new(),
                    granted_tools: Vec::new(),
                    allowed_http_origins: Vec::new(),
                    granted_exec: Vec::new(),
                    granted_exec_environment: Vec::new(),
                    limits: BTreeMap::new(),
                },
            )
            .unwrap();
        let result =
            prepared.run_with_observer(&mut RuntimeProviders::default(), &mut PanickingObserver);

        let ExecutionResult::Failed { error } = result else {
            panic!("observer panic must become a JOSH runtime failure")
        };
        assert_eq!(error.code, "runtime.panic");
        assert_eq!(error.message, "runtime invariant failed");
        assert!(!error.message.contains("provider-secret"));
        assert!(error.message.len() <= 1_024);
    }

    #[test]
    fn every_protocol_runtime_error_emission_uses_a_registered_code() {
        let registry: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/conformance/errors-0.1.json"))
                .unwrap();
        let registered = registry["registry"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["code"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        let runtime_codes = [
            allen_runtime::RuntimeErrorCode::ArithmeticOverflow,
            allen_runtime::RuntimeErrorCode::DivisionByZero,
            allen_runtime::RuntimeErrorCode::IndexOutOfBounds,
            allen_runtime::RuntimeErrorCode::MapKeyNotFound,
            allen_runtime::RuntimeErrorCode::DuplicateMapKey,
            allen_runtime::RuntimeErrorCode::ResourceLimit,
            allen_runtime::RuntimeErrorCode::Cancelled,
            allen_runtime::RuntimeErrorCode::Timeout,
            allen_runtime::RuntimeErrorCode::Panic,
            allen_runtime::RuntimeErrorCode::ProtocolViolation,
            allen_runtime::RuntimeErrorCode::ReplayDiverged,
            allen_runtime::RuntimeErrorCode::ReplayRuntimeDiverged,
            allen_runtime::RuntimeErrorCode::EntryNotFound,
            allen_runtime::RuntimeErrorCode::CapabilityDenied,
            allen_runtime::RuntimeErrorCode::InputTooLarge,
            allen_runtime::RuntimeErrorCode::InvalidInput,
            allen_runtime::RuntimeErrorCode::ManifestInvalid,
            allen_runtime::RuntimeErrorCode::CatalogMismatch,
        ];
        for code in runtime_codes {
            assert!(
                registered.contains(code.as_str()),
                "unregistered JOSH runtime code {}",
                code.as_str()
            );
        }
        let panic = runtime_panic();
        assert!(registered.contains(panic.code.as_str()));
        assert!(panic.message.len() <= 1_024);
        assert!(!panic.message.contains("provider-secret"));
    }

    #[test]
    fn stopped_runtime_outcome_is_preserved() {
        let mut stopped = artifact();
        stopped.module.constants = vec![Constant::String("requested stop".to_owned())];
        stopped.module.functions[0].registers = vec![ValueType::Unit, ValueType::String];
        stopped.module.functions[0].code = vec![
            Instruction::Const {
                destination: 1,
                constant: 0,
            },
            Instruction::Stop { reason: 1 },
        ];
        let mut session = Session::new();
        session.initialize(&initialize_params()).unwrap();
        let catalog = empty_catalog();
        set_projection(&mut session, SessionBindingLevel::None, &catalog);
        session.set_catalog(&catalog).unwrap();
        let loaded = session
            .load_program(&ProgramLoadParams::Bytecode {
                artifact: base64::engine::general_purpose::STANDARD
                    .encode(encode(&stopped).unwrap()),
            })
            .unwrap();
        let prepared = session
            .prepare_execution(
                "stop".to_owned(),
                ExecutionStartParams {
                    execution_id: "exec-stop".to_owned(),
                    program_id: loaded.program_id,
                    artifact_digest: loaded.artifact_digest,
                    entry: "main".to_owned(),
                    input: Value::Null,
                    working_directory: None,
                    granted_capabilities: Vec::new(),
                    granted_tools: Vec::new(),
                    allowed_http_origins: Vec::new(),
                    granted_exec: Vec::new(),
                    granted_exec_environment: Vec::new(),
                    limits: BTreeMap::from([("wall_ms".to_owned(), 1_000)]),
                },
            )
            .unwrap();
        assert_eq!(
            prepared.run(None),
            ExecutionResult::Stopped {
                reason: Some("requested stop".to_owned())
            }
        );
    }

    #[test]
    fn failed_load_and_start_allocate_no_reusable_identity() {
        let (mut session, loaded) = loaded_session();
        let invalid = session.load_program(&ProgramLoadParams::Bytecode {
            artifact: base64::engine::general_purpose::STANDARD.encode(b"not-bytecode"),
        });
        assert_eq!(invalid.unwrap_err().code, WireErrorCode::ProgramInvalid);

        let params = ExecutionStartParams {
            execution_id: "exec-1".to_owned(),
            program_id: loaded.program_id,
            artifact_digest: format!("sha256:{}", "0".repeat(64)),
            entry: "main".to_owned(),
            input: Value::Null,
            working_directory: None,
            granted_capabilities: Vec::new(),
            granted_tools: Vec::new(),
            allowed_http_origins: Vec::new(),
            granted_exec: Vec::new(),
            granted_exec_environment: Vec::new(),
            limits: BTreeMap::new(),
        };
        assert_eq!(
            session
                .prepare_execution("bad".to_owned(), params)
                .unwrap_err()
                .code,
            WireErrorCode::CatalogMismatch
        );
    }

    #[test]
    fn another_program_load_completes_while_execution_is_active() {
        let (mut session, loaded) = loaded_session();
        let _prepared = session
            .prepare_execution(
                "run".to_owned(),
                ExecutionStartParams {
                    execution_id: "exec-1".to_owned(),
                    program_id: loaded.program_id,
                    artifact_digest: loaded.artifact_digest,
                    entry: "main".to_owned(),
                    input: Value::Null,
                    working_directory: None,
                    granted_capabilities: Vec::new(),
                    granted_tools: Vec::new(),
                    allowed_http_origins: Vec::new(),
                    granted_exec: Vec::new(),
                    granted_exec_environment: Vec::new(),
                    limits: BTreeMap::new(),
                },
            )
            .unwrap();
        let second = session
            .load_program(&ProgramLoadParams::Bytecode {
                artifact: base64::engine::general_purpose::STANDARD.encode(artifact_bytes()),
            })
            .unwrap();
        assert_eq!(second.program_id, "program-2");
    }
}
