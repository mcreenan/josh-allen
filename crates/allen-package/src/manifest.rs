use std::collections::{BTreeMap, BTreeSet};

use semver::{Version, VersionReq};
use serde::Deserialize;

use allen_exec::CommandPattern;

use crate::{PackageError, PackageErrorCode};

pub const SUPPORTED_LANGUAGE: &str = "0.1.1";
const MAX_MANIFEST_TEXT_BYTES: usize = 1024 * 1024;
const MAX_BOUNDARY_TYPE_BYTES: usize = 4096;
const MAX_RESPONSE_ATTEMPTS: u32 = 3;
const SUPPORTED_CAPABILITIES: [&str; 14] = [
    "agent.ask",
    "agent.message",
    "agent.transcript",
    "exec.run",
    "fs.read",
    "fs.write",
    "net.http_get",
    "permission.request_external_fs",
    "model.request",
    "sub_agent.ask",
    "sub_agent.create",
    "sub_agent.message",
    "sub_agent.run",
    "user.ask",
];

/// The package identity and language requirement from `allen.toml`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub language: String,
}

/// One public entry contract. The compiler verifies the type strings exactly.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    pub function: String,
    pub input: String,
    pub output: String,
}

/// Required and optional authority. Local effects do not appear here.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    pub required: Vec<String>,
    pub optional: Vec<String>,
}

/// Canonical HTTPS origins requested by the package.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct HttpGetNetwork {
    pub origins: Vec<String>,
}

/// Strict network authority scopes.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    pub http_get: HttpGetNetwork,
}

/// One required typed tool selected from the frozen host catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolRequirement {
    pub name: String,
    pub version: String,
}

/// Required tools. Optional tools are not part of the current language.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Tools {
    pub required: Vec<ToolRequirement>,
}

/// Whitelisted process requests carried by a package manifest.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct ExecRequests {
    commands: Vec<String>,
    environment: Vec<String>,
}

/// One declared external template and its closed hole signature.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TemplateDeclaration {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) holes: BTreeMap<String, String>,
}

/// Supported launch ceilings.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ManifestLimits {
    pub wall_ms: Option<u64>,
    pub instructions: Option<u64>,
    pub heap_bytes: Option<u64>,
    pub maximum_allocation_bytes: Option<u64>,
    pub call_depth: Option<u32>,
    pub tasks: Option<u32>,
    pub concurrent_effects: Option<u32>,
    pub response_attempts: Option<u32>,
    pub effects: Option<u64>,
    pub cleanup_instructions: Option<u64>,
    pub input_bytes: Option<u64>,
    pub output_bytes: Option<u64>,
    pub fs_operations: Option<u64>,
    pub fs_read_bytes: Option<u64>,
    pub fs_write_bytes: Option<u64>,
    pub fs_file_bytes: Option<u64>,
    pub fs_entries: Option<u64>,
    pub http_requests: Option<u64>,
    pub http_redirects: Option<u64>,
    pub http_dns_addresses: Option<u64>,
    pub http_response_headers: Option<u64>,
    pub http_response_header_bytes: Option<u64>,
    pub http_compressed_bytes: Option<u64>,
    pub http_decoded_bytes: Option<u64>,
    pub http_decompression_ratio: Option<u64>,
    pub http_connect_ms: Option<u64>,
    pub http_first_byte_ms: Option<u64>,
    pub http_idle_ms: Option<u64>,
    pub http_total_ms: Option<u64>,
}

/// One exact local dependency declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub path: String,
    pub version: String,
}

/// The strict current `allen.toml` data model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub package: Package,
    #[serde(default, rename = "entry")]
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub tools: Tools,
    #[serde(default)]
    exec: ExecRequests,
    #[serde(default)]
    templates: Vec<TemplateDeclaration>,
    #[serde(default)]
    pub limits: ManifestLimits,
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
}

impl Manifest {
    /// Return the sorted, unique command patterns requested by this package.
    #[must_use]
    pub fn exec_commands(&self) -> &[String] {
        &self.exec.commands
    }

    /// Return the sorted, unique environment names requested by this package.
    #[must_use]
    pub fn exec_environment(&self) -> &[String] {
        &self.exec.environment
    }

    /// Return whether one argv vector is covered by a requested command pattern.
    #[must_use]
    pub fn allows_exec_argv<T: AsRef<str>>(&self, argv: &[T]) -> bool {
        self.exec
            .commands
            .iter()
            .any(|pattern| command_pattern_matches(pattern, argv))
    }

    /// Return whether a canonical host grant is no broader than this request.
    #[must_use]
    pub fn allows_exec_grant(&self, grant: &str) -> bool {
        validate_exec_command(grant).is_ok()
            && self
                .exec
                .commands
                .iter()
                .any(|request| command_pattern_covers(request, grant))
    }

