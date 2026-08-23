#![forbid(unsafe_code)]

//! Implementation of the descriptor-relative work-directory broker.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt, OpenOptionsSyncExt};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use cap_std::ambient_authority;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use cap_std::fs::OpenOptionsExt;
use cap_std::fs::{Dir, OpenOptions};

const TEMP_PREFIX: &str = ".allen-tmp-";
const TEMP_ATTEMPTS: u64 = 128;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The rights attached to one workspace capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rights {
    /// Permit reads and directory listings.
    pub read: bool,
    /// Permit atomic file writes.
    pub write: bool,
}

impl Rights {
    /// No filesystem rights.
    pub const NONE: Self = Self::new(false, false);
    /// Read and list rights only.
    pub const READ_ONLY: Self = Self::new(true, false);
    /// Read, list, and write rights.
    pub const READ_WRITE: Self = Self::new(true, true);

    /// Construct an exact rights set.
    #[must_use]
    pub const fn new(read: bool, write: bool) -> Self {
        Self { read, write }
    }
}

/// Limits enforced by one workspace broker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceLimits {
    /// Maximum UTF-8 bytes in a source path.
    pub max_path_bytes: usize,
    /// Maximum number of components in a source path.
    pub max_path_depth: usize,
    /// Maximum bytes in one file operation.
    pub max_file_bytes: u64,
    /// Maximum names returned by one directory listing.
    pub max_entries: usize,
    /// Maximum filesystem calls for the execution.
    pub max_operations: u64,
    /// Maximum total bytes read for the execution.
    pub max_read_bytes: u64,
    /// Maximum total bytes submitted for writes for the execution.
    pub max_write_bytes: u64,
}

impl Default for WorkspaceLimits {
    fn default() -> Self {
        Self {
            max_path_bytes: 4_096,
            max_path_depth: 256,
            max_file_bytes: 16 * 1024 * 1024,
            max_entries: 4_096,
            max_operations: 10_000,
            max_read_bytes: 64 * 1024 * 1024,
            max_write_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Charged filesystem usage for one broker.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceUsage {
    /// Filesystem calls started.
    pub operations: u64,
    /// Bytes read from opened files.
    pub read_bytes: u64,
    /// Bytes submitted for writes.
    pub write_bytes: u64,
}

/// One line containing a literal filesystem search query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchMatch {
    /// Normalized path relative to the capability root.
    pub path: String,
    /// One-based line number.
    pub line: i64,
    /// One-based UTF-8 byte column of the first match on the line.
    pub column: i64,
    /// Matching line without its line ending.
    pub text: String,
}

/// Execution-wide filesystem accounting shared by every capability root.
#[derive(Clone)]
pub struct ExecutionAccounting {
    limits: WorkspaceLimits,
    usage: Arc<Mutex<WorkspaceUsage>>,
}

impl ExecutionAccounting {
    /// Create one empty accounting state for an execution.
    #[must_use]
    pub fn new(limits: WorkspaceLimits) -> Self {
        Self {
            limits,
            usage: Arc::new(Mutex::new(WorkspaceUsage::default())),
        }
    }

    /// Return the immutable limits enforced by every sharing broker.
    #[must_use]
    pub const fn limits(&self) -> WorkspaceLimits {
        self.limits
    }

    /// Return a snapshot of the shared usage.
    #[must_use]
    pub fn usage(&self) -> WorkspaceUsage {
        self.usage.lock().map_or_else(
            |_| WorkspaceUsage {
                operations: self.limits.max_operations,
                read_bytes: self.limits.max_read_bytes,
                write_bytes: self.limits.max_write_bytes,
            },
            |usage| *usage,
        )
    }
}

impl fmt::Debug for ExecutionAccounting {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionAccounting")
            .field("limits", &self.limits)
            .field("usage", &self.usage())
            .finish()
    }
}

/// A stable, non-sensitive filesystem error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileErrorCode {
    /// The workspace platform is not supported.
    UnsupportedPlatform,
    /// The workspace right was not granted.
    PermissionDenied,
    /// The source path is not a valid normalized language path.
    InvalidPath,
    /// The target does not exist.
    NotFound,
    /// A path component is not a directory.
    NotDirectory,
    /// A directory was used as a file.
    IsDirectory,
    /// A symbolic link was denied.
    SymlinkDenied,
    /// A multiply-linked regular file was denied.
    HardLinkDenied,
    /// A retained target changed before a safe operation could use it.
    TargetChanged,
    /// A non-regular, non-directory filesystem object was denied.
    SpecialFileDenied,
    /// A file exceeds the per-operation byte limit.
    FileTooLarge,
    /// A directory listing exceeds its entry limit.
    TooManyEntries,
    /// The operation limit is exhausted.
    OperationLimit,
    /// The cumulative read-byte limit is exhausted.
    ReadLimit,
    /// The cumulative write-byte limit is exhausted.
    WriteLimit,
    /// File content or a directory name is not valid UTF-8.
    InvalidUtf8,
    /// The operating system rejected a safe descriptor-relative operation.
    Io,
}

impl FileErrorCode {
    /// Return the stable source-visible code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "fs.unsupported_platform",
            Self::PermissionDenied => "fs.permission_denied",
            Self::InvalidPath => "fs.invalid_path",
            Self::NotFound => "fs.not_found",
            Self::NotDirectory => "fs.not_directory",
            Self::IsDirectory => "fs.is_directory",
            Self::SymlinkDenied => "fs.symlink_denied",
            Self::HardLinkDenied => "fs.hard_link_denied",
            Self::TargetChanged => "fs.target_changed",
            Self::SpecialFileDenied => "fs.special_file_denied",
            Self::FileTooLarge => "fs.file_too_large",
            Self::TooManyEntries => "fs.too_many_entries",
            Self::OperationLimit => "resource.fs_operations",
            Self::ReadLimit => "resource.fs_read_bytes",
            Self::WriteLimit => "resource.fs_write_bytes",
            Self::InvalidUtf8 => "fs.invalid_utf8",
            Self::Io => "fs.io",
        }
    }
}

impl fmt::Display for FileErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A safe filesystem failure which never includes an ambient path or raw OS error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileError {
    /// Stable machine-readable error code.
    pub code: FileErrorCode,
    /// Stable safe message.
    pub message: &'static str,
}

impl FileError {
    const fn new(code: FileErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Construct a safe capability-denial error without operating-system detail.
    #[must_use]
    pub const fn permission_denied() -> Self {
        Self::new(
            FileErrorCode::PermissionDenied,
            "the workspace capability was not granted",
        )
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for FileError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &cap_fs_ext::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// The filesystem object kind retained while an external decision is pending.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalTargetKind {
    /// One regular file, or one absent file that a write grant can create.
    File,
    /// One existing directory.
    Directory,
}

enum RetainedRoot {
    ExistingFile {
        parent: Dir,
        name: String,
        file: cap_std::fs::File,
        identity: FileIdentity,
    },
    ExistingDirectory {
        directory: Dir,
    },
    AbsentFile {
        parent: Dir,
        name: String,
    },
}

/// One descriptor-retained external target awaiting a narrower decision.
pub struct RetainedExternalTarget {
    root: RetainedRoot,
    diagnostic_path: PathBuf,
    requested_rights: Rights,
    requested_recursive: bool,
}

impl fmt::Debug for RetainedExternalTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedExternalTarget")
            .field("kind", &self.kind())
            .field("requested_rights", &self.requested_rights)
            .field("requested_recursive", &self.requested_recursive)
            .finish_non_exhaustive()
    }
}

impl RetainedExternalTarget {
    /// Resolve and retain one absolute external file target before a decision.
    ///
    /// A missing file is accepted only when `requested_rights` includes write
    /// authority. The retained parent descriptor and final component preserve
    /// the create target without reopening its ambient path.
    ///
    /// # Errors
    ///
    /// Returns a safe error for a relative, non-normalized, symlinked,
    /// multiply-linked, special, or physically unavailable target.
    pub fn retain_file(
        path: impl AsRef<Path>,
        requested_rights: Rights,
        limits: WorkspaceLimits,
    ) -> Result<Self, FileError> {
        require_some_right(requested_rights)?;
        let (diagnostic_path, components) = normalized_absolute(path.as_ref(), limits)?;
        let (parent, name) = open_absolute_parent(&components)?;
        let root = match parent.symlink_metadata(&name) {
            Ok(metadata) => {
                validate_regular(&metadata)?;
                let identity = FileIdentity::from_metadata(&metadata);
                let file = open_retained_file(&parent, &name, requested_rights)?;
                let opened = file.metadata().map_err(map_io)?;
                validate_regular(&opened)?;
                if FileIdentity::from_metadata(&opened) != identity {
                    return Err(target_changed());
                }
                RetainedRoot::ExistingFile {
                    parent,
                    name,
                    file,
                    identity,
                }
            }
            Err(io_error)
                if io_error.kind() == io::ErrorKind::NotFound && requested_rights.write =>
            {
                RetainedRoot::AbsentFile { parent, name }
            }
            Err(io_error) => return Err(map_io(io_error)),
        };
        Ok(Self {
            root,
            diagnostic_path,
            requested_rights,
            requested_recursive: false,
        })
    }

