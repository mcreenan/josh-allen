use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(test)]
use std::fs;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use semver::Version;
use sha2::{Digest, Sha256};

use crate::lockfile::{
    LockedDependency, LockedPackage, canonical_lockfile, lockfile_from_parts, package_identity,
    parse_lockfile,
};
use crate::manifest::{
    Manifest, SUPPORTED_LANGUAGE, normalize_dependency_path, parse_exact_version,
    parse_language_requirement, parse_manifest, parse_version_requirement,
};
use crate::{Lockfile, PackageError, PackageErrorCode};

const MANIFEST_FILE: &str = "allen.toml";
const SOURCE_DIRECTORY: &str = "src";

/// Finite package graph and source loading limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadLimits {
    pub packages: usize,
    pub dependency_depth: usize,
    pub modules: usize,
    pub source_bytes: u64,
    pub manifest_bytes: u64,
    pub filesystem_entries: usize,
    pub path_bytes: usize,
    pub module_depth: usize,
}

impl Default for LoadLimits {
    fn default() -> Self {
        Self {
            packages: 128,
            dependency_depth: 32,
            modules: 4096,
            source_bytes: 64 * 1024 * 1024,
            manifest_bytes: 1024 * 1024,
            filesystem_entries: 16_384,
            path_bytes: 4096,
            module_depth: 64,
        }
    }
}

/// Exact package identity used by module resolution.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PackageId {
    pub name: String,
    pub version: String,
}

impl PackageId {
    /// Return `name@version`, the lock graph identity.
    #[must_use]
    pub fn canonical(&self) -> String {
        package_identity(&self.name, &self.version)
    }
}

/// One source module with a stable package-qualified identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceModule {
    pub identity: String,
    pub package: PackageId,
    pub path: String,
    pub source: String,
}

/// One resolved direct dependency edge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedDependency {
    pub alias: String,
    pub package: PackageId,
}

/// One exact package and its loaded source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPackage {
    pub id: PackageId,
    pub source: String,
    pub digest: String,
    pub manifest: Manifest,
    pub dependencies: Vec<ResolvedDependency>,
    pub modules: Vec<SourceModule>,
}

/// A verified, deterministic package graph ready for compilation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedPackage {
    pub root: PackageId,
    pub packages: Vec<ResolvedPackage>,
    pub modules: Vec<SourceModule>,
    pub lockfile: Lockfile,
}

/// Resolve the local graph and return canonical `allen.lock` bytes.
///
/// The caller writes the returned text explicitly. Check, build, and run must
/// use [`load_verified_package`] and never update a lock implicitly.
///
/// # Errors
///
/// Returns a stable package error for unsafe paths, invalid manifests, graph
/// conflicts, or exhausted bounds.
pub fn generate_lock(root: &Path, limits: &LoadLimits) -> Result<String, PackageError> {
    let loaded = load_unlocked(root, limits)?;
    Ok(canonical_lockfile(&loaded.lockfile))
}

/// Load a local package only when the supplied lock text is canonical and
/// exactly matches the current manifests and source bytes.
///
/// # Errors
///
/// Returns a stable package or lock error before the graph reaches the
/// compiler.
pub fn load_verified_package(
    root: &Path,
    lock_text: &str,
    limits: &LoadLimits,
) -> Result<LoadedPackage, PackageError> {
    let supplied = parse_lockfile(lock_text)?;
    if supplied.packages.len() > limits.packages {
        return Err(PackageError::new(
            PackageErrorCode::PackageLimit,
            "locked graph exceeds the configured package limit",
        ));
    }
    if canonical_lockfile(&supplied) != lock_text {
        return Err(PackageError::new(
            PackageErrorCode::NonCanonicalLockfile,
            "lockfile text is not canonical",
        ));
    }
    let loaded = load_unlocked(root, limits)?;
    if supplied != loaded.lockfile {
        return Err(PackageError::new(
            PackageErrorCode::LockMismatch,
            "lockfile does not match the current package graph",
        ));
    }
    Ok(loaded)
}

