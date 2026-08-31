//! Hardened, argv-only subprocess execution shared by ALLEN hosts.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest as _, Sha256};

#[cfg(target_os = "linux")]
use std::io::{Read, Seek, SeekFrom};
#[cfg(target_os = "linux")]
use std::process::{Child, Command, ExitStatus, Stdio};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::mpsc::{self, Receiver, TryRecvError};
#[cfg(target_os = "linux")]
use std::thread;

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd as _;
#[cfg(target_os = "linux")]
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};

#[cfg(target_os = "linux")]
const MAX_EXECUTABLE_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

#[cfg(all(test, target_os = "linux"))]
static PAUSE_BEFORE_IMAGE_SEAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(test, target_os = "linux"))]
static IMAGE_READY_TO_ATTACK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(test, target_os = "linux"))]
static PROCESS_RUN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One canonical package or host command pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandPattern {
    canonical: String,
    prefix: Vec<String>,
    remaining_wildcard: bool,
}

/// A command pattern was not in the canonical manifest form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandPatternError;

impl CommandPattern {
    /// Parse the printable-ASCII, space-delimited command pattern format.
    ///
    /// The executable token must be bare and slash-free. Argument-prefix
    /// tokens may contain slashes. Quoting and escaping are forbidden, and `*`
    /// is accepted only as a final whole token.
    ///
    /// # Errors
    ///
    /// Returns [`CommandPatternError`] when the text is not canonical.
    pub fn parse(pattern: &str) -> Result<Self, CommandPatternError> {
        if pattern.is_empty()
            || pattern.starts_with(' ')
            || pattern.ends_with(' ')
            || pattern.contains("  ")
        {
            return Err(CommandPatternError);
        }
        let mut tokens = pattern.split(' ').collect::<Vec<_>>();
        let Some(binary) = tokens.first() else {
            return Err(CommandPatternError);
        };
        if binary.contains('/') || *binary == "*" {
            return Err(CommandPatternError);
        }
        for (index, token) in tokens.iter().enumerate() {
            let final_wildcard = *token == "*" && index + 1 == tokens.len();
            if token.is_empty() || token.contains('*') && !final_wildcard {
                return Err(CommandPatternError);
            }
            if !final_wildcard
                && token
                    .bytes()
                    .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'\'' | b'"' | b'\\'))
            {
                return Err(CommandPatternError);
            }
        }
        let remaining_wildcard = tokens.last() == Some(&"*");
        if remaining_wildcard {
            tokens.pop();
        }
        Ok(Self {
            canonical: pattern.to_owned(),
            prefix: tokens.into_iter().map(str::to_owned).collect(),
            remaining_wildcard,
        })
    }

    /// Return the canonical pattern text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    /// Return the bare executable token.
    #[must_use]
    pub fn executable(&self) -> &str {
        self.prefix.first().map_or("", String::as_str)
    }

    /// Return whether this pattern accepts the complete argv vector.
    #[must_use]
    pub fn matches<T: AsRef<str>>(&self, argv: &[T]) -> bool {
        if (self.remaining_wildcard && argv.len() < self.prefix.len())
            || (!self.remaining_wildcard && argv.len() != self.prefix.len())
        {
            return false;
        }
        self.prefix
            .iter()
            .zip(argv)
            .all(|(expected, actual)| expected == actual.as_ref())
    }

    /// Return whether every argv accepted by `grant` is accepted by `self`.
    #[must_use]
    pub fn covers(&self, grant: &Self) -> bool {
        if self.remaining_wildcard {
            grant.prefix.starts_with(&self.prefix)
        } else {
            !grant.remaining_wildcard && grant.prefix == self.prefix
        }
    }
}

impl fmt::Display for CommandPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for CommandPatternError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("command pattern is not canonical")
    }
}

impl std::error::Error for CommandPatternError {}

/// An immutable process-environment snapshot.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct EnvironmentSnapshot {
    values: BTreeMap<OsString, OsString>,
}

