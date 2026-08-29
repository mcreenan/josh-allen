use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use allen_exec::{
    Deadline, EnvironmentSnapshot, ExecError, ExecErrorKind, ExecutableIdentity, ExecutionLimits,
    ExecutionRequest, PrivateInput, ProcessBroker,
};
use allen_schema::{
    CatalogLimits, Descriptor, FrozenCatalog, Idempotency as SchemaIdempotency, SchemaLimits,
    ToolDefinition, ToolName,
};
use josh_protocol::{
    CatalogSetParams, CatalogSetResult, Idempotency, ToolInvokeParams, ToolInvokeResult, Validate,
    WireError, WireErrorCode,
};
use serde_json::{Map, Value, json};

const MAX_EXECUTOR_INPUT_BYTES: usize = 1024 * 1024;
const MAX_EXECUTOR_STDOUT_BYTES: usize = 1024 * 1024;
const MAX_EXECUTOR_STDERR_BYTES: usize = 64 * 1024;
const MIN_DEADLINE_RESPONSE_MARGIN: Duration = Duration::from_millis(10);
const MAX_DEADLINE_RESPONSE_MARGIN: Duration = Duration::from_millis(250);
const ERROR_SCHEMA: &str = r#"{"additionalProperties":false,"properties":{"code":{"maxLength":128,"minLength":1,"type":"string"},"message":{"maxLength":2048,"minLength":1,"type":"string"}},"required":["code","message"],"type":"object"}"#;

pub(crate) struct ExecutorProvider {
    broker: ProcessBroker,
    executable: Option<ExecutableIdentity>,
    catalog_digest: String,
    contracts: BTreeMap<String, ToolContract>,
    granted_tools: BTreeSet<String>,
}

struct ToolContract {
    version: String,
    input_schema: String,
    output_schema: String,
    error_schema: String,
}

impl ExecutorProvider {
    pub(crate) fn preflight(
        catalog: &CatalogSetParams,
        frozen: &CatalogSetResult,
        granted_tools: &[String],
    ) -> Result<Self, String> {
        let schema_limits = SchemaLimits::default();
        let expected_error = allen_schema::ToolSchema::parse(ERROR_SCHEMA, &schema_limits)
            .map_err(|_| "cannot initialize the executor error contract".to_owned())?;
        let mut grants = BTreeSet::new();
        for grant in granted_tools {
            let name = ToolName::parse(grant)
                .map_err(|_| format!("--grant-tool '{grant}' is not a canonical tool name"))?;
            grants.insert(name.as_str().to_owned());
        }
        let mut definitions = Vec::with_capacity(catalog.tools.len());
        let mut contracts = BTreeMap::new();

        for tool in &catalog.tools {
            let definition = ToolDefinition::parse(
                &tool.name,
                &tool.version,
                &serde_json::to_string(&tool.input_schema)
                    .map_err(|_| "catalog contains an invalid executor input schema".to_owned())?,
                &serde_json::to_string(&tool.output_schema)
                    .map_err(|_| "catalog contains an invalid executor output schema".to_owned())?,
                &serde_json::to_string(&tool.error_schema)
                    .map_err(|_| "catalog contains an invalid executor error schema".to_owned())?,
                tool.effects.clone(),
                schema_idempotency(tool.idempotency),
                &schema_limits,
            )
            .map_err(|_| format!("catalog tool '{}' has an invalid contract", tool.name))?;
            if grants.contains(&tool.name)
                && !matches!(
                    definition.input_schema.descriptor(),
                    Descriptor::Record { .. }
                )
            {
                return Err(format!(
                    "catalog tool '{}' must have an object input schema for --executor",
                    tool.name
                ));
            }
            if grants.contains(&tool.name)
                && definition.error_schema.digest() != expected_error.digest()
            {
                return Err(format!(
                    "catalog tool '{}' must use the fixed executor error schema",
                    tool.name
                ));
            }
            contracts.insert(
                tool.name.clone(),
                ToolContract {
                    version: tool.version.clone(),
                    input_schema: definition.input_schema.digest().to_owned(),
                    output_schema: definition.output_schema.digest().to_owned(),
                    error_schema: definition.error_schema.digest().to_owned(),
                },
            );
            definitions.push(definition);
        }

        let local_frozen = FrozenCatalog::freeze(definitions, &CatalogLimits::default())
            .map_err(|_| "catalog cannot be frozen for the executor provider".to_owned())?;
        if local_frozen.digest() != frozen.catalog_digest {
            return Err(
                "executor catalog digest does not match the JOSH frozen catalog".to_owned(),
            );
        }

        for grant in &grants {
            if !contracts.contains_key(grant) {
                return Err(format!(
                    "--grant-tool '{grant}' is not present in the frozen catalog"
                ));
            }
        }

        let broker = ProcessBroker::new(EnvironmentSnapshot::capture());
        let executable = if grants.is_empty() {
            None
        } else {
            Some(resolve_executor(&broker)?)
        };
        Ok(Self {
            broker,
            executable,
            catalog_digest: frozen.catalog_digest.clone(),
            contracts,
            granted_tools: grants,
        })
    }