/// Verify one normalized, root-only in-memory source package.
///
/// Source keys are canonical paths below `src/`, including the `src/` prefix.
/// Dependencies are rejected because an in-memory tool bundle contains
/// exactly one root manifest and cannot prove an external package graph.
///
/// # Errors
///
/// Returns the same stable manifest, source, limit, and lock errors as the
/// filesystem loader.
#[allow(clippy::too_many_lines)]
pub fn load_verified_root_package(
    manifest_text: &str,
    sources: &BTreeMap<String, String>,
    lock_text: Option<&str>,
    limits: &LoadLimits,
) -> Result<LoadedPackage, PackageError> {
    validate_limits(limits)?;
    if u64::try_from(manifest_text.len()).unwrap_or(u64::MAX) > limits.manifest_bytes {
        return Err(PackageError::new(
            PackageErrorCode::InvalidManifest,
            "manifest exceeds the configured byte limit",
        ));
    }
    let total_source_bytes = sources.values().try_fold(
        u64::try_from(manifest_text.len()).unwrap_or(u64::MAX),
        |total, source| {
            total
                .checked_add(u64::try_from(source.len()).unwrap_or(u64::MAX))
                .ok_or_else(|| {
                    PackageError::new(
                        PackageErrorCode::SourceBytesLimit,
                        "source byte count overflow",
                    )
                })
        },
    )?;
    if total_source_bytes > limits.source_bytes {
        return Err(PackageError::new(
            PackageErrorCode::SourceBytesLimit,
            "package graph exceeds its source byte limit",
        ));
    }
    if sources.len() > limits.modules || sources.len() > limits.filesystem_entries {
        return Err(PackageError::new(
            PackageErrorCode::ModuleLimit,
            "source graph exceeds its module limit",
        ));
    }
    let manifest = parse_manifest(manifest_text)?;
    if manifest.entries.is_empty() {
        return Err(PackageError::new(
            PackageErrorCode::InvalidEntry,
            "root manifest has no entry",
        ));
    }
    if !manifest.dependencies.is_empty() {
        return Err(PackageError::new(
            PackageErrorCode::InvalidDependency,
            "in-memory root packages cannot contain dependencies",
        ));
    }
    let supported = Version::parse(SUPPORTED_LANGUAGE).map_err(|_| {
        PackageError::new(
            PackageErrorCode::InvalidLanguage,
            "supported language version is invalid",
        )
    })?;
    if !parse_language_requirement(&manifest.package.language)?.matches(&supported) {
        return Err(PackageError::new(
            PackageErrorCode::LanguageConflict,
            format!(
                "package '{}' does not support language {SUPPORTED_LANGUAGE}",
                manifest.package.name
            ),
        ));
    }
    let mut raw_modules = Vec::with_capacity(sources.len());
    for (path, source) in sources {
        validate_memory_source_path(path, limits)?;
        raw_modules.push((path.clone(), source.clone()));
    }
    validate_entry_modules(&manifest, &raw_modules)?;
    let id = PackageId {
        name: manifest.package.name.clone(),
        version: manifest.package.version.clone(),
    };
    let digest = content_digest(manifest_text.as_bytes(), &raw_modules);
    let modules = raw_modules
        .into_iter()
        .map(|(path, source)| SourceModule {
            identity: format!("pkg://{}@{}/{path}", id.name, id.version),
            package: id.clone(),
            path,
            source,
        })
        .collect::<Vec<_>>();
    let package = ResolvedPackage {
        id: id.clone(),
        source: ".".to_owned(),
        digest,
        manifest,
        dependencies: Vec::new(),
        modules: modules.clone(),
    };
    let lockfile = lockfile_from_parts(id.canonical(), vec![locked_package(&package)]);
    if let Some(text) = lock_text {
        let supplied = parse_lockfile(text)?;
        if canonical_lockfile(&supplied) != text {
            return Err(PackageError::new(
                PackageErrorCode::NonCanonicalLockfile,
                "lockfile text is not canonical",
            ));
        }
        if supplied != lockfile {
            return Err(PackageError::new(
                PackageErrorCode::LockMismatch,
                "lockfile does not match the current package graph",
            ));
        }
    }
    Ok(LoadedPackage {
        root: id,
        packages: vec![package],
        modules,
        lockfile,
    })
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn validate_memory_source_path(path: &str, limits: &LoadLimits) -> Result<(), PackageError> {
    if path.len() > limits.path_bytes
        || !path.starts_with("src/")
        || !path.ends_with(".allen")
        || path.contains('\\')
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidDependency,
            "source path is not canonical",
        ));
    }
    let components = path.split('/').collect::<Vec<_>>();
    if components.len() < 2
        || components.len().saturating_sub(2) > limits.module_depth
        || components.iter().any(|component| {
            component.is_empty()
                || *component == "."
                || *component == ".."
                || component.contains(':')
        })
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidDependency,
            "source path is not canonical",
        ));
    }
    Ok(())
}

fn load_unlocked(root: &Path, limits: &LoadLimits) -> Result<LoadedPackage, PackageError> {
    validate_limits(limits)?;
    let root_directory = open_root_directory(root)?;
    load_unlocked_from_open_root(root, limits, root_directory)
}

fn load_unlocked_from_open_root(
    root: &Path,
    limits: &LoadLimits,
    root_directory: Dir,
) -> Result<LoadedPackage, PackageError> {
    let mut loader = GraphLoader {
        root_path: root.to_path_buf(),
        root_directory,
        limits: *limits,
        packages: BTreeMap::new(),
        active: Vec::new(),
        identities: BTreeMap::new(),
        total_modules: 0,
        total_source_bytes: 0,
        filesystem_entries: 0,
    };
    let root_id = loader.visit(".", 0, true)?;
    let mut packages = loader.packages.into_values().collect::<Vec<_>>();
    packages.sort_by(|left, right| (&left.id, &left.source).cmp(&(&right.id, &right.source)));
    let modules = packages
        .iter()
        .flat_map(|package| package.modules.iter().cloned())
        .collect::<Vec<_>>();
    let locked = packages.iter().map(locked_package).collect();
    let lockfile = lockfile_from_parts(root_id.canonical(), locked);
    Ok(LoadedPackage {
        root: root_id,
        packages,
        modules,
        lockfile,
    })
}

struct GraphLoader {
    root_path: PathBuf,
    root_directory: Dir,
    limits: LoadLimits,
    packages: BTreeMap<String, ResolvedPackage>,
    active: Vec<String>,
    identities: BTreeMap<PackageId, String>,
    total_modules: usize,
    total_source_bytes: u64,
    filesystem_entries: usize,
}