impl fmt::Debug for EnvironmentSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvironmentSnapshot")
            .field("names", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl EnvironmentSnapshot {
    /// Capture the current process environment once.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            values: std::env::vars_os().collect(),
        }
    }

    /// Create an empty environment.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Insert or replace one environment value in this snapshot.
    pub fn insert(&mut self, name: OsString, value: OsString) {
        self.values.insert(name, value);
    }

    /// Read one environment value from this snapshot.
    #[must_use]
    pub fn get(&self, name: impl AsRef<OsStr>) -> Option<&OsStr> {
        self.values.get(name.as_ref()).map(OsString::as_os_str)
    }

    /// Return a stable digest of the exact names and values in this snapshot.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        for (name, value) in &self.values {
            update_os_string_digest(&mut digest, name);
            update_os_string_digest(&mut digest, value);
        }
        digest.finalize().into()
    }

    #[cfg(target_os = "linux")]
    fn apply(&self, command: &mut Command) {
        command.env_clear().envs(&self.values);
    }
}

fn update_os_string_digest(digest: &mut Sha256, value: &OsStr) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let bytes = value.as_bytes();
        digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
        digest.update(bytes);
    }
    #[cfg(not(unix))]
    {
        let value = value.to_string_lossy();
        digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
        digest.update(value.as_bytes());
    }
}

/// An immutable executable image opened and copied during preflight.
#[derive(Clone, Debug)]
pub struct ExecutableIdentity {
    path: PathBuf,
    digest: [u8; 32],
    #[cfg(target_os = "linux")]
    image: Arc<[u8]>,
}

impl ExecutableIdentity {
    /// Return the canonical absolute executable path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the SHA-256 digest of the exact executable bytes retained at preflight.
    #[must_use]
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    #[cfg(target_os = "linux")]
    fn stage(&self) -> Result<StagedExecutable, ExecError> {
        StagedExecutable::create(&self.image)
            .map_err(|_| ExecError::new(ExecErrorKind::ExecutablePreparationFailed))
    }
}

/// Bounds applied to one subprocess invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionLimits {
    pub stdin_bytes: usize,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

/// An absolute operation deadline created before request preparation.
#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    expires_at: Instant,
}

impl Deadline {
    /// Start a deadline from a remaining budget.
    ///
    /// # Errors
    ///
    /// Returns [`ExecErrorKind::InvalidDeadline`] when the budget overflows
    /// the platform's monotonic clock.
    pub fn from_budget(budget: Duration) -> Result<Self, ExecError> {
        Instant::now()
            .checked_add(budget)
            .map(|expires_at| Self { expires_at })
            .ok_or_else(|| ExecError::new(ExecErrorKind::InvalidDeadline))
    }

    /// Fail if request preparation has exhausted this deadline.
    ///
    /// # Errors
    ///
    /// Returns [`ExecErrorKind::TimedOut`] once the deadline has elapsed.
    pub fn ensure_remaining(self) -> Result<(), ExecError> {
        if Instant::now() >= self.expires_at {
            Err(ExecError::new(ExecErrorKind::TimedOut))
        } else {
            Ok(())
        }
    }
}

/// A subprocess request. Arguments never pass through a shell.
#[derive(Clone, Debug)]
pub struct ExecutionRequest {
    pub arguments: Vec<OsString>,
    pub stdin: Vec<u8>,
    pub limits: ExecutionLimits,
    pub deadline: Deadline,
}

/// A platform-neutral subprocess exit description.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ProcessStatus {
    /// Return whether the process exited normally with status zero.
    #[must_use]
    pub fn success(self) -> bool {
        self.code == Some(0)
    }
}

/// Bounded subprocess output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionOutput {
    pub status: ProcessStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Sanitized subprocess failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecErrorKind {
    UnsupportedPlatform,
    ExecutableNotFound,
    ExecutablePreparationFailed,
    InvalidDeadline,
    InputLimitExceeded,
    StdoutLimitExceeded,
    StderrLimitExceeded,
    TimedOut,
    SpawnFailed,
    OutputReadFailed,
    TerminationFailed,
}

/// A subprocess failure that carries no child output or operating-system text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecError {
    kind: ExecErrorKind,
}

impl ExecError {
    const fn new(kind: ExecErrorKind) -> Self {
        Self { kind }
    }

    /// Return the stable failure category.
    #[must_use]
    pub fn kind(self) -> ExecErrorKind {
        self.kind
    }
}