    pub(crate) fn invoke(&self, params: &ToolInvokeParams) -> Result<ToolInvokeResult, WireError> {
        // The wire value is remaining time measured before transport. Reserve
        // a small response margin so the runtime can receive the failure.
        let requested_budget = Duration::from_millis(params.deadline_ms);
        let response_margin = (requested_budget / 5)
            .clamp(MIN_DEADLINE_RESPONSE_MARGIN, MAX_DEADLINE_RESPONSE_MARGIN);
        let attempt_budget = requested_budget.saturating_sub(response_margin);
        let deadline = Deadline::from_budget(attempt_budget)
            .map_err(|_| unavailable("executor deadline is invalid"))?;
        params
            .validate()
            .map_err(|_| protocol_error("executor received invalid tool parameters"))?;
        if params.execution_id != "exec-1" {
            return Err(protocol_error(
                "executor request has an unexpected execution identifier",
            ));
        }
        if !self.granted_tools.contains(&params.tool) {
            return Err(wire_error(
                WireErrorCode::ToolDenied,
                "tool is not granted to the executor provider",
            ));
        }
        let contract = self.contracts.get(&params.tool).ok_or_else(|| {
            protocol_error("executor request tool is absent from the frozen catalog")
        })?;
        if params.catalog_digest != self.catalog_digest
            || params.tool_version != contract.version
            || params.input_schema != contract.input_schema
            || params.output_schema != contract.output_schema
            || params.error_schema != contract.error_schema
        {
            return Err(protocol_error(
                "executor request does not match the frozen tool contract",
            ));
        }

        let input = serde_json::to_vec(&params.input)
            .map_err(|_| unavailable("executor input could not be encoded"))?;
        if input.len() > MAX_EXECUTOR_INPUT_BYTES {
            return Err(unavailable("executor input exceeds the provider limit"));
        }
        deadline.ensure_remaining().map_err(map_exec_error)?;
        let input_file = PrivateInput::create(&input)
            .map_err(|_| unavailable("executor input file could not be created"))?;
        let execution = (|| {
            deadline.ensure_remaining().map_err(map_exec_error)?;
            let executable = self
                .executable
                .as_ref()
                .ok_or_else(|| unavailable("executor is unavailable"))?;
            self.broker
                .run(
                    executable,
                    ExecutionRequest {
                        arguments: vec![
                            OsString::from("call"),
                            OsString::from(&params.tool),
                            input_argument(input_file.path()),
                        ],
                        stdin: Vec::new(),
                        limits: ExecutionLimits {
                            stdin_bytes: 0,
                            stdout_bytes: MAX_EXECUTOR_STDOUT_BYTES,
                            stderr_bytes: MAX_EXECUTOR_STDERR_BYTES,
                        },
                        deadline,
                    },
                )
                .map_err(map_exec_error)
        })();
        input_file
            .cleanup()
            .map_err(|_| unavailable("executor input cleanup failed"))?;
        let completed = execution?;
        if !completed.status.success() {
            return Err(unavailable("executor call failed"));
        }
        parse_executor_result(&completed.stdout)
    }
}