impl GraphLoader {
    #[allow(clippy::too_many_lines)]
    fn visit(
        &mut self,
        source: &str,
        depth: usize,
        is_root: bool,
    ) -> Result<PackageId, PackageError> {
        if depth > self.limits.dependency_depth {
            return Err(PackageError::new(
                PackageErrorCode::DependencyDepthLimit,
                "dependency graph exceeds its depth limit",
            ));
        }
        if let Some(package) = self.packages.get(source) {
            return Ok(package.id.clone());
        }
        if let Some(position) = self.active.iter().position(|item| item == source) {
            let mut cycle = self.active[position..].to_vec();
            cycle.push(source.to_owned());
            return Err(PackageError::new(
                PackageErrorCode::DependencyCycle,
                format!("dependency cycle: {}", cycle.join(" -> ")),
            ));
        }
        if self.packages.len() + self.active.len() >= self.limits.packages {
            return Err(PackageError::new(
                PackageErrorCode::PackageLimit,
                "package graph exceeds its package limit",
            ));
        }
        let directory_path = package_directory(&self.root_path, source)?;
        let directory = self.open_package_directory(source)?;
        let manifest_path = directory_path.join(MANIFEST_FILE);
        let manifest_bytes = read_regular_file_at(
            &directory,
            MANIFEST_FILE,
            &manifest_path,
            self.limits.manifest_bytes,
        )?;
        self.charge_source_bytes(u64::try_from(manifest_bytes.len()).unwrap_or(u64::MAX))?;
        let manifest_text = std::str::from_utf8(&manifest_bytes).map_err(|_| {
            PackageError::at(
                PackageErrorCode::InvalidManifest,
                &manifest_path,
                "manifest is not UTF-8",
            )
        })?;
        let manifest = parse_manifest(manifest_text).map_err(|mut error| {
            error.path = Some(manifest_path.clone());
            error
        })?;
        if is_root && manifest.entries.is_empty() {
            return Err(PackageError::at(
                PackageErrorCode::InvalidEntry,
                &manifest_path,
                "root manifest has no entry",
            ));
        }
        let supported = Version::parse(SUPPORTED_LANGUAGE).expect("supported language is valid");
        if !parse_language_requirement(&manifest.package.language)?.matches(&supported) {
            return Err(PackageError::at(
                PackageErrorCode::LanguageConflict,
                &manifest_path,
                format!(
                    "package '{}' does not support language {SUPPORTED_LANGUAGE}",
                    manifest.package.name
                ),
            ));
        }
        let id = PackageId {
            name: manifest.package.name.clone(),
            version: manifest.package.version.clone(),
        };
        if let Some(previous) = self.identities.get(&id) {
            if previous != source {
                return Err(PackageError::new(
                    PackageErrorCode::DuplicateIdentity,
                    format!(
                        "package identity '{}' has sources '{}' and '{source}'",
                        id.canonical(),
                        previous
                    ),
                ));
            }
        }
        let raw_modules = self.load_modules(&directory, &directory_path)?;
        validate_entry_modules(&manifest, &raw_modules)?;
        let digest = content_digest(&manifest_bytes, &raw_modules);

        self.active.push(source.to_owned());
        self.identities.insert(id.clone(), source.to_owned());
        let mut dependencies = Vec::with_capacity(manifest.dependencies.len());
        for (alias, dependency) in &manifest.dependencies {
            let dependency_source = normalize_dependency_path(&dependency.path)?;
            let dependency_id = self.visit(&dependency_source, depth + 1, false)?;
            let selected = parse_exact_version(&dependency_id.version)?;
            if !parse_version_requirement(&dependency.version)?.matches(&selected) {
                return Err(PackageError::new(
                    PackageErrorCode::VersionConflict,
                    format!(
                        "dependency '{alias}' requires '{}' but selected {}",
                        dependency.version, dependency_id.version
                    ),
                ));
            }
            dependencies.push(ResolvedDependency {
                alias: alias.clone(),
                package: dependency_id,
            });
        }
        let popped = self.active.pop();
        debug_assert_eq!(popped.as_deref(), Some(source));
        dependencies.sort_by(|left, right| left.alias.cmp(&right.alias));
        let modules = raw_modules
            .into_iter()
            .map(|(path, source)| SourceModule {
                identity: format!("pkg://{}@{}/{path}", id.name, id.version),
                package: id.clone(),
                path,
                source,
            })
            .collect();
        self.packages.insert(
            source.to_owned(),
            ResolvedPackage {
                id: id.clone(),
                source: source.to_owned(),
                digest,
                manifest,
                dependencies,
                modules,
            },
        );
        Ok(id)
    }

    fn open_package_directory(&self, source: &str) -> Result<Dir, PackageError> {
        if source == "." {
            return self
                .root_directory
                .try_clone()
                .map_err(|error| io_error(&self.root_path, error));
        }
        let normalized = normalize_dependency_path(source)?;
        let mut directory = self
            .root_directory
            .try_clone()
            .map_err(|error| io_error(&self.root_path, error))?;
        let mut display_path = self.root_path.clone();
        for component in normalized.split('/') {
            display_path.push(component);
            directory = open_directory_no_follow(
                &directory,
                component,
                &display_path,
                "dependency path cannot resolve through a symlink",
                "dependency path component is not a directory",
            )?;
        }
        Ok(directory)
    }