    /// Iterate over canonical template declarations in name order.
    #[must_use]
    pub fn templates(
        &self,
    ) -> impl ExactSizeIterator<Item = (&str, &str, &BTreeMap<String, String>)> {
        self.templates.iter().map(|template| {
            (
                template.name.as_str(),
                template.path.as_str(),
                &template.holes,
            )
        })
    }

    pub(crate) fn template_declarations(&self) -> &[TemplateDeclaration] {
        &self.templates
    }
}

/// Parse and validate one strict package manifest.
///
/// # Errors
///
/// Returns a stable manifest error for malformed or unsupported input.
pub fn parse_manifest(text: &str) -> Result<Manifest, PackageError> {
    if text.len() > MAX_MANIFEST_TEXT_BYTES {
        return Err(PackageError::new(
            PackageErrorCode::InvalidManifest,
            "manifest exceeds the text limit",
        ));
    }
    let mut manifest: Manifest = toml::from_str(text).map_err(|error| {
        PackageError::new(
            PackageErrorCode::InvalidManifest,
            format!("manifest is not strict TOML: {error}"),
        )
    })?;
    validate_package(&manifest.package)?;
    validate_entries(&mut manifest.entries)?;
    validate_capabilities(&mut manifest.capabilities)?;
    validate_network(&mut manifest.network, &manifest.capabilities)?;
    validate_tools(&mut manifest.tools)?;
    validate_exec(&mut manifest.exec)?;
    validate_templates(&mut manifest.templates)?;
    validate_limits(&manifest.limits)?;
    validate_dependencies(&manifest.dependencies)?;
    Ok(manifest)
}

fn validate_templates(templates: &mut [TemplateDeclaration]) -> Result<(), PackageError> {
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for template in templates.iter() {
        if !is_source_identifier(&template.name) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidManifest,
                format!(
                    "template name '{}' is not a source identifier",
                    template.name
                ),
            ));
        }
        if !names.insert(template.name.as_str()) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidManifest,
                format!("template name '{}' is duplicated", template.name),
            ));
        }
        let normalized_path = normalize_template_path(&template.path)?;
        if !paths.insert(normalized_path) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidManifest,
                format!("template path '{}' is duplicated", template.path),
            ));
        }
        for (hole, value_type) in &template.holes {
            if !is_source_identifier(hole) {
                return Err(PackageError::new(
                    PackageErrorCode::InvalidManifest,
                    format!(
                        "template '{}' hole '{hole}' is not a source identifier",
                        template.name
                    ),
                ));
            }
            if !matches!(value_type.as_str(), "Bool" | "Int" | "Float" | "String") {
                return Err(PackageError::new(
                    PackageErrorCode::InvalidManifest,
                    format!(
                        "template '{}' hole '{hole}' has unsupported type '{value_type}'",
                        template.name
                    ),
                ));
            }
        }
    }
    templates.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(())
}

fn validate_exec(exec: &mut ExecRequests) -> Result<(), PackageError> {
    for pattern in &exec.commands {
        validate_exec_command(pattern)?;
    }
    exec.commands.sort();
    exec.commands.dedup();

    for name in &exec.environment {
        if !is_environment_name(name) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidCapability,
                format!("exec environment name '{name}' is not canonical"),
            ));
        }
        if name.eq_ignore_ascii_case("LC_ALL") || name.eq_ignore_ascii_case("TZ") {
            return Err(PackageError::new(
                PackageErrorCode::InvalidCapability,
                format!("exec environment name '{name}' is reserved by the host"),
            ));
        }
    }
    exec.environment.sort();
    exec.environment.dedup();
    Ok(())
}

fn validate_exec_command(pattern: &str) -> Result<(), PackageError> {
    CommandPattern::parse(pattern).map(|_| ()).map_err(|_| {
        PackageError::new(
            PackageErrorCode::InvalidCapability,
            format!("exec command pattern '{pattern}' is not canonical"),
        )
    })
}

fn is_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|first| {
        (first.is_ascii_alphabetic() || first == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

fn command_pattern_matches<T: AsRef<str>>(pattern: &str, argv: &[T]) -> bool {
    CommandPattern::parse(pattern).is_ok_and(|pattern| pattern.matches(argv))
}

fn command_pattern_covers(request: &str, grant: &str) -> bool {
    CommandPattern::parse(request)
        .is_ok_and(|request| CommandPattern::parse(grant).is_ok_and(|grant| request.covers(&grant)))
}

fn validate_tools(tools: &mut Tools) -> Result<(), PackageError> {
    for tool in &tools.required {
        allen_schema::ToolRequirement::parse(&tool.name, &tool.version)
            .map_err(|_| invalid_tool(&tool.name, &tool.version))?;
    }
    tools
        .required
        .sort_by(|left, right| left.name.cmp(&right.name));
    if tools
        .required
        .windows(2)
        .any(|pair| pair[0].name == pair[1].name)
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidTool,
            "required tool name is duplicated",
        ));
    }
    Ok(())
}