impl fmt::Display for ExecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            ExecErrorKind::UnsupportedPlatform => "subprocess execution is unsupported",
            ExecErrorKind::ExecutableNotFound => "executable was not found",
            ExecErrorKind::ExecutablePreparationFailed => "executable could not be prepared",
            ExecErrorKind::InvalidDeadline => "subprocess deadline is invalid",
            ExecErrorKind::InputLimitExceeded => "subprocess input exceeds its limit",
            ExecErrorKind::StdoutLimitExceeded => "subprocess stdout exceeds its limit",
            ExecErrorKind::StderrLimitExceeded => "subprocess stderr exceeds its limit",
            ExecErrorKind::TimedOut => "subprocess timed out",
            ExecErrorKind::SpawnFailed => "subprocess could not be started",
            ExecErrorKind::OutputReadFailed => "subprocess output could not be read",
            ExecErrorKind::TerminationFailed => "subprocess could not be terminated",
        })
    }
}

impl std::error::Error for ExecError {}

/// A subprocess broker bound to one explicit environment snapshot.
#[derive(Clone, Debug)]
pub struct ProcessBroker {
    #[cfg(target_os = "linux")]
    environment: EnvironmentSnapshot,
    #[cfg(target_os = "linux")]
    resolution_path: Option<OsString>,
}

impl ProcessBroker {
    /// Create a broker with an immutable process environment.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(environment: EnvironmentSnapshot) -> Self {
        #[cfg(target_os = "linux")]
        {
            let resolution_path = environment.get("PATH").map(OsStr::to_os_string);
            Self {
                environment,
                resolution_path,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = environment;
            Self {}
        }
    }

