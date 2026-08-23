use std::fmt;
use std::path::PathBuf;

/// Stable package-load failure classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackageErrorCode {
    Io,
    InvalidManifest,
    InvalidLockfile,
    NonCanonicalLockfile,
    InvalidName,
    InvalidVersion,
    InvalidLanguage,
    InvalidEntry,
    InvalidCapability,
    InvalidTool,
    InvalidLimit,
    InvalidDependency,
    PathEscape,
    Symlink,
    SpecialFile,
    MissingSource,
    DependencyCycle,
    DuplicateIdentity,
    VersionConflict,
    LanguageConflict,
    PackageLimit,
    DependencyDepthLimit,
    ModuleLimit,
    SourceBytesLimit,
    LockMismatch,
}

impl PackageErrorCode {
    /// Return the stable diagnostic code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "package.io",
            Self::InvalidManifest => "manifest.invalid",
            Self::InvalidLockfile => "lock.invalid",
            Self::NonCanonicalLockfile => "lock.non_canonical",
            Self::InvalidName => "package.name",
            Self::InvalidVersion => "package.version",
            Self::InvalidLanguage => "package.language",
            Self::InvalidEntry => "manifest.entry",
            Self::InvalidCapability => "manifest.capability",
            Self::InvalidTool => "manifest.tool",
            Self::InvalidLimit => "manifest.limit",
            Self::InvalidDependency => "manifest.dependency",
            Self::PathEscape => "package.path_escape",
            Self::Symlink => "package.symlink",
            Self::SpecialFile => "package.special_file",
            Self::MissingSource => "package.missing_source",
            Self::DependencyCycle => "package.dependency_cycle",
            Self::DuplicateIdentity => "package.duplicate_identity",
            Self::VersionConflict => "package.version_conflict",
            Self::LanguageConflict => "package.language_conflict",
            Self::PackageLimit => "package.count_limit",
            Self::DependencyDepthLimit => "package.depth_limit",
            Self::ModuleLimit => "package.module_limit",
            Self::SourceBytesLimit => "package.source_bytes_limit",
            Self::LockMismatch => "lock.mismatch",
        }
    }
}

/// One bounded package diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageError {
    pub code: PackageErrorCode,
    pub path: Option<PathBuf>,
    pub message: String,
}

impl PackageError {
    pub(crate) fn new(code: PackageErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            path: None,
            message: message.into(),
        }
    }

    pub(crate) fn at(
        code: PackageErrorCode,
        path: impl Into<PathBuf>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            path: Some(path.into()),
            message: message.into(),
        }
    }
}

impl fmt::Display for PackageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(
                formatter,
                "{}: {}: {}",
                self.code.as_str(),
                path.display(),
                self.message
            )
        } else {
            write!(formatter, "{}: {}", self.code.as_str(), self.message)
        }
    }
}

impl std::error::Error for PackageError {}