fn invalid_tool(name: &str, version: &str) -> PackageError {
    PackageError::new(
        PackageErrorCode::InvalidTool,
        format!("required tool '{name}' with range '{version}' is not canonical"),
    )
}

fn validate_network(
    network: &mut Network,
    capabilities: &Capabilities,
) -> Result<(), PackageError> {
    for origin in &network.http_get.origins {
        canonical_https_origin(origin)?;
    }
    network.http_get.origins.sort();
    if network
        .http_get
        .origins
        .windows(2)
        .any(|pair| pair[0] == pair[1])
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidCapability,
            "HTTP origins are duplicated",
        ));
    }
    let requests_http = capabilities
        .required
        .iter()
        .chain(&capabilities.optional)
        .any(|capability| capability == "net.http_get");
    if requests_http == network.http_get.origins.is_empty() {
        return Err(PackageError::new(
            PackageErrorCode::InvalidCapability,
            "net.http_get and network.http_get.origins must be declared together",
        ));
    }
    Ok(())
}

/// Validate and return one canonical HTTPS origin.
///
/// # Errors
///
/// Returns a stable capability error for every noncanonical or non-HTTPS
/// origin.
pub fn canonical_https_origin(origin: &str) -> Result<String, PackageError> {
    let parsed = url::Url::parse(origin).map_err(|_| {
        PackageError::new(
            PackageErrorCode::InvalidCapability,
            format!("HTTP origin '{origin}' is invalid"),
        )
    })?;
    let canonical = parsed.origin().ascii_serialization();
    if parsed.scheme() != "https"
        || parsed.cannot_be_a_base()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
        || canonical == "null"
        || canonical != origin
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidCapability,
            format!("HTTP origin '{origin}' is not canonical HTTPS"),
        ));
    }
    Ok(canonical)
}

fn validate_package(package: &Package) -> Result<(), PackageError> {
    if !is_package_name(&package.name) {
        return Err(PackageError::new(
            PackageErrorCode::InvalidName,
            format!("package name '{}' is not canonical", package.name),
        ));
    }
    parse_exact_version(&package.version)?;
    parse_language_requirement(&package.language)?;
    Ok(())
}

fn validate_entries(entries: &mut [Entry]) -> Result<(), PackageError> {
    let mut names = BTreeSet::new();
    for entry in entries.iter() {
        if !is_source_identifier(&entry.name) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidEntry,
                format!("entry name '{}' is not a source identifier", entry.name),
            ));
        }
        if !names.insert(entry.name.as_str()) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidEntry,
                format!("entry name '{}' is duplicated", entry.name),
            ));
        }
        let Some((module, function)) = entry.function.rsplit_once("::") else {
            return Err(PackageError::new(
                PackageErrorCode::InvalidEntry,
                format!(
                    "entry function '{}' is not module-qualified",
                    entry.function
                ),
            ));
        };
        if normalize_source_path(module).as_deref() != Ok(module) || !is_source_identifier(function)
        {
            return Err(PackageError::new(
                PackageErrorCode::InvalidEntry,
                format!("entry function '{}' is not canonical", entry.function),
            ));
        }
        validate_boundary_type(&entry.input, "input")?;
        validate_boundary_type(&entry.output, "output")?;
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(())
}

fn validate_boundary_type(value: &str, field: &str) -> Result<(), PackageError> {
    if value.is_empty()
        || value.len() > MAX_BOUNDARY_TYPE_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidEntry,
            format!("entry {field} type is not canonical"),
        ));
    }
    Ok(())
}

fn validate_capabilities(capabilities: &mut Capabilities) -> Result<(), PackageError> {
    let mut all = BTreeSet::new();
    for capability in capabilities.required.iter().chain(&capabilities.optional) {
        if !SUPPORTED_CAPABILITIES.contains(&capability.as_str()) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidCapability,
                format!("capability '{capability}' is not available in language 0.1"),
            ));
        }
        if !all.insert(capability.as_str()) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidCapability,
                format!("capability '{capability}' is duplicated"),
            ));
        }
    }
    capabilities.required.sort();
    capabilities.optional.sort();
    Ok(())
}

