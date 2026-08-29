#![forbid(unsafe_code)]

use allen_bytecode::{Artifact, DecodeLimits, VerifiedArtifact, decode_and_verify, encode};
use allen_compiler::{
    Diagnostic, PackageSourceBundle, SourceTest, assemble_loaded_source_test,
    assemble_loose_compilation, assemble_source_test, compile_bundle_with_prepared_source,
    compile_prepared_inline_manifest_source, compile_source_test, discover_source_tests,
    prepare_loaded_source_tests, prepare_source, render_diagnostic,
};
use allen_package::{LoadLimits, LoadedPackage, canonical_https_origin, generate_lock};
use allen_runtime::{
    HostPolicy, RawLaunchRequest, RuntimeProviders, execute_prepared_with_context, launch_raw,
    launch_raw_with_context, prepare_raw_launch,
};
use allen_testkit::{
    NoToolSchemas, ReplayError, ReplayLimits, ReplayLog, ReplayingEffectProvider, ToolResultSchema,
};
use allen_vm::{
    CancellationSource, Checkpoint, CheckpointObserver, ExecutionLimits, ExecutionOutcome,
    TaskEvent, execute_verified_artifact_outcome_with_limits,
    execute_verified_artifact_outcome_with_observer,
};
use base64::Engine as _;
use josh_protocol::{CatalogSetParams, Validate as _};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command as ProcessCommand, ExitCode, Stdio};

#[derive(Clone, Copy)]
enum Command {
    Check,
    Run,
    Build,
    Inspect,
    Lock,
    Test,
}

#[derive(Default)]
struct RunOptions {
    trace_tasks: bool,
    entry: Option<String>,
    input: Option<String>,
    workdir: Option<String>,
    allowed_net_origins: Vec<String>,
    granted_exec: Vec<String>,
    granted_exec_environment: Vec<String>,
}

const WORKER_PROTOCOL: &str = "allen-cli-worker/1";
const MAX_WORKER_REQUEST_BYTES: usize = 25 * 1024 * 1024;
const MAX_WORKER_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_WORKER_ARTIFACT_TEXT_BYTES: usize = 23 * 1024 * 1024;
const MAX_WORKER_INPUT_BYTES: usize = 1024 * 1024;
const MAX_WORKER_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_WORKER_STOP_REASON_BYTES: usize = 1024;
const MAX_WORKER_ERROR_CODE_BYTES: usize = 128;
const MAX_WORKER_ERROR_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_WORKER_TRACE_EVENTS: usize = 4_096;
const MAX_WORKER_TRACE_LINE_BYTES: usize = 512;
const WORKER_CPU_SECONDS: u64 = 60;
const WORKER_ADDRESS_SPACE_BYTES: u64 = 1024 * 1024 * 1024;
const WORKER_FILE_SIZE_BYTES: u64 = 16 * 1024 * 1024;