    /// Resolve and retain one absolute existing external directory before a decision.
    ///
    /// # Errors
    ///
    /// Returns a safe error for a relative, non-normalized, missing,
    /// symlinked, or non-directory target.
    pub fn retain_directory(
        path: impl AsRef<Path>,
        requested_rights: Rights,
        requested_recursive: bool,
        limits: WorkspaceLimits,
    ) -> Result<Self, FileError> {
        require_some_right(requested_rights)?;
        let (diagnostic_path, components) = normalized_absolute(path.as_ref(), limits)?;
        let directory = open_absolute_directory(&components)?;
        Ok(Self {
            root: RetainedRoot::ExistingDirectory { directory },
            diagnostic_path,
            requested_rights,
            requested_recursive,
        })
    }

    /// Return the retained target kind without exposing authority.
    #[must_use]
    pub const fn kind(&self) -> ExternalTargetKind {
        match &self.root {
            RetainedRoot::ExistingFile { .. } | RetainedRoot::AbsentFile { .. } => {
                ExternalTargetKind::File
            }
            RetainedRoot::ExistingDirectory { .. } => ExternalTargetKind::Directory,
        }
    }

    /// Return the canonical diagnostic path presented to a decision provider.
    ///
    /// This path is metadata. Grant creation uses only retained descriptors.
    #[must_use]
    pub fn diagnostic_path(&self) -> &Path {
        &self.diagnostic_path
    }

    /// Return the maximum rights in the original request.
    #[must_use]
    pub const fn requested_rights(&self) -> Rights {
        self.requested_rights
    }

    /// Return whether the original directory request allowed recursion.
    ///
    /// File requests always return `false`.
    #[must_use]
    pub const fn requested_recursive(&self) -> bool {
        self.requested_recursive
    }

    /// Consume the pending target and create one equal or narrower broker.
    ///
    /// A file decision must keep the exact file path and cannot be recursive.
    /// A directory decision can select the retained directory or an existing
    /// descriptor-relative descendant directory. All narrowing components are
    /// opened without following symlinks.
    ///
    /// # Errors
    ///
    /// Returns a safe error when the decision broadens rights or recursion,
    /// changes the file target, leaves the retained directory, encounters an
    /// unsafe object, or observes a changed retained file identity.
    pub fn into_grant(
        self,
        narrowed_path: impl AsRef<Path>,
        rights: Rights,
        recursive: bool,
        accounting: ExecutionAccounting,
    ) -> Result<WorkspaceBroker, FileError> {
        let limits = accounting.limits();
        self.into_grant_with_limits(narrowed_path, rights, recursive, limits, accounting)
    }

    /// Consume the pending target and create one equal or narrower broker with
    /// limits that can only narrow the execution-wide ceilings.
    ///
    /// # Errors
    ///
    /// Returns the errors from [`Self::into_grant`] and rejects any local
    /// limit that exceeds the shared execution limit.
    pub fn into_grant_with_limits(
        self,
        narrowed_path: impl AsRef<Path>,
        rights: Rights,
        recursive: bool,
        grant_limits: WorkspaceLimits,
        accounting: ExecutionAccounting,
    ) -> Result<WorkspaceBroker, FileError> {
        if !rights_subset(rights, self.requested_rights) || recursive && !self.requested_recursive {
            return Err(error(
                FileErrorCode::PermissionDenied,
                "the external grant decision broadens the request",
            ));
        }
        if !limits_subset(grant_limits, accounting.limits()) {
            return Err(error(
                FileErrorCode::PermissionDenied,
                "the external grant decision broadens the byte ceiling",
            ));
        }
        let narrowed_path = normalized_absolute(narrowed_path.as_ref(), grant_limits)?.0;
        match self.root {
            RetainedRoot::ExistingFile {
                parent,
                name,
                file,
                identity,
            } => {
                if recursive || narrowed_path != self.diagnostic_path {
                    return Err(error(
                        FileErrorCode::PermissionDenied,
                        "a file grant decision must keep the requested file",
                    ));
                }
                validate_retained_name(&parent, &name, identity)?;
                Ok(WorkspaceBroker::from_file_root(
                    parent,
                    name,
                    Some((file, identity)),
                    rights,
                    grant_limits,
                    accounting,
                ))
            }
            RetainedRoot::AbsentFile { parent, name } => {
                if recursive || narrowed_path != self.diagnostic_path {
                    return Err(error(
                        FileErrorCode::PermissionDenied,
                        "a file grant decision must keep the requested file",
                    ));
                }
                match parent.symlink_metadata(&name) {
                    Err(io_error) if io_error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => return Err(target_changed()),
                    Err(io_error) => return Err(map_io(io_error)),
                }
                Ok(WorkspaceBroker::from_file_root(
                    parent,
                    name,
                    None,
                    rights,
                    grant_limits,
                    accounting,
                ))
            }
            RetainedRoot::ExistingDirectory { directory } => {
                let relative = narrowed_path
                    .strip_prefix(&self.diagnostic_path)
                    .map_err(|_| {
                        error(
                            FileErrorCode::PermissionDenied,
                            "the directory grant decision leaves the requested target",
                        )
                    })?;
                let components = normalized_relative_components(relative, grant_limits)?;
                let mut narrowed = directory;
                for component in components {
                    narrowed = open_child_directory(&narrowed, &component)?;
                }
                Ok(WorkspaceBroker::from_directory_root(
                    narrowed,
                    rights,
                    recursive,
                    grant_limits,
                    accounting,
                ))
            }
        }
    }
}

enum CapabilityRoot {
    Directory {
        directory: Dir,
        recursive: bool,
    },
    File {
        parent: Dir,
        name: String,
        state: Mutex<FileRootState>,
    },
}

struct FileRootState {
    retained: Option<cap_std::fs::File>,
    identity: Option<FileIdentity>,
}

/// An opened, descriptor-relative workspace or external grant capability.
pub struct WorkspaceBroker {
    root: CapabilityRoot,
    rights: Rights,
    limits: WorkspaceLimits,
    accounting: ExecutionAccounting,
}

impl fmt::Debug for WorkspaceBroker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceBroker")
            .field(
                "kind",
                &match &self.root {
                    CapabilityRoot::Directory { .. } => ExternalTargetKind::Directory,
                    CapabilityRoot::File { .. } => ExternalTargetKind::File,
                },
            )
            .field("rights", &self.rights)
            .field("limits", &self.limits)
            .field("usage", &self.usage())
            .finish_non_exhaustive()
    }
}

impl WorkspaceBroker {
    /// Open an explicitly selected ambient directory and confine later access to it.
    ///
    /// # Errors
    ///
    /// Returns a safe error when the platform is unsupported or the root cannot be opened as a
    /// directory.
    pub fn open_ambient(
        root: impl AsRef<Path>,
        rights: Rights,
        limits: WorkspaceLimits,
    ) -> Result<Self, FileError> {
        Self::open_ambient_with_accounting(root, rights, ExecutionAccounting::new(limits))
    }