fn validate_limits(limits: &ManifestLimits) -> Result<(), PackageError> {
    let u64_limits = [
        ("wall_ms", limits.wall_ms),
        ("instructions", limits.instructions),
        ("heap_bytes", limits.heap_bytes),
        ("maximum_allocation_bytes", limits.maximum_allocation_bytes),
        ("effects", limits.effects),
        ("cleanup_instructions", limits.cleanup_instructions),
        ("input_bytes", limits.input_bytes),
        ("output_bytes", limits.output_bytes),
        ("fs_operations", limits.fs_operations),
        ("fs_read_bytes", limits.fs_read_bytes),
        ("fs_write_bytes", limits.fs_write_bytes),
        ("fs_file_bytes", limits.fs_file_bytes),
        ("fs_entries", limits.fs_entries),
        ("http_requests", limits.http_requests),
        ("http_redirects", limits.http_redirects),
        ("http_dns_addresses", limits.http_dns_addresses),
        ("http_response_headers", limits.http_response_headers),
        (
            "http_response_header_bytes",
            limits.http_response_header_bytes,
        ),
        ("http_compressed_bytes", limits.http_compressed_bytes),
        ("http_decoded_bytes", limits.http_decoded_bytes),
        ("http_decompression_ratio", limits.http_decompression_ratio),
        ("http_connect_ms", limits.http_connect_ms),
        ("http_first_byte_ms", limits.http_first_byte_ms),
        ("http_idle_ms", limits.http_idle_ms),
        ("http_total_ms", limits.http_total_ms),
    ];
    if let Some((name, _)) = u64_limits
        .into_iter()
        .find(|(_, value)| value.is_some_and(|value| value == 0))
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLimit,
            format!("limit '{name}' must be greater than zero"),
        ));
    }
    let u32_limits = [
        ("call_depth", limits.call_depth),
        ("tasks", limits.tasks),
        ("concurrent_effects", limits.concurrent_effects),
        ("response_attempts", limits.response_attempts),
    ];
    if let Some((name, _)) = u32_limits
        .into_iter()
        .find(|(_, value)| value.is_some_and(|value| value == 0))
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLimit,
            format!("limit '{name}' must be greater than zero"),
        ));
    }
    if limits
        .response_attempts
        .is_some_and(|value| value > MAX_RESPONSE_ATTEMPTS)
    {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLimit,
            format!("limit 'response_attempts' cannot exceed {MAX_RESPONSE_ATTEMPTS}"),
        ));
    }
    if matches!(
        (limits.maximum_allocation_bytes, limits.heap_bytes),
        (Some(maximum), Some(heap)) if maximum > heap
    ) {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLimit,
            "maximum_allocation_bytes cannot exceed heap_bytes",
        ));
    }
    Ok(())
}

fn validate_dependencies(dependencies: &BTreeMap<String, Dependency>) -> Result<(), PackageError> {
    for (alias, dependency) in dependencies {
        if !is_source_identifier(alias) {
            return Err(PackageError::new(
                PackageErrorCode::InvalidDependency,
                format!("dependency alias '{alias}' is not a source identifier"),
            ));
        }
        normalize_dependency_path(&dependency.path)?;
        parse_version_requirement(&dependency.version)?;
    }
    Ok(())
}

pub(crate) fn parse_exact_version(value: &str) -> Result<Version, PackageError> {
    let version = Version::parse(value).map_err(|error| {
        PackageError::new(
            PackageErrorCode::InvalidVersion,
            format!("version '{value}' is invalid: {error}"),
        )
    })?;
    if version.to_string() != value {
        return Err(PackageError::new(
            PackageErrorCode::InvalidVersion,
            format!("version '{value}' is not canonical"),
        ));
    }
    Ok(version)
}

pub(crate) fn parse_language_requirement(value: &str) -> Result<VersionReq, PackageError> {
    if value.is_empty() || value.trim() != value {
        return Err(PackageError::new(
            PackageErrorCode::InvalidLanguage,
            "language requirement is empty or has outer whitespace",
        ));
    }
    VersionReq::parse(value).map_err(|error| {
        PackageError::new(
            PackageErrorCode::InvalidLanguage,
            format!("language requirement '{value}' is invalid: {error}"),
        )
    })
}

pub(crate) fn parse_version_requirement(value: &str) -> Result<VersionReq, PackageError> {
    if value.is_empty() || value.trim() != value {
        return Err(PackageError::new(
            PackageErrorCode::InvalidDependency,
            "dependency version requirement is empty or has outer whitespace",
        ));
    }
    VersionReq::parse(value).map_err(|error| {
        PackageError::new(
            PackageErrorCode::InvalidDependency,
            format!("dependency version requirement '{value}' is invalid: {error}"),
        )
    })
}