/// The closed, bounded parent-to-worker request. The child independently
/// decodes and verifies the artifact before it executes it.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkerRequest {
    protocol: String,
    artifact: String,
    entry: String,
    input_bytes: String,
    workspace_root: Option<String>,
    allowed_http_origins: Vec<String>,
    granted_exec: Vec<String>,
    granted_exec_environment: Vec<String>,
    trace_tasks: bool,
    source_style_errors: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkerResourceLimits {
    cpu: bool,
    address_space: bool,
    file_size: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerResponse {
    Completed {
        output: String,
        trace: Vec<String>,
        resource_limits: WorkerResourceLimits,
    },
    Stopped {
        reason: Option<String>,
        trace: Vec<String>,
        resource_limits: WorkerResourceLimits,
    },
    RuntimeError {
        code: String,
        message: String,
        trace: Vec<String>,
        resource_limits: WorkerResourceLimits,
    },
}

impl WorkerResponse {
    fn trace(&self) -> &[String] {
        match self {
            Self::Completed { trace, .. }
            | Self::Stopped { trace, .. }
            | Self::RuntimeError { trace, .. } => trace,
        }
    }

    fn resource_limits(&self) -> &WorkerResourceLimits {
        match self {
            Self::Completed {
                resource_limits, ..
            }
            | Self::Stopped {
                resource_limits, ..
            }
            | Self::RuntimeError {
                resource_limits, ..
            } => resource_limits,
        }
    }

    fn validate(&self) -> Result<(), String> {
        let trace = self.trace();
        if trace.len() > MAX_WORKER_TRACE_EVENTS
            || trace
                .iter()
                .any(|line| line.len() > MAX_WORKER_TRACE_LINE_BYTES)
        {
            return Err("worker response trace is invalid".to_owned());
        }
        let resource_limits = self.resource_limits();
        if !resource_limits.cpu || !resource_limits.file_size {
            return Err("worker response resource limits are invalid".to_owned());
        }
        match self {
            Self::Completed { output, .. } => {
                if output.len() > MAX_WORKER_OUTPUT_BYTES {
                    return Err("worker response output exceeds its limit".to_owned());
                }
            }
            Self::Stopped { reason, .. } => {
                if reason
                    .as_ref()
                    .is_some_and(|reason| reason.len() > MAX_WORKER_STOP_REASON_BYTES)
                {
                    return Err("worker response stop reason is invalid".to_owned());
                }
            }
            Self::RuntimeError { code, message, .. } => {
                if code.is_empty()
                    || code.len() > MAX_WORKER_ERROR_CODE_BYTES
                    || message.len() > MAX_WORKER_ERROR_MESSAGE_BYTES
                {
                    return Err("worker response runtime error is invalid".to_owned());
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn main() -> ExitCode {
    if env::args().nth(1).as_deref() == Some("--internal-worker") {
        return internal_worker();
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<(), ExitCode> {
    let mut arguments = env::args().peekable();
    let program = arguments.next().unwrap_or_else(|| "allen".to_owned());
    let command = match arguments.next().as_deref() {
        Some("-h" | "--help") => {
            println!("{}", usage_text(&program));
            return Ok(());
        }
        Some("check") => Command::Check,
        Some("run") => Command::Run,
        Some("build") => Command::Build,
        Some("inspect") => Command::Inspect,
        Some("lock") => Command::Lock,
        Some("test") => Command::Test,
        _ => return usage(&program),
    };
    let remaining = arguments.collect::<Vec<_>>();
    if matches!(command, Command::Test) {
        return run_source_tests(&program, &remaining);
    }
    let (show_effects, run_options, path, output) = match command {
        Command::Check => match remaining.as_slice() {
            [path] => (false, RunOptions::default(), path.clone(), None),
            [flag, path] if flag == "--show-effects" => {
                (true, RunOptions::default(), path.clone(), None)
            }
            _ => return usage(&program),
        },
        Command::Run => {
            let (options, path) = parse_run_arguments(&program, &remaining)?;
            (false, options, path, None)
        }
        Command::Build => match remaining.as_slice() {
            [path, flag, output] if flag == "-o" => (
                false,
                RunOptions::default(),
                path.clone(),
                Some(output.clone()),
            ),
            _ => return usage(&program),
        },
        Command::Inspect | Command::Lock => match remaining.as_slice() {
            [path] => (false, RunOptions::default(), path.clone(), None),
            _ => return usage(&program),
        },
        Command::Test => unreachable!("test command returned above"),
    };

    match command {
        Command::Check => {
            if is_artifact_path(Path::new(&path)) {
                eprintln!("{path}: error: check requires an ALLEN source file");
                return Err(ExitCode::from(2));
            }
            let artifact = compile_input_artifact(&path, show_effects)?;
            let bytes = encode(&artifact).map_err(|error| report_artifact_error(&error))?;
            decode_and_verify(&bytes, &DecodeLimits::default())
                .map_err(|error| report_artifact_error(&error))?;
        }
        Command::Build => {
            let Some(output) = output else {
                return usage(&program);
            };
            let artifact = compile_input_artifact(&path, false)?;
            let bytes = encode(&artifact).map_err(|error| report_artifact_error(&error))?;
            decode_and_verify(&bytes, &DecodeLimits::default())
                .map_err(|error| report_artifact_error(&error))?;
            fs::write(&output, bytes).map_err(|error| {
                eprintln!("{output}: error: cannot write artifact: {error}");
                ExitCode::from(1)
            })?;
        }
        Command::Run => {
            let loaded_artifact = is_artifact_path(Path::new(&path));
            let (verified, worker_artifact) = if loaded_artifact {
                let bytes = read_artifact(&path)?;
                let verified = decode_and_verify(&bytes, &DecodeLimits::default())
                    .map_err(|error| report_artifact_error(&error))?;
                (verified, bytes)
            } else {
                let artifact = compile_input_artifact(&path, false)?;
                let bytes = encode(&artifact).map_err(|error| report_artifact_error(&error))?;
                let verified = decode_and_verify(&bytes, &DecodeLimits::default())
                    .map_err(|error| report_artifact_error(&error))?;
                (verified, bytes)
            };
            if uses_contract_layout(&verified) {
                let entry = run_options.entry.as_deref().unwrap_or("main").to_owned();
                if entry_uses_filesystem(&verified, &entry) && run_options.workdir.is_none() {
                    eprintln!("{path}: error: filesystem entries require --workdir <directory>");
                    return Err(ExitCode::from(2));
                }
                let input = read_json_input(
                    run_options.input.as_deref(),
                    entry_input_limit(&verified, 1024 * 1024),
                )?;
                run_in_worker(&WorkerRequest {
                    protocol: WORKER_PROTOCOL.to_owned(),
                    artifact: artifact_text(&worker_artifact),
                    entry,
                    input_bytes: base64::engine::general_purpose::STANDARD.encode(input),
                    workspace_root: run_options.workdir,
                    allowed_http_origins: run_options.allowed_net_origins,
                    granted_exec: run_options.granted_exec,
                    granted_exec_environment: run_options.granted_exec_environment,
                    trace_tasks: run_options.trace_tasks,
                    source_style_errors: !loaded_artifact,
                })?;
                return Ok(());
            }
            if run_options.entry.is_some()
                || run_options.input.is_some()
                || run_options.workdir.is_some()
                || !run_options.allowed_net_origins.is_empty()
                || !run_options.granted_exec.is_empty()
                || !run_options.granted_exec_environment.is_empty()
            {
                eprintln!(
                    "{path}: error: entry input and work directories require a manifest contract"
                );
                return Err(ExitCode::from(2));
            }
            run_in_worker(&WorkerRequest {
                protocol: WORKER_PROTOCOL.to_owned(),
                artifact: artifact_text(&worker_artifact),
                entry: "main".to_owned(),
                input_bytes: base64::engine::general_purpose::STANDARD.encode(b"null"),
                workspace_root: None,
                allowed_http_origins: Vec::new(),
                granted_exec: Vec::new(),
                granted_exec_environment: Vec::new(),
                trace_tasks: run_options.trace_tasks,
                source_style_errors: !loaded_artifact,
            })?;
        }
        Command::Inspect => {
            inspect_artifact(&path)?;
        }
        Command::Lock => {
            let root = Path::new(&path);
            let lock = generate_lock(root, &LoadLimits::default()).map_err(|error| {
                eprintln!("{error}");
                ExitCode::from(1)
            })?;
            let lock_path = root.join("allen.lock");
            fs::write(&lock_path, lock.as_bytes()).map_err(|error| {
                eprintln!(
                    "{}: error: cannot write lockfile: {error}",
                    lock_path.display()
                );
                ExitCode::from(1)
            })?;
        }
        Command::Test => unreachable!("test command returned above"),
    }
    Ok(())
}

fn artifact_text(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn uses_contract_layout(artifact: &VerifiedArtifact) -> bool {
    artifact.manifest().is_some()
}

fn run_in_worker(request: &WorkerRequest) -> Result<(), ExitCode> {
    let executable =
        env::current_exe().map_err(|_| worker_failure("cannot locate worker executable"))?;
    let mut child = ProcessCommand::new(executable)
        .arg("--internal-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| worker_failure("cannot start worker process"))?;
    let Some(mut stdin) = child.stdin.take() else {
        return Err(worker_failure("worker input is unavailable"));
    };
    if write_worker_message(&mut stdin, request, MAX_WORKER_REQUEST_BYTES).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(worker_failure("cannot send worker request"));
    }
    drop(stdin);

    let output = child
        .wait_with_output()
        .map_err(|_| worker_failure("cannot wait for worker process"))?;
    if !output.status.success() {
        return Err(worker_failure("worker process failed"));
    }
    if !output.stderr.is_empty() {
        return Err(worker_failure(
            "worker process wrote unexpected diagnostics",
        ));
    }
    let response: WorkerResponse = read_worker_message(
        &mut std::io::Cursor::new(output.stdout),
        MAX_WORKER_RESPONSE_BYTES,
    )
    .map_err(|_| worker_failure("worker response is invalid"))?;
    response
        .validate()
        .map_err(|_| worker_failure("worker response is invalid"))?;
    for event in response.trace() {
        eprintln!("{event}");
    }
    match response {
        WorkerResponse::Completed { output, .. } => println!("{output}"),
        WorkerResponse::Stopped {
            reason: Some(reason),
            ..
        } => println!("stopped: {}", allen_vm::Value::String(reason.into())),
        WorkerResponse::Stopped { reason: None, .. } => println!("stopped"),
        WorkerResponse::RuntimeError { code, message, .. } => {
            if code == "program.failed" {
                eprintln!(
                    "runtime error[{code}]: {}",
                    escape_untrusted_terminal_text(&message)
                );
            } else {
                eprintln!("runtime error[{code}]: {message}");
            }
            return Err(ExitCode::from(1));
        }
    }
    Ok(())
}

fn worker_failure(message: &'static str) -> ExitCode {
    eprintln!("runtime error[runtime.panic]: {message}");
    ExitCode::from(1)
}

fn internal_worker() -> ExitCode {
    if env::args().count() != 2 {
        eprintln!("internal worker error: unexpected arguments");
        return ExitCode::from(2);
    }
    let resource_limits = match apply_worker_limits() {
        Ok(limits) => limits,
        Err(message) => {
            eprintln!("internal worker error: {message}");
            return ExitCode::from(1);
        }
    };
    let request =
        match read_worker_message::<WorkerRequest>(&mut std::io::stdin(), MAX_WORKER_REQUEST_BYTES)
        {
            Ok(request) => request,
            Err(message) => {
                eprintln!("internal worker error: {message}");
                return ExitCode::from(1);
            }
        };
    if let Err(message) = validate_worker_request(&request) {
        eprintln!("internal worker error: {message}");
        return ExitCode::from(1);
    }
    let response = match execute_worker_request(request, resource_limits) {
        Ok(response) => response,
        Err(message) => {
            eprintln!("internal worker error: {message}");
            return ExitCode::from(1);
        }
    };
    if let Err(message) = response.validate() {
        eprintln!("internal worker error: {message}");
        return ExitCode::from(1);
    }
    if let Err(message) =
        write_worker_message(&mut std::io::stdout(), &response, MAX_WORKER_RESPONSE_BYTES)
    {
        eprintln!("internal worker error: {message}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn validate_worker_request(request: &WorkerRequest) -> Result<(), String> {
    if request.protocol != WORKER_PROTOCOL
        || request.artifact.is_empty()
        || request.artifact.len() > MAX_WORKER_ARTIFACT_TEXT_BYTES
        || request.entry.is_empty()
        || request.entry.len() > 128
        || request
            .workspace_root
            .as_ref()
            .is_some_and(String::is_empty)
        || request.allowed_http_origins.len() > 256
        || request.granted_exec.len() > 256
        || request.granted_exec_environment.len() > 256
    {
        return Err("worker request is invalid".to_owned());
    }
    let input = base64::engine::general_purpose::STANDARD
        .decode(&request.input_bytes)
        .map_err(|_| "worker input is invalid")?;
    if base64::engine::general_purpose::STANDARD.encode(&input) != request.input_bytes {
        return Err("worker input is invalid".to_owned());
    }
    if input.len() > MAX_WORKER_INPUT_BYTES {
        return Err("worker input exceeds its limit".to_owned());
    }
    let mut previous = None;
    for origin in &request.allowed_http_origins {
        if canonical_https_origin(origin).map_err(|_| "worker network origin is invalid")?
            != *origin
            || previous.is_some_and(|value: &String| value >= origin)
        {
            return Err("worker network origins are invalid".to_owned());
        }
        previous = Some(origin);
    }
    let mut previous = None;
    for pattern in &request.granted_exec {
        if allen_exec::CommandPattern::parse(pattern).is_err()
            || previous.is_some_and(|value: &String| value >= pattern)
        {
            return Err("worker exec grants are invalid".to_owned());
        }
        previous = Some(pattern);
    }
    let mut previous = None;
    for name in &request.granted_exec_environment {
        let mut bytes = name.bytes();
        if !bytes.next().is_some_and(|first| {
            (first.is_ascii_alphabetic() || first == b'_')
                && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && !name.eq_ignore_ascii_case("LC_ALL")
                && !name.eq_ignore_ascii_case("TZ")
        }) || previous.is_some_and(|value: &String| value >= name)
        {
            return Err("worker exec environment grants are invalid".to_owned());
        }
        previous = Some(name);
    }
    Ok(())
}

fn bounded_stop_reason(reason: String) -> Option<String> {
    (reason.len() <= MAX_WORKER_STOP_REASON_BYTES).then_some(reason)
}

fn escape_untrusted_terminal_text(text: &str) -> String {
    text.chars().flat_map(char::escape_default).collect()
}

fn execute_worker_request(
    request: WorkerRequest,
    resource_limits: WorkerResourceLimits,
) -> Result<WorkerResponse, String> {
    let artifact = base64::engine::general_purpose::STANDARD
        .decode(request.artifact)
        .map_err(|_| "worker artifact is invalid")?;
    let verified = decode_and_verify(&artifact, &DecodeLimits::default())
        .map_err(|_| "worker artifact is invalid")?;
    let input = base64::engine::general_purpose::STANDARD
        .decode(request.input_bytes)
        .map_err(|_| "worker input is invalid")?;
    let mut trace = TraceCollector::default();
    if uses_contract_layout(&verified) {
        let mut granted_capabilities = std::collections::BTreeSet::new();
        if let Some(manifest) = verified.manifest() {
            granted_capabilities.extend(manifest.required_capabilities.iter().cloned());
            granted_capabilities.extend(manifest.optional_capabilities.iter().cloned());
        }
        let mut policy = HostPolicy {
            granted_capabilities,
            limits: cli_execution_limits(),
            input_bytes: 1024 * 1024,
            output_bytes: 1024 * 1024,
            workspace_root: request.workspace_root.map(Into::into),
            http_origins: request.allowed_http_origins.into_iter().collect(),
            granted_exec: request.granted_exec.into_iter().collect(),
            granted_exec_environment: request.granted_exec_environment.into_iter().collect(),
            ..HostPolicy::default()
        };
        policy.workspace_rights = workspace_rights(&verified, &request.entry);
        let launch_request = RawLaunchRequest {
            entry: request.entry,
            input,
        };
        let outcome = if request.trace_tasks {
            launch_raw_with_context(
                &verified,
                &launch_request,
                &policy,
                &mut RuntimeProviders::default(),
                &mut NeverCancelled,
                &mut trace,
            )
        } else {
            launch_raw(&verified, &launch_request, &policy)
        };
        return Ok(match outcome {
            Ok(outcome) => match outcome.execution {
                ExecutionOutcome::Completed(_) => WorkerResponse::Completed {
                    output: outcome.output.to_string(),
                    trace: trace.0,
                    resource_limits,
                },
                ExecutionOutcome::Stopped { reason, .. } => WorkerResponse::Stopped {
                    reason: bounded_stop_reason(reason),
                    trace: trace.0,
                    resource_limits,
                },
            },
            Err(error) => WorkerResponse::RuntimeError {
                code: error.code.as_str().to_owned(),
                message: error.to_string(),
                trace: trace.0,
                resource_limits,
            },
        });
    }
    let outcome = if request.trace_tasks {
        execute_verified_artifact_outcome_with_observer(
            &verified,
            cli_execution_limits(),
            &mut trace,
        )
    } else {
        execute_verified_artifact_outcome_with_limits(&verified, cli_execution_limits())
    };
    Ok(match outcome {
        Ok(ExecutionOutcome::Completed(result)) => WorkerResponse::Completed {
            output: result.value.to_string(),
            trace: trace.0,
            resource_limits,
        },
        Ok(ExecutionOutcome::Stopped { reason, .. }) => WorkerResponse::Stopped {
            reason: bounded_stop_reason(reason),
            trace: trace.0,
            resource_limits,
        },
        Err(error) => WorkerResponse::RuntimeError {
            code: error.code().to_owned(),
            message: if request.source_style_errors {
                error.error.to_string()
            } else {
                error.to_string()
            },
            trace: trace.0,
            resource_limits,
        },
    })
}

fn write_worker_message<T: Serialize>(
    writer: &mut impl Write,
    value: &T,
    maximum: usize,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "worker message cannot be encoded")?;
    if bytes.is_empty() || bytes.len() > maximum || bytes.len() > u32::MAX as usize {
        return Err("worker message exceeds its limit".to_owned());
    }
    let length = u32::try_from(bytes.len())
        .map_err(|_| "worker message exceeds its limit")?
        .to_be_bytes();
    writer
        .write_all(&length)
        .and_then(|()| writer.write_all(&bytes))
        .and_then(|()| writer.flush())
        .map_err(|_| "worker message cannot be written".to_owned())
}

fn read_worker_message<T: DeserializeOwned>(
    reader: &mut impl Read,
    maximum: usize,
) -> Result<T, String> {
    let mut header = [0; 4];
    reader
        .read_exact(&mut header)
        .map_err(|_| "worker message header is invalid")?;
    let length = usize::try_from(u32::from_be_bytes(header))
        .map_err(|_| "worker message length is invalid")?;
    if length == 0 || length > maximum {
        return Err("worker message length is invalid".to_owned());
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| "worker message is truncated")?;
    let mut trailing = [0; 1];
    match reader.read(&mut trailing) {
        Ok(0) => {}
        Ok(_) => return Err("worker message has trailing data".to_owned()),
        Err(_) => return Err("worker message cannot be read".to_owned()),
    }
    serde_json::from_slice(&bytes).map_err(|_| "worker message JSON is invalid".to_owned())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn apply_worker_limits() -> Result<WorkerResourceLimits, String> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    for (resource, ceiling) in [
        (Resource::Cpu, WORKER_CPU_SECONDS),
        (Resource::Fsize, WORKER_FILE_SIZE_BYTES),
    ] {
        let existing = getrlimit(resource);
        let maximum = existing.maximum.map_or(ceiling, |value| value.min(ceiling));
        let current = existing.current.map_or(maximum, |value| value.min(maximum));
        setrlimit(
            resource,
            Rlimit {
                current: Some(current),
                maximum: Some(maximum),
            },
        )
        .map_err(|_| "worker resource limits cannot be applied")?;
    }
    // Darwin exposes RLIMIT_AS but rejects setting it with EINVAL. Linux
    // applies it. The VM's independent allocation ceiling remains mandatory
    // when Darwin does not.
    let resource = Resource::As;
    let existing = getrlimit(resource);
    let maximum = existing
        .maximum
        .map_or(WORKER_ADDRESS_SPACE_BYTES, |value| {
            value.min(WORKER_ADDRESS_SPACE_BYTES)
        });
    let current = existing.current.map_or(maximum, |value| value.min(maximum));
    let address_space_result = setrlimit(
        resource,
        Rlimit {
            current: Some(current),
            maximum: Some(maximum),
        },
    );
    #[cfg(target_os = "macos")]
    let address_space = match address_space_result {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => false,
        Err(_) => return Err("worker address-space limit cannot be applied".to_owned()),
    };
    #[cfg(target_os = "linux")]
    let address_space = match address_space_result {
        Ok(()) => true,
        Err(_) => return Err("worker address-space limit cannot be applied".to_owned()),
    };
    Ok(WorkerResourceLimits {
        cpu: true,
        address_space,
        file_size: true,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn apply_worker_limits() -> Result<WorkerResourceLimits, String> {
    Err("worker resource limits are unsupported on this platform".to_owned())
}

fn inspect_artifact(path: &str) -> Result<(), ExitCode> {
    let bytes = read_artifact(path)?;
    let artifact = decode_and_verify(&bytes, &DecodeLimits::default())
        .map_err(|error| report_artifact_error(&error))?;
    let metadata = artifact.metadata();
    println!("bytecode_version: {}", metadata.bytecode_version);
    println!("language_version: {}", metadata.language_version);
    println!("compiler_version: {}", metadata.compiler_version);
    println!("target_profile: {}", metadata.target_profile);
    println!("content_digest: {}", hex_digest(artifact.content_digest()));
    for section in artifact.section_summaries() {
        println!("section.{}: {}", section.name, section.entries);
    }
    if let Some(manifest) = artifact.manifest() {
        println!(
            "manifest.package: {}@{}",
            manifest.package, manifest.version
        );
        println!("manifest.language: {}", manifest.language_requirement);
        println!(
            "manifest.required_capabilities: [{}]",
            manifest.required_capabilities.join(", ")
        );
        println!(
            "manifest.optional_capabilities: [{}]",
            manifest.optional_capabilities.join(", ")
        );
        println!(
            "manifest.https_origins: [{}]",
            manifest.https_origins.join(", ")
        );
        println!(
            "manifest.exec_commands: [{}]",
            manifest.exec_commands.join(", ")
        );
        println!(
            "manifest.exec_environment: [{}]",
            manifest.exec_environment.join(", ")
        );
        for entry in artifact.entries() {
            println!(
                "contract.entry.{}: function={} input_schema={} output_schema={}",
                entry.name, entry.function, entry.input_schema, entry.output_schema
            );
        }
        for import in artifact.imports() {
            println!(
                "contract.import.{}:{}: {}@{} {} {}",
                import.importer,
                import.alias,
                import.package,
                import.version,
                import.module,
                hex_digest(&import.content_digest)
            );
        }
    }
    println!(
        "entry: {}",
        artifact.verified_module().entry_function().name
    );
    println!(
        "debug: {}",
        if artifact.debug().is_some() {
            "present"
        } else {
            "absent"
        }
    );
    Ok(())
}

fn usage(program: &str) -> Result<(), ExitCode> {
    eprintln!("{}", usage_text(program));
    Err(ExitCode::from(2))
}

fn usage_text(program: &str) -> String {
    format!(
        "usage: {program} check [--show-effects] <source.allen|package-directory>\n       \
         {program} run [--trace-tasks] [--entry <name>] [--input <json-file|->] [--workdir <directory>] [--allow-net-origin <https-origin>]... [--grant-exec <pattern>]... [--grant-exec-env <NAME>]... <source.allen|package-directory|artifact.allenb>\n       \
         {program} build <source.allen|package-directory> -o <artifact.allenb>\n       \
         {program} inspect <artifact.allenb>\n       \
         {program} lock <package-directory>\n       \
         {program} test [--filter <text>] [--replay <journal.json>] [--catalog <catalog.json>] <source.allen|package-directory>"
    )
}

#[derive(Default)]
struct TestOptions {
    filter: Option<String>,
    replay: Option<String>,
    catalog: Option<String>,
    path: Option<String>,
}

fn run_source_tests(program: &str, arguments: &[String]) -> Result<(), ExitCode> {
    let options = parse_test_arguments(program, arguments)?;
    let path = options.path.expect("validated test path");
    let catalog = options
        .catalog
        .as_deref()
        .map(read_frozen_catalog)
        .transpose()
        .map_err(|message| {
            eprintln!("{path}: error: {message}");
            ExitCode::from(2)
        })?;
    let (bundle, loaded) = load_test_bundle(Path::new(&path)).map_err(|message| {
        eprintln!("{path}: error: {message}");
        ExitCode::from(1)
    })?;
    let discovered = discover_source_tests(&bundle).map_err(|diagnostics| {
        report_test_diagnostics(&bundle, diagnostics);
        ExitCode::from(1)
    })?;
    let selected = discovered
        .into_iter()
        .filter(|test| {
            options
                .filter
                .as_ref()
                .is_none_or(|filter| source_test_display(test).contains(filter))
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        eprintln!("{path}: error: no source tests matched");
        return Err(ExitCode::from(2));
    }
    let effectful = selected
        .iter()
        .filter(|test| !test.effects.is_empty())
        .count();
    if options.replay.is_some() && (selected.len() != 1 || effectful != 1) {
        eprintln!("{path}: error: --replay requires exactly one selected effectful test");
        return Err(ExitCode::from(2));
    }
    if effectful != 0 && options.replay.is_none() {
        eprintln!(
            "{path}: error: effectful source tests require an exact allen-testkit replay journal"
        );
        return Err(ExitCode::from(1));
    }
    let replay = options
        .replay
        .as_deref()
        .map(read_replay_log)
        .transpose()
        .map_err(|message| {
            eprintln!("{path}: error: {message}");
            ExitCode::from(1)
        })?;
    let total = selected.len();
    let mut failed = 0usize;
    for test in selected {
        let display = source_test_display(&test);
        let result = run_one_source_test(
            &bundle,
            loaded.as_ref(),
            catalog.as_ref(),
            &test,
            replay.as_ref(),
        );
        match result {
            Ok(()) => println!("test {display} ... ok"),
            Err(message) => {
                failed += 1;
                println!("test {display} ... FAILED");
                eprintln!("{display}: {message}");
            }
        }
    }
    println!(
        "test result: {}. {} passed; {} failed",
        if failed == 0 { "ok" } else { "FAILED" },
        total - failed,
        failed
    );
    if failed == 0 {
        Ok(())
    } else {
        Err(ExitCode::from(1))
    }
}

fn parse_test_arguments(program: &str, arguments: &[String]) -> Result<TestOptions, ExitCode> {
    let mut options = TestOptions::default();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--filter" | "--replay" | "--catalog" => {
                let flag = arguments[index].as_str();
                let Some(value) = arguments
                    .get(index + 1)
                    .filter(|value| !value.is_empty() && !value.starts_with('-'))
                else {
                    return usage(program).map(|()| unreachable!());
                };
                let slot = match flag {
                    "--filter" => &mut options.filter,
                    "--replay" => &mut options.replay,
                    "--catalog" => &mut options.catalog,
                    _ => unreachable!(),
                };
                if slot.replace(value.clone()).is_some() {
                    return usage(program).map(|()| unreachable!());
                }
                index += 2;
            }
            value if !value.starts_with('-') && options.path.is_none() => {
                options.path = Some(value.to_owned());
                index += 1;
            }
            _ => return usage(program).map(|()| unreachable!()),
        }
    }
    if options.path.is_none() {
        return usage(program).map(|()| unreachable!());
    }
    Ok(options)
}

fn load_test_bundle(path: &Path) -> Result<(PackageSourceBundle, Option<LoadedPackage>), String> {
    if path.is_dir() {
        let loaded = crate::package::load_test_package(path)?;
        let bundle = prepare_loaded_source_tests(&loaded)?;
        return Ok((bundle, Some(loaded)));
    }
    let (root, sources) = load_source_bundle(path)?;
    Ok((
        PackageSourceBundle {
            root,
            sources,
            import_targets: BTreeMap::new(),
            entry_points: Vec::new(),
            entry_modules: Vec::new(),
        },
        None,
    ))
}

fn source_test_display(test: &SourceTest) -> String {
    format!(
        "{}::{}",
        escape_untrusted_terminal_text(&test.module),
        serde_json::to_string(&test.name).expect("test name JSON encoding cannot fail")
    )
}

fn report_test_diagnostics(bundle: &PackageSourceBundle, diagnostics: Vec<Diagnostic>) {
    for diagnostic in diagnostics {
        let module = diagnostic.source.as_deref().unwrap_or(&bundle.root);
        let source = bundle.sources.get(module).map_or("", String::as_str);
        eprintln!("{}", render_diagnostic(module, source, &diagnostic));
    }
}

fn run_one_source_test(
    bundle: &PackageSourceBundle,
    loaded: Option<&LoadedPackage>,
    catalog: Option<&allen_schema::FrozenCatalog>,
    test: &SourceTest,
    replay: Option<&ReplayLog>,
) -> Result<(), String> {
    let artifact = if let Some(loaded) = loaded {
        assemble_loaded_source_test(loaded, catalog, &test.module, &test.name)?
            .package
            .artifact
    } else {
        let compiled =
            compile_source_test(bundle, &test.module, &test.name).map_err(|diagnostics| {
                diagnostics
                    .into_iter()
                    .map(|diagnostic| {
                        let module = diagnostic.source.as_deref().unwrap_or(&test.module);
                        let source = bundle.sources.get(module).map_or("", String::as_str);
                        render_diagnostic(module, source, &diagnostic)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })?;
        assemble_source_test(compiled.compilation)?
    };
    let bytes = encode(&artifact).map_err(|error| error.to_string())?;
    let verified =
        decode_and_verify(&bytes, &DecodeLimits::default()).map_err(|error| error.to_string())?;
    if let Some(log) = replay {
        return if let Some(catalog) = catalog {
            let schemas = allen_testkit::ArtifactToolSchemas::new(&verified, catalog)
                .map_err(|error| replay_error(&error))?;
            execute_source_test_replay_with_schemas(&verified, log, schemas, Some(catalog.clone()))
        } else {
            execute_source_test_replay(&verified, log)
        };
    }
    let request = RawLaunchRequest {
        entry: "test".to_owned(),
        input: b"null".to_vec(),
    };
    let policy = HostPolicy {
        limits: cli_execution_limits(),
        granted_tools: catalog.map_or_else(std::collections::BTreeSet::new, |_| {
            verified
                .manifest()
                .map(|manifest| {
                    manifest
                        .required_tools
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect()
                })
                .unwrap_or_default()
        }),
        tool_catalog: catalog.cloned(),
        ..HostPolicy::default()
    };
    match launch_raw(&verified, &request, &policy) {
        Ok(outcome) => match outcome.execution {
            ExecutionOutcome::Completed(_) => Ok(()),
            ExecutionOutcome::Stopped { reason, .. } => Err(format!(
                "stopped: {}",
                escape_untrusted_terminal_text(&reason)
            )),
        },
        Err(error) => {
            let message = if error.code.as_str() == "program.failed" {
                escape_untrusted_terminal_text(&error.message)
            } else {
                error.message
            };
            Err(format!("runtime error[{}]: {message}", error.code.as_str()))
        }
    }
}

fn read_replay_log(path: &str) -> Result<ReplayLog, String> {
    let json = fs::read_to_string(path).map_err(|error| format!("cannot read replay: {error}"))?;
    ReplayLog::from_json(&json, ReplayLimits::default())
        .map_err(|error| format!("invalid replay journal: {error}"))
}

fn read_frozen_catalog(path: &str) -> Result<allen_schema::FrozenCatalog, String> {
    let json = fs::read_to_string(path).map_err(|error| format!("cannot read catalog: {error}"))?;
    let params: CatalogSetParams =
        serde_json::from_str(&json).map_err(|_| "invalid frozen catalog document".to_owned())?;
    params
        .validate()
        .map_err(|_| "invalid frozen catalog document".to_owned())?;
    if !params.metadata.complete {
        return Err("frozen catalog must be complete".to_owned());
    }
    let limits = allen_schema::SchemaLimits::default();
    let tools = params
        .tools
        .iter()
        .map(|tool| {
            allen_schema::ToolDefinition::parse(
                &tool.name,
                &tool.version,
                &serde_json::to_string(&tool.input_schema).map_err(|_| ())?,
                &serde_json::to_string(&tool.output_schema).map_err(|_| ())?,
                &serde_json::to_string(&tool.error_schema).map_err(|_| ())?,
                tool.effects.clone(),
                match tool.idempotency {
                    josh_protocol::Idempotency::Unknown => allen_schema::Idempotency::Unknown,
                    josh_protocol::Idempotency::Idempotent => allen_schema::Idempotency::Idempotent,
                    josh_protocol::Idempotency::NonIdempotent => {
                        allen_schema::Idempotency::NonIdempotent
                    }
                },
                &limits,
            )
            .map_err(|_| ())
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|()| "invalid frozen catalog document".to_owned())?;
    allen_schema::FrozenCatalog::freeze_with_dialect(
        &params.schema_dialect,
        tools,
        &allen_schema::CatalogLimits::default(),
    )
    .map_err(|_| "invalid frozen catalog document".to_owned())
}

fn execute_source_test_replay(artifact: &VerifiedArtifact, log: &ReplayLog) -> Result<(), String> {
    execute_source_test_replay_with_schemas(artifact, log, NoToolSchemas, None)
}

fn execute_source_test_replay_with_schemas<S: ToolResultSchema>(
    artifact: &VerifiedArtifact,
    log: &ReplayLog,
    schemas: S,
    tool_catalog: Option<allen_schema::FrozenCatalog>,
) -> Result<(), String> {
    if log.header().artifact_digest != *artifact.content_digest() {
        return Err("replay journal artifact digest does not match selected test".to_owned());
    }
    let request = RawLaunchRequest {
        entry: "test".to_owned(),
        input: b"null".to_vec(),
    };
    let mut granted_capabilities = std::collections::BTreeSet::new();
    if let Some(manifest) = artifact.manifest() {
        granted_capabilities.extend(manifest.required_capabilities.iter().cloned());
        granted_capabilities.extend(manifest.optional_capabilities.iter().cloned());
    }
    let policy = HostPolicy {
        limits: cli_execution_limits(),
        granted_capabilities,
        granted_tools: tool_catalog
            .as_ref()
            .map_or_else(std::collections::BTreeSet::new, |_| {
                artifact
                    .manifest()
                    .map(|manifest| {
                        manifest
                            .required_tools
                            .iter()
                            .map(|tool| tool.name.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            }),
        tool_catalog,
        ..HostPolicy::default()
    };
    let prepared = prepare_raw_launch(artifact, &request, &policy)
        .map_err(|error| format!("replay preflight failed: {error}"))?;
    let binding = prepared.effect_execution_binding();
    let mut expected = log.header().clone();
    expected.bytecode_version = binding.bytecode_version;
    expected.artifact_digest = binding.artifact_digest;
    expected.contract_digest = binding.contract_digest;
    expected.language_digest = binding.language_digest;
    expected.runtime_digest = binding.runtime_digest;
    expected.policy_digest = binding.policy_digest;
    expected.catalog_digest = binding.catalog_digest;
    expected.capability_digest = binding.capability_digest;
    expected.error_registry_digest = binding.error_registry_digest;
    expected
        .effective_manifest_grants
        .clone_from(&binding.effective_manifest_grants);
    expected
        .requested_exec_commands
        .clone_from(&binding.requested_exec_commands);
    expected
        .requested_exec_environment
        .clone_from(&binding.requested_exec_environment);
    expected
        .effective_exec_grants
        .clone_from(&binding.effective_exec_grants);
    expected
        .effective_exec_environment
        .clone_from(&binding.effective_exec_environment);
    expected.effective_exec_environment_digest = binding.effective_exec_environment_digest;
    expected.pinned_exec_identity_digest = binding.pinned_exec_identity_digest;
    let mut provider = ReplayingEffectProvider::new(
        log,
        &expected,
        ReplayLimits::default(),
        schemas,
        &artifact.verified_module().module().enum_types,
    )
    .map_err(|error| replay_error(&error))?;
    let mut providers = RuntimeProviders {
        effect_override: Some(&mut provider),
        ..RuntimeProviders::default()
    };
    let mut observer = TraceCollector::default();
    let mut cancellation = NeverCancelled;
    match execute_prepared_with_context(prepared, &mut providers, &mut cancellation, &mut observer)
    {
        Ok(outcome) => match outcome.execution {
            ExecutionOutcome::Completed(_) => Ok(()),
            ExecutionOutcome::Stopped { reason, .. } => Err(format!(
                "stopped: {}",
                escape_untrusted_terminal_text(&reason)
            )),
        },
        Err(error) => {
            let message = if error.code.as_str() == "program.failed" {
                escape_untrusted_terminal_text(&error.message)
            } else {
                error.message
            };
            Err(format!("runtime error[{}]: {message}", error.code.as_str()))
        }
    }
}

fn replay_error(error: &ReplayError) -> String {
    format!("invalid or divergent replay journal: {error}")
}

#[allow(clippy::too_many_lines)]
fn parse_run_arguments(
    program: &str,
    arguments: &[String],
) -> Result<(RunOptions, String), ExitCode> {
    let mut options = RunOptions::default();
    let mut path = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--trace-tasks" if !options.trace_tasks => {
                options.trace_tasks = true;
                index += 1;
            }
            "--entry" | "--input" | "--workdir" => {
                let flag = arguments[index].as_str();
                let Some(value) = arguments.get(index + 1).filter(|value| {
                    !value.is_empty()
                        && (!value.starts_with('-') || (flag == "--input" && *value == "-"))
                }) else {
                    let _ = usage(program);
                    return Err(ExitCode::from(2));
                };
                let slot = match flag {
                    "--entry" => &mut options.entry,
                    "--input" => &mut options.input,
                    "--workdir" => &mut options.workdir,
                    _ => unreachable!(),
                };
                if slot.replace(value.clone()).is_some() {
                    let _ = usage(program);
                    return Err(ExitCode::from(2));
                }
                index += 2;
            }
            "--allow-net-origin" => {
                let Some(value) = arguments
                    .get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                else {
                    let _ = usage(program);
                    return Err(ExitCode::from(2));
                };
                let origin = canonical_https_origin(value).map_err(|error| {
                    eprintln!("{value}: error: {error}");
                    ExitCode::from(2)
                })?;
                if options.allowed_net_origins.contains(&origin) {
                    eprintln!("{origin}: error: network origin is duplicated");
                    return Err(ExitCode::from(2));
                }
                options.allowed_net_origins.push(origin);
                index += 2;
            }
            "--grant-exec" => {
                let Some(value) = arguments
                    .get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                else {
                    let _ = usage(program);
                    return Err(ExitCode::from(2));
                };
                allen_exec::CommandPattern::parse(value).map_err(|_| {
                    eprintln!("{value}: error: exec command pattern is not canonical");
                    ExitCode::from(2)
                })?;
                options.granted_exec.push(value.clone());
                index += 2;
            }
            "--grant-exec-env" => {
                let Some(value) = arguments
                    .get(index + 1)
                    .filter(|value| !value.starts_with('-'))
                else {
                    let _ = usage(program);
                    return Err(ExitCode::from(2));
                };
                let mut bytes = value.bytes();
                if !bytes.next().is_some_and(|first| {
                    (first.is_ascii_alphabetic() || first == b'_')
                        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                        && !value.eq_ignore_ascii_case("LC_ALL")
                        && !value.eq_ignore_ascii_case("TZ")
                }) {
                    eprintln!("{value}: error: exec environment name is not canonical");
                    return Err(ExitCode::from(2));
                }
                options.granted_exec_environment.push(value.clone());
                index += 2;
            }
            value if !value.starts_with('-') && path.is_none() => {
                path = Some(value.to_owned());
                index += 1;
            }
            _ => {
                let _ = usage(program);
                return Err(ExitCode::from(2));
            }
        }
    }
    options.allowed_net_origins.sort();
    options.granted_exec.sort();
    options.granted_exec.dedup();
    options.granted_exec_environment.sort();
    options.granted_exec_environment.dedup();
    path.map(|path| (options, path)).ok_or_else(|| {
        let _ = usage(program);
        ExitCode::from(2)
    })
}

fn read_json_input(path: Option<&str>, limit: usize) -> Result<Vec<u8>, ExitCode> {
    let Some(path) = path else {
        return Ok(b"null".to_vec());
    };
    let mut bytes = Vec::new();
    let bounded = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    if path == "-" {
        std::io::stdin()
            .take(bounded)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                eprintln!("stdin: error: cannot read JSON input: {error}");
                ExitCode::from(1)
            })?;
    } else {
        fs::File::open(path)
            .map_err(|error| {
                eprintln!("{path}: error: cannot open JSON input: {error}");
                ExitCode::from(1)
            })?
            .take(bounded)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                eprintln!("{path}: error: cannot read JSON input: {error}");
                ExitCode::from(1)
            })?;
    }
    if bytes.len() > limit {
        eprintln!("{path}: error: JSON input exceeds the byte limit");
        return Err(ExitCode::from(1));
    }
    Ok(bytes)
}

fn entry_effects<'a>(artifact: &'a VerifiedArtifact, name: &str) -> Option<&'a [String]> {
    let entry = artifact.entries().iter().find(|entry| entry.name == name)?;
    let function = artifact
        .verified_module()
        .module()
        .functions
        .get(entry.function as usize)?;
    artifact
        .verified_module()
        .module()
        .effect_sets
        .get(function.effects as usize)
        .map(Vec::as_slice)
}

fn entry_uses_filesystem(artifact: &VerifiedArtifact, entry: &str) -> bool {
    let Some(effects) = entry_effects(artifact, entry) else {
        return false;
    };
    effects
        .iter()
        .any(|effect| matches!(effect.as_str(), "fs.read" | "fs.write"))
        || artifact.manifest().is_some_and(|manifest| {
            manifest
                .required_capabilities
                .iter()
                .chain(&manifest.optional_capabilities)
                .any(|capability| matches!(capability.as_str(), "fs.read" | "fs.write"))
        })
}

fn workspace_rights(artifact: &VerifiedArtifact, entry: &str) -> allen_sandbox_fs::Rights {
    let effects = entry_effects(artifact, entry).unwrap_or_default();
    let requested = artifact.manifest();
    let read = effects.iter().any(|effect| effect == "fs.read")
        || requested.is_some_and(|manifest| {
            manifest
                .required_capabilities
                .iter()
                .chain(&manifest.optional_capabilities)
                .any(|capability| capability == "fs.read")
        });
    let write = effects.iter().any(|effect| effect == "fs.write")
        || requested.is_some_and(|manifest| {
            manifest
                .required_capabilities
                .iter()
                .chain(&manifest.optional_capabilities)
                .any(|capability| capability == "fs.write")
        });
    allen_sandbox_fs::Rights::new(read, write)
}

fn entry_input_limit(artifact: &VerifiedArtifact, host: usize) -> usize {
    artifact
        .manifest()
        .and_then(|manifest| {
            manifest
                .limits
                .iter()
                .find(|(name, _)| name == "input_bytes")
        })
        .and_then(|(_, value)| usize::try_from(*value).ok())
        .map_or(host, |value| host.min(value))
}

#[derive(Default)]
struct TraceCollector(Vec<String>);

struct NeverCancelled;

impl CancellationSource for NeverCancelled {
    fn is_cancelled(&mut self) -> bool {
        false
    }
}

impl CheckpointObserver for TraceCollector {
    fn checkpoint(&mut self, _checkpoint: Checkpoint) {}

    fn task_event(&mut self, event: TaskEvent) {
        if self.0.len() < MAX_WORKER_TRACE_EVENTS {
            self.0.push(format!(
                "task_event sequence={} task_id={} owner_id={} kind={}",
                event.sequence, event.task_id, event.owner_id, event.kind
            ));
        }
    }
}

fn compile_source_artifact(path: &str, show_effects: bool) -> Result<Artifact, ExitCode> {
    let source = fs::read_to_string(path).map_err(|error| {
        eprintln!("{path}: error: cannot read source file: {error}");
        ExitCode::from(1)
    })?;
    let input_path = Path::new(path);
    let root_name = source_root_name(input_path).map_err(|error| {
        eprintln!("{path}: error: {error}");
        ExitCode::from(1)
    })?;
    let prepared = prepare_source(&root_name, &source).map_err(|diagnostic| {
        eprintln!("{}", render_diagnostic(path, &source, &diagnostic));
        ExitCode::from(1)
    })?;
    if prepared.inline_manifest().is_some() {
        if has_enclosing_file_manifest(input_path) {
            eprintln!("{path}: error: inline manifest conflicts with allen.toml");
            return Err(ExitCode::from(1));
        }
        let (manifest, compilation) =
            compile_prepared_inline_manifest_source(prepared).map_err(|diagnostics| {
                for diagnostic in diagnostics {
                    eprintln!("{}", render_diagnostic(path, &source, &diagnostic));
                }
                ExitCode::from(1)
            })?;
        let Some(manifest) = manifest else {
            unreachable!("the source begins with a manifest")
        };
        if show_effects {
            for entry in &compilation.effect_report {
                println!(
                    "{}",
                    format_effect_report_entry(&entry.module, &entry.function, &entry.effects,)
                );
            }
        }
        return crate::package::from_inline(manifest, compilation).map_err(|error| {
            eprintln!("{path}: error: {error}");
            ExitCode::from(1)
        });
    }
    let (root, sources) = load_source_bundle(Path::new(path)).map_err(|error| {
        eprintln!("{path}: error: cannot load source bundle: {error}");
        ExitCode::from(1)
    })?;
    let compilation =
        compile_bundle_with_prepared_source(&root, &sources, prepared).map_err(|diagnostics| {
            for diagnostic in diagnostics {
                eprintln!(
                    "{}",
                    render_bundle_diagnostic(input_path, &root, &sources, &diagnostic)
                );
            }
            ExitCode::from(1)
        })?;
    if show_effects {
        for entry in &compilation.effect_report {
            println!(
                "{}",
                format_effect_report_entry(&entry.module, &entry.function, &entry.effects,)
            );
        }
    }
    assemble_loose_compilation(&root, compilation)
        .map(|compiled| compiled.artifact)
        .map_err(|error| {
            eprintln!("{path}: error: {error}");
            ExitCode::from(1)
        })
}

fn format_effect_report_entry(module: &str, function: &str, effects: &[String]) -> String {
    let name = format!("{module}::{function}");
    if effects.is_empty() {
        name
    } else {
        format!("{name} effects [{}]", effects.join(", "))
    }
}

fn render_bundle_diagnostic(
    input: &Path,
    root: &str,
    sources: &BTreeMap<String, String>,
    diagnostic: &Diagnostic,
) -> String {
    let module = diagnostic.source.as_deref().unwrap_or(root);
    let source = sources
        .get(module)
        .or_else(|| sources.get(root))
        .map_or("", String::as_str);
    let display_path = if module == root {
        input.to_path_buf()
    } else {
        input
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(module)
    };
    render_diagnostic(&display_path.to_string_lossy(), source, diagnostic)
}

fn has_enclosing_file_manifest(path: &Path) -> bool {
    path.parent().is_some_and(|parent| {
        parent
            .ancestors()
            .any(|ancestor| ancestor.join("allen.toml").is_file())
    })
}

fn compile_input_artifact(path: &str, show_effects: bool) -> Result<Artifact, ExitCode> {
    let input = Path::new(path);
    if input.is_dir() {
        let compiled = crate::package::load_and_compile(input).map_err(|error| {
            eprintln!("{path}: error: {error}");
            ExitCode::from(1)
        })?;
        if show_effects {
            for effect in compiled.effects {
                println!("{effect}");
            }
        }
        Ok(compiled.artifact)
    } else {
        compile_source_artifact(path, show_effects)
    }
}

fn read_artifact(path: &str) -> Result<Vec<u8>, ExitCode> {
    fs::read(path).map_err(|error| {
        eprintln!("{path}: error: cannot read artifact: {error}");
        ExitCode::from(1)
    })
}

fn report_artifact_error(error: &allen_bytecode::ArtifactError) -> ExitCode {
    eprintln!("artifact error[{}]: {error}", error.code().as_str());
    ExitCode::from(1)
}

fn cli_execution_limits() -> ExecutionLimits {
    ExecutionLimits {
        instructions: 10_000_000,
        allocation_bytes: 64 * 1024 * 1024,
        maximum_allocation_bytes: 8 * 1024 * 1024,
        call_depth: 128,
        wall_time: std::time::Duration::from_secs(30),
        tasks: 1_024,
        concurrent_effects: 256,
        cleanup_instructions: 10_000,
    }
}

fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut output = String::with_capacity(64);
    for byte in digest {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn is_artifact_path(path: &Path) -> bool {
    path.extension().and_then(|value| value.to_str()) == Some("allenb")
}

fn load_source_bundle(path: &Path) -> Result<(String, BTreeMap<String, String>), String> {
    let root_directory = path.parent().unwrap_or_else(|| Path::new("."));
    let root_name = source_root_name(path)?;
    let mut sources = BTreeMap::new();
    collect_allen_sources(root_directory, root_directory, &mut sources)?;
    if !sources.contains_key(&root_name) {
        sources.insert(
            root_name.clone(),
            fs::read_to_string(path).map_err(|error| error.to_string())?,
        );
    }
    Ok((root_name, sources))
}

fn source_root_name(path: &Path) -> Result<String, String> {
    Ok(path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "source path has no UTF-8 file name".to_owned())?
        .to_owned())
}

fn collect_allen_sources(
    root: &Path,
    directory: &Path,
    sources: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let entry_path = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_dir() {
            collect_allen_sources(root, &entry_path, sources)?;
        } else if file_type.is_file()
            && entry_path.extension().and_then(|value| value.to_str()) == Some("allen")
        {
            let relative = entry_path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?;
            let key = relative
                .components()
                .map(|component| {
                    component
                        .as_os_str()
                        .to_str()
                        .ok_or_else(|| "source bundle contains a non-UTF-8 path".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?
                .join("/");
            let value = fs::read_to_string(&entry_path).map_err(|error| error.to_string())?;
            sources.insert(key, value);
        }
    }
    Ok(())
}

#[cfg(test)]
mod worker_tests {
    use super::*;

    #[test]
    fn every_public_diagnostic_code_has_one_diagnostic_registry_entry() {
        use allen_bytecode::ArtifactErrorCode;
        use allen_package::PackageErrorCode;
        use allen_schema::SchemaErrorCode;

        let registry: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/conformance/errors-0.1.json"))
                .unwrap();
        let rows = registry["registry"].as_array().unwrap();
        let mut expected = allen_compiler::DIAGNOSTIC_CODES.to_vec();
        expected.extend([
            ArtifactErrorCode::InvalidMagic.as_str(),
            ArtifactErrorCode::InvalidHeader.as_str(),
            ArtifactErrorCode::UnsupportedVersion.as_str(),
            ArtifactErrorCode::UnsupportedProfile.as_str(),
            ArtifactErrorCode::ArtifactTooLarge.as_str(),
            ArtifactErrorCode::SectionTooLarge.as_str(),
            ArtifactErrorCode::MissingSection.as_str(),
            ArtifactErrorCode::DuplicateSection.as_str(),
            ArtifactErrorCode::UnknownSection.as_str(),
            ArtifactErrorCode::SectionOrder.as_str(),
            ArtifactErrorCode::DigestMismatch.as_str(),
            ArtifactErrorCode::Truncated.as_str(),
            ArtifactErrorCode::TrailingBytes.as_str(),
            ArtifactErrorCode::InvalidUtf8.as_str(),
            ArtifactErrorCode::LimitExceeded.as_str(),
            ArtifactErrorCode::InvalidScalar.as_str(),
            ArtifactErrorCode::NonCanonical.as_str(),
            ArtifactErrorCode::VerificationFailed.as_str(),
            ArtifactErrorCode::InvalidDebug.as_str(),
        ]);
        expected.extend([
            PackageErrorCode::Io.as_str(),
            PackageErrorCode::InvalidManifest.as_str(),
            PackageErrorCode::InvalidLockfile.as_str(),
            PackageErrorCode::NonCanonicalLockfile.as_str(),
            PackageErrorCode::InvalidName.as_str(),
            PackageErrorCode::InvalidVersion.as_str(),
            PackageErrorCode::InvalidLanguage.as_str(),
            PackageErrorCode::InvalidEntry.as_str(),
            PackageErrorCode::InvalidCapability.as_str(),
            PackageErrorCode::InvalidTool.as_str(),
            PackageErrorCode::InvalidLimit.as_str(),
            PackageErrorCode::InvalidDependency.as_str(),
            PackageErrorCode::PathEscape.as_str(),
            PackageErrorCode::Symlink.as_str(),
            PackageErrorCode::SpecialFile.as_str(),
            PackageErrorCode::MissingSource.as_str(),
            PackageErrorCode::DependencyCycle.as_str(),
            PackageErrorCode::DuplicateIdentity.as_str(),
            PackageErrorCode::VersionConflict.as_str(),
            PackageErrorCode::LanguageConflict.as_str(),
            PackageErrorCode::PackageLimit.as_str(),
            PackageErrorCode::DependencyDepthLimit.as_str(),
            PackageErrorCode::ModuleLimit.as_str(),
            PackageErrorCode::SourceBytesLimit.as_str(),
            PackageErrorCode::LockMismatch.as_str(),
        ]);
        expected.extend([
            SchemaErrorCode::InvalidJson.as_str(),
            SchemaErrorCode::DuplicateKey.as_str(),
            SchemaErrorCode::InvalidForm.as_str(),
            SchemaErrorCode::UnsupportedKeyword.as_str(),
            SchemaErrorCode::InvalidBound.as_str(),
            SchemaErrorCode::UnsortedSet.as_str(),
            SchemaErrorCode::InvalidReference.as_str(),
            SchemaErrorCode::CyclicReference.as_str(),
            SchemaErrorCode::UnusedDefinition.as_str(),
            SchemaErrorCode::Limit.as_str(),
        ]);

        let mut unique = std::collections::BTreeSet::new();
        for code in expected {
            assert!(
                unique.insert(code),
                "duplicate public diagnostic code {code}"
            );
            let matching = rows
                .iter()
                .filter(|row| row["code"].as_str() == Some(code))
                .collect::<Vec<_>>();
            assert_eq!(
                matching.len(),
                1,
                "registry must contain {code} exactly once"
            );
            assert_eq!(
                matching[0]["channel"].as_str(),
                Some("diagnostic"),
                "{code}"
            );
        }
    }

    fn resource_limits() -> WorkerResourceLimits {
        WorkerResourceLimits {
            cpu: true,
            address_space: false,
            file_size: true,
        }
    }

    #[test]
    fn worker_response_round_trip_retains_limit_capabilities() {
        let response = WorkerResponse::Completed {
            output: "42".to_owned(),
            trace: vec!["task_event sequence=1 task_id=1 owner_id=0 kind=spawned".to_owned()],
            resource_limits: resource_limits(),
        };
        let mut bytes = Vec::new();
        write_worker_message(&mut bytes, &response, 1024).unwrap();
        let decoded: WorkerResponse =
            read_worker_message(&mut std::io::Cursor::new(bytes), 1024).unwrap();
        let WorkerResponse::Completed {
            resource_limits, ..
        } = decoded
        else {
            panic!("response shape changed")
        };
        assert!(resource_limits.cpu);
        assert!(!resource_limits.address_space);
        assert!(resource_limits.file_size);
    }

    #[test]
    fn worker_exchange_rejects_oversized_trailing_and_unknown_messages() {
        let oversized = u32::try_from(1025_usize).unwrap().to_be_bytes().to_vec();
        assert!(
            read_worker_message::<WorkerResponse>(&mut std::io::Cursor::new(oversized), 1024)
                .is_err()
        );

        let mut trailing = Vec::new();
        write_worker_message(
            &mut trailing,
            &WorkerResponse::Stopped {
                reason: Some("done".to_owned()),
                trace: Vec::new(),
                resource_limits: resource_limits(),
            },
            1024,
        )
        .unwrap();
        trailing.push(0);
        assert!(
            read_worker_message::<WorkerResponse>(&mut std::io::Cursor::new(trailing), 1024)
                .is_err()
        );

        let body = br#"{"outcome":"completed","output":"42","trace":[],"resource_limits":{"cpu":true,"address_space":false,"file_size":true},"unexpected":true}"#;
        let mut unknown = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
        unknown.extend_from_slice(body);
        assert!(
            read_worker_message::<WorkerResponse>(&mut std::io::Cursor::new(unknown), 1024)
                .is_err()
        );
    }

    #[test]
    fn effectful_source_test_uses_exact_prepared_replay_binding() {
        let bundle = PackageSourceBundle {
            root: "main.allen".to_owned(),
            sources: BTreeMap::from([(
                "main.allen".to_owned(),
                "test \"recorded\" effects [agent.message] { () }".to_owned(),
            )]),
            import_targets: BTreeMap::new(),
            entry_points: Vec::new(),
            entry_modules: Vec::new(),
        };
        let compiled = compile_source_test(&bundle, "main.allen", "recorded").unwrap();
        let artifact = assemble_source_test(compiled.compilation).unwrap();
        let bytes = encode(&artifact).unwrap();
        let artifact = decode_and_verify(&bytes, &DecodeLimits::default()).unwrap();
        let request = RawLaunchRequest {
            entry: "test".to_owned(),
            input: b"null".to_vec(),
        };
        let policy = HostPolicy {
            limits: cli_execution_limits(),
            granted_capabilities: std::collections::BTreeSet::from(["agent.message".to_owned()]),
            ..HostPolicy::default()
        };
        let prepared = prepare_raw_launch(&artifact, &request, &policy).unwrap();
        let binding = prepared.effect_execution_binding();
        let header = allen_testkit::ReplayHeader {
            bytecode_version: binding.bytecode_version,
            artifact_digest: binding.artifact_digest,
            contract_digest: binding.contract_digest,
            language_digest: binding.language_digest,
            runtime_digest: binding.runtime_digest,
            policy_digest: binding.policy_digest,
            catalog_digest: binding.catalog_digest,
            capability_digest: binding.capability_digest,
            error_registry_digest: binding.error_registry_digest,
            effective_manifest_grants: binding.effective_manifest_grants,
            requested_exec_commands: binding.requested_exec_commands,
            requested_exec_environment: binding.requested_exec_environment,
            effective_exec_grants: binding.effective_exec_grants,
            effective_exec_environment: binding.effective_exec_environment,
            effective_exec_environment_digest: binding.effective_exec_environment_digest,
            pinned_exec_identity_digest: binding.pinned_exec_identity_digest,
            scheduler_completion_order: Vec::new(),
        };
        let log = ReplayLog::new(
            header,
            Vec::new(),
            allen_testkit::ReplayExecutionOutcome::Completed,
            ReplayLimits::default(),
        )
        .unwrap();
        execute_source_test_replay(&artifact, &log).unwrap();

        let wrong = compile_source_test(
            &PackageSourceBundle {
                sources: BTreeMap::from([(
                    "main.allen".to_owned(),
                    "test \"recorded\" effects [agent.message] { let value = 1; () }".to_owned(),
                )]),
                ..bundle
            },
            "main.allen",
            "recorded",
        )
        .unwrap();
        let wrong = assemble_source_test(wrong.compilation).unwrap();
        let wrong = decode_and_verify(&encode(&wrong).unwrap(), &DecodeLimits::default()).unwrap();
        assert!(execute_source_test_replay(&wrong, &log).is_err());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn package_tool_source_test_replays_with_exact_catalog_and_artifact_binding() {
        use allen_schema::{CatalogLimits, Idempotency, SchemaLimits, ToolDefinition};
        use allen_testkit::{
            ArtifactToolSchemas, EffectKind, EffectOutcome, EffectRequest, Recorder,
            RefuseSensitive, ReplayExecutionOutcome,
        };
        use allen_vm::{EnumIdentity, EnumPayload, EnumValue, Value, encode_canonical_with_limit};
        use std::rc::Rc;

        let catalog_path = std::env::temp_dir().join(format!(
            "allen-source-test-catalog-{}.json",
            std::process::id()
        ));
        fs::write(
            &catalog_path,
            r#"{"schema_dialect":"https://json-schema.org/draft/2020-12/schema","metadata":{"source":"allen-test","source_revision":"fixture-1","observed_at_unix_ms":1,"freshness":"current","complete":true},"tools":[{"name":"example.lookup","version":"1.2.3","description":"Fixture lookup.","input_schema":{"type":"boolean"},"output_schema":{"type":"string","maxLength":8},"error_schema":{"type":"string","maxLength":8},"effects":["external.read"],"idempotency":"idempotent"}]}"#,
        )
        .unwrap();
        let catalog = read_frozen_catalog(catalog_path.to_str().unwrap()).unwrap();
        fs::remove_file(catalog_path).unwrap();
        let manifest = r#"[package]
name = "tool-tests"
version = "0.1.0"
language = "^0.1"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Void"

[[tools.required]]
name = "example.lookup"
version = ">=1.2.0, <2.0.0"
"#;
        let sources = BTreeMap::from([(
            "src/main.allen".to_owned(),
            r#"export fn main() returns Void { () }
test "tool replay" effects [tool.example.lookup@1] {
  match await tools.example.lookup.call(true) {
    Ok(value) => if (value == "found") { () } else { fail("wrong tool value") }
    Err(_) => fail("tool failed")
  }
}
"#
            .to_owned(),
        )]);
        let loaded = allen_package::load_verified_root_package(
            manifest,
            &sources,
            None,
            &LoadLimits::default(),
        )
        .unwrap();
        let module = "pkg://tool-tests@0.1.0/src/main.allen";
        let assembled =
            assemble_loaded_source_test(&loaded, Some(&catalog), module, "tool replay").unwrap();
        let bytes = encode(&assembled.package.artifact).unwrap();
        let artifact = decode_and_verify(&bytes, &DecodeLimits::default()).unwrap();
        let request = RawLaunchRequest {
            entry: "test".to_owned(),
            input: b"null".to_vec(),
        };
        let policy = HostPolicy {
            limits: cli_execution_limits(),
            granted_tools: std::collections::BTreeSet::from(["example.lookup".to_owned()]),
            tool_catalog: Some(catalog.clone()),
            ..HostPolicy::default()
        };
        let prepared = prepare_raw_launch(&artifact, &request, &policy).unwrap();
        let binding = prepared.effect_execution_binding();
        let header = allen_testkit::ReplayHeader {
            bytecode_version: binding.bytecode_version,
            artifact_digest: binding.artifact_digest,
            contract_digest: binding.contract_digest,
            language_digest: binding.language_digest,
            runtime_digest: binding.runtime_digest,
            policy_digest: binding.policy_digest,
            catalog_digest: binding.catalog_digest,
            capability_digest: binding.capability_digest,
            error_registry_digest: binding.error_registry_digest,
            effective_manifest_grants: binding.effective_manifest_grants,
            requested_exec_commands: binding.requested_exec_commands,
            requested_exec_environment: binding.requested_exec_environment,
            effective_exec_grants: binding.effective_exec_grants,
            effective_exec_environment: binding.effective_exec_environment,
            effective_exec_environment_digest: binding.effective_exec_environment_digest,
            pinned_exec_identity_digest: binding.pinned_exec_identity_digest,
            scheduler_completion_order: Vec::new(),
        };
        let schemas = ArtifactToolSchemas::new(&artifact, &catalog).unwrap();
        let result_type = schemas.result_type(0).unwrap();
        let effect_request = EffectRequest::from_value(
            EffectKind::Tool(0),
            &Value::Bool(true),
            &result_type,
            ReplayLimits::default(),
        )
        .unwrap();
        let result = Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::Result,
            type_name: "Result".into(),
            variant: 0,
            variant_name: "Ok".into(),
            payload: EnumPayload::Tuple(Rc::from([Value::String("found".into())])),
        }));
        let oversized = Value::Enum(Rc::new(EnumValue {
            identity: EnumIdentity::Result,
            type_name: "Result".into(),
            variant: 0,
            variant_name: "Ok".into(),
            payload: EnumPayload::Tuple(Rc::from([Value::String("too-long!".into())])),
        }));
        assert!(!schemas.validate_result(0, &oversized));
        let outcome = EffectOutcome::Ok(
            encode_canonical_with_limit(&result, ReplayLimits::default().payload_bytes as u64)
                .unwrap(),
        );
        let mut recorder = Recorder::with_header(ReplayLimits::default(), header);
        recorder
            .record(effect_request, outcome, false, &RefuseSensitive)
            .unwrap();
        let log = recorder
            .finish_with_execution_outcome(ReplayExecutionOutcome::Completed)
            .unwrap();
        execute_source_test_replay_with_schemas(&artifact, &log, schemas, Some(catalog.clone()))
            .unwrap();
        let bundle = prepare_loaded_source_tests(&loaded).unwrap();
        let selected = discover_source_tests(&bundle)
            .unwrap()
            .into_iter()
            .find(|test| test.name == "tool replay")
            .unwrap();
        run_one_source_test(
            &bundle,
            Some(&loaded),
            Some(&catalog),
            &selected,
            Some(&log),
        )
        .unwrap();

        let different_catalog = allen_schema::FrozenCatalog::freeze(
            vec![
                ToolDefinition::parse(
                    "example.lookup",
                    "1.2.4",
                    r#"{"type":"boolean"}"#,
                    r#"{"type":"string","maxLength":8}"#,
                    r#"{"type":"string","maxLength":8}"#,
                    vec!["external.read".to_owned()],
                    Idempotency::Idempotent,
                    &SchemaLimits::default(),
                )
                .unwrap(),
            ],
            &CatalogLimits::default(),
        )
        .unwrap();
        assert!(ArtifactToolSchemas::new(&artifact, &different_catalog).is_err());
    }
}