    /// Open a work directory that shares execution-wide accounting with other roots.
    ///
    /// # Errors
    ///
    /// Returns a safe error when the platform is unsupported or the root cannot be opened as a
    /// directory.
    pub fn open_ambient_with_accounting(
        root: impl AsRef<Path>,
        rights: Rights,
        accounting: ExecutionAccounting,
    ) -> Result<Self, FileError> {
        let _limits = accounting.limits();
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (root, rights, _limits, accounting);
            return Err(error(
                FileErrorCode::UnsupportedPlatform,
                "the workspace platform is not supported",
            ));
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let root = Dir::open_ambient_dir(root, ambient_authority()).map_err(map_io)?;
            let metadata = root.dir_metadata().map_err(map_io)?;
            if !metadata.is_dir() {
                return Err(error(
                    FileErrorCode::NotDirectory,
                    "the workspace root is not a directory",
                ));
            }
            let limits = accounting.limits();
            Ok(Self::from_directory_root(
                root, rights, true, limits, accounting,
            ))
        }
    }

    fn from_directory_root(
        directory: Dir,
        rights: Rights,
        recursive: bool,
        limits: WorkspaceLimits,
        accounting: ExecutionAccounting,
    ) -> Self {
        Self {
            root: CapabilityRoot::Directory {
                directory,
                recursive,
            },
            rights,
            limits,
            accounting,
        }
    }

    fn from_file_root(
        parent: Dir,
        name: String,
        retained: Option<(cap_std::fs::File, FileIdentity)>,
        rights: Rights,
        limits: WorkspaceLimits,
        accounting: ExecutionAccounting,
    ) -> Self {
        let (retained, identity) = retained.map_or((None, None), |(file, identity)| {
            (Some(file), Some(identity))
        });
        Self {
            root: CapabilityRoot::File {
                parent,
                name,
                state: Mutex::new(FileRootState { retained, identity }),
            },
            rights,
            limits,
            accounting,
        }
    }

    /// Return the rights attached to this workspace.
    #[must_use]
    pub const fn rights(&self) -> Rights {
        self.rights
    }

    /// Return the broker limits.
    #[must_use]
    pub const fn limits(&self) -> WorkspaceLimits {
        self.limits
    }

    /// Return a snapshot of charged usage.
    #[must_use]
    pub fn usage(&self) -> WorkspaceUsage {
        self.accounting.usage()
    }

    /// Read one regular file as bytes.
    ///
    /// # Errors
    ///
    /// Returns a safe error for a denied right, invalid path, denied filesystem object, exhausted
    /// limit, concurrent file change, or descriptor I/O failure.
    pub fn read_bytes(&self, path: &str) -> Result<Vec<u8>, FileError> {
        self.require_read()?;
        match &self.root {
            CapabilityRoot::Directory { .. } => {
                let path = ValidatedPath::file(path, self.limits)?;
                self.charge_operation()?;
                let (parent, name) = self.open_parent(&path)?;
                let path_metadata = parent.symlink_metadata(name).map_err(map_io)?;
                validate_regular(&path_metadata)?;
                let mut options = OpenOptions::new();
                options.read(true);
                options.follow(FollowSymlinks::No);
                options.nonblock(true);
                let mut file = parent.open_with(name, &options).map_err(map_io)?;
                self.read_opened_file(&mut file)
            }
            CapabilityRoot::File { state, .. } => {
                require_file_root_path(path)?;
                self.charge_operation()?;
                let mut state = state.lock().map_err(|_| {
                    error(FileErrorCode::Io, "the retained file state is unavailable")
                })?;
                let file = state.retained.as_mut().ok_or_else(|| {
                    error(FileErrorCode::NotFound, "the retained file does not exist")
                })?;
                file.seek(SeekFrom::Start(0)).map_err(map_io)?;
                self.read_opened_file(file)
            }
        }
    }

    fn read_opened_file(&self, file: &mut cap_std::fs::File) -> Result<Vec<u8>, FileError> {
        let before = file.metadata().map_err(map_io)?;
        validate_regular(&before)?;
        let length = before.len();
        self.reserve_read(length)?;

        let capacity = usize::try_from(length).map_err(|_| {
            error(
                FileErrorCode::FileTooLarge,
                "the file exceeds the byte limit",
            )
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        let read_result = Read::by_ref(file).take(length).read_to_end(&mut bytes);
        if let Err(io_error) = read_result {
            self.refund_read(length.saturating_sub(bytes.len() as u64));
            return Err(map_io(io_error));
        }
        self.refund_read(length.saturating_sub(bytes.len() as u64));
        let after = file.metadata().map_err(map_io)?;
        validate_regular(&after)?;
        if after.len() != bytes.len() as u64 {
            return Err(error(
                FileErrorCode::Io,
                "the file changed while it was read",
            ));
        }
        Ok(bytes)
    }

    /// Read one regular UTF-8 file.
    ///
    /// # Errors
    ///
    /// Returns the errors from [`Self::read_bytes`] and rejects invalid UTF-8 content.
    pub fn read_text(&self, path: &str) -> Result<String, FileError> {
        String::from_utf8(self.read_bytes(path)?)
            .map_err(|_| error(FileErrorCode::InvalidUtf8, "the file is not valid UTF-8"))
    }

    /// Atomically create or replace one regular file from bytes.
    ///
    /// # Errors
    ///
    /// Returns a safe error for a denied right, invalid path, denied filesystem object, exhausted
    /// limit, or descriptor I/O failure. An uncommitted temporary file is removed on failure.
    pub fn write_bytes(&self, path: &str, value: &[u8]) -> Result<(), FileError> {
        self.require_write()?;
        let byte_count = value.len() as u64;
        match &self.root {
            CapabilityRoot::Directory { .. } => {
                let path = ValidatedPath::file(path, self.limits)?;
                self.charge_write(byte_count)?;
                let (parent, name) = self.open_parent(&path)?;
                match parent.symlink_metadata(name) {
                    Ok(metadata) => validate_regular(&metadata)?,
                    Err(io_error) if io_error.kind() == io::ErrorKind::NotFound => {}
                    Err(io_error) => return Err(map_io(io_error)),
                }
                atomic_replace(&parent, name, value)
            }
            CapabilityRoot::File {
                parent,
                name,
                state,
            } => {
                require_file_root_path(path)?;
                self.charge_write(byte_count)?;
                let mut state = state.lock().map_err(|_| {
                    error(FileErrorCode::Io, "the retained file state is unavailable")
                })?;
                match state.identity {
                    Some(identity) => validate_retained_name(parent, name, identity)?,
                    None => match parent.symlink_metadata(name) {
                        Err(io_error) if io_error.kind() == io::ErrorKind::NotFound => {}
                        Ok(_) => return Err(target_changed()),
                        Err(io_error) => return Err(map_io(io_error)),
                    },
                }
                atomic_replace(parent, name, value)?;
                let retained = open_retained_file(parent, name, self.rights)?;
                let metadata = retained.metadata().map_err(map_io)?;
                validate_regular(&metadata)?;
                state.identity = Some(FileIdentity::from_metadata(&metadata));
                state.retained = Some(retained);
                Ok(())
            }
        }
    }

    /// Atomically create or replace one regular UTF-8 file.
    ///
    /// # Errors
    ///
    /// Returns the errors from [`Self::write_bytes`].
    pub fn write_text(&self, path: &str, value: &str) -> Result<(), FileError> {
        self.write_bytes(path, value.as_bytes())
    }

    /// List UTF-8 names in ascending byte order.
    ///
    /// # Errors
    ///
    /// Returns a safe error for a denied right, invalid path, denied directory component,
    /// non-UTF-8 name, exhausted limit, or descriptor I/O failure.
    pub fn list(&self, path: &str) -> Result<Vec<String>, FileError> {
        self.require_read()?;
        if matches!(&self.root, CapabilityRoot::File { .. }) {
            require_file_root_path(path)?;
            return Err(error(
                FileErrorCode::NotDirectory,
                "a file grant cannot list a directory",
            ));
        }
        let path = ValidatedPath::directory(path, self.limits)?;
        self.charge_operation()?;
        let directory = self.open_directory(&path)?;
        let entries = directory.entries().map_err(map_io)?;
        let mut names = Vec::new();
        let mut entry_count = 0_usize;
        for entry in entries {
            let entry = entry.map_err(map_io)?;
            entry_count = entry_count.checked_add(1).ok_or_else(|| {
                error(
                    FileErrorCode::TooManyEntries,
                    "the directory exceeds the entry limit",
                )
            })?;
            if entry_count > self.limits.max_entries {
                return Err(error(
                    FileErrorCode::TooManyEntries,
                    "the directory exceeds the entry limit",
                ));
            }
            let name = entry.file_name().into_string().map_err(|_| {
                error(
                    FileErrorCode::InvalidUtf8,
                    "a directory entry name is not valid UTF-8",
                )
            })?;
            names.push(name);
        }
        names.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        Ok(names)
    }

    /// Recursively search regular UTF-8 files for a literal, case-sensitive string.
    ///
    /// Results are ordered by path and then line number. Each matching line appears once and
    /// reports the byte column of its first match. Symbolic links and special files are skipped,
    /// as are files whose contents are not valid UTF-8.
    ///
    /// # Errors
    ///
    /// Returns a safe error for a denied right, invalid path, denied directory component,
    /// multiply-linked file, exhausted limit, or descriptor I/O failure.
    pub fn search(&self, path: &str, query: &str) -> Result<Vec<SearchMatch>, FileError> {
        self.require_read()?;
        let CapabilityRoot::Directory { recursive, .. } = &self.root else {
            require_file_root_path(path)?;
            return Err(error(
                FileErrorCode::NotDirectory,
                "a file grant cannot search a directory",
            ));
        };
        let path = ValidatedPath::directory(path, self.limits)?;
        let directory = self.open_directory(&path)?;
        let mut components = path
            .components
            .iter()
            .map(|component| (*component).to_owned())
            .collect::<Vec<_>>();
        let mut matches = Vec::new();
        self.search_directory(&directory, &mut components, query, *recursive, &mut matches)?;
        matches.sort_unstable_by(|left, right| {
            left.path
                .as_bytes()
                .cmp(right.path.as_bytes())
                .then_with(|| left.line.cmp(&right.line))
        });
        Ok(matches)
    }

    fn search_directory(
        &self,
        directory: &Dir,
        components: &mut Vec<String>,
        query: &str,
        recursive: bool,
        matches: &mut Vec<SearchMatch>,
    ) -> Result<(), FileError> {
        self.charge_operation()?;
        let entries = directory.entries().map_err(map_io)?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(map_io)?;
            if names.len() >= self.limits.max_entries {
                return Err(error(
                    FileErrorCode::TooManyEntries,
                    "a searched directory exceeds the entry limit",
                ));
            }
            names.push(entry.file_name().into_string().map_err(|_| {
                error(
                    FileErrorCode::InvalidUtf8,
                    "a directory entry name is not valid UTF-8",
                )
            })?);
        }
        names.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

        for name in names {
            components.push(name.clone());
            self.validate_search_path(components)?;
            let metadata = directory.symlink_metadata(&name).map_err(map_io)?;
            if metadata.is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
                components.pop();
                continue;
            }
            if metadata.is_dir() {
                if recursive {
                    let child = directory.open_dir_nofollow(&name).map_err(map_io)?;
                    self.search_directory(&child, components, query, true, matches)?;
                }
            } else {
                validate_regular(&metadata)?;
                self.charge_operation()?;
                let mut options = OpenOptions::new();
                options.read(true);
                options.follow(FollowSymlinks::No);
                options.nonblock(true);
                let mut file = directory.open_with(&name, &options).map_err(map_io)?;
                let bytes = self.read_opened_file(&mut file)?;
                if let Ok(text) = String::from_utf8(bytes) {
                    let result_path = components.join("/");
                    for (line_index, line) in text.lines().enumerate() {
                        let Some(column) = line.find(query) else {
                            continue;
                        };
                        if matches.len() >= self.limits.max_entries {
                            return Err(error(
                                FileErrorCode::TooManyEntries,
                                "the filesystem search exceeds the match limit",
                            ));
                        }
                        matches.push(SearchMatch {
                            path: result_path.clone(),
                            line: i64::try_from(line_index + 1).map_err(|_| {
                                error(
                                    FileErrorCode::FileTooLarge,
                                    "a search match exceeds the integer range",
                                )
                            })?,
                            column: i64::try_from(column + 1).map_err(|_| {
                                error(
                                    FileErrorCode::FileTooLarge,
                                    "a search match exceeds the integer range",
                                )
                            })?,
                            text: line.to_owned(),
                        });
                    }
                }
            }
            components.pop();
        }
        Ok(())
    }

    fn validate_search_path(&self, components: &[String]) -> Result<(), FileError> {
        if components.len() > self.limits.max_path_depth {
            return Err(invalid_path(
                "a filesystem search path exceeds the component limit",
            ));
        }
        let path_bytes = components
            .iter()
            .try_fold(components.len().saturating_sub(1), |total, component| {
                total.checked_add(component.len())
            });
        let Some(path_bytes) = path_bytes else {
            return Err(invalid_path(
                "a filesystem search path exceeds the byte limit",
            ));
        };
        if path_bytes > self.limits.max_path_bytes {
            return Err(invalid_path(
                "a filesystem search path exceeds the byte limit",
            ));
        }
        Ok(())
    }

    fn require_read(&self) -> Result<(), FileError> {
        if self.rights.read {
            Ok(())
        } else {
            Err(error(
                FileErrorCode::PermissionDenied,
                "the workspace does not grant read access",
            ))
        }
    }

    fn require_write(&self) -> Result<(), FileError> {
        if self.rights.write {
            Ok(())
        } else {
            Err(error(
                FileErrorCode::PermissionDenied,
                "the workspace does not grant write access",
            ))
        }
    }

    fn charge_operation(&self) -> Result<(), FileError> {
        let mut usage = self.lock_usage()?;
        let next = usage.operations.checked_add(1).ok_or_else(|| {
            error(
                FileErrorCode::OperationLimit,
                "the filesystem operation limit is exhausted",
            )
        })?;
        if next > self.limits.max_operations {
            return Err(error(
                FileErrorCode::OperationLimit,
                "the filesystem operation limit is exhausted",
            ));
        }
        usage.operations = next;
        Ok(())
    }

    fn reserve_read(&self, bytes: u64) -> Result<(), FileError> {
        if bytes > self.limits.max_file_bytes {
            return Err(error(
                FileErrorCode::FileTooLarge,
                "the file exceeds the byte limit",
            ));
        }
        let mut usage = self.lock_usage()?;
        let next = usage.read_bytes.checked_add(bytes).ok_or_else(|| {
            error(
                FileErrorCode::ReadLimit,
                "the filesystem read-byte limit is exhausted",
            )
        })?;
        if next > self.limits.max_read_bytes {
            return Err(error(
                FileErrorCode::ReadLimit,
                "the filesystem read-byte limit is exhausted",
            ));
        }
        usage.read_bytes = next;
        Ok(())
    }

    fn refund_read(&self, bytes: u64) {
        if let Ok(mut usage) = self.accounting.usage.lock() {
            usage.read_bytes = usage.read_bytes.saturating_sub(bytes);
        }
    }

    fn charge_write(&self, bytes: u64) -> Result<(), FileError> {
        if bytes > self.limits.max_file_bytes {
            return Err(error(
                FileErrorCode::FileTooLarge,
                "the file exceeds the byte limit",
            ));
        }
        let mut usage = self.lock_usage()?;
        let operations = usage.operations.checked_add(1).ok_or_else(|| {
            error(
                FileErrorCode::OperationLimit,
                "the filesystem operation limit is exhausted",
            )
        })?;
        let write_bytes = usage.write_bytes.checked_add(bytes).ok_or_else(|| {
            error(
                FileErrorCode::WriteLimit,
                "the filesystem write-byte limit is exhausted",
            )
        })?;
        if operations > self.limits.max_operations {
            return Err(error(
                FileErrorCode::OperationLimit,
                "the filesystem operation limit is exhausted",
            ));
        }
        if write_bytes > self.limits.max_write_bytes {
            return Err(error(
                FileErrorCode::WriteLimit,
                "the filesystem write-byte limit is exhausted",
            ));
        }
        usage.operations = operations;
        usage.write_bytes = write_bytes;
        Ok(())
    }

    fn lock_usage(&self) -> Result<std::sync::MutexGuard<'_, WorkspaceUsage>, FileError> {
        self.accounting.usage.lock().map_err(|_| {
            error(
                FileErrorCode::Io,
                "the filesystem accounting state is unavailable",
            )
        })
    }

    fn open_parent<'path>(
        &self,
        path: &'path ValidatedPath<'path>,
    ) -> Result<(Dir, &'path str), FileError> {
        let (name, parents) = path
            .components
            .split_last()
            .ok_or_else(|| error(FileErrorCode::InvalidPath, "the path does not name a file"))?;
        let CapabilityRoot::Directory {
            directory: root,
            recursive,
        } = &self.root
        else {
            return Err(error(
                FileErrorCode::InvalidPath,
                "a file grant accepts only the root file path",
            ));
        };
        if !recursive && !parents.is_empty() {
            return Err(error(
                FileErrorCode::PermissionDenied,
                "the directory grant is not recursive",
            ));
        }
        let mut directory = root.try_clone().map_err(map_io)?;
        for component in parents {
            directory = open_child_directory(&directory, component)?;
        }
        Ok((directory, name))
    }

    fn open_directory(&self, path: &ValidatedPath<'_>) -> Result<Dir, FileError> {
        let CapabilityRoot::Directory {
            directory: root,
            recursive,
        } = &self.root
        else {
            return Err(error(
                FileErrorCode::NotDirectory,
                "a file grant cannot open a directory",
            ));
        };
        if !recursive && !path.components.is_empty() {
            return Err(error(
                FileErrorCode::PermissionDenied,
                "the directory grant is not recursive",
            ));
        }
        let mut directory = root.try_clone().map_err(map_io)?;
        for component in &path.components {
            directory = open_child_directory(&directory, component)?;
        }
        Ok(directory)
    }
}