pub(crate) fn normalize_dependency_path(value: &str) -> Result<String, PackageError> {
    normalize_path(value, false).map_err(|()| {
        PackageError::new(
            PackageErrorCode::PathEscape,
            format!("dependency path '{value}' is not normalized below the root package"),
        )
    })
}

pub(crate) fn normalize_template_path(value: &str) -> Result<String, PackageError> {
    normalize_path(value, false).map_err(|()| {
        PackageError::new(
            PackageErrorCode::InvalidManifest,
            format!("template path '{value}' is not normalized below the package root"),
        )
    })
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub(crate) fn normalize_source_path(value: &str) -> Result<String, PackageError> {
    let normalized = normalize_path(value, false).map_err(|()| {
        PackageError::new(
            PackageErrorCode::InvalidEntry,
            format!("source path '{value}' is not normalized"),
        )
    })?;
    if !normalized.starts_with("src/") || !normalized.ends_with(".allen") {
        return Err(PackageError::new(
            PackageErrorCode::InvalidEntry,
            format!("source path '{value}' must be a .allen file below src"),
        ));
    }
    Ok(normalized)
}

pub(crate) fn is_package_name(value: &str) -> bool {
    let mut previous_hyphen = false;
    for (index, byte) in value.bytes().enumerate() {
        let valid = if index == 0 {
            byte.is_ascii_lowercase()
        } else {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
        };
        if !valid || (byte == b'-' && previous_hyphen) {
            return false;
        }
        previous_hyphen = byte == b'-';
    }
    !value.is_empty() && !value.ends_with('-') && value.len() <= 128
}

pub(crate) fn is_source_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|first| {
        (first.is_ascii_lowercase() || first == b'_')
            && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            && value.len() <= 128
    })
}

