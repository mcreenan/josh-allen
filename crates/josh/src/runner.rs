use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use josh_protocol::{
    CatalogMetadata, CatalogSetParams, CatalogSetResult, DEFAULT_MAX_FRAME_BYTES, ExecutionMode,
    ExecutionResult, ExecutionStartParams, FrameReader, InitializeParams, InitializeResult,
    InvokingSessionId, PeerInfo, ProgramLoadParams, ProgramLoadResult, ProtocolLimits,
    RuntimeReadyParams, SCHEMA_DIALECT, SerializedWriter, SourceFile, Validate, WireError,
    WireErrorCode, WireMessage, notification_params, response_result,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::cli::{RunOptions, argument_error, parse_run};

const MAX_INPUT_BYTES: usize = 1024 * 1024;
pub(crate) fn run(arguments: &[String]) -> ExitCode {
    let program = arguments.first().map_or("josh", String::as_str);
    let options = match parse_run(program, &arguments[2..]) {
        Ok(Some(options)) => options,
        Ok(None) => return ExitCode::SUCCESS,
        Err(error) => {
            return argument_error(&error);
        }
    };
    match execute(&options) {
        Ok(outcome) => {
            match serde_json::to_string(&outcome) {
                Ok(json) => println!("{json}"),
                Err(error) => {
                    eprintln!("josh: cannot encode execution outcome: {error}");
                    return ExitCode::FAILURE;
                }
            }
            match outcome {
                ExecutionResult::Completed { .. } | ExecutionResult::Stopped { .. } => {
                    ExitCode::SUCCESS
                }
                ExecutionResult::Failed { .. } | ExecutionResult::Cancelled { .. } => {
                    ExitCode::FAILURE
                }
            }
        }
        Err(error) => {
            eprintln!("josh: {error}");
            ExitCode::FAILURE
        }
    }
}

fn execute(options: &RunOptions) -> Result<ExecutionResult, String> {
    let load = load_params(Path::new(&options.path))?;
    let explicit_input = read_input(options.input.as_deref())?;
    let catalog = read_catalog(options.catalog.as_deref())?;
    let workdir = options
        .workdir
        .as_deref()
        .map(canonical_directory)
        .transpose()?;
    let mut limits = BTreeMap::new();
    if let Some(wall_ms) = options.wall_ms {
        limits.insert("wall_ms".to_owned(), wall_ms);
    }
    let mut client = Client::spawn(options.trace_events)?;
    let result = (|| {
        client.expect_ready()?;
        let initialize = InitializeParams {
            host: PeerInfo {
                name: "josh-runner".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            protocol_versions: vec![josh_protocol::PROTOCOL_VERSION.to_owned()],
            language_versions: vec![">=0.1.0, <0.2.0".to_owned()],
            execution_mode: ExecutionMode::Unattended,
            invoking_session_id: InvokingSessionId::Null,
            standard_capabilities: options.grants.clone(),
            limits: protocol_limits(),
            extensions: Vec::new(),
        };
        let _: InitializeResult = client.request("h-1", "initialize", &initialize)?;
        let catalog_result: CatalogSetResult = client.request("h-2", "catalog/set", &catalog)?;
        let loaded: ProgramLoadResult = client.request("h-3", "program/load", &load)?;
        let input = if options.catalog_input {
            serde_json::to_value(&catalog_result)
                .map_err(|error| format!("cannot encode frozen catalog input: {error}"))?
        } else {
            explicit_input
        };
        let start = ExecutionStartParams {
            execution_id: "exec-1".to_owned(),
            program_id: loaded.program_id,
            artifact_digest: loaded.artifact_digest,
            entry: options.entry.clone(),
            input,
            working_directory: workdir,
            granted_capabilities: options.grants.clone(),
            granted_tools: Vec::new(),
            allowed_http_origins: options.allowed_http_origins.clone(),
            limits,
        };
        client.request("h-4", "execution/start", &start)
    })();
    client.finish(result.is_ok())?;
    result
}

fn read_catalog(path: Option<&str>) -> Result<CatalogSetParams, String> {
    let Some(path) = path else {
        return Ok(CatalogSetParams {
            schema_dialect: SCHEMA_DIALECT.to_owned(),
            metadata: CatalogMetadata::complete(
                "josh-runner",
                env!("CARGO_PKG_VERSION"),
                current_unix_ms()?,
            ),
            tools: Vec::new(),
        });
    };
    let text = read_bounded_utf8(Path::new(path), MAX_INPUT_BYTES)?;
    let catalog: CatalogSetParams = serde_json::from_str(&text)
        .map_err(|error| format!("catalog is not valid JSON: {error}"))?;
    catalog
        .validate()
        .map_err(|error| format!("catalog is invalid: {error}"))?;
    Ok(catalog)
}

fn current_unix_ms() -> Result<u64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch".to_owned())?;
    u64::try_from(elapsed.as_millis()).map_err(|_| "system clock value is too large".to_owned())
}

fn protocol_limits() -> ProtocolLimits {
    ProtocolLimits {
        max_frame_bytes: u64::try_from(DEFAULT_MAX_FRAME_BYTES).unwrap_or(4_194_304),
        max_active_requests: 64,
        max_loaded_programs: 32,
        max_total_executions: 1,
        max_catalog_tools: 256,
        max_catalog_bytes: 3_145_728,
    }
}

fn canonical_directory(path: &str) -> Result<String, String> {
    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("cannot use working directory '{path}': {error}"))?;
    if !canonical.is_dir() {
        return Err(format!("working directory '{path}' is not a directory"));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn read_input(specification: Option<&str>) -> Result<Value, String> {
    let Some(specification) = specification else {
        return Ok(Value::Null);
    };
    let text = if specification == "-" {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(u64::try_from(MAX_INPUT_BYTES).unwrap_or(u64::MAX) + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read input from stdin: {error}"))?;
        if bytes.len() > MAX_INPUT_BYTES {
            return Err("input exceeds 1 MiB".to_owned());
        }
        String::from_utf8(bytes).map_err(|_| "input from stdin is not UTF-8".to_owned())?
    } else if let Some(path) = specification.strip_prefix('@') {
        read_bounded_utf8(Path::new(path), MAX_INPUT_BYTES)?
    } else {
        if specification.len() > MAX_INPUT_BYTES {
            return Err("input exceeds 1 MiB".to_owned());
        }
        specification.to_owned()
    };
    serde_json::from_str(&text).map_err(|error| format!("input is not valid JSON: {error}"))
}

fn load_params(path: &Path) -> Result<ProgramLoadParams, String> {
    if path.is_dir() {
        return load_package(path);
    }
    if !path.is_file() {
        return Err(format!("'{}' is not a file or directory", path.display()));
    }
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("allen") => Ok(ProgramLoadParams::SourceBundle {
            files: vec![SourceFile {
                path: "src/main.allen".to_owned(),
                encoding: josh_protocol::FileEncoding::Utf8,
                content: read_bounded_utf8(path, DEFAULT_MAX_FRAME_BYTES)?,
            }],
        }),
        Some("allenb") => {
            let bytes = read_bounded_bytes(path, DEFAULT_MAX_FRAME_BYTES)?;
            Ok(ProgramLoadParams::Bytecode {
                artifact: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        }
        _ => Err(format!(
            "'{}' must be an .allen source file, .allenb artifact, or package directory",
            path.display()
        )),
    }
}

fn load_package(root: &Path) -> Result<ProgramLoadParams, String> {
    let manifest = root.join("allen.toml");
    if !manifest.is_file() {
        return Err(format!(
            "package directory '{}' has no allen.toml",
            root.display()
        ));
    }
    let mut paths = vec![PathBuf::from("allen.toml")];
    if root.join("allen.lock").is_file() {
        paths.push(PathBuf::from("allen.lock"));
    }
    collect_sources(root, Path::new("src"), &mut paths)?;
    paths.sort();
    let mut files = Vec::with_capacity(paths.len());
    for relative in paths {
        let path = root.join(&relative);
        files.push(SourceFile {
            path: relative.to_string_lossy().replace('\\', "/"),
            encoding: josh_protocol::FileEncoding::Utf8,
            content: read_bounded_utf8(&path, DEFAULT_MAX_FRAME_BYTES)?,
        });
    }
    Ok(ProgramLoadParams::SourceBundle { files })
}

fn collect_sources(root: &Path, relative: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    let directory = root.join(relative);
    let entries = fs::read_dir(&directory)
        .map_err(|error| format!("cannot read '{}': {error}", directory.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read package entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect '{}': {error}", entry.path().display()))?;
        let child = relative.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(format!(
                "package source '{}' must not be a symlink",
                entry.path().display()
            ));
        }
        if file_type.is_dir() {
            collect_sources(root, &child, output)?;
        } else if file_type.is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("allen")
        {
            output.push(child);
        }
    }
    Ok(())
}

fn read_bounded_utf8(path: &Path, maximum: usize) -> Result<String, String> {
    String::from_utf8(read_bounded_bytes(path, maximum)?)
        .map_err(|_| format!("'{}' is not UTF-8", path.display()))
}

fn read_bounded_bytes(path: &Path, maximum: usize) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("cannot open '{}': {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(maximum).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    if bytes.len() > maximum {
        return Err(format!(
            "'{}' exceeds the {} byte runner limit",
            path.display(),
            maximum
        ));
    }
    Ok(bytes)
}

struct Client {
    child: Child,
    writer: Option<SerializedWriter<ChildStdin>>,
    reader: FrameReader<ChildStdout>,
    trace_events: bool,
}

impl Client {
    fn spawn(trace_events: bool) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("cannot locate the josh executable: {error}"))?;
        let mut child = Command::new(executable)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("cannot start the JOSH endpoint: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "cannot open JOSH stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "cannot open JOSH stdout".to_owned())?;
        Ok(Self {
            child,
            writer: Some(SerializedWriter::new(stdin, DEFAULT_MAX_FRAME_BYTES)),
            reader: FrameReader::new(stdout, DEFAULT_MAX_FRAME_BYTES),
            trace_events,
        })
    }

    fn expect_ready(&mut self) -> Result<(), String> {
        let message = self.receive()?;
        notification_params::<RuntimeReadyParams>(&message, "runtime/ready")
            .map(|_| ())
            .map_err(|error| format!("invalid runtime/ready notification: {error}"))
    }

    fn request<P, R>(&mut self, id: &str, method: &str, params: &P) -> Result<R, String>
    where
        P: Serialize,
        R: DeserializeOwned + Validate,
    {
        let params = serde_json::to_value(params)
            .map_err(|error| format!("cannot encode {method} request: {error}"))?;
        self.send(&WireMessage::Request {
            id: id.to_owned(),
            method: method.to_owned(),
            params,
        })?;
        loop {
            let message = self.receive()?;
            match &message {
                WireMessage::Notification { method, .. } => {
                    if self.trace_events {
                        let json = serde_json::to_string(&message)
                            .unwrap_or_else(|_| format!("notification {method}"));
                        eprintln!("{json}");
                    }
                }
                WireMessage::Request {
                    id: runtime_id,
                    method,
                    ..
                } => self.reject_provider_request(runtime_id, method)?,
                WireMessage::Response {
                    id: response_id,
                    error: Some(error),
                    ..
                } if response_id == id => {
                    return Err(format!(
                        "JOSH {method} failed with {:?}: {}",
                        error.code, error.message
                    ));
                }
                WireMessage::Response {
                    id: response_id, ..
                } if response_id == id => {
                    return response_result(&message)
                        .map_err(|error| format!("invalid JOSH {method} response: {error}"));
                }
                WireMessage::Response { id, .. } => {
                    return Err(format!("received response for unexpected request '{id}'"));
                }
                WireMessage::Cancel { id, .. } => {
                    return Err(format!("runtime cancelled unexpected request '{id}'"));
                }
            }
        }
    }

    fn reject_provider_request(&self, id: &str, method: &str) -> Result<(), String> {
        let code = match method {
            "tool/invoke" => WireErrorCode::ToolUnavailable,
            method if method.starts_with("agent/") => WireErrorCode::AgentUnavailable,
            method if method.starts_with("model/") => WireErrorCode::ModelUnavailable,
            method if method.starts_with("user/") => WireErrorCode::UserUnavailable,
            method if method.starts_with("sub_agent/") => WireErrorCode::SubAgentUnavailable,
            "permission/request" => WireErrorCode::PermissionUnavailable,
            _ => WireErrorCode::RequestMethodNotFound,
        };
        self.send(&WireMessage::Response {
            id: id.to_owned(),
            result: None,
            error: Some(WireError {
                code,
                message: format!("provider method '{method}' is unavailable in josh run"),
                data: None,
            }),
        })
    }

    fn send(&self, message: &WireMessage) -> Result<(), String> {
        self.writer
            .as_ref()
            .ok_or_else(|| "JOSH stdin is closed".to_owned())?
            .write_message(message)
            .map_err(|error| error.to_string())
    }

    fn receive(&mut self) -> Result<WireMessage, String> {
        self.reader
            .read_message()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "JOSH closed stdout before completing the request".to_owned())
    }

    fn finish(&mut self, graceful: bool) -> Result<(), String> {
        self.writer.take();
        if !graceful {
            let _ = self.child.kill();
        }
        let status = self
            .child
            .wait()
            .map_err(|error| format!("cannot wait for JOSH endpoint: {error}"))?;
        if graceful && !status.success() {
            return Err(format!("JOSH endpoint exited with {status}"));
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.writer.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