fn require_some_right(rights: Rights) -> Result<(), FileError> {
    if rights.read || rights.write {
        Ok(())
    } else {
        Err(error(
            FileErrorCode::PermissionDenied,
            "an external request must name at least one filesystem right",
        ))
    }
}

const fn rights_subset(candidate: Rights, requested: Rights) -> bool {
    (!candidate.read || requested.read) && (!candidate.write || requested.write)
}

const fn limits_subset(candidate: WorkspaceLimits, requested: WorkspaceLimits) -> bool {
    candidate.max_path_bytes <= requested.max_path_bytes
        && candidate.max_path_depth <= requested.max_path_depth
        && candidate.max_file_bytes <= requested.max_file_bytes
        && candidate.max_entries <= requested.max_entries
        && candidate.max_operations <= requested.max_operations
        && candidate.max_read_bytes <= requested.max_read_bytes
        && candidate.max_write_bytes <= requested.max_write_bytes
}

fn normalized_absolute(
    path: &Path,
    limits: WorkspaceLimits,
) -> Result<(PathBuf, Vec<String>), FileError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid_path("the external path is not valid UTF-8"))?;
    if !path.is_absolute() || text.contains('\0') || text.len() > limits.max_path_bytes {
        return Err(invalid_path(
            "the external target is not a normalized absolute path",
        ));
    }
    let mut components = Vec::new();
    let mut saw_root = false;
    for component in path.components() {
        match component {
            Component::RootDir if !saw_root => saw_root = true,
            Component::Normal(value) if saw_root => {
                let value = value.to_str().ok_or_else(|| {
                    invalid_path("the external path component is not valid UTF-8")
                })?;
                if value.is_empty() || value == "." || value == ".." {
                    return Err(invalid_path(
                        "the external path contains a denied component",
                    ));
                }
                components.push(value.to_owned());
                if components.len() > limits.max_path_depth {
                    return Err(invalid_path(
                        "the external path exceeds the component limit",
                    ));
                }
            }
            _ => {
                return Err(invalid_path(
                    "the external target is not a normalized absolute path",
                ));
            }
        }
    }
    if !saw_root {
        return Err(invalid_path(
            "the external target is not a normalized absolute path",
        ));
    }
    let mut normalized = PathBuf::from("/");
    for component in &components {
        normalized.push(component);
    }
    if normalized.to_str() != Some(text) {
        return Err(invalid_path(
            "the external target is not a normalized absolute path",
        ));
    }
    Ok((normalized, components))
}