fn normalize_path(value: &str, allow_dot: bool) -> Result<String, ()> {
    if value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains('\0')
        || value.contains('\\')
        || value.contains(':')
        || value.len() > 4096
    {
        return Err(());
    }
    if allow_dot && value == "." {
        return Ok(value.to_owned());
    }
    if value
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(());
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
[package]
name = "review-release"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Input"
output = "Result<Report, Error>"

[capabilities]
required = ["fs.read"]
optional = ["fs.write"]

[limits]
wall_ms = 30000
heap_bytes = 1024
maximum_allocation_bytes = 512

[dependencies.text_utils]
path = "packages/text-utils"
version = "^1.2.0"
"#;

    #[test]
    fn parses_and_normalizes_the_manifest_model() {
        let manifest = parse_manifest(MANIFEST).unwrap();
        assert_eq!(manifest.package.name, "review-release");
        assert_eq!(manifest.entries[0].name, "main");
        assert_eq!(manifest.capabilities.required, ["fs.read"]);
        assert_eq!(
            manifest.dependencies["text_utils"].path,
            "packages/text-utils"
        );
    }

    #[test]
    fn parses_sorts_and_validates_required_tools() {
        let text = format!(
            "{MANIFEST}\n[[tools.required]]\nname = \"release-tools.create-issue\"\nversion = \">=2.0.0, <3.0.0\"\n\n[[tools.required]]\nname = \"deploy\"\nversion = \">=1.2.3, <2.0.0\"\n"
        );
        let manifest = parse_manifest(&text).unwrap();
        assert_eq!(
            manifest
                .tools
                .required
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["deploy", "release-tools.create-issue"]
        );
    }

    #[test]
    fn parses_canonical_exec_requests_and_matches_argv_exactly() {
        let text = format!(
            r#"{MANIFEST}
[exec]
commands = ["git status", "aws cloudwatch *", "aws cloudwatch *"]
environment = ["HOME", "AWS_REGION", "Path", "AWS_REGION"]
"#
        );
        let manifest = parse_manifest(&text).unwrap();
        assert_eq!(manifest.exec_commands(), ["aws cloudwatch *", "git status"]);
        assert_eq!(manifest.exec_environment(), ["AWS_REGION", "HOME", "Path"]);
        assert!(manifest.allows_exec_argv(&["git", "status"]));
        assert!(!manifest.allows_exec_argv(&["git", "status", "--short"]));
        assert!(manifest.allows_exec_argv(&["aws", "cloudwatch"]));
        assert!(manifest.allows_exec_argv(&[
            "aws",
            "cloudwatch",
            "describe-alarms",
            "--state-value",
            "ALARM",
        ]));
        assert!(!manifest.allows_exec_argv(&["aws", "s3", "ls"]));
        assert!(manifest.allows_exec_grant("git status"));
        assert!(!manifest.allows_exec_grant("git status *"));
        assert!(manifest.allows_exec_grant("aws cloudwatch *"));
        assert!(manifest.allows_exec_grant("aws cloudwatch describe-alarms"));
        assert!(manifest.allows_exec_grant("aws cloudwatch describe-alarms *"));
        assert!(!manifest.allows_exec_grant("aws *"));
        assert!(!manifest.allows_exec_grant("aws cloudwatch*"));
    }

    #[test]
    fn manifest_without_exec_preserves_an_empty_request() {
        let manifest = parse_manifest(MANIFEST).unwrap();
        assert!(manifest.exec_commands().is_empty());
        assert!(manifest.exec_environment().is_empty());
        assert!(!manifest.allows_exec_argv(&["anything"]));
        assert_eq!(manifest.templates().len(), 0);
    }

    #[test]
    fn parses_and_sorts_canonical_template_declarations() {
        let text = format!(
            r#"{MANIFEST}
[[templates]]
name = "summary"
path = "templates/summary.txt"
holes = {{ title = "String" }}

[[templates]]
name = "alert"
path = "templates/alert.txt"
holes = {{ title = "String", active = "Bool", ratio = "Float", count = "Int" }}
"#
        );
        let manifest = parse_manifest(&text).unwrap();
        let templates = manifest.templates().collect::<Vec<_>>();
        assert_eq!(
            templates
                .iter()
                .map(|(name, path, _)| (*name, *path))
                .collect::<Vec<_>>(),
            [
                ("alert", "templates/alert.txt"),
                ("summary", "templates/summary.txt"),
            ]
        );
        assert_eq!(
            templates[0]
                .2
                .iter()
                .map(|(name, value_type)| (name.as_str(), value_type.as_str()))
                .collect::<Vec<_>>(),
            [
                ("active", "Bool"),
                ("count", "Int"),
                ("ratio", "Float"),
                ("title", "String"),
            ]
        );
    }

    #[test]
    fn rejects_invalid_template_declarations() {
        let declaration = |name: &str, path: &str, holes: &str| {
            format!(
                "{MANIFEST}\n[[templates]]\nname = \"{name}\"\npath = \"{path}\"\nholes = {holes}\n"
            )
        };
        for text in [
            declaration("Bad", "templates/a.txt", r#"{ value = "String" }"#),
            declaration("a", "../a.txt", r#"{ value = "String" }"#),
            declaration("a", "/a.txt", r#"{ value = "String" }"#),
            declaration("a", "templates//a.txt", r#"{ value = "String" }"#),
            declaration("a", "templates/./a.txt", r#"{ value = "String" }"#),
            declaration("a", "templates\\a.txt", r#"{ value = "String" }"#),
            declaration("a", "templates/a.txt", r#"{ Bad = "String" }"#),
            declaration("a", "templates/a.txt", r#"{ value = "Bytes" }"#),
        ] {
            assert_eq!(
                parse_manifest(&text).unwrap_err().code,
                PackageErrorCode::InvalidManifest,
            );
        }

        let duplicate = format!(
            r#"{MANIFEST}
[[templates]]
name = "same"
path = "templates/a.txt"
holes = {{}}

[[templates]]
name = "same"
path = "templates/b.txt"
holes = {{}}
"#
        );
        let duplicate_error = parse_manifest(&duplicate).unwrap_err();
        assert_eq!(duplicate_error.code, PackageErrorCode::InvalidManifest);
        assert_eq!(
            duplicate_error.message,
            "template name 'same' is duplicated"
        );

        let duplicate_path = format!(
            r#"{MANIFEST}
[[templates]]
name = "first"
path = "templates/shared.txt"
holes = {{ value = "String" }}

[[templates]]
name = "second"
path = "templates/shared.txt"
holes = {{ value = "Int" }}
"#
        );
        let error = parse_manifest(&duplicate_path).unwrap_err();
        assert_eq!(error.code, PackageErrorCode::InvalidManifest);
        assert_eq!(
            error.message,
            "template path 'templates/shared.txt' is duplicated"
        );
    }

    #[test]
    fn rejects_unknown_template_keys() {
        let text = format!(
            r#"{MANIFEST}
[[templates]]
name = "alert"
path = "templates/alert.txt"
holes = {{ title = "String" }}
escaping = "shell"
"#
        );
        assert_eq!(
            parse_manifest(&text).unwrap_err().code,
            PackageErrorCode::InvalidManifest,
        );
    }

    #[test]
    fn rejects_noncanonical_exec_command_patterns() {
        for commands in [
            r#"[""]"#,
            r#"[" aws"]"#,
            r#"["aws "]"#,
            r#"["aws  cloudwatch"]"#,
            r#"["aws\tcloudwatch"]"#,
            "['aws\u{a0}cloudwatch']",
            "['aw\u{200d}s cloudwatch']",
            "['\u{e5}ws cloudwatch']",
            "['aws clo\u{200d}udwatch']",
            "['aws caf\u{e9}']",
            r#"['aws "cloudwatch"']"#,
            r#"["aws 'cloudwatch'"]"#,
            r"['aws \cloudwatch']",
            r#"["/usr/bin/aws cloudwatch"]"#,
            r#"["bin/aws cloudwatch"]"#,
            r#"["*"]"#,
            r#"["aws cloud*"]"#,
            r#"["aws * cloudwatch"]"#,
            r#"["aws cloudwatch * more"]"#,
        ] {
            let text = format!("{MANIFEST}\n[exec]\ncommands = {commands}\n");
            assert_eq!(
                parse_manifest(&text).unwrap_err().code,
                PackageErrorCode::InvalidCapability,
                "commands = {commands}",
            );
        }

        let slash_in_argument =
            format!("{MANIFEST}\n[exec]\ncommands = [\"aws /var/log/messages\"]\n");
        assert!(parse_manifest(&slash_in_argument).is_ok());
    }

    #[test]
    fn rejects_invalid_or_reserved_exec_environment_names() {
        for name in [
            "",
            "1AWS",
            "AWS-REGION",
            "AWS.REGION",
            "ÅWS_REGION",
            "LC_ALL",
            "lc_all",
            "Lc_All",
            "TZ",
            "tz",
            "Tz",
        ] {
            let text = format!("{MANIFEST}\n[exec]\nenvironment = [\"{name}\"]\n");
            assert_eq!(
                parse_manifest(&text).unwrap_err().code,
                PackageErrorCode::InvalidCapability,
                "environment = {name}",
            );
        }
    }

    #[test]
    fn rejects_unknown_exec_keys() {
        let text = format!("{MANIFEST}\n[exec]\ncommands = [\"aws *\"]\nshell = true\n");
        assert_eq!(
            parse_manifest(&text).unwrap_err().code,
            PackageErrorCode::InvalidManifest,
        );
    }

    #[test]
    fn rejects_invalid_required_tools() {
        let cases = [
            ("bad..name", ">=1.0.0, <2.0.0"),
            ("bad name", ">=1.0.0, <2.0.0"),
            ("deploy", "^1.0.0"),
            ("deploy", ">=01.0.0, <2.0.0"),
            ("deploy", ">=2.0.0, <2.0.0"),
            ("deploy", ">=1.0.0, <2.0.0-beta"),
        ];
        for (name, version) in cases {
            let text = format!(
                "{MANIFEST}\n[[tools.required]]\nname = \"{name}\"\nversion = \"{version}\"\n"
            );
            assert_eq!(
                parse_manifest(&text).unwrap_err().code,
                PackageErrorCode::InvalidTool,
                "{name} {version}"
            );
        }

        let duplicate = format!(
            "{MANIFEST}\n[[tools.required]]\nname = \"deploy\"\nversion = \">=1.0.0, <2.0.0\"\n\n[[tools.required]]\nname = \"deploy\"\nversion = \">=2.0.0, <3.0.0\"\n"
        );
        assert_eq!(
            parse_manifest(&duplicate).unwrap_err().code,
            PackageErrorCode::InvalidTool
        );
    }

    #[test]
    fn rejects_unknown_duplicate_and_unsupported_fields() {
        for (text, code) in [
            (
                format!("{MANIFEST}\ntools = []\n"),
                PackageErrorCode::InvalidManifest,
            ),
            (
                MANIFEST.replace("[limits]", "[limits]\nwall_ms = 1"),
                PackageErrorCode::InvalidManifest,
            ),
            (
                MANIFEST.replace("optional = [\"fs.write\"]", "optional = [\"net.http_get\"]"),
                PackageErrorCode::InvalidCapability,
            ),
            (
                MANIFEST.replace("optional = [\"fs.write\"]", "optional = [\"fs.read\"]"),
                PackageErrorCode::InvalidCapability,
            ),
        ] {
            assert_eq!(parse_manifest(&text).unwrap_err().code, code);
        }
    }

    #[test]
    fn rejects_invalid_names_paths_versions_entries_and_limits() {
        let cases = [
            ("review-release", "Review", PackageErrorCode::InvalidName),
            (
                "version = \"0.1.0\"",
                "version = \"01.1.0\"",
                PackageErrorCode::InvalidVersion,
            ),
            (
                "path = \"packages/text-utils\"",
                "path = \"../text-utils\"",
                PackageErrorCode::PathEscape,
            ),
            (
                "function = \"src/main.allen::main\"",
                "function = \"main.allen::main\"",
                PackageErrorCode::InvalidEntry,
            ),
            (
                "wall_ms = 30000",
                "wall_ms = 0",
                PackageErrorCode::InvalidLimit,
            ),
        ];
        for (from, to, code) in cases {
            assert_eq!(
                parse_manifest(&MANIFEST.replace(from, to))
                    .unwrap_err()
                    .code,
                code
            );
        }
    }

    #[test]
    fn accepts_sorted_canonical_https_origins_with_network_authority() {
        let text = MANIFEST.replace(
            "optional = [\"fs.write\"]",
            "optional = [\"fs.write\", \"net.http_get\"]\n\n[network.http_get]\norigins = [\"https://xn--bcher-kva.example\", \"https://api.example.com:8443\", \"https://api.example.com\"]",
        );
        let manifest = parse_manifest(&text).unwrap();
        assert_eq!(
            manifest.network.http_get.origins,
            [
                "https://api.example.com".to_owned(),
                "https://api.example.com:8443".to_owned(),
                "https://xn--bcher-kva.example".to_owned()
            ]
        );
    }

    #[test]
    fn accepts_and_sorts_invoking_agent_provider_capabilities() {
        let text = MANIFEST.replace(
            "required = [\"fs.read\"]",
            "required = [\"agent.transcript\", \"fs.read\", \"agent.ask\", \"agent.message\"]",
        );
        let manifest = parse_manifest(&text).unwrap();
        assert_eq!(
            manifest.capabilities.required,
            [
                "agent.ask".to_owned(),
                "agent.message".to_owned(),
                "agent.transcript".to_owned(),
                "fs.read".to_owned(),
            ]
        );
    }

    #[test]
    fn accepts_and_sorts_response_provider_capabilities() {
        let text = MANIFEST.replace(
            "required = [\"fs.read\"]",
            "required = [\"user.ask\", \"fs.read\", \"model.request\"]",
        );
        let manifest = parse_manifest(&text).unwrap();
        assert_eq!(
            manifest.capabilities.required,
            [
                "fs.read".to_owned(),
                "model.request".to_owned(),
                "user.ask".to_owned(),
            ]
        );
    }

    #[test]
    fn accepts_and_sorts_sub_agent_capabilities() {
        let text = MANIFEST.replace(
            "required = [\"fs.read\"]",
            "required = [\"sub_agent.run\", \"sub_agent.ask\", \"fs.read\", \"sub_agent.message\", \"sub_agent.create\"]",
        );
        let manifest = parse_manifest(&text).unwrap();
        assert_eq!(
            manifest.capabilities.required,
            [
                "fs.read".to_owned(),
                "sub_agent.ask".to_owned(),
                "sub_agent.create".to_owned(),
                "sub_agent.message".to_owned(),
                "sub_agent.run".to_owned(),
            ]
        );
    }

    #[test]
    fn validates_response_attempt_limit() {
        let with_limit = |value| {
            MANIFEST.replace(
                "maximum_allocation_bytes = 512",
                &format!("maximum_allocation_bytes = 512\nresponse_attempts = {value}"),
            )
        };
        let manifest = parse_manifest(&with_limit(3)).unwrap();
        assert_eq!(manifest.limits.response_attempts, Some(3));
        for value in [0, 4] {
            let error = parse_manifest(&with_limit(value)).unwrap_err();
            assert_eq!(error.code, PackageErrorCode::InvalidLimit);
        }
    }

    #[test]
    fn rejects_noncanonical_or_unpaired_http_origins() {
        for origin in [
            "http://api.example.com",
            "https://user@api.example.com",
            "https://api.example.com/path",
            "https://api.example.com/",
            "https://API.example.com",
        ] {
            let text = MANIFEST.replace(
                "optional = [\"fs.write\"]",
                &format!(
                    "optional = [\"fs.write\", \"net.http_get\"]\n\n[network.http_get]\norigins = [\"{origin}\"]"
                ),
            );
            assert_eq!(
                parse_manifest(&text).unwrap_err().code,
                PackageErrorCode::InvalidCapability
            );
        }
        let without_capability =
            format!("{MANIFEST}\n[network.http_get]\norigins = [\"https://api.example.com\"]\n");
        assert_eq!(
            parse_manifest(&without_capability).unwrap_err().code,
            PackageErrorCode::InvalidCapability
        );
        let without_origins = MANIFEST.replace(
            "optional = [\"fs.write\"]",
            "optional = [\"fs.write\", \"net.http_get\"]",
        );
        assert_eq!(
            parse_manifest(&without_origins).unwrap_err().code,
            PackageErrorCode::InvalidCapability
        );
    }
}
