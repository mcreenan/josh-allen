use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use serde::Deserialize;

use crate::manifest::{
    SUPPORTED_LANGUAGE, is_package_name, is_source_identifier, normalize_dependency_path,
    parse_exact_version,
};
use crate::{PackageError, PackageErrorCode};

pub const LOCK_VERSION: u32 = 1;
const MAX_LOCK_BYTES: usize = 16 * 1024 * 1024;
const MAX_LOCK_PACKAGES: usize = 16_384;
const MAX_LOCK_DEPENDENCIES: usize = 65_536;

/// The canonical `allen.lock` model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Lockfile {
    pub lock_version: u32,
    pub language: String,
    pub root: String,
    #[serde(default, rename = "package")]
    pub packages: Vec<LockedPackage>,
}

/// One exact package source in the lock graph.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub source: String,
    pub digest: String,
    #[serde(default)]
    pub dependencies: Vec<LockedDependency>,
}

/// One sorted dependency edge in the lock graph.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LockedDependency {
    pub alias: String,
    pub package: String,
}

/// Parse and validate the lockfile model.
///
/// Call [`canonical_lockfile`] and compare its bytes when canonical text is
/// required.
///
/// # Errors
///
/// Returns a stable lock error for malformed graphs and values.
pub fn parse_lockfile(text: &str) -> Result<Lockfile, PackageError> {
    if text.len() > MAX_LOCK_BYTES {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLockfile,
            "lockfile exceeds its byte limit",
        ));
    }
    let lock: Lockfile = toml::from_str(text).map_err(|error| {
        PackageError::new(
            PackageErrorCode::InvalidLockfile,
            format!("lockfile is not strict TOML: {error}"),
        )
    })?;
    validate_lockfile(&lock)?;
    Ok(lock)
}

/// Render the canonical lockfile representation.
#[must_use]
pub fn canonical_lockfile(lock: &Lockfile) -> String {
    let mut output = String::new();
    writeln!(output, "lock_version = {}", lock.lock_version).expect("String writes cannot fail");
    writeln!(output, "language = {}", quote(&lock.language)).expect("String writes cannot fail");
    writeln!(output, "root = {}", quote(&lock.root)).expect("String writes cannot fail");
    for package in &lock.packages {
        output.push_str("\n[[package]]\n");
        writeln!(output, "name = {}", quote(&package.name)).expect("String writes cannot fail");
        writeln!(output, "version = {}", quote(&package.version))
            .expect("String writes cannot fail");
        writeln!(output, "source = {}", quote(&package.source)).expect("String writes cannot fail");
        writeln!(output, "digest = {}", quote(&package.digest)).expect("String writes cannot fail");
        if package.dependencies.is_empty() {
            output.push_str("dependencies = []\n");
        } else {
            output.push_str("dependencies = [\n");
            for dependency in &package.dependencies {
                writeln!(
                    output,
                    "  {{ alias = {}, package = {} }},",
                    quote(&dependency.alias),
                    quote(&dependency.package)
                )
                .expect("String writes cannot fail");
            }
            output.push_str("]\n");
        }
    }
    output
}

fn validate_lockfile(lock: &Lockfile) -> Result<(), PackageError> {
    if lock.lock_version != LOCK_VERSION {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLockfile,
            format!("lock version {} is not supported", lock.lock_version),
        ));
    }
    if lock.language != SUPPORTED_LANGUAGE {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLockfile,
            format!("locked language '{}' is not supported", lock.language),
        ));
    }
    if lock.packages.is_empty() {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLockfile,
            "lockfile contains no packages",
        ));
    }
    if lock.packages.len() > MAX_LOCK_PACKAGES
        || lock
            .packages
            .iter()
            .try_fold(0_usize, |total, package| {
                total.checked_add(package.dependencies.len())
            })
            .is_none_or(|total| total > MAX_LOCK_DEPENDENCIES)
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLockfile,
            "lockfile graph exceeds its table limits",
        ));
    }
    let mut identities = BTreeSet::new();
    let mut sources = BTreeSet::new();
    let mut previous = None;
    for package in &lock.packages {
        validate_locked_package(package)?;
        let key = (&package.name, &package.version, &package.source);
        if previous.is_some_and(|value| value >= key) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidLockfile,
                "locked packages are not in canonical order",
            ));
        }
        previous = Some(key);
        let identity = package_identity(&package.name, &package.version);
        if !identities.insert(identity) || !sources.insert(package.source.as_str()) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidLockfile,
                "lockfile contains a duplicate package identity or source",
            ));
        }
    }
    if !identities.contains(&lock.root) {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLockfile,
            "locked root does not identify one package row",
        ));
    }
    for package in &lock.packages {
        for dependency in &package.dependencies {
            if !identities.contains(&dependency.package) {
                return Err(PackageError::new(
                    PackageErrorCode::InvalidLockfile,
                    format!(
                        "dependency '{}' names missing package '{}'",
                        dependency.alias, dependency.package
                    ),
                ));
            }
        }
    }
    validate_lock_graph(lock)?;
    Ok(())
}