    /// Create a broker whose executable search path is kept out of the child environment.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn with_resolution_path(
        environment: EnvironmentSnapshot,
        resolution_path: Option<OsString>,
    ) -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                environment,
                resolution_path,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (environment, resolution_path);
            Self {}
        }
    }

    /// Resolve one bare executable through the snapshot's `PATH`.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error when the platform is unsupported or no
    /// executable file can be resolved and inspected.
    pub fn resolve(&self, name: impl AsRef<OsStr>) -> Result<ExecutableIdentity, ExecError> {
        #[cfg(target_os = "linux")]
        {
            let name = name.as_ref();
            if name.is_empty() || Path::new(name).components().count() != 1 {
                return Err(ExecError::new(ExecErrorKind::ExecutableNotFound));
            }
            let path = self
                .resolution_path
                .as_deref()
                .ok_or_else(|| ExecError::new(ExecErrorKind::ExecutableNotFound))?;
            for directory in std::env::split_paths(path) {
                let candidate = directory.join(name);
                if !current_user_can_execute(&candidate) {
                    continue;
                }
                let Ok(canonical) = fs::canonicalize(candidate) else {
                    continue;
                };
                let Ok(mut source) = File::open(&canonical) else {
                    continue;
                };
                let Ok(opened_metadata) = source.metadata() else {
                    continue;
                };
                let Ok(path_metadata) = canonical.metadata() else {
                    continue;
                };
                if !opened_metadata.is_file()
                    || !same_file(&opened_metadata, &path_metadata)
                    || !current_user_can_execute(&canonical)
                    || opened_metadata.len() > MAX_EXECUTABLE_IMAGE_BYTES
                {
                    continue;
                }
                let mut image = Vec::new();
                if Read::take(&mut source, MAX_EXECUTABLE_IMAGE_BYTES + 1)
                    .read_to_end(&mut image)
                    .is_err()
                    || image.is_empty()
                    || u64::try_from(image.len()).ok() != Some(opened_metadata.len())
                {
                    continue;
                }
                return Ok(ExecutableIdentity {
                    path: canonical,
                    digest: Sha256::digest(&image).into(),
                    image: Arc::from(image),
                });
            }
            Err(ExecError::new(ExecErrorKind::ExecutableNotFound))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            Err(ExecError::new(ExecErrorKind::UnsupportedPlatform))
        }
    }

    /// Run an already-resolved executable with bounded pipes and group cleanup.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for image preparation, exhausted deadlines,
    /// limit violations, spawn failures, or pipe failures.
    pub fn run(
        &self,
        executable: &ExecutableIdentity,
        request: ExecutionRequest,
    ) -> Result<ExecutionOutput, ExecError> {
        #[cfg(target_os = "linux")]
        {
            request.deadline.ensure_remaining()?;
            if request.stdin.len() > request.limits.stdin_bytes {
                return Err(ExecError::new(ExecErrorKind::InputLimitExceeded));
            }
            let staged = executable.stage()?;
            request.deadline.ensure_remaining()?;

            let has_stdin = !request.stdin.is_empty();
            let mut command = Command::new(staged.path());
            command
                .args(&request.arguments)
                .stdin(if has_stdin {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
            self.environment.apply(&mut command);
            let mut child = command
                .spawn()
                .map_err(|_| ExecError::new(ExecErrorKind::SpawnFailed))?;
            let Some(stdout) = child.stdout.take() else {
                return Err(finish_failed_attempt(
                    &mut child,
                    ExecErrorKind::OutputReadFailed,
                ));
            };
            let Some(stderr) = child.stderr.take() else {
                return Err(finish_failed_attempt(
                    &mut child,
                    ExecErrorKind::OutputReadFailed,
                ));
            };
            let stdout = spawn_bounded_reader(stdout, request.limits.stdout_bytes, "stdout")
                .map_err(|_| finish_failed_attempt(&mut child, ExecErrorKind::OutputReadFailed))?;
            let stderr = spawn_bounded_reader(stderr, request.limits.stderr_bytes, "stderr")
                .map_err(|_| finish_failed_attempt(&mut child, ExecErrorKind::OutputReadFailed))?;
            if has_stdin {
                let Some(stdin) = child.stdin.take() else {
                    return Err(finish_failed_attempt(
                        &mut child,
                        ExecErrorKind::OutputReadFailed,
                    ));
                };
                spawn_stdin_writer(stdin, request.stdin).map_err(|_| {
                    finish_failed_attempt(&mut child, ExecErrorKind::OutputReadFailed)
                })?;
            }
            wait_for_process(&mut child, &stdout, &stderr, request.deadline.expires_at)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (executable, request);
            Err(ExecError::new(ExecErrorKind::UnsupportedPlatform))
        }
    }
}

/// A user-only temporary input file removed with its directory on drop.
#[derive(Debug)]
pub struct PrivateInput {
    directory: PathBuf,
    path: PathBuf,
    cleaned: bool,
}

impl PrivateInput {
    /// Create a private temporary input file.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if a user-only directory and file cannot be
    /// created. Non-Unix platforms fail closed.
    pub fn create(bytes: &[u8]) -> io::Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = bytes;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private subprocess input is unsupported",
            ));
        }
        #[cfg(unix)]
        {
            let directory = create_unique_private_directory("input")?;
            let path = directory.join("input");
            if let Err(error) = create_private_file(&path, bytes) {
                let _ = fs::remove_dir_all(&directory);
                return Err(error);
            }
            Ok(Self {
                directory,
                path,
                cleaned: false,
            })
        }
    }

    /// Return the temporary file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the private input and its containing directory before reporting
    /// the subprocess result.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when ordinary cleanup cannot remove the private
    /// directory. Drop retains a best-effort retry for unwinding paths.
    pub fn cleanup(mut self) -> io::Result<()> {
        fs::remove_dir_all(&self.directory)?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for PrivateInput {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

#[cfg(target_os = "linux")]
fn current_user_can_execute(path: &Path) -> bool {
    use rustix::fs::{Access, AtFlags, CWD, accessat};

    accessat(CWD, path, Access::EXEC_OK, AtFlags::EACCESS).is_ok()
}

#[cfg(target_os = "linux")]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct StagedExecutable {
    _file: File,
    path: PathBuf,
}

#[cfg(target_os = "linux")]
impl StagedExecutable {
    fn create(image: &[u8]) -> io::Result<Self> {
        use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};
        use rustix::io::{FdFlags, fcntl_setfd};

        let descriptor = memfd_create(
            "allen-exec-image",
            MemfdFlags::ALLOW_SEALING | MemfdFlags::EXEC,
        )
        .or_else(|_| memfd_create("allen-exec-image", MemfdFlags::ALLOW_SEALING))?;
        let mut writable = File::from(descriptor);
        writable.write_all(image)?;
        #[cfg(test)]
        if PAUSE_BEFORE_IMAGE_SEAL.load(Ordering::Acquire) {
            IMAGE_READY_TO_ATTACK.store(true, Ordering::Release);
            while PAUSE_BEFORE_IMAGE_SEAL.load(Ordering::Acquire) {
                thread::yield_now();
            }
        }
        fcntl_add_seals(
            &writable,
            SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
        )?;
        writable.seek(SeekFrom::Start(0))?;
        let mut sealed_image = Vec::new();
        writable.read_to_end(&mut sealed_image)?;
        if sealed_image != image {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sealed executable image differs from its preflight snapshot",
            ));
        }
        let sealed_path = PathBuf::from(format!("/proc/self/fd/{}", writable.as_raw_fd()));
        let file = File::open(sealed_path)?;
        drop(writable);
        fcntl_setfd(&file, FdFlags::empty())?;
        let descriptor = file.as_raw_fd();
        Ok(Self {
            _file: file,
            path: PathBuf::from(format!("/proc/self/fd/{descriptor}")),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(target_os = "linux")]
struct BoundedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

#[cfg(target_os = "linux")]
fn spawn_bounded_reader(
    reader: impl Read + Send + 'static,
    maximum: usize,
    stream: &str,
) -> io::Result<Receiver<io::Result<BoundedOutput>>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(format!("allen-exec-{stream}"))
        .spawn(move || {
            let _ = sender.send(read_bounded(reader, maximum));
        })?;
    Ok(receiver)
}