fn schema_idempotency(value: Idempotency) -> SchemaIdempotency {
    match value {
        Idempotency::Unknown => SchemaIdempotency::Unknown,
        Idempotency::Idempotent => SchemaIdempotency::Idempotent,
        Idempotency::NonIdempotent => SchemaIdempotency::NonIdempotent,
    }
}

#[cfg(target_os = "linux")]
fn resolve_executor(broker: &ProcessBroker) -> Result<ExecutableIdentity, String> {
    broker
        .resolve("executor")
        .map_err(|_| "--executor requires an executable named 'executor' on PATH".to_owned())
}

#[cfg(not(target_os = "linux"))]
fn resolve_executor(_broker: &ProcessBroker) -> Result<ExecutableIdentity, String> {
    Err("--executor tool grants are unsupported on this platform".to_owned())
}

fn map_exec_error(error: ExecError) -> WireError {
    match error.kind() {
        ExecErrorKind::ExecutablePreparationFailed | ExecErrorKind::SpawnFailed => {
            unavailable("executor could not be started")
        }
        ExecErrorKind::InvalidDeadline => unavailable("executor deadline is invalid"),
        ExecErrorKind::InputLimitExceeded => {
            unavailable("executor input exceeds the provider limit")
        }
        ExecErrorKind::StdoutLimitExceeded | ExecErrorKind::StderrLimitExceeded => {
            unavailable("executor output exceeds the provider limit")
        }
        ExecErrorKind::TimedOut => unavailable("executor call timed out"),
        ExecErrorKind::OutputReadFailed => unavailable("executor output could not be read"),
        ExecErrorKind::TerminationFailed => unavailable("executor termination failed"),
        ExecErrorKind::UnsupportedPlatform | ExecErrorKind::ExecutableNotFound => {
            unavailable("executor is unavailable")
        }
    }
}

fn input_argument(path: &Path) -> OsString {
    let mut argument = OsString::from("@");
    argument.push(path);
    argument
}

fn parse_executor_result(bytes: &[u8]) -> Result<ToolInvokeResult, WireError> {
    let value = josh_protocol::decode_value(bytes)
        .map_err(|_| unavailable("executor returned an invalid result"))?;
    let object = value
        .as_object()
        .ok_or_else(|| unavailable("executor returned an invalid result"))?;
    match object.get("ok").and_then(Value::as_bool) {
        Some(true) if exact_keys(object, &["data", "ok"]) => Ok(ToolInvokeResult::Ok {
            value: object["data"].clone(),
        }),
        Some(false) if exact_keys(object, &["error", "ok"]) => {
            let error = object["error"]
                .as_object()
                .filter(|error| exact_keys(error, &["code", "message"]))
                .ok_or_else(|| unavailable("executor returned an invalid result"))?;
            let code = bounded_string(error.get("code"), 128)
                .ok_or_else(|| unavailable("executor returned an invalid result"))?;
            let message = bounded_string(error.get("message"), 2048)
                .ok_or_else(|| unavailable("executor returned an invalid result"))?;
            Ok(ToolInvokeResult::Error {
                error: json!({"code": code, "message": message}),
            })
        }
        _ => Err(unavailable("executor returned an invalid result")),
    }
}

fn exact_keys(object: &Map<String, Value>, expected: &[&str]) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn bounded_string(value: Option<&Value>, maximum: usize) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.chars().count() <= maximum)
}

fn unavailable(message: &'static str) -> WireError {
    wire_error(WireErrorCode::ToolUnavailable, message)
}

fn protocol_error(message: &'static str) -> WireError {
    wire_error(WireErrorCode::ProtocolViolation, message)
}

fn wire_error(code: WireErrorCode, message: &'static str) -> WireError {
    WireError {
        code,
        message: message.to_owned(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_results_reject_duplicate_keys_at_every_depth() {
        for invalid in [
            br#"{"ok":true,"ok":true,"data":{"text":"value"}}"#.as_slice(),
            br#"{"ok":true,"data":{"text":"first","text":"second"}}"#.as_slice(),
            br#"{"ok":false,"error":{"code":"one","code":"two","message":"bad"}}"#.as_slice(),
        ] {
            assert_eq!(
                parse_executor_result(invalid).unwrap_err().code,
                WireErrorCode::ToolUnavailable
            );
        }
    }
}