    fn load_modules(
        &mut self,
        package: &Dir,
        package_path: &Path,
    ) -> Result<Vec<(String, String)>, PackageError> {
        let source_root = package_path.join(SOURCE_DIRECTORY);
        let Some(source_directory) = open_optional_directory_no_follow(
            package,
            SOURCE_DIRECTORY,
            &source_root,
            "source directory cannot be a symlink",
            "source root is not a directory",
        )?
        else {
            return Ok(Vec::new());
        };
        let mut modules = Vec::new();
        self.walk_source(&source_directory, &source_root, "src", 0, &mut modules)?;
        modules.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(modules)
    }

    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn walk_source(
        &mut self,
        directory: &Dir,
        display_directory: &Path,
        relative: &str,
        depth: usize,
        modules: &mut Vec<(String, String)>,
    ) -> Result<(), PackageError> {
        if depth > self.limits.module_depth {
            return Err(PackageError::at(
                PackageErrorCode::ModuleLimit,
                display_directory,
                "source tree exceeds its depth limit",
            ));
        }
        let mut names = Vec::new();
        for entry in directory
            .entries()
            .map_err(|error| io_error(display_directory, error))?
        {
            let entry = entry.map_err(|error| io_error(display_directory, error))?;
            self.filesystem_entries = self.filesystem_entries.checked_add(1).ok_or_else(|| {
                PackageError::new(
                    PackageErrorCode::ModuleLimit,
                    "filesystem entry count overflow",
                )
            })?;
            if self.filesystem_entries > self.limits.filesystem_entries {
                return Err(PackageError::new(
                    PackageErrorCode::ModuleLimit,
                    "source graph exceeds its filesystem entry limit",
                ));
            }
            let name = entry.file_name().into_string().map_err(|_| {
                PackageError::at(
                    PackageErrorCode::InvalidDependency,
                    display_directory,
                    "source path is not UTF-8",
                )
            })?;
            names.push(name);
        }
        names.sort();
        for name in names {
            let path = display_directory.join(&name);
            if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':']) {
                return Err(PackageError::at(
                    PackageErrorCode::InvalidDependency,
                    &path,
                    "source path component is not canonical",
                ));
            }
            let module_path = format!("{relative}/{name}");
            if module_path.len() > self.limits.path_bytes {
                return Err(PackageError::at(
                    PackageErrorCode::ModuleLimit,
                    &path,
                    "source path exceeds its byte limit",
                ));
            }
            if let Ok(child) = directory.open_dir_nofollow(&name) {
                validate_opened_directory(&child, &path)?;
                self.walk_source(&child, &path, &module_path, depth + 1, modules)?;
            } else {
                if directory
                    .symlink_metadata(&name)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(PackageError::at(
                        PackageErrorCode::Symlink,
                        &path,
                        "source tree cannot contain a symlink",
                    ));
                }
                if !name.ends_with(".allen") {
                    return Err(PackageError::at(
                        PackageErrorCode::SpecialFile,
                        &path,
                        "source-only packages can contain only .allen files below src",
                    ));
                }
                self.total_modules = self.total_modules.checked_add(1).ok_or_else(|| {
                    PackageError::new(PackageErrorCode::ModuleLimit, "module count overflow")
                })?;
                if self.total_modules > self.limits.modules {
                    return Err(PackageError::new(
                        PackageErrorCode::ModuleLimit,
                        "package graph exceeds its module limit",
                    ));
                }
                let bytes = read_regular_file_at(directory, &name, &path, self.remaining_bytes())?;
                self.charge_source_bytes(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
                let source = String::from_utf8(bytes).map_err(|_| {
                    PackageError::at(
                        PackageErrorCode::InvalidDependency,
                        &path,
                        "source module is not UTF-8",
                    )
                })?;
                modules.push((module_path, source));
            }
        }
        Ok(())
    }

    fn remaining_bytes(&self) -> u64 {
        self.limits
            .source_bytes
            .saturating_sub(self.total_source_bytes)
    }

    fn charge_source_bytes(&mut self, bytes: u64) -> Result<(), PackageError> {
        let next = self.total_source_bytes.checked_add(bytes).ok_or_else(|| {
            PackageError::new(
                PackageErrorCode::SourceBytesLimit,
                "source byte count overflow",
            )
        })?;
        if next > self.limits.source_bytes {
            return Err(PackageError::new(
                PackageErrorCode::SourceBytesLimit,
                "package graph exceeds its source byte limit",
            ));
        }
        self.total_source_bytes = next;
        Ok(())
    }
}

fn validate_limits(limits: &LoadLimits) -> Result<(), PackageError> {
    if limits.packages == 0
        || limits.modules == 0
        || limits.source_bytes == 0
        || limits.manifest_bytes == 0
        || limits.filesystem_entries == 0
        || limits.path_bytes == 0
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLimit,
            "package load limits must be greater than zero",
        ));
    }
    Ok(())
}

fn package_directory(root: &Path, source: &str) -> Result<PathBuf, PackageError> {
    if source == "." {
        return Ok(root.to_path_buf());
    }
    let normalized = normalize_dependency_path(source)?;
    Ok(root.join(normalized))
}