#[cfg(target_os = "linux")]
fn spawn_stdin_writer(mut writer: impl Write + Send + 'static, bytes: Vec<u8>) -> io::Result<()> {
    thread::Builder::new()
        .name("allen-exec-stdin".to_owned())
        .spawn(move || {
            let _ = writer.write_all(&bytes);
        })?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn wait_for_process(
    child: &mut Child,
    stdout_receiver: &Receiver<io::Result<BoundedOutput>>,
    stderr_receiver: &Receiver<io::Result<BoundedOutput>>,
    deadline: Instant,
) -> Result<ExecutionOutput, ExecError> {
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    loop {
        receive_output(stdout_receiver, &mut stdout)
            .and_then(|()| receive_output(stderr_receiver, &mut stderr))
            .map_err(|kind| finish_failed_attempt(child, kind))?;
        if stdout.as_ref().is_some_and(|output| output.exceeded) {
            return Err(finish_failed_attempt(
                child,
                ExecErrorKind::StdoutLimitExceeded,
            ));
        }
        if stderr.as_ref().is_some_and(|output| output.exceeded) {
            return Err(finish_failed_attempt(
                child,
                ExecErrorKind::StderrLimitExceeded,
            ));
        }
        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|_| finish_failed_attempt(child, ExecErrorKind::OutputReadFailed))?;
        }
        if stdout.is_some() && stderr.is_some() {
            if let Some(status) = status {
                return Ok(ExecutionOutput {
                    status: process_status(status),
                    stdout: stdout.take().expect("stdout checked above").bytes,
                    stderr: stderr.take().expect("stderr checked above").bytes,
                });
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(finish_failed_attempt(child, ExecErrorKind::TimedOut));
        }
        thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
}

#[cfg(target_os = "linux")]
fn receive_output(
    receiver: &Receiver<io::Result<BoundedOutput>>,
    output: &mut Option<BoundedOutput>,
) -> Result<(), ExecErrorKind> {
    if output.is_some() {
        return Ok(());
    }
    match receiver.try_recv() {
        Ok(Ok(value)) => {
            *output = Some(value);
            Ok(())
        }
        Ok(Err(_)) | Err(TryRecvError::Disconnected) => Err(ExecErrorKind::OutputReadFailed),
        Err(TryRecvError::Empty) => Ok(()),
    }
}

#[cfg(target_os = "linux")]
fn finish_failed_attempt(child: &mut Child, kind: ExecErrorKind) -> ExecError {
    terminate_and_reap(child).map_or_else(|error| error, |()| ExecError::new(kind))
}

#[cfg(target_os = "linux")]
fn terminate_and_reap(child: &mut Child) -> Result<(), ExecError> {
    let leader_exited = match child.try_wait() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(_) => return Err(ExecError::new(ExecErrorKind::TerminationFailed)),
    };
    // The group leader can exit while a descendant still holds an inherited
    // output pipe open. Kill the group even after the leader has been reaped.
    terminate_process_group(child, leader_exited)?;
    if leader_exited {
        return Ok(());
    }
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(1))
        .ok_or_else(|| ExecError::new(ExecErrorKind::TerminationFailed))?;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(1)),
            Ok(None) | Err(_) => {
                return Err(ExecError::new(ExecErrorKind::TerminationFailed));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn terminate_process_group(child: &mut Child, leader_exited: bool) -> Result<(), ExecError> {
    use rustix::io::Errno;
    use rustix::process::{Pid, Signal, kill_process_group};

    let process_group = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    match process_group.map(|pid| kill_process_group(pid, Signal::KILL)) {
        Some(Ok(())) => Ok(()),
        Some(Err(Errno::SRCH)) if leader_exited => Ok(()),
        Some(Err(_)) | None if !leader_exited => child
            .kill()
            .map_err(|_| ExecError::new(ExecErrorKind::TerminationFailed)),
        Some(Err(_)) | None => Err(ExecError::new(ExecErrorKind::TerminationFailed)),
    }
}

#[cfg(target_os = "linux")]
fn read_bounded(mut reader: impl Read, maximum: usize) -> io::Result<BoundedOutput> {
    let mut bytes = Vec::new();
    let mut exceeded = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let remaining = maximum.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..count.min(remaining)]);
        if count > remaining {
            exceeded = true;
            break;
        }
    }
    Ok(BoundedOutput { bytes, exceeded })
}