fn validate_lock_graph(lock: &Lockfile) -> Result<(), PackageError> {
    let identities = lock
        .packages
        .iter()
        .map(|package| (package_identity(&package.name, &package.version), package))
        .collect::<Vec<_>>();
    let packages = identities
        .iter()
        .map(|(identity, package)| (identity.as_str(), *package))
        .collect::<BTreeMap<_, _>>();
    let mut states = BTreeMap::new();
    states.insert(lock.root.as_str(), 1_u8);
    let mut pending = vec![(lock.root.as_str(), 0_usize)];
    while let Some((identity, next)) = pending.last_mut() {
        let package = packages[*identity];
        if let Some(dependency) = package.dependencies.get(*next) {
            *next += 1;
            match states.get(dependency.package.as_str()).copied() {
                Some(1) => {
                    return Err(PackageError::new(
                        PackageErrorCode::InvalidLockfile,
                        "lockfile dependency graph contains a cycle",
                    ));
                }
                Some(2) => {}
                None => {
                    states.insert(dependency.package.as_str(), 1);
                    pending.push((dependency.package.as_str(), 0));
                }
                Some(_) => unreachable!("lock traversal state is internal"),
            }
        } else {
            let (complete, _) = pending.pop().expect("pending lock node exists");
            states.insert(complete, 2);
        }
    }
    if states.len() != lock.packages.len() {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLockfile,
            "lockfile contains a package unreachable from the root",
        ));
    }
    Ok(())
}

fn validate_locked_package(package: &LockedPackage) -> Result<(), PackageError> {
    if !is_package_name(&package.name) {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLockfile,
            format!("locked package name '{}' is not canonical", package.name),
        ));
    }
    parse_exact_version(&package.version)
        .map_err(|error| PackageError::new(PackageErrorCode::InvalidLockfile, error.message))?;
    if package.source != "." {
        normalize_dependency_path(&package.source)
            .map_err(|error| PackageError::new(PackageErrorCode::InvalidLockfile, error.message))?;
    }
    if package.digest.len() != 71
        || !package.digest.starts_with("sha256:")
        || !package.digest[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLockfile,
            format!("package '{}' has a noncanonical digest", package.name),
        ));
    }
    let mut aliases = BTreeSet::new();
    let mut previous = None;
    for dependency in &package.dependencies {
        if !is_source_identifier(&dependency.alias)
            || parse_package_identity(&dependency.package).is_err()
        {
            return Err(PackageError::new(
                PackageErrorCode::InvalidLockfile,
                format!("package '{}' has an invalid dependency edge", package.name),
            ));
        }
        if previous.is_some_and(|value: &str| value >= dependency.alias.as_str())
            || !aliases.insert(dependency.alias.as_str())
        {
            return Err(PackageError::new(
                PackageErrorCode::InvalidLockfile,
                format!(
                    "package '{}' dependency edges are not unique and sorted",
                    package.name
                ),
            ));
        }
        previous = Some(dependency.alias.as_str());
    }
    Ok(())
}

pub(crate) fn package_identity(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

fn parse_package_identity(value: &str) -> Result<(&str, &str), ()> {
    let (name, version) = value.rsplit_once('@').ok_or(())?;
    if !is_package_name(name) || parse_exact_version(version).is_err() {
        return Err(());
    }
    Ok((name, version))
}

fn quote(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '\u{0008}' => output.push_str("\\b"),
            '\t' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\u{000c}' => output.push_str("\\f"),
            '\r' => output.push_str("\\r"),
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            value if value.is_control() => {
                write!(output, "\\u{:04X}", u32::from(value)).expect("String writes cannot fail");
            }
            value => output.push(value),
        }
    }
    output.push('"');
    output
}

pub(crate) fn lockfile_from_parts(root: String, mut packages: Vec<LockedPackage>) -> Lockfile {
    packages.sort_by(|left, right| {
        (&left.name, &left.version, &left.source).cmp(&(&right.name, &right.version, &right.source))
    });
    Lockfile {
        lock_version: LOCK_VERSION,
        language: SUPPORTED_LANGUAGE.to_owned(),
        root,
        packages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_parses_exact_canonical_lock_text() {
        let lock = lockfile_from_parts(
            "root@0.1.0".to_owned(),
            vec![
                LockedPackage {
                    name: "root".to_owned(),
                    version: "0.1.0".to_owned(),
                    source: ".".to_owned(),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    dependencies: vec![LockedDependency {
                        alias: "text_utils".to_owned(),
                        package: "text-utils@1.2.0".to_owned(),
                    }],
                },
                LockedPackage {
                    name: "text-utils".to_owned(),
                    version: "1.2.0".to_owned(),
                    source: "packages/text-utils".to_owned(),
                    digest: format!("sha256:{}", "b".repeat(64)),
                    dependencies: Vec::new(),
                },
            ],
        );
        let text = canonical_lockfile(&lock);
        assert_eq!(parse_lockfile(&text).unwrap(), lock);
        assert_eq!(canonical_lockfile(&parse_lockfile(&text).unwrap()), text);
    }

    #[test]
    fn rejects_unknown_duplicate_and_unsorted_lock_data() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let base = format!(
            "lock_version = 1\nlanguage = \"0.1.0\"\nroot = \"a@1.0.0\"\n\n[[package]]\nname = \"a\"\nversion = \"1.0.0\"\nsource = \".\"\ndigest = \"{digest}\"\ndependencies = []\n"
        );
        assert!(parse_lockfile(&base).is_ok());
        assert_eq!(
            parse_lockfile(&base.replace("dependencies", "unknown = 1\ndependencies"))
                .unwrap_err()
                .code,
            PackageErrorCode::InvalidLockfile
        );
    }
}