fn normalized_relative_components(
    path: &Path,
    limits: WorkspaceLimits,
) -> Result<Vec<String>, FileError> {
    if path.as_os_str().is_empty() {
        return Ok(Vec::new());
    }
    if path.is_absolute() {
        return Err(invalid_path("the narrowed target is not relative"));
    }
    let text = path
        .to_str()
        .ok_or_else(|| invalid_path("the narrowed path is not valid UTF-8"))?;
    if text.len() > limits.max_path_bytes {
        return Err(invalid_path("the narrowed path exceeds the byte limit"));
    }
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .ok_or_else(|| invalid_path("the narrowed path component is not valid UTF-8")),
            _ => Err(invalid_path(
                "the narrowed target contains a denied component",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if components.len() > limits.max_path_depth {
        return Err(invalid_path(
            "the narrowed path exceeds the component limit",
        ));
    }
    Ok(components)
}

fn open_absolute_root() -> Result<Dir, FileError> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        return Err(error(
            FileErrorCode::UnsupportedPlatform,
            "the workspace platform is not supported",
        ));
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        Dir::open_ambient_dir("/", ambient_authority()).map_err(map_io)
    }
}

fn open_absolute_directory(components: &[String]) -> Result<Dir, FileError> {
    let mut directory = open_absolute_root()?;
    for component in components {
        directory = open_child_directory(&directory, component)?;
    }
    Ok(directory)
}

fn open_absolute_parent(components: &[String]) -> Result<(Dir, String), FileError> {
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| invalid_path("the external file path does not name a file"))?;
    Ok((open_absolute_directory(parents)?, name.clone()))
}

fn open_retained_file(
    parent: &Dir,
    name: &str,
    rights: Rights,
) -> Result<cap_std::fs::File, FileError> {
    let mut options = OpenOptions::new();
    options.read(rights.read);
    options.write(rights.write);
    options.follow(FollowSymlinks::No);
    options.nonblock(true);
    parent.open_with(name, &options).map_err(map_io)
}

fn validate_retained_name(
    parent: &Dir,
    name: &str,
    identity: FileIdentity,
) -> Result<(), FileError> {
    let metadata = parent.symlink_metadata(name).map_err(map_io)?;
    validate_regular(&metadata)?;
    if FileIdentity::from_metadata(&metadata) == identity {
        Ok(())
    } else {
        Err(target_changed())
    }
}

fn require_file_root_path(path: &str) -> Result<(), FileError> {
    if path == "." {
        Ok(())
    } else {
        Err(invalid_path("a file grant accepts only the root path '.'"))
    }
}

fn atomic_replace(parent: &Dir, name: &str, value: &[u8]) -> Result<(), FileError> {
    let (temporary_name, mut temporary) = create_temporary(parent)?;
    let write_result = (|| {
        temporary.write_all(value).map_err(map_io)?;
        temporary.sync_all().map_err(map_io)?;
        let metadata = temporary.metadata().map_err(map_io)?;
        validate_regular(&metadata)?;
        drop(temporary);
        parent.rename(&temporary_name, parent, name).map_err(map_io)
    })();
    if write_result.is_err() {
        let _ = parent.remove_file(&temporary_name);
    }
    write_result
}

const fn target_changed() -> FileError {
    error(
        FileErrorCode::TargetChanged,
        "the retained filesystem target changed",
    )
}

#[derive(Debug)]
struct ValidatedPath<'path> {
    components: Vec<&'path str>,
}

impl<'path> ValidatedPath<'path> {
    fn file(path: &'path str, limits: WorkspaceLimits) -> Result<Self, FileError> {
        Self::parse(path, limits, false)
    }

    fn directory(path: &'path str, limits: WorkspaceLimits) -> Result<Self, FileError> {
        Self::parse(path, limits, true)
    }

    fn parse(
        path: &'path str,
        limits: WorkspaceLimits,
        root_allowed: bool,
    ) -> Result<Self, FileError> {
        if root_allowed && path == "." {
            if path.len() > limits.max_path_bytes {
                return Err(invalid_path("the path exceeds the byte limit"));
            }
            return Ok(Self {
                components: Vec::new(),
            });
        }
        if path.is_empty()
            || path.len() > limits.max_path_bytes
            || Path::new(path).is_absolute()
            || path.contains('\0')
            || path.contains('\\')
            || path.contains(':')
        {
            return Err(invalid_path("the path is not a normalized relative path"));
        }
        let components: Vec<_> = path.split('/').collect();
        if components.len() > limits.max_path_depth {
            return Err(invalid_path("the path exceeds the component limit"));
        }
        if components
            .iter()
            .any(|component| component.is_empty() || *component == "." || *component == "..")
        {
            return Err(invalid_path("the path contains a denied component"));
        }
        Ok(Self { components })
    }
}

fn open_child_directory(parent: &Dir, name: &str) -> Result<Dir, FileError> {
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.is_symlink() => Err(error(
            FileErrorCode::SymlinkDenied,
            "a symbolic link component is denied",
        )),
        Ok(metadata) if !metadata.is_dir() => Err(error(
            FileErrorCode::NotDirectory,
            "a path component is not a directory",
        )),
        Ok(_) => parent.open_dir_nofollow(name).map_err(map_io),
        Err(io_error) => Err(map_io(io_error)),
    }
}

fn validate_regular(metadata: &cap_fs_ext::Metadata) -> Result<(), FileError> {
    if metadata.is_symlink() {
        return Err(error(
            FileErrorCode::SymlinkDenied,
            "symbolic links are denied",
        ));
    }
    if metadata.is_dir() {
        return Err(error(
            FileErrorCode::IsDirectory,
            "a directory cannot be used as a file",
        ));
    }
    if !metadata.is_file() {
        return Err(error(
            FileErrorCode::SpecialFileDenied,
            "special filesystem objects are denied",
        ));
    }
    if metadata.nlink() > 1 {
        return Err(error(
            FileErrorCode::HardLinkDenied,
            "multiply-linked regular files are denied",
        ));
    }
    Ok(())
}