#[cfg(target_os = "linux")]
fn process_status(status: ExitStatus) -> ProcessStatus {
    ProcessStatus {
        code: status.code(),
        #[cfg(unix)]
        signal: status.signal(),
        #[cfg(not(unix))]
        signal: None,
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(unix)]
fn create_unique_private_directory(label: &str) -> io::Result<PathBuf> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..32 {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "allen-exec-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        match create_private_directory(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate private subprocess directory",
    ))
}

#[cfg(unix)]
fn create_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file: File = options.open(path)?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_patterns_match_and_cover_with_only_a_final_whole_token_wildcard() {
        let exact = CommandPattern::parse("git status").unwrap();
        let prefix = CommandPattern::parse("aws cloudwatch *").unwrap();

        assert_eq!(exact.as_str(), "git status");
        assert_eq!(exact.executable(), "git");
        assert!(exact.matches(&["git", "status"]));
        assert!(!exact.matches(&["git", "status", "--short"]));
        assert!(prefix.matches(&["aws", "cloudwatch"]));
        assert!(prefix.matches(&["aws", "cloudwatch", "describe-alarms"]));
        assert!(!prefix.matches(&["aws", "s3", "ls"]));

        assert!(prefix.covers(&CommandPattern::parse("aws cloudwatch").unwrap()));
        assert!(prefix.covers(&CommandPattern::parse("aws cloudwatch logs *").unwrap()));
        assert!(!prefix.covers(&CommandPattern::parse("aws *").unwrap()));
        assert!(!exact.covers(&CommandPattern::parse("git status *").unwrap()));
    }

    #[test]
    fn command_patterns_reject_noncanonical_or_non_ascii_tokens() {
        for invalid in [
            "",
            " aws",
            "aws ",
            "aws  cloudwatch",
            "aws\tcloudwatch",
            "aw\u{200d}s cloudwatch",
            "åws cloudwatch",
            "aws café",
            "aws \"cloudwatch\"",
            "aws 'cloudwatch'",
            "aws \\cloudwatch",
            "/usr/bin/aws cloudwatch",
            "bin/aws cloudwatch",
            "*",
            "aws cloud*",
            "aws * cloudwatch",
            "aws cloudwatch * more",
        ] {
            assert_eq!(
                CommandPattern::parse(invalid),
                Err(CommandPatternError),
                "{invalid:?}"
            );
        }
        assert!(CommandPattern::parse("aws /var/log/messages").is_ok());
    }

    #[cfg(unix)]
    mod unix {
        use std::os::unix::fs::PermissionsExt as _;

        use super::*;

        #[cfg(target_os = "linux")]
        fn process_run_test_guard() -> std::sync::MutexGuard<'static, ()> {
            PROCESS_RUN_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        #[cfg(target_os = "linux")]
        struct Fixture {
            root: PathBuf,
            broker: ProcessBroker,
            executable: ExecutableIdentity,
        }

        #[cfg(target_os = "linux")]
        impl Fixture {
            fn new(body: &str) -> Self {
                let nonce = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let root = std::env::temp_dir()
                    .join(format!("allen-exec-test-{}-{nonce}", std::process::id()));
                let bin = root.join("bin");
                fs::create_dir_all(&bin).unwrap();
                let executable_path = bin.join("fixture-command");
                fs::write(&executable_path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
                let mut permissions = fs::metadata(&executable_path).unwrap().permissions();
                permissions.set_mode(0o700);
                fs::set_permissions(&executable_path, permissions).unwrap();
                let inherited_path = std::env::var_os("PATH").unwrap_or_default();
                let mut paths = vec![bin];
                paths.extend(std::env::split_paths(&inherited_path));
                let mut environment = EnvironmentSnapshot::empty();
                environment.insert(OsString::from("PATH"), std::env::join_paths(paths).unwrap());
                environment.insert(OsString::from("BROKER_VALUE"), OsString::from("snapshot"));
                let broker = ProcessBroker::new(environment);
                let executable = broker.resolve("fixture-command").unwrap();
                Self {
                    root,
                    broker,
                    executable,
                }
            }

            fn run(&self, arguments: &[&str], stdin: &[u8]) -> Result<ExecutionOutput, ExecError> {
                self.broker.run(
                    &self.executable,
                    ExecutionRequest {
                        arguments: arguments.iter().map(OsString::from).collect(),
                        stdin: stdin.to_vec(),
                        limits: ExecutionLimits {
                            stdin_bytes: 1024,
                            stdout_bytes: 1024,
                            stderr_bytes: 1024,
                        },
                        deadline: Deadline::from_budget(Duration::from_secs(2)).unwrap(),
                    },
                )
            }
        }

        #[cfg(target_os = "linux")]
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.root);
            }
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn broker_uses_argv_pipes_and_the_explicit_environment_snapshot() {
            let _guard = process_run_test_guard();
            let fixture = Fixture::new(
                r#"printf '%s\n' "$1"
printf '%s\n' "$BROKER_VALUE"
cat
printf '%s' 'bounded stderr' >&2"#,
            );
            let hostile = "$(touch must-not-run)";
            let output = fixture.run(&[hostile], b"stdin bytes").unwrap();

            assert!(output.status.success());
            assert_eq!(
                output.stdout,
                b"$(touch must-not-run)\nsnapshot\nstdin bytes"
            );
            assert_eq!(output.stderr, b"bounded stderr");
            assert!(!fixture.root.join("must-not-run").exists());
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn broker_enforces_input_output_and_deadline_limits() {
            let _guard = process_run_test_guard();
            let input_fixture = Fixture::new("cat");
            let input_error = input_fixture
                .broker
                .run(
                    &input_fixture.executable,
                    ExecutionRequest {
                        arguments: Vec::new(),
                        stdin: b"too large".to_vec(),
                        limits: ExecutionLimits {
                            stdin_bytes: 1,
                            stdout_bytes: 1024,
                            stderr_bytes: 1024,
                        },
                        deadline: Deadline::from_budget(Duration::from_secs(1)).unwrap(),
                    },
                )
                .unwrap_err();
            assert_eq!(input_error.kind(), ExecErrorKind::InputLimitExceeded);

            let output_fixture = Fixture::new("printf '%s' 'too large'");
            let output_error = output_fixture
                .broker
                .run(
                    &output_fixture.executable,
                    ExecutionRequest {
                        arguments: Vec::new(),
                        stdin: Vec::new(),
                        limits: ExecutionLimits {
                            stdin_bytes: 0,
                            stdout_bytes: 1,
                            stderr_bytes: 1,
                        },
                        deadline: Deadline::from_budget(Duration::from_secs(1)).unwrap(),
                    },
                )
                .unwrap_err();
            assert_eq!(output_error.kind(), ExecErrorKind::StdoutLimitExceeded);

            let timeout_fixture = Fixture::new("while :; do :; done");
            let timeout_error = timeout_fixture
                .broker
                .run(
                    &timeout_fixture.executable,
                    ExecutionRequest {
                        arguments: Vec::new(),
                        stdin: Vec::new(),
                        limits: ExecutionLimits {
                            stdin_bytes: 0,
                            stdout_bytes: 1,
                            stderr_bytes: 1,
                        },
                        deadline: Deadline::from_budget(Duration::from_millis(20)).unwrap(),
                    },
                )
                .unwrap_err();
            assert_eq!(timeout_error.kind(), ExecErrorKind::TimedOut);
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn executable_image_ignores_a_path_replacement_after_preflight() {
            let _guard = process_run_test_guard();
            let fixture = Fixture::new("printf original");
            let replacement = fixture.root.join("replacement");
            fs::write(&replacement, "#!/bin/sh\nprintf replacement\n").unwrap();
            let mut permissions = fs::metadata(&replacement).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&replacement, permissions).unwrap();
            fs::rename(replacement, fixture.executable.path()).unwrap();

            let output = fixture.run(&[], b"").unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"original");
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn executable_image_ignores_an_in_place_overwrite_after_preflight() {
            let _guard = process_run_test_guard();
            let fixture = Fixture::new("printf original");
            let mut replacement = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(fixture.executable.path())
                .unwrap();
            replacement
                .write_all(b"#!/bin/sh\nprintf replacement\n")
                .unwrap();
            drop(replacement);

            for _ in 0..2 {
                let output = fixture.run(&[], b"").unwrap();
                assert!(output.status.success());
                assert_eq!(output.stdout, b"original");
            }
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn a_live_descriptor_mutation_cannot_change_the_sealed_image() {
            use std::sync::atomic::Ordering;

            let _guard = process_run_test_guard();
            let fixture = Fixture::new("printf original");
            IMAGE_READY_TO_ATTACK.store(false, Ordering::Release);
            PAUSE_BEFORE_IMAGE_SEAL.store(true, Ordering::Release);
            let runner = std::thread::spawn(move || fixture.run(&[], b""));

            while !IMAGE_READY_TO_ATTACK.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            let mut changed = false;
            for entry in fs::read_dir("/proc/self/fd").unwrap() {
                let path = entry.unwrap().path();
                let Ok(target) = fs::read_link(&path) else {
                    continue;
                };
                if !target.to_string_lossy().contains("memfd:allen-exec-image") {
                    continue;
                }
                if let Ok(mut image) = OpenOptions::new().write(true).truncate(true).open(path) {
                    image
                        .write_all(b"#!/bin/sh\nprintf attacker-controlled\n")
                        .unwrap();
                    changed = true;
                    break;
                }
            }
            PAUSE_BEFORE_IMAGE_SEAL.store(false, Ordering::Release);
            assert!(changed, "the live-style attacker did not locate the memfd");

            let error = runner.join().unwrap().unwrap_err();
            assert_eq!(error.kind(), ExecErrorKind::ExecutablePreparationFailed);
        }

        #[test]
        #[cfg(target_os = "macos")]
        fn macos_fails_closed_without_creating_an_executable_stage() {
            use std::collections::BTreeSet;

            let before = fs::read_dir(std::env::temp_dir())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("allen-exec-image-")
                })
                .map(|entry| entry.path())
                .collect::<BTreeSet<_>>();
            let mut environment = EnvironmentSnapshot::capture();
            environment.insert(OsString::from("PATH"), OsString::from("/bin:/usr/bin"));
            let broker = ProcessBroker::new(environment);

            let error = broker.resolve("sh").unwrap_err();
            assert_eq!(error.kind(), ExecErrorKind::UnsupportedPlatform);
            let after = fs::read_dir(std::env::temp_dir())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("allen-exec-image-")
                })
                .map(|entry| entry.path())
                .collect::<BTreeSet<_>>();
            assert_eq!(after, before);
        }

        #[test]
        fn private_input_cleans_siblings_without_following_external_symlinks() {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let outside = std::env::temp_dir()
                .join(format!("allen-exec-outside-{}-{nonce}", std::process::id()));
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("must-remain"), b"outside").unwrap();

            let private = PrivateInput::create(b"secret").unwrap();
            let path = private.path().to_owned();
            let directory = path.parent().unwrap().to_owned();
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(fs::read(&path).unwrap(), b"secret");
            fs::write(directory.join("input.copy"), b"provider copy").unwrap();
            std::os::unix::fs::symlink(&outside, directory.join("outside-link")).unwrap();
            private.cleanup().unwrap();
            assert!(!path.exists());
            assert!(!directory.exists());
            assert_eq!(fs::read(outside.join("must-remain")).unwrap(), b"outside");
            fs::remove_dir_all(outside).unwrap();
        }
    }
}