fn validate_entry_modules(
    manifest: &Manifest,
    modules: &[(String, String)],
) -> Result<(), PackageError> {
    let paths = modules
        .iter()
        .map(|(path, _)| path.as_str())
        .collect::<BTreeSet<_>>();
    for entry in &manifest.entries {
        let module = entry
            .function
            .rsplit_once("::")
            .map(|(module, _)| module)
            .ok_or_else(|| {
                PackageError::new(
                    PackageErrorCode::InvalidEntry,
                    "entry function is not module-qualified",
                )
            })?;
        if !paths.contains(module) {
            return Err(PackageError::new(
                PackageErrorCode::MissingSource,
                format!("entry '{}' names missing module '{module}'", entry.name),
            ));
        }
    }
    Ok(())
}

fn content_digest(manifest: &[u8], modules: &[(String, String)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"allen-package-content-v1\0");
    hash_item(&mut hasher, MANIFEST_FILE.as_bytes(), manifest);
    for (path, source) in modules {
        hash_item(&mut hasher, path.as_bytes(), source.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_item(hasher: &mut Sha256, path: &[u8], content: &[u8]) {
    hasher.update(u64::try_from(path.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(path);
    hasher.update(
        u64::try_from(content.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hasher.update(content);
}

fn locked_package(package: &ResolvedPackage) -> LockedPackage {
    LockedPackage {
        name: package.id.name.clone(),
        version: package.id.version.clone(),
        source: package.source.clone(),
        digest: package.digest.clone(),
        dependencies: package
            .dependencies
            .iter()
            .map(|dependency| LockedDependency {
                alias: dependency.alias.clone(),
                package: dependency.package.canonical(),
            })
            .collect(),
    }
}

fn open_root_directory(root: &Path) -> Result<Dir, PackageError> {
    let directory = if let Some(name) = root.file_name() {
        let parent_path = root
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = Dir::open_ambient_dir(parent_path, ambient_authority())
            .map_err(|error| io_error(parent_path, error))?;
        match parent.open_dir_nofollow(name) {
            Ok(directory) => directory,
            Err(open_error) => {
                return match parent.symlink_metadata(name) {
                    Ok(metadata) if metadata.file_type().is_symlink() => Err(PackageError::at(
                        PackageErrorCode::Symlink,
                        root,
                        "package root cannot be a symlink",
                    )),
                    Ok(metadata) if !metadata.is_dir() => Err(PackageError::at(
                        PackageErrorCode::SpecialFile,
                        root,
                        "package root is not a directory",
                    )),
                    _ => Err(io_error(root, open_error)),
                };
            }
        }
    } else {
        Dir::open_ambient_dir(root, ambient_authority()).map_err(|error| io_error(root, error))?
    };
    validate_opened_directory(&directory, root)?;
    Ok(directory)
}

fn open_optional_directory_no_follow(
    parent: &Dir,
    name: &str,
    path: &Path,
    symlink_message: &'static str,
    special_message: &'static str,
) -> Result<Option<Dir>, PackageError> {
    match parent.open_dir_nofollow(name) {
        Ok(directory) => {
            validate_opened_directory(&directory, path)?;
            Ok(Some(directory))
        }
        Err(open_error) => match parent.symlink_metadata(name) {
            Err(metadata_error)
                if open_error.kind() == std::io::ErrorKind::NotFound
                    && metadata_error.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Ok(metadata) if metadata.file_type().is_symlink() => Err(PackageError::at(
                PackageErrorCode::Symlink,
                path,
                symlink_message,
            )),
            Ok(metadata) if !metadata.is_dir() => Err(PackageError::at(
                PackageErrorCode::SpecialFile,
                path,
                special_message,
            )),
            _ => Err(io_error(path, open_error)),
        },
    }
}

fn open_directory_no_follow(
    parent: &Dir,
    name: &str,
    path: &Path,
    symlink_message: &'static str,
    special_message: &'static str,
) -> Result<Dir, PackageError> {
    match open_optional_directory_no_follow(parent, name, path, symlink_message, special_message)? {
        Some(directory) => Ok(directory),
        None => Err(io_error(path, "No such file or directory")),
    }
}

fn validate_opened_directory(directory: &Dir, path: &Path) -> Result<(), PackageError> {
    let metadata = directory
        .dir_metadata()
        .map_err(|error| io_error(path, error))?;
    if !metadata.is_dir() {
        return Err(PackageError::at(
            PackageErrorCode::SpecialFile,
            path,
            "opened package object is not a directory",
        ));
    }
    Ok(())
}

fn read_regular_file_at(
    directory: &Dir,
    name: &str,
    path: &Path,
    limit: u64,
) -> Result<Vec<u8>, PackageError> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    options.nonblock(true);
    let mut file = match directory.open_with(name, &options) {
        Ok(file) => file,
        Err(open_error) => {
            return match directory.symlink_metadata(name) {
                Ok(metadata) if metadata.file_type().is_symlink() => Err(PackageError::at(
                    PackageErrorCode::Symlink,
                    path,
                    "package file cannot be a symlink",
                )),
                Ok(metadata) if !metadata.is_file() => Err(PackageError::at(
                    PackageErrorCode::SpecialFile,
                    path,
                    "package file is not regular",
                )),
                _ => Err(io_error(path, open_error)),
            };
        }
    };
    let opened = file.metadata().map_err(|error| io_error(path, error))?;
    if !opened.is_file() {
        return Err(PackageError::at(
            PackageErrorCode::SpecialFile,
            path,
            "package file is not regular",
        ));
    }
    if opened.nlink() != 1 {
        return Err(PackageError::at(
            PackageErrorCode::SpecialFile,
            path,
            "package file cannot have multiple hard links",
        ));
    }
    if opened.len() > limit {
        return Err(PackageError::at(
            PackageErrorCode::SourceBytesLimit,
            path,
            "file exceeds its byte limit",
        ));
    }
    let capacity = usize::try_from(opened.len()).map_err(|_| {
        PackageError::at(
            PackageErrorCode::SourceBytesLimit,
            path,
            "file length does not fit memory",
        )
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(PackageError::at(
            PackageErrorCode::SourceBytesLimit,
            path,
            "file exceeds its byte limit",
        ));
    }
    Ok(bytes)
}

fn io_error(path: &Path, error: impl std::fmt::Display) -> PackageError {
    PackageError::at(PackageErrorCode::Io, path, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("allen-package-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, path: &str, contents: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut file = File::create(path).unwrap();
            file.write_all(contents.as_bytes()).unwrap();
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn root_manifest(dependencies: &str) -> String {
        format!(
            r#"[package]
name = "app"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Int"

{dependencies}
"#
        )
    }

    fn dependency_manifest(name: &str, version: &str, dependencies: &str) -> String {
        format!(
            r#"[package]
name = "{name}"
version = "{version}"
language = ">=0.1.0, <0.2.0"

{dependencies}
"#
        )
    }

    fn package_fixture() -> TestDirectory {
        let directory = TestDirectory::new();
        directory.write(
            MANIFEST_FILE,
            &root_manifest(
                "[dependencies.text_utils]\npath = \"packages/text-utils\"\nversion = \"^1.2.0\"",
            ),
        );
        directory.write("src/main.allen", "export fn main() returns Int { 42 }\n");
        directory.write(
            "packages/text-utils/allen.toml",
            &dependency_manifest("text-utils", "1.2.3", ""),
        );
        directory.write(
            "packages/text-utils/src/text.allen",
            "export fn normalize() returns Int { 1 }\n",
        );
        directory
    }

    fn root_only_fixture() -> (TestDirectory, String, BTreeMap<String, String>) {
        let directory = TestDirectory::new();
        let manifest = root_manifest("");
        let source = "export fn main() returns Int { 42 }\n".to_owned();
        directory.write(MANIFEST_FILE, &manifest);
        directory.write("src/main.allen", &source);
        let sources = BTreeMap::from([("src/main.allen".to_owned(), source)]);
        (directory, manifest, sources)
    }

    #[test]
    fn verified_memory_root_matches_the_filesystem_graph() {
        let (directory, manifest, sources) = root_only_fixture();
        let limits = LoadLimits::default();
        let lock = generate_lock(&directory.0, &limits).unwrap();
        let filesystem = load_verified_package(&directory.0, &lock, &limits).unwrap();
        let memory = load_verified_root_package(&manifest, &sources, Some(&lock), &limits).unwrap();

        assert_eq!(memory, filesystem);
        assert_eq!(canonical_lockfile(&memory.lockfile), lock);
    }

    #[test]
    fn verified_memory_root_rejects_stale_and_noncanonical_locks() {
        let (directory, manifest, mut sources) = root_only_fixture();
        let limits = LoadLimits::default();
        let lock = generate_lock(&directory.0, &limits).unwrap();

        sources.insert(
            "src/main.allen".to_owned(),
            "export fn main() returns Int { 7 }\n".to_owned(),
        );
        assert_eq!(
            load_verified_root_package(&manifest, &sources, Some(&lock), &limits)
                .unwrap_err()
                .code,
            PackageErrorCode::LockMismatch
        );
        assert_eq!(
            load_verified_root_package(
                &manifest,
                &sources,
                Some(&format!("# comment\n{lock}")),
                &limits,
            )
            .unwrap_err()
            .code,
            PackageErrorCode::NonCanonicalLockfile
        );
    }

    #[test]
    fn verified_memory_root_rejects_dependencies_unsafe_paths_and_limits() {
        let (_, manifest, sources) = root_only_fixture();
        let dependency_manifest = root_manifest(
            "[dependencies.text_utils]\npath = \"packages/text-utils\"\nversion = \"1.0.0\"",
        );
        assert_eq!(
            load_verified_root_package(
                &dependency_manifest,
                &sources,
                None,
                &LoadLimits::default(),
            )
            .unwrap_err()
            .code,
            PackageErrorCode::InvalidDependency
        );

        for path in ["../main.allen", "src/../main.allen", "/src/main.allen"] {
            let unsafe_sources = BTreeMap::from([(
                path.to_owned(),
                "export fn main() returns Int { 42 }\n".to_owned(),
            )]);
            assert_eq!(
                load_verified_root_package(
                    &manifest,
                    &unsafe_sources,
                    None,
                    &LoadLimits::default(),
                )
                .unwrap_err()
                .code,
                PackageErrorCode::InvalidDependency,
                "path {path} must be rejected",
            );
        }

        let byte_limits = LoadLimits {
            source_bytes: 1,
            ..LoadLimits::default()
        };
        assert_eq!(
            load_verified_root_package(&manifest, &sources, None, &byte_limits)
                .unwrap_err()
                .code,
            PackageErrorCode::SourceBytesLimit
        );

        let module_limits = LoadLimits {
            modules: 1,
            ..LoadLimits::default()
        };
        let two_sources = BTreeMap::from([
            (
                "src/main.allen".to_owned(),
                "export fn main() returns Int { 42 }\n".to_owned(),
            ),
            (
                "src/extra.allen".to_owned(),
                "export fn extra() returns Int { 7 }\n".to_owned(),
            ),
        ]);
        assert_eq!(
            load_verified_root_package(&manifest, &two_sources, None, &module_limits)
                .unwrap_err()
                .code,
            PackageErrorCode::ModuleLimit
        );
    }

    #[test]
    fn lock_generation_and_verified_loading_are_deterministic() {
        let directory = package_fixture();
        let limits = LoadLimits::default();
        let first = generate_lock(&directory.0, &limits).unwrap();
        let second = generate_lock(&directory.0, &limits).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first,
            r#"lock_version = 1
language = "0.1.0"
root = "app@0.1.0"

[[package]]
name = "app"
version = "0.1.0"
source = "."
digest = "sha256:507909028582e5c8c505297fad954dfbe1e072eddc7733514bba23fd3ecb2e22"
dependencies = [
  { alias = "text_utils", package = "text-utils@1.2.3" },
]

[[package]]
name = "text-utils"
version = "1.2.3"
source = "packages/text-utils"
digest = "sha256:1f4676f97668f0cbe9682d46bff9b21190c628ae968b755dbed6e8fd7910399a"
dependencies = []
"#
        );
        let loaded = load_verified_package(&directory.0, &first, &limits).unwrap();
        assert_eq!(loaded.root.canonical(), "app@0.1.0");
        assert_eq!(loaded.packages.len(), 2);
        assert_eq!(
            loaded
                .modules
                .iter()
                .map(|module| module.identity.as_str())
                .collect::<Vec<_>>(),
            [
                "pkg://app@0.1.0/src/main.allen",
                "pkg://text-utils@1.2.3/src/text.allen"
            ]
        );
    }

    #[test]
    fn stale_and_noncanonical_locks_fail_before_loading() {
        let directory = package_fixture();
        let limits = LoadLimits::default();
        let lock = generate_lock(&directory.0, &limits).unwrap();
        let noncanonical = format!("# comment\n{lock}");
        assert_eq!(
            load_verified_package(&directory.0, &noncanonical, &limits)
                .unwrap_err()
                .code,
            PackageErrorCode::NonCanonicalLockfile
        );
        directory.write("src/main.allen", "export fn main() returns Int { 7 }\n");
        assert_eq!(
            load_verified_package(&directory.0, &lock, &limits)
                .unwrap_err()
                .code,
            PackageErrorCode::LockMismatch
        );
    }

    #[test]
    fn rejects_dependency_cycles() {
        let cycle = TestDirectory::new();
        cycle.write(
            MANIFEST_FILE,
            &root_manifest("[dependencies.a]\npath = \"packages/a\"\nversion = \"1.0.0\""),
        );
        cycle.write("src/main.allen", "export fn main() returns Int { 1 }");
        cycle.write(
            "packages/a/allen.toml",
            &dependency_manifest(
                "package-a",
                "1.0.0",
                "[dependencies.b]\npath = \"packages/b\"\nversion = \"1.0.0\"",
            ),
        );
        cycle.write("packages/a/src/a.allen", "export fn a() returns Int { 1 }");
        cycle.write(
            "packages/b/allen.toml",
            &dependency_manifest(
                "package-b",
                "1.0.0",
                "[dependencies.a]\npath = \"packages/a\"\nversion = \"1.0.0\"",
            ),
        );
        cycle.write("packages/b/src/b.allen", "export fn b() returns Int { 1 }");
        assert_eq!(
            generate_lock(&cycle.0, &LoadLimits::default())
                .unwrap_err()
                .code,
            PackageErrorCode::DependencyCycle
        );
    }

    #[test]
    fn rejects_versions_languages_and_duplicate_identities() {
        let duplicate = package_fixture();
        duplicate.write(
            "packages/text-utils/allen.toml",
            &dependency_manifest(
                "text-utils",
                "1.2.3",
                "[dependencies.app]\npath = \"packages/app-link\"\nversion = \"0.1.0\"",
            ),
        );
        duplicate.write("packages/app-link/allen.toml", &root_manifest(""));
        duplicate.write(
            "packages/app-link/src/main.allen",
            "export fn main() returns Int { 1 }",
        );
        assert_eq!(
            generate_lock(&duplicate.0, &LoadLimits::default())
                .unwrap_err()
                .code,
            PackageErrorCode::DuplicateIdentity
        );

        let version = package_fixture();
        version.write(
            MANIFEST_FILE,
            &root_manifest(
                "[dependencies.text_utils]\npath = \"packages/text-utils\"\nversion = \"^2.0.0\"",
            ),
        );
        assert_eq!(
            generate_lock(&version.0, &LoadLimits::default())
                .unwrap_err()
                .code,
            PackageErrorCode::VersionConflict
        );

        let language = package_fixture();
        language.write(
            "packages/text-utils/allen.toml",
            &dependency_manifest("text-utils", "1.2.3", "")
                .replace(">=0.1.0, <0.2.0", ">=0.2.0, <0.3.0"),
        );
        assert_eq!(
            generate_lock(&language.0, &LoadLimits::default())
                .unwrap_err()
                .code,
            PackageErrorCode::LanguageConflict
        );
    }

    #[test]
    fn enforces_package_module_depth_and_byte_limits() {
        let directory = package_fixture();
        for (limits, code) in [
            (
                LoadLimits {
                    packages: 1,
                    ..LoadLimits::default()
                },
                PackageErrorCode::PackageLimit,
            ),
            (
                LoadLimits {
                    dependency_depth: 0,
                    ..LoadLimits::default()
                },
                PackageErrorCode::DependencyDepthLimit,
            ),
            (
                LoadLimits {
                    modules: 1,
                    ..LoadLimits::default()
                },
                PackageErrorCode::ModuleLimit,
            ),
            (
                LoadLimits {
                    source_bytes: 1,
                    ..LoadLimits::default()
                },
                PackageErrorCode::SourceBytesLimit,
            ),
        ] {
            assert_eq!(generate_lock(&directory.0, &limits).unwrap_err().code, code);
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dependency_and_source_symlinks() {
        use std::os::unix::fs::symlink;

        let dependency_link = package_fixture();
        fs::rename(
            dependency_link.0.join("packages/text-utils"),
            dependency_link.0.join("packages/real-text-utils"),
        )
        .unwrap();
        symlink(
            "real-text-utils",
            dependency_link.0.join("packages/text-utils"),
        )
        .unwrap();
        assert_eq!(
            generate_lock(&dependency_link.0, &LoadLimits::default())
                .unwrap_err()
                .code,
            PackageErrorCode::Symlink
        );

        let source_link = package_fixture();
        source_link.write("outside.allen", "export fn outside() returns Int { 1 }");
        symlink("../outside.allen", source_link.0.join("src/link.allen")).unwrap();
        assert_eq!(
            generate_lock(&source_link.0, &LoadLimits::default())
                .unwrap_err()
                .code,
            PackageErrorCode::Symlink
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_root_is_not_redirected_by_an_ambient_path_swap() {
        use std::os::unix::fs::symlink;

        let directory = package_fixture();
        let replacement = TestDirectory::new();
        replacement.write(MANIFEST_FILE, &root_manifest(""));
        replacement.write("src/main.allen", "export fn main() returns Int { 999 }\n");
        let root = directory.0.clone();
        let moved = root.with_extension("opened-root");
        let opened = open_root_directory(&root).unwrap();

        fs::rename(&root, &moved).unwrap();
        symlink(&replacement.0, &root).unwrap();
        let result = load_unlocked_from_open_root(&root, &LoadLimits::default(), opened);
        fs::remove_file(&root).unwrap();
        fs::rename(&moved, &root).unwrap();

        let loaded = result.unwrap();
        assert!(
            loaded
                .modules
                .iter()
                .any(|module| module.source.contains("main() returns Int { 42 }"))
        );
        assert!(
            !loaded
                .modules
                .iter()
                .any(|module| module.source.contains("999"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_source_directory_rejects_a_file_symlink_swap() {
        use std::os::unix::fs::symlink;

        let directory = package_fixture();
        directory.write("outside.allen", "export fn main() returns Int { 999 }\n");
        let root = open_root_directory(&directory.0).unwrap();
        let source_path = directory.0.join(SOURCE_DIRECTORY);
        let source = open_directory_no_follow(
            &root,
            SOURCE_DIRECTORY,
            &source_path,
            "source directory cannot be a symlink",
            "source root is not a directory",
        )
        .unwrap();
        fs::rename(
            source_path.join("main.allen"),
            source_path.join("safe.allen"),
        )
        .unwrap();
        symlink("../outside.allen", source_path.join("main.allen")).unwrap();

        assert_eq!(
            read_regular_file_at(&source, "main.allen", &source_path.join("main.allen"), 1024,)
                .unwrap_err()
                .code,
            PackageErrorCode::Symlink
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hard_linked_package_files() {
        let directory = package_fixture();
        fs::hard_link(
            directory.0.join("src/main.allen"),
            directory.0.join("src/linked.allen"),
        )
        .unwrap();
        assert_eq!(
            generate_lock(&directory.0, &LoadLimits::default())
                .unwrap_err()
                .code,
            PackageErrorCode::SpecialFile
        );
    }

    #[test]
    fn bounds_directory_enumeration_before_collecting_all_entries() {
        let directory = package_fixture();
        for index in 0..256 {
            directory.write(
                &format!("src/generated_{index:03}.allen"),
                "fn generated() returns Int { 1 }\n",
            );
        }
        let limits = LoadLimits {
            filesystem_entries: 8,
            ..LoadLimits::default()
        };
        assert_eq!(
            generate_lock(&directory.0, &limits).unwrap_err().code,
            PackageErrorCode::ModuleLimit
        );
    }

    #[test]
    fn rejects_non_source_files_below_src() {
        let directory = package_fixture();
        directory.write("src/native.so", "not source");
        assert_eq!(
            generate_lock(&directory.0, &LoadLimits::default())
                .unwrap_err()
                .code,
            PackageErrorCode::SpecialFile
        );
    }
}