fn create_temporary(parent: &Dir) -> Result<(String, cap_std::fs::File), FileError> {
    for _ in 0..TEMP_ATTEMPTS {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!("{TEMP_PREFIX}{}-{sequence}", std::process::id());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        options.follow(FollowSymlinks::No);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        options.mode(0o600);
        match parent.open_with(&name, &options) {
            Ok(file) => return Ok((name, file)),
            Err(io_error) if io_error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(io_error) => return Err(map_io(io_error)),
        }
    }
    Err(error(
        FileErrorCode::Io,
        "an exclusive temporary file could not be created",
    ))
}

const fn error(code: FileErrorCode, message: &'static str) -> FileError {
    FileError::new(code, message)
}

const fn invalid_path(message: &'static str) -> FileError {
    error(FileErrorCode::InvalidPath, message)
}

fn map_io(io_error: io::Error) -> FileError {
    let kind = io_error.kind();
    drop(io_error);
    match kind {
        io::ErrorKind::NotFound => error(FileErrorCode::NotFound, "the path was not found"),
        io::ErrorKind::PermissionDenied => error(
            FileErrorCode::PermissionDenied,
            "the operating system denied the filesystem operation",
        ),
        io::ErrorKind::NotADirectory => error(
            FileErrorCode::NotDirectory,
            "a path component is not a directory",
        ),
        io::ErrorKind::IsADirectory => error(
            FileErrorCode::IsDirectory,
            "a directory cannot be used as a file",
        ),
        _ => error(
            FileErrorCode::Io,
            "the operating system rejected the filesystem operation",
        ),
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs::{self, hard_link};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "allen-sandbox-fs-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn broker(&self) -> WorkspaceBroker {
            WorkspaceBroker::open_ambient(
                self.path(),
                Rights::READ_WRITE,
                WorkspaceLimits::default(),
            )
            .expect("open test workspace")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_writes_and_lists_through_descriptors() {
        let root = TestDir::new();
        fs::create_dir(root.path().join("data")).unwrap();
        fs::write(root.path().join("data/z.txt"), b"z").unwrap();
        fs::write(root.path().join("data/a.txt"), b"alpha").unwrap();
        let broker = root.broker();

        assert_eq!(broker.read_text("data/a.txt").unwrap(), "alpha");
        assert_eq!(broker.read_bytes("data/z.txt").unwrap(), b"z");
        broker.write_text("data/new.txt", "new").unwrap();
        broker.write_bytes("data/a.txt", b"replaced").unwrap();
        assert_eq!(
            fs::read(root.path().join("data/a.txt")).unwrap(),
            b"replaced"
        );
        assert_eq!(
            broker.list("data").unwrap(),
            vec!["a.txt", "new.txt", "z.txt"]
        );
        assert_eq!(broker.usage().operations, 5);
        assert_eq!(broker.usage().read_bytes, 6);
        assert_eq!(broker.usage().write_bytes, 11);
    }

    #[test]
    fn searches_utf8_files_recursively_in_stable_order() {
        let root = TestDir::new();
        fs::create_dir(root.path().join("a")).unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join(".hidden"), b"needle hidden\n").unwrap();
        fs::write(
            root.path().join("a.txt"),
            b"Needle no\nprefix needle and needle\n",
        )
        .unwrap();
        fs::write(root.path().join("a/z.txt"), b"needle in child\n").unwrap();
        fs::write(root.path().join("nested/b.txt"), "\u{e9} needle\n").unwrap();
        fs::write(root.path().join("binary"), [0xff, 0x00]).unwrap();
        symlink("a.txt", root.path().join("linked")).unwrap();
        let broker = root.broker();

        assert_eq!(
            broker.search(".", "needle").unwrap(),
            vec![
                SearchMatch {
                    path: ".hidden".to_owned(),
                    line: 1,
                    column: 1,
                    text: "needle hidden".to_owned(),
                },
                SearchMatch {
                    path: "a.txt".to_owned(),
                    line: 2,
                    column: 8,
                    text: "prefix needle and needle".to_owned(),
                },
                SearchMatch {
                    path: "a/z.txt".to_owned(),
                    line: 1,
                    column: 1,
                    text: "needle in child".to_owned(),
                },
                SearchMatch {
                    path: "nested/b.txt".to_owned(),
                    line: 1,
                    column: 4,
                    text: "\u{e9} needle".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn search_enforces_the_match_limit() {
        let root = TestDir::new();
        fs::write(root.path().join("many.txt"), b"match\nmatch\n").unwrap();
        let broker = WorkspaceBroker::open_ambient(
            root.path(),
            Rights::READ_ONLY,
            WorkspaceLimits {
                max_entries: 1,
                ..WorkspaceLimits::default()
            },
        )
        .unwrap();

        assert_eq!(
            broker.search(".", "match").unwrap_err().code,
            FileErrorCode::TooManyEntries
        );
    }

    #[test]
    fn only_exact_root_dot_is_accepted() {
        let root = TestDir::new();
        let broker = root.broker();
        assert!(broker.list(".").is_ok());
        for path in [
            "", "/tmp", "a/./b", "a/../b", "a//b", "a/", "\\tmp", "C:tmp", "\0",
        ] {
            assert_eq!(
                broker.read_bytes(path).unwrap_err().code,
                FileErrorCode::InvalidPath,
                "{path:?}"
            );
        }
        assert_eq!(
            broker.read_bytes(".").unwrap_err().code,
            FileErrorCode::InvalidPath
        );
    }

    #[test]
    fn list_includes_user_names_with_the_atomic_temporary_prefix() {
        let root = TestDir::new();
        fs::write(root.path().join(".allen-tmp-user"), b"visible").unwrap();
        fs::write(root.path().join("ordinary"), b"visible").unwrap();
        let broker = root.broker();

        assert_eq!(broker.read_text(".allen-tmp-user").unwrap(), "visible");
        assert_eq!(
            broker.list(".").unwrap(),
            vec![".allen-tmp-user", "ordinary"]
        );
    }

    #[test]
    fn listing_counts_prefixed_entries_before_enforcing_its_limit() {
        let root = TestDir::new();
        for name in [".allen-tmp-user-a", ".allen-tmp-user-b", "ordinary"] {
            fs::write(root.path().join(name), b"visible").unwrap();
        }
        let broker = WorkspaceBroker::open_ambient(
            root.path(),
            Rights::READ_ONLY,
            WorkspaceLimits {
                max_entries: 1,
                ..WorkspaceLimits::default()
            },
        )
        .unwrap();

        assert_eq!(
            broker.list(".").unwrap_err().code,
            FileErrorCode::TooManyEntries
        );
    }

    #[test]
    fn permission_denied_constructor_is_safe_and_stable() {
        assert_eq!(
            FileError::permission_denied(),
            FileError {
                code: FileErrorCode::PermissionDenied,
                message: "the workspace capability was not granted",
            }
        );
    }

    #[test]
    fn denies_intermediate_and_final_symlinks() {
        let root = TestDir::new();
        fs::create_dir(root.path().join("real")).unwrap();
        fs::write(root.path().join("real/file"), b"inside").unwrap();
        symlink("real", root.path().join("linked-dir")).unwrap();
        symlink("real/file", root.path().join("linked-file")).unwrap();
        let broker = root.broker();

        assert_eq!(
            broker.read_bytes("linked-dir/file").unwrap_err().code,
            FileErrorCode::SymlinkDenied
        );
        assert_eq!(
            broker.read_bytes("linked-file").unwrap_err().code,
            FileErrorCode::SymlinkDenied
        );
        assert_eq!(
            broker.write_bytes("linked-file", b"no").unwrap_err().code,
            FileErrorCode::SymlinkDenied
        );
    }

    #[test]
    fn denies_hard_links_directories_and_special_files() {
        let root = TestDir::new();
        fs::write(root.path().join("original"), b"secret").unwrap();
        hard_link(root.path().join("original"), root.path().join("linked")).unwrap();
        fs::create_dir(root.path().join("directory")).unwrap();
        let broker = root.broker();

        assert_eq!(
            broker.read_bytes("linked").unwrap_err().code,
            FileErrorCode::HardLinkDenied
        );
        assert_eq!(
            broker.write_bytes("linked", b"no").unwrap_err().code,
            FileErrorCode::HardLinkDenied
        );
        assert_eq!(
            broker.read_bytes("directory").unwrap_err().code,
            FileErrorCode::IsDirectory
        );
        let socket = root.path().join("socket");
        if let Ok(_listener) = UnixListener::bind(&socket) {
            assert_eq!(
                broker.read_bytes("socket").unwrap_err().code,
                FileErrorCode::SpecialFileDenied
            );
        }
        #[cfg(target_os = "linux")]
        {
            let fifo = root.path().join("fifo");
            rustix::fs::mkfifoat(
                rustix::fs::CWD,
                &fifo,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .unwrap();
            assert_eq!(
                broker.read_bytes("fifo").unwrap_err().code,
                FileErrorCode::SpecialFileDenied
            );
        }

        let devices = WorkspaceBroker::open_ambient(
            Path::new("/dev"),
            Rights::READ_ONLY,
            WorkspaceLimits::default(),
        )
        .unwrap();
        assert_eq!(
            devices.read_bytes("null").unwrap_err().code,
            FileErrorCode::SpecialFileDenied
        );
    }

    #[test]
    fn enforces_rights_and_resource_limits_before_file_io() {
        let root = TestDir::new();
        fs::write(root.path().join("five"), b"12345").unwrap();
        let limits = WorkspaceLimits {
            max_file_bytes: 4,
            max_operations: 2,
            max_read_bytes: 3,
            max_write_bytes: 3,
            ..WorkspaceLimits::default()
        };
        let broker =
            WorkspaceBroker::open_ambient(root.path(), Rights::READ_WRITE, limits).unwrap();
        assert_eq!(
            broker.read_bytes("five").unwrap_err().code,
            FileErrorCode::FileTooLarge
        );
        assert_eq!(
            broker.write_bytes("new", b"1234").unwrap_err().code,
            FileErrorCode::WriteLimit
        );
        assert_eq!(broker.usage().operations, 1);
        assert_eq!(broker.usage().write_bytes, 0);
        assert!(!root.path().join("new").exists());
        assert!(broker.list(".").is_ok());
        assert_eq!(
            broker.list(".").unwrap_err().code,
            FileErrorCode::OperationLimit
        );

        let read_only = WorkspaceBroker::open_ambient(
            root.path(),
            Rights::READ_ONLY,
            WorkspaceLimits::default(),
        )
        .unwrap();
        assert_eq!(
            read_only.write_text("denied", "x").unwrap_err().code,
            FileErrorCode::PermissionDenied
        );
        assert!(!root.path().join("denied").exists());
    }

    #[test]
    fn enforces_path_depth_and_listing_limits() {
        let root = TestDir::new();
        fs::write(root.path().join("a"), b"a").unwrap();
        fs::write(root.path().join("b"), b"b").unwrap();
        let limits = WorkspaceLimits {
            max_path_bytes: 8,
            max_path_depth: 1,
            max_entries: 1,
            ..WorkspaceLimits::default()
        };
        let broker =
            WorkspaceBroker::open_ambient(root.path(), Rights::READ_WRITE, limits).unwrap();
        assert_eq!(
            broker.read_bytes("a/b").unwrap_err().code,
            FileErrorCode::InvalidPath
        );
        assert_eq!(
            broker.read_bytes("123456789").unwrap_err().code,
            FileErrorCode::InvalidPath
        );
        assert_eq!(
            broker.list(".").unwrap_err().code,
            FileErrorCode::TooManyEntries
        );
    }

    #[test]
    fn invalid_utf8_text_is_safe_error() {
        let root = TestDir::new();
        fs::write(root.path().join("bad"), [0xff]).unwrap();
        let error = root.broker().read_text("bad").unwrap_err();
        assert_eq!(error.code, FileErrorCode::InvalidUtf8);
        assert!(
            !error
                .message
                .contains(root.path().to_string_lossy().as_ref())
        );
    }

    #[test]
    fn rejects_non_utf8_directory_names() {
        let root = TestDir::new();
        let create = fs::write(root.path().join(OsString::from_vec(vec![0xff])), b"value");
        match create {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::ILSEQ.raw_os_error()) => {
                return;
            }
            Err(error) => panic!("create non-UTF-8 test name: {error}"),
        }
        assert_eq!(
            root.broker().list(".").unwrap_err().code,
            FileErrorCode::InvalidUtf8
        );
    }

    #[test]
    fn cumulative_read_limit_is_reserved_before_content_read() {
        let root = TestDir::new();
        fs::write(root.path().join("value"), b"1234").unwrap();
        let limits = WorkspaceLimits {
            max_read_bytes: 3,
            ..WorkspaceLimits::default()
        };
        let broker = WorkspaceBroker::open_ambient(root.path(), Rights::READ_ONLY, limits).unwrap();
        assert_eq!(
            broker.read_bytes("value").unwrap_err().code,
            FileErrorCode::ReadLimit
        );
        assert_eq!(broker.usage().read_bytes, 0);
    }

    #[test]
    fn atomic_replace_does_not_leave_internal_temporary_files() {
        let root = TestDir::new();
        fs::write(root.path().join("target"), b"old").unwrap();
        let broker = root.broker();
        let replacement_size = 64 * 1024 + 1;
        broker
            .write_bytes("target", &vec![b'x'; replacement_size])
            .unwrap();
        assert_eq!(
            fs::read(root.path().join("target")).unwrap().len(),
            replacement_size
        );
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(TEMP_PREFIX)
        }));
    }

    #[test]
    fn execution_accounting_is_shared_across_workspace_and_grant_roots() {
        let root = TestDir::new();
        fs::write(root.path().join("one"), b"1").unwrap();
        fs::write(root.path().join("two"), b"22").unwrap();
        let limits = WorkspaceLimits {
            max_operations: 2,
            max_read_bytes: 3,
            ..WorkspaceLimits::default()
        };
        let accounting = ExecutionAccounting::new(limits);
        let workspace = WorkspaceBroker::open_ambient_with_accounting(
            root.path(),
            Rights::READ_ONLY,
            accounting.clone(),
        )
        .unwrap();
        let external_path = fs::canonicalize(root.path().join("two")).unwrap();
        let grant = RetainedExternalTarget::retain_file(&external_path, Rights::READ_ONLY, limits)
            .unwrap()
            .into_grant(&external_path, Rights::READ_ONLY, false, accounting.clone())
            .unwrap();

        assert_eq!(workspace.read_text("one").unwrap(), "1");
        assert_eq!(grant.read_text(".").unwrap(), "22");
        assert_eq!(accounting.usage().operations, 2);
        assert_eq!(accounting.usage().read_bytes, 3);
        assert_eq!(
            workspace.read_text("one").unwrap_err().code,
            FileErrorCode::OperationLimit
        );
    }

    #[test]
    fn external_grant_limits_can_only_narrow_shared_accounting() {
        let root = TestDir::new();
        fs::write(root.path().join("value"), b"12").unwrap();
        let requested = fs::canonicalize(root.path().join("value")).unwrap();
        let shared_limits = WorkspaceLimits {
            max_read_bytes: 10,
            ..WorkspaceLimits::default()
        };
        let accounting = ExecutionAccounting::new(shared_limits);
        let local_limits = WorkspaceLimits {
            max_read_bytes: 1,
            ..shared_limits
        };
        let grant =
            RetainedExternalTarget::retain_file(&requested, Rights::READ_ONLY, shared_limits)
                .unwrap()
                .into_grant_with_limits(
                    &requested,
                    Rights::READ_ONLY,
                    false,
                    local_limits,
                    accounting.clone(),
                )
                .unwrap();
        assert_eq!(grant.limits(), local_limits);
        assert_eq!(
            grant.read_text(".").unwrap_err().code,
            FileErrorCode::ReadLimit
        );
        assert_eq!(accounting.usage().read_bytes, 0);

        let broad_limits = WorkspaceLimits {
            max_read_bytes: shared_limits.max_read_bytes + 1,
            ..shared_limits
        };
        let error =
            RetainedExternalTarget::retain_file(&requested, Rights::READ_ONLY, shared_limits)
                .unwrap()
                .into_grant_with_limits(
                    &requested,
                    Rights::READ_ONLY,
                    false,
                    broad_limits,
                    accounting,
                )
                .unwrap_err();
        assert_eq!(error.code, FileErrorCode::PermissionDenied);
    }

    #[test]
    fn retained_file_grant_uses_only_dot_and_detects_predecision_replacement() {
        let root = TestDir::new();
        let requested = root.path().join("requested");
        fs::write(&requested, b"original").unwrap();
        let canonical = fs::canonicalize(&requested).unwrap();
        let retained = RetainedExternalTarget::retain_file(
            &canonical,
            Rights::READ_WRITE,
            WorkspaceLimits::default(),
        )
        .unwrap();
        assert_eq!(retained.kind(), ExternalTargetKind::File);
        assert_eq!(retained.diagnostic_path(), canonical);

        fs::rename(&requested, root.path().join("old")).unwrap();
        fs::write(&requested, b"replacement").unwrap();
        assert_eq!(
            retained
                .into_grant(
                    &canonical,
                    Rights::READ_ONLY,
                    false,
                    ExecutionAccounting::new(WorkspaceLimits::default()),
                )
                .unwrap_err()
                .code,
            FileErrorCode::TargetChanged
        );

        let current = RetainedExternalTarget::retain_file(
            &canonical,
            Rights::READ_WRITE,
            WorkspaceLimits::default(),
        )
        .unwrap()
        .into_grant(
            &canonical,
            Rights::READ_WRITE,
            false,
            ExecutionAccounting::new(WorkspaceLimits::default()),
        )
        .unwrap();
        assert_eq!(current.read_text(".").unwrap(), "replacement");
        assert_eq!(
            current.read_text("requested").unwrap_err().code,
            FileErrorCode::InvalidPath
        );
        assert_eq!(
            current.list(".").unwrap_err().code,
            FileErrorCode::NotDirectory
        );
        current.write_text(".", "updated").unwrap();
        assert_eq!(current.read_text(".").unwrap(), "updated");
    }

    #[test]
    fn absent_file_retains_parent_and_rejects_a_new_object_before_decision() {
        let root = TestDir::new();
        let canonical_root = fs::canonicalize(root.path()).unwrap();
        let missing = canonical_root.join("created");
        assert_eq!(
            RetainedExternalTarget::retain_file(
                &missing,
                Rights::READ_ONLY,
                WorkspaceLimits::default(),
            )
            .unwrap_err()
            .code,
            FileErrorCode::NotFound
        );

        let retained = RetainedExternalTarget::retain_file(
            &missing,
            Rights::READ_WRITE,
            WorkspaceLimits::default(),
        )
        .unwrap();
        fs::create_dir(root.path().join("created")).unwrap();
        assert_eq!(
            retained
                .into_grant(
                    &missing,
                    Rights::READ_WRITE,
                    false,
                    ExecutionAccounting::new(WorkspaceLimits::default()),
                )
                .unwrap_err()
                .code,
            FileErrorCode::TargetChanged
        );

        fs::remove_dir(root.path().join("created")).unwrap();
        let grant = RetainedExternalTarget::retain_file(
            &missing,
            Rights::READ_WRITE,
            WorkspaceLimits::default(),
        )
        .unwrap()
        .into_grant(
            &missing,
            Rights::READ_WRITE,
            false,
            ExecutionAccounting::new(WorkspaceLimits::default()),
        )
        .unwrap();
        assert_eq!(
            grant.read_text(".").unwrap_err().code,
            FileErrorCode::NotFound
        );
        grant.write_text(".", "created safely").unwrap();
        assert_eq!(grant.read_text(".").unwrap(), "created safely");
    }

    #[test]
    fn directory_grants_narrow_by_descriptor_and_enforce_recursion() {
        let root = TestDir::new();
        fs::create_dir_all(root.path().join("requested/narrow/deep")).unwrap();
        fs::write(root.path().join("requested/narrow/direct"), b"direct").unwrap();
        fs::write(root.path().join("requested/narrow/deep/nested"), b"nested").unwrap();
        let requested = fs::canonicalize(root.path().join("requested")).unwrap();
        let narrow = requested.join("narrow");
        let grant = RetainedExternalTarget::retain_directory(
            &requested,
            Rights::READ_ONLY,
            false,
            WorkspaceLimits::default(),
        )
        .unwrap()
        .into_grant(
            &narrow,
            Rights::READ_ONLY,
            false,
            ExecutionAccounting::new(WorkspaceLimits::default()),
        )
        .unwrap();

        assert_eq!(grant.read_text("direct").unwrap(), "direct");
        assert!(grant.list(".").unwrap().contains(&"deep".to_owned()));
        assert_eq!(
            grant.read_text("deep/nested").unwrap_err().code,
            FileErrorCode::PermissionDenied
        );

        let broadening = RetainedExternalTarget::retain_directory(
            &requested,
            Rights::READ_ONLY,
            false,
            WorkspaceLimits::default(),
        )
        .unwrap();
        assert_eq!(
            broadening
                .into_grant(
                    &requested,
                    Rights::READ_ONLY,
                    true,
                    ExecutionAccounting::new(WorkspaceLimits::default()),
                )
                .unwrap_err()
                .code,
            FileErrorCode::PermissionDenied
        );
    }

    #[test]
    fn retained_directory_does_not_reopen_a_replaced_ambient_path() {
        let root = TestDir::new();
        fs::create_dir(root.path().join("requested")).unwrap();
        fs::write(root.path().join("requested/value"), b"retained").unwrap();
        let requested = fs::canonicalize(root.path().join("requested")).unwrap();
        let retained = RetainedExternalTarget::retain_directory(
            &requested,
            Rights::READ_ONLY,
            true,
            WorkspaceLimits::default(),
        )
        .unwrap();

        fs::rename(root.path().join("requested"), root.path().join("moved")).unwrap();
        fs::create_dir(root.path().join("requested")).unwrap();
        fs::write(root.path().join("requested/value"), b"replacement").unwrap();
        let grant = retained
            .into_grant(
                &requested,
                Rights::READ_ONLY,
                true,
                ExecutionAccounting::new(WorkspaceLimits::default()),
            )
            .unwrap();
        assert_eq!(grant.read_text("value").unwrap(), "retained");
    }

    #[test]
    fn external_retention_rejects_symlinks_hardlinks_and_kind_mismatches() {
        let root = TestDir::new();
        fs::write(root.path().join("original"), b"value").unwrap();
        hard_link(root.path().join("original"), root.path().join("hard")).unwrap();
        symlink("original", root.path().join("symbolic")).unwrap();
        fs::create_dir(root.path().join("directory")).unwrap();
        let canonical_root = fs::canonicalize(root.path()).unwrap();

        assert_eq!(
            RetainedExternalTarget::retain_file(
                canonical_root.join("hard"),
                Rights::READ_ONLY,
                WorkspaceLimits::default(),
            )
            .unwrap_err()
            .code,
            FileErrorCode::HardLinkDenied
        );
        assert_eq!(
            RetainedExternalTarget::retain_file(
                canonical_root.join("symbolic"),
                Rights::READ_ONLY,
                WorkspaceLimits::default(),
            )
            .unwrap_err()
            .code,
            FileErrorCode::SymlinkDenied
        );
        assert_eq!(
            RetainedExternalTarget::retain_file(
                canonical_root.join("directory"),
                Rights::READ_ONLY,
                WorkspaceLimits::default(),
            )
            .unwrap_err()
            .code,
            FileErrorCode::IsDirectory
        );
        assert_eq!(
            RetainedExternalTarget::retain_directory(
                canonical_root.join("original"),
                Rights::READ_ONLY,
                false,
                WorkspaceLimits::default(),
            )
            .unwrap_err()
            .code,
            FileErrorCode::NotDirectory
        );
    }

    #[test]
    fn bounded_symlink_swap_never_reaches_outside_file() {
        let root = TestDir::new();
        let outside = TestDir::new();
        let target = root.path().join("target");
        let outside_file = outside.path().join("outside");
        fs::write(&target, b"inside").unwrap();
        fs::write(&outside_file, b"outside-canary").unwrap();

        let broker = Arc::new(root.broker());
        let stopped = Arc::new(AtomicBool::new(false));
        let swap_target = target.clone();
        let swap_outside = outside_file.clone();
        let swap_stopped = Arc::clone(&stopped);
        let swapper = std::thread::spawn(move || {
            while !swap_stopped.load(Ordering::Relaxed) {
                let _ = fs::remove_file(&swap_target);
                let _ = symlink(&swap_outside, &swap_target);
                std::thread::yield_now();
                let _ = fs::remove_file(&swap_target);
                let _ = fs::write(&swap_target, b"inside");
            }
        });

        for _ in 0..256 {
            if let Ok(bytes) = broker.read_bytes("target") {
                assert!(b"inside".starts_with(&bytes) || b"replacement".starts_with(&bytes));
            }
            let _ = broker.write_bytes("target", b"replacement");
        }
        stopped.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        assert_eq!(fs::read(&outside_file).unwrap(), b"outside-canary");
    }
}
