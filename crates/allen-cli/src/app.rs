#![forbid(unsafe_code)]

use allen_bytecode::{Artifact, DecodeLimits, VerifiedArtifact, decode_and_verify, encode};
use allen_compiler::{
    Diagnostic, assemble_loose_compilation, compile_bundle_with_prepared_source,
    compile_prepared_inline_manifest_source, prepare_source, render_diagnostic,
};
use allen_package::{LoadLimits, canonical_https_origin, generate_lock};
use allen_runtime::{HostPolicy, LaunchRequest, RuntimeProviders, launch, launch_with_context};
use allen_vm::{
    CancellationSource, Checkpoint, CheckpointObserver, ExecutionLimits, ExecutionOutcome,
    TaskEvent, execute_verified_artifact_outcome_with_limits,
    execute_verified_artifact_outcome_with_observer,
};
use base64::Engine as _;
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
}

#[derive(Default)]
struct RunOptions {
    trace_tasks: bool,
    entry: Option<String>,
    input: Option<String>,
    workdir: Option<String>,
    allowed_net_origins: Vec<String>,
}

const WORKER_PROTOCOL: &str = "allen-cli-worker/1";
const MAX_WORKER_REQUEST_BYTES: usize = 24 * 1024 * 1024;
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
    input: serde_json::Value,
    workspace_root: Option<String>,
    allowed_http_origins: Vec<String>,
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
        _ => return usage(&program),
    };
    let remaining = arguments.collect::<Vec<_>>();
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
                    input,
                    workspace_root: run_options.workdir,
                    allowed_http_origins: run_options.allowed_net_origins,
                    trace_tasks: run_options.trace_tasks,
                    source_style_errors: !loaded_artifact,
                })?;
                return Ok(());
            }
            if run_options.entry.is_some()
                || run_options.input.is_some()
                || run_options.workdir.is_some()
                || !run_options.allowed_net_origins.is_empty()
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
                input: serde_json::Value::Null,
                workspace_root: None,
                allowed_http_origins: Vec::new(),
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
            eprintln!("runtime error[{code}]: {message}");
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
    {
        return Err("worker request is invalid".to_owned());
    }
    if serde_json::to_vec(&request.input)
        .map_err(|_| "worker input is invalid")?
        .len()
        > MAX_WORKER_INPUT_BYTES
    {
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
    Ok(())
}

fn bounded_stop_reason(reason: String) -> Option<String> {
    (reason.len() <= MAX_WORKER_STOP_REASON_BYTES).then_some(reason)
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
            ..HostPolicy::default()
        };
        policy.workspace_rights = workspace_rights(&verified, &request.entry);
        let launch_request = LaunchRequest {
            entry: request.entry,
            input: request.input,
        };
        let outcome = if request.trace_tasks {
            launch_with_context(
                &verified,
                &launch_request,
                &policy,
                &mut RuntimeProviders::default(),
                &mut NeverCancelled,
                &mut trace,
            )
        } else {
            launch(&verified, &launch_request, &policy)
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
         {program} run [--trace-tasks] [--entry <name>] [--input <json-file|->] [--workdir <directory>] [--allow-net-origin <https-origin>]... <source.allen|package-directory|artifact.allenb>\n       \
         {program} build <source.allen|package-directory> -o <artifact.allenb>\n       \
         {program} inspect <artifact.allenb>\n       \
         {program} lock <package-directory>"
    )
}

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
                let Some(value) = arguments.get(index + 1) else {
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
                let Some(value) = arguments.get(index + 1) else {
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
    path.map(|path| (options, path)).ok_or_else(|| {
        let _ = usage(program);
        ExitCode::from(2)
    })
}

fn read_json_input(path: Option<&str>, limit: usize) -> Result<serde_json::Value, ExitCode> {
    let Some(path) = path else {
        return Ok(serde_json::Value::Null);
    };
    let mut text = String::new();
    let bounded = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    if path == "-" {
        std::io::stdin()
            .take(bounded)
            .read_to_string(&mut text)
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
            .read_to_string(&mut text)
            .map_err(|error| {
                eprintln!("{path}: error: cannot read JSON input: {error}");
                ExitCode::from(1)
            })?;
    }
    if text.len() > limit {
        eprintln!("{path}: error: JSON input exceeds the byte limit");
        return Err(ExitCode::from(1));
    }
    serde_json::from_str(&text).map_err(|error| {
        eprintln!("{path}: error: invalid JSON input: {error}");
        ExitCode::from(1)
    })
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
}
