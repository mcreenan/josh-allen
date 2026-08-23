use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::rc::Rc;

use crate::{
    BoolBinaryOp, CapabilityOperation, CheckedIntOperation, CompareOp, Constant, Conversion,
    EnumPayloadType, EnumSwitchArm, EnumType, EnumVariant, Function, FunctionId, Instruction,
    MAX_VALUE_NESTING, Module, NumericBinaryOp, RecordField, Register, SafeCollectionOperation,
    StringOperation, ToolVerificationContract, ValueType, VerifiedModule, canonical_float_bits,
    verify_internal,
};

pub const ARTIFACT_MAGIC: [u8; 8] = *b"ALLEN\0\x01\0";
/// Current artifact-format identifier stored in the binary header.
pub const BYTECODE_VERSION: u16 = 13;
pub const HEADER_SIZE: usize = 64;

const MANDATORY_SECTION_COUNT: usize = 9;
const MAX_SECTION_COUNT: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
enum SectionId {
    Strings = 1,
    Constants = 2,
    Types = 3,
    Functions = 4,
    Effects = 5,
    Entries = 6,
    Schemas = 7,
    Imports = 8,
    ManifestContracts = 9,
    Debug = 10,
}

impl SectionId {
    const MANDATORY: [Self; MANDATORY_SECTION_COUNT] = [
        Self::Strings,
        Self::Constants,
        Self::Types,
        Self::Functions,
        Self::Effects,
        Self::Entries,
        Self::Schemas,
        Self::Imports,
        Self::ManifestContracts,
    ];

    fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::Strings),
            2 => Some(Self::Constants),
            3 => Some(Self::Types),
            4 => Some(Self::Functions),
            5 => Some(Self::Effects),
            6 => Some(Self::Entries),
            7 => Some(Self::Schemas),
            8 => Some(Self::Imports),
            9 => Some(Self::ManifestContracts),
            10 => Some(Self::Debug),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SemanticVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl SemanticVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl fmt::Display for SemanticVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum TargetProfile {
    Portable = 1,
}

impl fmt::Display for TargetProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Portable => formatter.write_str("portable"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactMetadata {
    pub bytecode_version: u16,
    pub language_version: SemanticVersion,
    pub compiler_version: SemanticVersion,
    pub target_profile: TargetProfile,
}

impl Default for ArtifactMetadata {
    fn default() -> Self {
        Self {
            bytecode_version: BYTECODE_VERSION,
            language_version: SemanticVersion::new(0, 1, 0),
            compiler_version: SemanticVersion::new(0, 1, 0),
            target_profile: TargetProfile::Portable,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugLocation {
    pub function: FunctionId,
    pub instruction: u32,
    pub source: u32,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugInfo {
    pub sources: Vec<String>,
    pub locations: Vec<DebugLocation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StrictSchema {
    pub value_type: ValueType,
}

/// Encode one value type with a stable, self-delimiting representation.
///
/// This encoding is independent of Rust's debug formatting and of the
/// artifact string and type tables. Enum references retain their canonical
/// artifact-local enum ID.
#[must_use]
pub fn canonical_value_type_bytes(value_type: &ValueType) -> Vec<u8> {
    let mut output = b"ALLEN-VALUE-TYPE\0\x01".to_vec();
    encode_canonical_value_type(&mut output, value_type);
    output
}

/// Compute the stable SHA-256 digest of one strict boundary schema.
#[must_use]
pub fn compute_strict_schema_digest(schema: &StrictSchema) -> [u8; 32] {
    sha256(&canonical_value_type_bytes(&schema.value_type))
}

fn encode_canonical_value_type(output: &mut Vec<u8>, value_type: &ValueType) {
    match value_type {
        ValueType::Int => output.push(0),
        ValueType::Bool => output.push(1),
        ValueType::Float => output.push(2),
        ValueType::String => output.push(3),
        ValueType::Bytes => output.push(4),
        ValueType::Unit => output.push(5),
        ValueType::Never => output.push(6),
        ValueType::List(element) => {
            output.push(7);
            encode_canonical_value_type(output, element);
        }
        ValueType::Map(key, value) => {
            output.push(8);
            encode_canonical_value_type(output, key);
            encode_canonical_value_type(output, value);
        }
        ValueType::Tuple(elements) => {
            output.push(9);
            put_canonical_count(output, elements.len());
            for element in elements {
                encode_canonical_value_type(output, element);
            }
        }
        ValueType::Record(fields) => {
            output.push(10);
            put_canonical_count(output, fields.len());
            for field in fields {
                put_canonical_text(output, &field.name);
                encode_canonical_value_type(output, &field.value_type);
            }
        }
        ValueType::Enum(id) => {
            output.push(11);
            output.extend_from_slice(&id.to_le_bytes());
        }
        ValueType::Option(value) => {
            output.push(12);
            encode_canonical_value_type(output, value);
        }
        ValueType::Result(ok, error) => {
            output.push(13);
            encode_canonical_value_type(output, ok);
            encode_canonical_value_type(output, error);
        }
        ValueType::Function {
            parameters,
            return_type,
            effects,
        } => {
            output.push(14);
            put_canonical_count(output, parameters.len());
            for parameter in parameters {
                encode_canonical_value_type(output, parameter);
            }
            encode_canonical_value_type(output, return_type);
            output.extend_from_slice(&effects.to_le_bytes());
        }
        ValueType::Unknown => output.push(15),
        ValueType::Future(value) => {
            output.push(16);
            encode_canonical_value_type(output, value);
        }
        ValueType::Task(value) => {
            output.push(17);
            encode_canonical_value_type(output, value);
        }
        ValueType::Workspace => output.push(18),
        ValueType::ExternalFsAccess => output.push(19),
        ValueType::SubAgent => output.push(20),
    }
}

fn put_canonical_count(output: &mut Vec<u8>, count: usize) {
    output.extend_from_slice(&u64::try_from(count).unwrap_or(u64::MAX).to_le_bytes());
}

fn put_canonical_text(output: &mut Vec<u8>, text: &str) {
    put_canonical_count(output, text.len());
    output.extend_from_slice(text.as_bytes());
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryContract {
    pub name: String,
    pub function: FunctionId,
    pub input_schema: u32,
    pub output_schema: u32,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportContract {
    pub importer: String,
    pub alias: String,
    pub package: String,
    pub version: String,
    pub module: String,
    pub content_digest: [u8; 32],
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestContract {
    pub package: String,
    pub version: String,
    pub language_requirement: String,
    pub required_capabilities: Vec<String>,
    pub optional_capabilities: Vec<String>,
    pub limits: Vec<(String, u64)>,
    pub https_origins: Vec<String>,
    pub required_tools: Vec<ToolContract>,
    pub tool_contract_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolContract {
    pub name: String,
    pub version: String,
    pub version_requirement: String,
    pub effect: String,
    pub input_schema: u32,
    pub output_schema: u32,
    pub error_schema: u32,
    pub input_digest: [u8; 32],
    pub output_digest: [u8; 32],
    pub error_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Artifact {
    pub metadata: ArtifactMetadata,
    pub module: Module,
    pub debug: Option<DebugInfo>,
    pub schemas: Vec<StrictSchema>,
    pub entries: Vec<EntryContract>,
    pub imports: Vec<ImportContract>,
    pub manifest: Option<ManifestContract>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SectionSummary {
    pub name: &'static str,
    pub entries: usize,
}

impl Artifact {
    #[must_use]
    pub fn section_summaries(&self) -> Vec<SectionSummary> {
        let mut type_entries = collect_types(self);
        type_entries.sort_by_key(|value_type| format!("{value_type:?}"));
        type_entries.dedup();
        let mut summaries = vec![
            SectionSummary {
                name: "strings",
                entries: collect_strings(self).len(),
            },
            SectionSummary {
                name: "constants",
                entries: self.module.constants.len(),
            },
            SectionSummary {
                name: "types",
                entries: type_entries.len(),
            },
            SectionSummary {
                name: "functions",
                entries: self.module.functions.len(),
            },
            SectionSummary {
                name: "effects",
                entries: self.module.effect_sets.len(),
            },
            SectionSummary {
                name: "entries",
                entries: self.entries.len(),
            },
            SectionSummary {
                name: "schemas",
                entries: self.schemas.len(),
            },
            SectionSummary {
                name: "imports",
                entries: self.imports.len(),
            },
            SectionSummary {
                name: "manifest_contracts",
                entries: usize::from(self.manifest.is_some()),
            },
        ];
        summaries.push(SectionSummary {
            name: "tools",
            entries: self
                .manifest
                .as_ref()
                .map_or(0, |manifest| manifest.required_tools.len()),
        });
        if let Some(debug) = &self.debug {
            summaries.push(SectionSummary {
                name: "debug",
                entries: debug.locations.len(),
            });
        }
        summaries
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedArtifact {
    artifact: Artifact,
    content_digest: [u8; 32],
}

impl DecodedArtifact {
    #[must_use]
    pub const fn artifact(&self) -> &Artifact {
        &self.artifact
    }

    #[must_use]
    pub const fn metadata(&self) -> &ArtifactMetadata {
        &self.artifact.metadata
    }

    #[must_use]
    pub const fn module(&self) -> &Module {
        &self.artifact.module
    }

    #[must_use]
    pub const fn debug(&self) -> Option<&DebugInfo> {
        self.artifact.debug.as_ref()
    }

    #[must_use]
    pub const fn content_digest(&self) -> &[u8; 32] {
        &self.content_digest
    }

    #[must_use]
    pub fn section_summaries(&self) -> Vec<SectionSummary> {
        self.artifact.section_summaries()
    }

    /// Re-encode the decoded artifact in canonical form.
    ///
    /// # Errors
    ///
    /// Returns an error if the decoded model cannot be represented.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ArtifactError> {
        encode(&self.artifact)
    }

    /// Re-encode the decoded artifact in canonical form within explicit limits.
    ///
    /// # Errors
    ///
    /// Returns an error if the decoded model cannot be represented within `limits`.
    pub fn canonical_bytes_with_limits(
        &self,
        limits: &DecodeLimits,
    ) -> Result<Vec<u8>, ArtifactError> {
        encode_with_limits(&self.artifact, limits)
    }

    #[must_use]
    pub fn into_artifact(self) -> Artifact {
        self.artifact
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedArtifact {
    metadata: ArtifactMetadata,
    module: VerifiedModule,
    debug: Option<DebugInfo>,
    schemas: Vec<StrictSchema>,
    entries: Vec<EntryContract>,
    imports: Vec<ImportContract>,
    manifest: Option<ManifestContract>,
    content_digest: [u8; 32],
}

impl VerifiedArtifact {
    #[must_use]
    pub const fn metadata(&self) -> &ArtifactMetadata {
        &self.metadata
    }

    #[must_use]
    pub const fn verified_module(&self) -> &VerifiedModule {
        &self.module
    }

    #[must_use]
    pub const fn debug(&self) -> Option<&DebugInfo> {
        self.debug.as_ref()
    }

    #[must_use]
    pub fn schemas(&self) -> &[StrictSchema] {
        &self.schemas
    }

    #[must_use]
    pub fn entries(&self) -> &[EntryContract] {
        &self.entries
    }

    #[must_use]
    pub fn imports(&self) -> &[ImportContract] {
        &self.imports
    }

    #[must_use]
    pub const fn manifest(&self) -> Option<&ManifestContract> {
        self.manifest.as_ref()
    }

    #[must_use]
    pub const fn content_digest(&self) -> &[u8; 32] {
        &self.content_digest
    }

    #[must_use]
    pub fn section_summaries(&self) -> Vec<SectionSummary> {
        let artifact = Artifact {
            metadata: self.metadata,
            module: self.module.module().clone(),
            debug: self.debug.clone(),
            schemas: self.schemas.clone(),
            entries: self.entries.clone(),
            imports: self.imports.clone(),
            manifest: self.manifest.clone(),
        };
        artifact.section_summaries()
    }

    #[must_use]
    pub fn into_verified_module(self) -> VerifiedModule {
        self.module
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeLimits {
    pub artifact_bytes: usize,
    pub section_bytes: usize,
    pub string_bytes: usize,
    pub table_entries: usize,
    pub functions: usize,
    pub registers_per_function: usize,
    pub instructions_per_function: usize,
    pub operands_per_instruction: usize,
    pub type_depth: usize,
    pub debug_records: usize,
    pub verifier_state_bytes: usize,
    pub expanded_type_nodes: usize,
    pub decoded_model_bytes: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            artifact_bytes: 64 * 1024 * 1024,
            section_bytes: 32 * 1024 * 1024,
            string_bytes: 1024 * 1024,
            table_entries: 1_000_000,
            functions: 100_000,
            registers_per_function: 65_536,
            instructions_per_function: 1_000_000,
            operands_per_instruction: 65_536,
            type_depth: MAX_VALUE_NESTING,
            debug_records: 1_000_000,
            verifier_state_bytes: 64 * 1024 * 1024,
            expanded_type_nodes: 1_000_000,
            decoded_model_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactErrorCode {
    InvalidMagic,
    InvalidHeader,
    UnsupportedVersion,
    UnsupportedProfile,
    ArtifactTooLarge,
    SectionTooLarge,
    MissingSection,
    DuplicateSection,
    UnknownSection,
    SectionOrder,
    DigestMismatch,
    Truncated,
    TrailingBytes,
    InvalidUtf8,
    LimitExceeded,
    InvalidScalar,
    NonCanonical,
    VerificationFailed,
    InvalidDebug,
}

impl ArtifactErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidMagic => "ARTIFACT_INVALID_MAGIC",
            Self::InvalidHeader => "ARTIFACT_INVALID_HEADER",
            Self::UnsupportedVersion => "ARTIFACT_UNSUPPORTED_VERSION",
            Self::UnsupportedProfile => "ARTIFACT_UNSUPPORTED_PROFILE",
            Self::ArtifactTooLarge => "ARTIFACT_TOO_LARGE",
            Self::SectionTooLarge => "ARTIFACT_SECTION_TOO_LARGE",
            Self::MissingSection => "ARTIFACT_MISSING_SECTION",
            Self::DuplicateSection => "ARTIFACT_DUPLICATE_SECTION",
            Self::UnknownSection => "ARTIFACT_UNKNOWN_SECTION",
            Self::SectionOrder => "ARTIFACT_SECTION_ORDER",
            Self::DigestMismatch => "ARTIFACT_DIGEST_MISMATCH",
            Self::Truncated => "ARTIFACT_TRUNCATED",
            Self::TrailingBytes => "ARTIFACT_TRAILING_BYTES",
            Self::InvalidUtf8 => "ARTIFACT_INVALID_UTF8",
            Self::LimitExceeded => "ARTIFACT_LIMIT_EXCEEDED",
            Self::InvalidScalar => "ARTIFACT_INVALID_SCALAR",
            Self::NonCanonical => "ARTIFACT_NON_CANONICAL",
            Self::VerificationFailed => "ARTIFACT_VERIFICATION_FAILED",
            Self::InvalidDebug => "ARTIFACT_INVALID_DEBUG",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactError {
    code: ArtifactErrorCode,
    message: String,
}

impl ArtifactError {
    fn new(code: ArtifactErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn code(&self) -> ArtifactErrorCode {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "artifact error [{}]: {}",
            self.code.as_str(),
            self.message
        )
    }
}

impl std::error::Error for ArtifactError {}

struct Section {
    id: SectionId,
    payload: Vec<u8>,
}

/// Encode a canonical current-format artifact.
///
/// # Errors
///
/// Returns an error when a value is non-canonical or cannot be represented.
pub fn encode(artifact: &Artifact) -> Result<Vec<u8>, ArtifactError> {
    encode_with_limits(artifact, &DecodeLimits::default())
}

/// Encode a canonical artifact within explicit format limits.
///
/// # Errors
///
/// Returns an error when a value is non-canonical, over limit, or cannot be represented.
#[allow(clippy::too_many_lines)]
pub fn encode_with_limits(
    artifact: &Artifact,
    limits: &DecodeLimits,
) -> Result<Vec<u8>, ArtifactError> {
    let version = artifact.metadata.bytecode_version;
    if version != BYTECODE_VERSION {
        return Err(ArtifactError::new(
            ArtifactErrorCode::UnsupportedVersion,
            format!("bytecode version {version} is not supported"),
        ));
    }
    validate_encode_model(artifact, limits)?;
    validate_debug_shape(artifact.debug.as_ref())?;
    let strings = collect_strings(artifact);
    if strings.len() > limits.table_entries
        || strings
            .iter()
            .any(|value| value.len() > limits.string_bytes)
    {
        return Err(limit("string table"));
    }
    let string_ids = strings
        .iter()
        .enumerate()
        .map(|(index, value)| Ok((value.as_str(), to_u32(index, "string table")?)))
        .collect::<Result<BTreeMap<_, _>, ArtifactError>>()?;
    let mut keyed_types = collect_types(artifact)
        .into_iter()
        .map(|value_type| Ok((type_key(&value_type, &string_ids)?, value_type)))
        .collect::<Result<Vec<_>, ArtifactError>>()?;
    keyed_types.sort_by(|left, right| left.0.cmp(&right.0));
    keyed_types.dedup_by(|left, right| left.0 == right.0);
    if keyed_types.len() > limits.table_entries {
        return Err(limit("type table"));
    }
    let expanded_nodes = keyed_types
        .iter()
        .try_fold(0_usize, |total, (_, value_type)| {
            total
                .checked_add(type_node_count(value_type)?)
                .ok_or_else(|| limit("expanded type nodes"))
        })?;
    if expanded_nodes > limits.expanded_type_nodes {
        return Err(limit("expanded type nodes"));
    }
    let types = keyed_types
        .into_iter()
        .map(|(_, value_type)| value_type)
        .collect::<Vec<_>>();
    let type_ids = types
        .iter()
        .enumerate()
        .map(|(index, value)| Ok((type_key(value, &string_ids)?, to_u32(index, "type table")?)))
        .collect::<Result<BTreeMap<_, _>, ArtifactError>>()?;

    let sections = encode_sections(artifact, &strings, &string_ids, &types, &type_ids)?;

    if sections
        .iter()
        .any(|section| section.payload.len() > limits.section_bytes)
    {
        return Err(ArtifactError::new(
            ArtifactErrorCode::SectionTooLarge,
            "encoded section bytes exceed the format limit",
        ));
    }

    let mut body = Vec::new();
    for section in &sections {
        put_u16(&mut body, section.id as u16);
        put_u64(
            &mut body,
            u64::try_from(section.payload.len()).map_err(|_| {
                ArtifactError::new(
                    ArtifactErrorCode::LimitExceeded,
                    "section byte length is not representable",
                )
            })?,
        );
        body.extend_from_slice(&section.payload);
    }
    let digest = sha256(&body);

    let mut output = Vec::with_capacity(HEADER_SIZE.saturating_add(body.len()));
    output.extend_from_slice(&ARTIFACT_MAGIC);
    put_u16(&mut output, 64);
    put_u16(&mut output, version);
    put_version(&mut output, artifact.metadata.language_version);
    put_version(&mut output, artifact.metadata.compiler_version);
    put_u16(&mut output, artifact.metadata.target_profile as u16);
    put_u16(
        &mut output,
        u16::try_from(sections.len()).map_err(|_| limit("section count"))?,
    );
    put_u32(&mut output, 0);
    output.extend_from_slice(&digest);
    debug_assert_eq!(output.len(), HEADER_SIZE);
    output.extend_from_slice(&body);
    if output.len() > limits.artifact_bytes {
        return Err(ArtifactError::new(
            ArtifactErrorCode::ArtifactTooLarge,
            "encoded artifact bytes exceed the format limit",
        ));
    }
    Ok(output)
}

#[allow(clippy::too_many_lines)]
fn validate_encode_model(artifact: &Artifact, limits: &DecodeLimits) -> Result<(), ArtifactError> {
    let manifest = artifact
        .manifest
        .as_ref()
        .ok_or_else(|| noncanonical("artifacts require exactly one manifest contract"))?;
    {
        if artifact.entries.is_empty() {
            return Err(noncanonical(
                "contract artifacts require entries and a manifest contract",
            ));
        }
        for entry in &artifact.entries {
            let function = artifact
                .module
                .functions
                .get(entry.function as usize)
                .ok_or_else(|| noncanonical("entry contract function is out of range"))?;
            let input = artifact
                .schemas
                .get(entry.input_schema as usize)
                .map(|schema| &schema.value_type)
                .ok_or_else(|| noncanonical("entry input schema is out of range"))?;
            let output = artifact
                .schemas
                .get(entry.output_schema as usize)
                .map(|schema| &schema.value_type)
                .ok_or_else(|| noncanonical("entry output schema is out of range"))?;
            let parameter_type = function
                .parameters
                .first()
                .and_then(|register| function.registers.get(*register as usize));
            if function.parameters.len() > 1
                || (function.parameters.is_empty() && input != &ValueType::Unit)
                || (function.parameters.len() == 1 && Some(input) != parameter_type)
                || output != &function.return_type
                || boundary_forbidden(input)
                || boundary_forbidden(output)
            {
                return Err(noncanonical("entry contract signature is invalid"));
            }
        }
        if !is_package_name(&manifest.package)
            || !is_canonical_version(&manifest.version)
            || !is_language_requirement(&manifest.language_requirement)
            || artifact
                .entries
                .iter()
                .any(|entry| !is_source_identifier(&entry.name))
            || manifest
                .required_capabilities
                .iter()
                .chain(&manifest.optional_capabilities)
                .any(|capability| !is_supported_capability(capability))
            || manifest.limits.iter().any(|(name, value)| {
                *value == 0
                    || name == "response_attempts" && *value > 3
                    || !EXECUTION_LIMITS.contains(&name.as_str())
                        && !HTTP_LIMITS.contains(&name.as_str())
                        && !RESPONSE_LIMITS.contains(&name.as_str())
            })
            || artifact.imports.iter().any(|import| {
                !is_package_identity(&import.importer)
                    || !is_source_identifier(&import.alias)
                    || !is_package_name(&import.package)
                    || !is_canonical_version(&import.version)
                    || !is_package_module_path(&import.module)
            })
        {
            return Err(noncanonical("contract identity is not canonical"));
        }
        let heap = manifest
            .limits
            .iter()
            .find(|(name, _)| name == "heap_bytes")
            .map(|(_, value)| *value);
        let maximum = manifest
            .limits
            .iter()
            .find(|(name, _)| name == "maximum_allocation_bytes")
            .map(|(_, value)| *value);
        if matches!((maximum, heap), (Some(maximum), Some(heap)) if maximum > heap) {
            return Err(noncanonical(
                "maximum allocation limit exceeds the heap limit",
            ));
        }
        if manifest
            .required_capabilities
            .iter()
            .any(|cap| manifest.optional_capabilities.binary_search(cap).is_ok())
        {
            return Err(noncanonical(
                "manifest required and optional capabilities must be disjoint",
            ));
        }
        for entry in &artifact.entries {
            let function = artifact
                .module
                .functions
                .get(entry.function as usize)
                .ok_or_else(|| noncanonical("entry contract function is out of range"))?;
            let effects = artifact
                .module
                .effect_sets
                .get(function.effects as usize)
                .ok_or_else(|| noncanonical("entry effect set is out of range"))?;
            for effect in effects {
                if effect != "task.spawn"
                    && effect != "debug.inspect"
                    && effect != "capability.inspect"
                    && manifest
                        .required_capabilities
                        .binary_search(effect)
                        .is_err()
                    && manifest
                        .optional_capabilities
                        .binary_search(effect)
                        .is_err()
                    && !manifest
                        .required_tools
                        .iter()
                        .any(|tool| tool.effect == *effect)
                {
                    return Err(noncanonical(
                        "manifest capabilities do not cover entry effects",
                    ));
                }
            }
        }
        validate_contract_graph(artifact, manifest)?;
    }
    if manifest
        .https_origins
        .iter()
        .any(|origin| !is_canonical_https_origin(origin))
    {
        return Err(noncanonical("HTTPS origin is not canonical"));
    }
    validate_tools(artifact, manifest)?;
    for function in &artifact.module.functions {
        for instruction in &function.code {
            let Some(output) = crate::typed_response_output_type(function, instruction) else {
                continue;
            };
            if !crate::is_strict_schema_type(output)
                || !artifact
                    .schemas
                    .iter()
                    .any(|schema| &schema.value_type == output)
            {
                return Err(noncanonical(
                    "typed response output requires an embedded strict schema",
                ));
            }
        }
    }
    if artifact.entries.iter().any(|entry| {
        entry.function as usize >= artifact.module.functions.len()
            || entry.input_schema as usize >= artifact.schemas.len()
            || entry.output_schema as usize >= artifact.schemas.len()
    }) {
        return Err(noncanonical("entry contract reference is out of range"));
    }
    if artifact.imports.iter().any(|import| {
        import.importer.is_empty()
            || import.alias.is_empty()
            || import.package.is_empty()
            || import.version.is_empty()
            || import.module.is_empty()
    }) {
        return Err(noncanonical("import contract identity is empty"));
    }
    if !artifact
        .entries
        .windows(2)
        .all(|pair| pair[0].name < pair[1].name)
        || !artifact.imports.windows(2).all(|pair| {
            (
                pair[0].importer.as_str(),
                pair[0].alias.as_str(),
                pair[0].package.as_str(),
                pair[0].version.as_str(),
                pair[0].module.as_str(),
            ) < (
                pair[1].importer.as_str(),
                pair[1].alias.as_str(),
                pair[1].package.as_str(),
                pair[1].version.as_str(),
                pair[1].module.as_str(),
            )
        })
        || {
            !manifest
                .required_capabilities
                .windows(2)
                .all(|pair| pair[0] < pair[1])
                || !manifest
                    .optional_capabilities
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
                || !manifest.limits.windows(2).all(|pair| pair[0].0 < pair[1].0)
                || !manifest
                    .https_origins
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
                || !manifest
                    .required_tools
                    .windows(2)
                    .all(|pair| pair[0].name < pair[1].name)
        }
    {
        return Err(noncanonical("contract tables must be sorted and unique"));
    }
    if artifact.module.constants.len() > limits.table_entries
        || artifact.module.enum_types.len() > limits.table_entries
        || artifact.module.effect_sets.len() > limits.table_entries
        || artifact.module.functions.len() > limits.functions
        || artifact.module.async_functions.len() > limits.functions
    {
        return Err(limit("artifact model table"));
    }
    if !artifact
        .module
        .async_functions
        .windows(2)
        .all(|pair| pair[0] < pair[1])
        || artifact
            .module
            .async_functions
            .last()
            .is_some_and(|id| *id as usize >= artifact.module.functions.len())
    {
        return Err(noncanonical(
            "async function IDs must be unique, sorted, and in range",
        ));
    }
    if artifact.module.constants.iter().any(
        |constant| matches!(constant, Constant::Bytes(value) if value.len() > limits.string_bytes),
    ) {
        return Err(limit("byte constant bytes"));
    }
    for function in &artifact.module.functions {
        if function.parameters.len() > limits.registers_per_function
            || function.captures.len() > limits.registers_per_function
            || function.registers.len() > limits.registers_per_function
            || function.code.len() > limits.instructions_per_function
        {
            return Err(limit("function model"));
        }
        for instruction in &function.code {
            if instruction_operand_count(instruction)? > limits.operands_per_instruction {
                return Err(limit("operands"));
            }
        }
    }
    if artifact.debug.as_ref().is_some_and(|debug| {
        debug.sources.len() > limits.debug_records || debug.locations.len() > limits.debug_records
    }) {
        return Err(limit("debug records"));
    }
    for value_type in artifact_value_types(artifact) {
        validate_type_depth(value_type, limits.type_depth)?;
    }
    Ok(())
}

fn boundary_forbidden(value: &ValueType) -> bool {
    !crate::is_strict_schema_type(value)
}

fn artifact_uses_agent_transcript(artifact: &Artifact) -> bool {
    artifact.module.functions.iter().any(|function| {
        function.code.iter().any(|instruction| {
            matches!(
                instruction,
                Instruction::EffectCall {
                    operation: crate::EffectOperation::AgentTranscript,
                    ..
                }
            )
        })
    })
}

fn is_supported_capability(capability: &str) -> bool {
    matches!(
        capability,
        "fs.read"
            | "fs.write"
            | "net.http_get"
            | "permission.request_external_fs"
            | "agent.message"
            | "agent.ask"
            | "agent.transcript"
            | "model.request"
            | "user.ask"
            | "sub_agent.create"
            | "sub_agent.run"
            | "sub_agent.message"
            | "sub_agent.ask"
    )
}

fn validate_tools(artifact: &Artifact, manifest: &ManifestContract) -> Result<(), ArtifactError> {
    if manifest.tool_contract_digest != compute_tool_contract_digest(&manifest.required_tools) {
        return Err(noncanonical(
            "tool contract digest does not match required tools",
        ));
    }
    for tool in &manifest.required_tools {
        if !is_canonical_tool_name(&tool.name)
            || !is_canonical_tool_version(&tool.version)
            || !is_tool_version_requirement(&tool.version_requirement)
            || !tool_requirement_contains(&tool.version_requirement, &tool.version)
            || !crate::is_canonical_effect_id(&tool.effect)
            || !tool.effect.starts_with("tool.")
            || expected_tool_effect(&tool.name, &tool.version).as_deref()
                != Some(tool.effect.as_str())
            || tool.input_schema as usize >= artifact.schemas.len()
            || tool.output_schema as usize >= artifact.schemas.len()
            || tool.error_schema as usize >= artifact.schemas.len()
        {
            return Err(noncanonical("typed tool contract is invalid"));
        }
    }
    for function in &artifact.module.functions {
        let effects = artifact
            .module
            .effect_sets
            .get(function.effects as usize)
            .ok_or_else(|| noncanonical("function effect set is out of range"))?;
        for instruction in &function.code {
            let Instruction::ToolInvoke {
                destination,
                tool,
                input,
            } = instruction
            else {
                continue;
            };
            let contract = manifest
                .required_tools
                .get(*tool as usize)
                .ok_or_else(|| noncanonical("tool invocation contract is out of range"))?;
            let input_type = function
                .registers
                .get(*input as usize)
                .ok_or_else(|| noncanonical("tool invocation input is out of range"))?;
            let output_type = function
                .registers
                .get(*destination as usize)
                .ok_or_else(|| noncanonical("tool invocation destination is out of range"))?;
            let expected_input = &artifact.schemas[contract.input_schema as usize].value_type;
            let output = artifact.schemas[contract.output_schema as usize]
                .value_type
                .clone();
            let declared_error = artifact.schemas[contract.error_schema as usize]
                .value_type
                .clone();
            let ValueType::Future(result) = output_type else {
                return Err(noncanonical("tool invocation result is not Future"));
            };
            let ValueType::Result(actual_output, wrapper) = result.as_ref() else {
                return Err(noncanonical("tool invocation result is not Result"));
            };
            if actual_output.as_ref() != &output
                || crate::tool_declared_error_type(&artifact.module, wrapper, &contract.name)
                    != Some(&declared_error)
            {
                return Err(noncanonical("tool invocation error wrapper is invalid"));
            }
            let expected_result = output_type.clone();
            if input_type != expected_input
                || output_type != &expected_result
                || effects.binary_search(&contract.effect).is_err()
            {
                return Err(noncanonical(
                    "tool invocation does not match its typed contract",
                ));
            }
        }
    }
    Ok(())
}

fn expected_tool_effect(name: &str, version: &str) -> Option<String> {
    let version = semver::Version::parse(version).ok()?;
    if version.major == 0 {
        return None;
    }
    let segments = name
        .split('.')
        .map(|segment| {
            let mut output = String::new();
            let mut first_preserved = None;
            for byte in segment.bytes() {
                if byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' {
                    first_preserved.get_or_insert(byte);
                    output.push(char::from(byte));
                } else {
                    use std::fmt::Write as _;
                    write!(output, "_x{byte:02x}_").expect("writing into a String cannot fail");
                }
            }
            if first_preserved.is_some_and(|byte| byte.is_ascii_digit()) {
                output.insert_str(0, "_n_");
            }
            output
        })
        .collect::<Vec<_>>()
        .join(".");
    Some(format!("tool.{segments}@{}", version.major))
}

fn is_canonical_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.split('.').all(|segment| {
            !segment.is_empty()
                && segment.len() <= 63
                && !segment
                    .chars()
                    .any(|value| value.is_control() || value.is_whitespace())
        })
}

fn is_canonical_tool_version(value: &str) -> bool {
    semver::Version::parse(value).is_ok_and(|version| {
        version.major != 0
            && version.pre.is_empty()
            && version.build.is_empty()
            && version.to_string() == value
    })
}

fn is_tool_version_requirement(value: &str) -> bool {
    let Some((lower, upper)) = value.split_once(", ") else {
        return false;
    };
    let (Some(lower), Some(upper)) = (lower.strip_prefix(">="), upper.strip_prefix('<')) else {
        return false;
    };
    if !is_canonical_release_version(lower) || !is_canonical_release_version(upper) {
        return false;
    }
    let Ok(lower) = semver::Version::parse(lower) else {
        return false;
    };
    let Ok(upper) = semver::Version::parse(upper) else {
        return false;
    };
    lower < upper
}

fn is_canonical_release_version(value: &str) -> bool {
    semver::Version::parse(value).is_ok_and(|version| {
        version.pre.is_empty() && version.build.is_empty() && version.to_string() == value
    })
}

fn tool_requirement_contains(requirement: &str, version: &str) -> bool {
    let Some((lower, upper)) = requirement.split_once(", ") else {
        return false;
    };
    let (Some(lower), Some(upper)) = (lower.strip_prefix(">="), upper.strip_prefix('<')) else {
        return false;
    };
    let (Ok(lower), Ok(upper), Ok(version)) = (
        semver::Version::parse(lower),
        semver::Version::parse(upper),
        semver::Version::parse(version),
    ) else {
        return false;
    };
    lower <= version && version < upper
}

/// Compute the canonical digest for one sorted selected-tool contract table.
#[must_use]
pub fn compute_tool_contract_digest(tools: &[ToolContract]) -> [u8; 32] {
    let mut json = String::from("{\"tools\":[");
    for (index, tool) in tools.iter().enumerate() {
        if index != 0 {
            json.push(',');
        }
        json.push_str("{\"effect\":");
        push_json_string(&mut json, &tool.effect);
        json.push_str(",\"error_schema\":");
        push_json_string(
            &mut json,
            &format!("sha256:{}", lower_hex(&tool.error_digest)),
        );
        json.push_str(",\"input_schema\":");
        push_json_string(
            &mut json,
            &format!("sha256:{}", lower_hex(&tool.input_digest)),
        );
        json.push_str(",\"name\":");
        push_json_string(&mut json, &tool.name);
        json.push_str(",\"output_schema\":");
        push_json_string(
            &mut json,
            &format!("sha256:{}", lower_hex(&tool.output_digest)),
        );
        json.push_str(",\"version\":");
        push_json_string(&mut json, &tool.version);
        json.push_str(",\"version_requirement\":");
        push_json_string(&mut json, &tool.version_requirement);
        json.push('}');
    }
    json.push_str("]}");
    sha256(json.as_bytes())
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\u0008"),
            '\u{0c}' => output.push_str("\\u000c"),
            '\n' => output.push_str("\\u000a"),
            '\r' => output.push_str("\\u000d"),
            '\t' => output.push_str("\\u0009"),
            value if value <= '\u{1f}' => {
                use std::fmt::Write as _;
                write!(output, "\\u{:04x}", u32::from(value))
                    .expect("writing into a String cannot fail");
            }
            value => output.push(value),
        }
    }
    output.push('"');
}

const EXECUTION_LIMITS: [&str; 16] = [
    "call_depth",
    "cleanup_instructions",
    "concurrent_effects",
    "effects",
    "fs_entries",
    "fs_file_bytes",
    "fs_operations",
    "fs_read_bytes",
    "fs_write_bytes",
    "heap_bytes",
    "input_bytes",
    "instructions",
    "maximum_allocation_bytes",
    "output_bytes",
    "tasks",
    "wall_ms",
];

const HTTP_LIMITS: [&str; 12] = [
    "http_compressed_bytes",
    "http_connect_ms",
    "http_decoded_bytes",
    "http_decompression_ratio",
    "http_dns_addresses",
    "http_first_byte_ms",
    "http_idle_ms",
    "http_redirects",
    "http_requests",
    "http_response_header_bytes",
    "http_response_headers",
    "http_total_ms",
];

const RESPONSE_LIMITS: [&str; 1] = ["response_attempts"];

fn is_source_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|first| {
        (first.is_ascii_lowercase() || first == b'_')
            && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    })
}

fn is_package_name(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase())
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn is_canonical_version(value: &str) -> bool {
    semver::Version::parse(value).is_ok_and(|version| version.to_string() == value)
}

fn is_language_requirement(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && semver::VersionReq::parse(value)
            .is_ok_and(|requirement| requirement.matches(&semver::Version::new(0, 1, 0)))
}

fn is_canonical_https_origin(value: &str) -> bool {
    let Some(authority) = value.strip_prefix("https://") else {
        return false;
    };
    if authority.is_empty()
        || authority.bytes().any(|byte| byte.is_ascii_control())
        || authority.contains(['/', '?', '#', '@'])
    {
        return false;
    }
    let (host, port) = if authority.starts_with('[') {
        let Some(close) = authority.find(']') else {
            return false;
        };
        let host = &authority[1..close];
        let suffix = &authority[close + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            let Some(port) = suffix.strip_prefix(':') else {
                return false;
            };
            Some(port)
        };
        let Ok(address) = host.parse::<std::net::Ipv6Addr>() else {
            return false;
        };
        if address.to_string() != host {
            return false;
        }
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, Some(port)),
            _ => (authority, None),
        }
    };
    if let Some(port) = port {
        if port.is_empty() || port.starts_with('0') || port.parse::<u16>().is_err() || port == "443"
        {
            return false;
        }
    }
    if authority.starts_with('[') {
        return true;
    }
    if let Ok(address) = host.parse::<std::net::Ipv4Addr>() {
        return address.to_string() == host;
    }
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

fn is_package_identity(value: &str) -> bool {
    value
        .rsplit_once('@')
        .is_some_and(|(name, version)| is_package_name(name) && is_canonical_version(version))
}

fn is_package_module_path(value: &str) -> bool {
    value.starts_with("src/") && is_normalized_utf8_source_path(value)
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_normalized_utf8_source_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path.ends_with(".allen")
        && !path
            .chars()
            .any(|character| character.is_control() || character == '\\')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[allow(clippy::too_many_lines)]
fn validate_contract_graph(
    artifact: &Artifact,
    manifest: &ManifestContract,
) -> Result<(), ArtifactError> {
    let root = format!("{}@{}", manifest.package, manifest.version);
    let mut graph = BTreeMap::<String, BTreeSet<String>>::new();
    graph.entry(root.clone()).or_default();
    let mut aliases = BTreeMap::<(String, String), String>::new();
    let mut digests = BTreeMap::<String, [u8; 32]>::new();
    for import in &artifact.imports {
        let target = format!("{}@{}", import.package, import.version);
        graph
            .entry(import.importer.clone())
            .or_default()
            .insert(target.clone());
        graph.entry(target.clone()).or_default();
        if aliases
            .insert(
                (import.importer.clone(), import.alias.clone()),
                target.clone(),
            )
            .is_some_and(|previous| previous != target)
        {
            return Err(noncanonical(
                "one package import alias cannot name two package identities",
            ));
        }
        if digests
            .insert(target.clone(), import.content_digest)
            .is_some_and(|previous| previous != import.content_digest)
        {
            return Err(noncanonical(
                "one imported package identity cannot have two content digests",
            ));
        }
    }

    let mut reachable = BTreeSet::from([root.clone()]);
    let mut pending = vec![root.clone()];
    while let Some(package) = pending.pop() {
        if let Some(targets) = graph.get(&package) {
            for target in targets.iter().rev() {
                if reachable.insert(target.clone()) {
                    pending.push(target.clone());
                }
            }
        }
    }
    if graph.keys().any(|package| !reachable.contains(package)) {
        return Err(noncanonical(
            "import contract package graph contains an unreachable package",
        ));
    }

    let mut indegree = reachable
        .iter()
        .map(|package| (package.clone(), 0_usize))
        .collect::<BTreeMap<_, _>>();
    for (importer, targets) in &graph {
        if reachable.contains(importer) {
            for target in targets {
                let value = indegree
                    .get_mut(target)
                    .ok_or_else(|| noncanonical("import graph target is invalid"))?;
                *value = value
                    .checked_add(1)
                    .ok_or_else(|| noncanonical("import graph is too large"))?;
            }
        }
    }
    let mut ready = indegree
        .iter()
        .filter_map(|(package, degree)| (*degree == 0).then_some(package.clone()))
        .collect::<Vec<_>>();
    let mut visited = 0_usize;
    while let Some(package) = ready.pop() {
        visited += 1;
        if let Some(targets) = graph.get(&package) {
            for target in targets {
                let degree = indegree
                    .get_mut(target)
                    .ok_or_else(|| noncanonical("import graph target is invalid"))?;
                *degree -= 1;
                if *degree == 0 {
                    ready.push(target.clone());
                }
            }
        }
    }
    if visited != reachable.len() {
        return Err(noncanonical(
            "import contract package graph contains a cycle",
        ));
    }

    let allowed_symbol_prefixes = reachable
        .iter()
        .map(|identity| package_symbol_prefix(identity))
        .collect::<Result<Vec<_>, _>>()?;
    let root_symbol_prefix = package_symbol_prefix(&root)?;
    for entry in &artifact.entries {
        let function = artifact
            .module
            .functions
            .get(entry.function as usize)
            .ok_or_else(|| noncanonical("entry contract function is out of range"))?;
        if !function.name.starts_with(&root_symbol_prefix) {
            return Err(noncanonical(
                "entry contract function does not belong to the manifest package",
            ));
        }
    }
    if artifact.module.functions.iter().any(|function| {
        !allowed_symbol_prefixes
            .iter()
            .any(|prefix| function.name.starts_with(prefix))
    }) {
        return Err(noncanonical(
            "executable package symbol is not reachable from the manifest package",
        ));
    }
    for enum_type in &artifact.module.enum_types {
        if artifact_uses_agent_transcript(artifact)
            && enum_type == &crate::transcript_part_enum_type()
        {
            continue;
        }
        let Some((module, type_name)) = enum_type.name.rsplit_once("::") else {
            return Err(noncanonical(
                "nominal enum identity does not name a package module",
            ));
        };
        if !is_type_identifier(type_name) {
            return Err(noncanonical(
                "nominal enum type name is not a canonical source identifier",
            ));
        }
        let Some(rest) = module.strip_prefix("pkg://") else {
            return Err(noncanonical(
                "nominal enum identity does not name a package module",
            ));
        };
        let Some((identity, path)) = rest.split_once('/') else {
            return Err(noncanonical("nominal enum package identity is malformed"));
        };
        if !reachable.contains(identity) || !is_package_module_path(path) {
            return Err(noncanonical(
                "nominal enum package is not reachable from the manifest package",
            ));
        }
    }
    if artifact.schemas.iter().enumerate().any(|(index, schema)| {
        artifact.schemas[..index]
            .iter()
            .any(|previous| previous == schema)
    }) {
        return Err(noncanonical("schema contracts must be unique"));
    }
    if let Some(debug) = &artifact.debug {
        for source in &debug.sources {
            let Some(rest) = source.strip_prefix("pkg://") else {
                return Err(noncanonical("debug source must use a package identity"));
            };
            let Some((identity, path)) = rest.split_once('/') else {
                return Err(noncanonical("debug source is malformed"));
            };
            if !reachable.contains(identity) || !is_package_module_path(path) {
                return Err(noncanonical(
                    "debug source package is not reachable from the manifest package",
                ));
            }
        }
    }
    Ok(())
}

fn package_symbol_prefix(identity: &str) -> Result<String, ArtifactError> {
    let (name, version) = identity
        .rsplit_once('@')
        .filter(|(name, version)| is_package_name(name) && is_canonical_version(version))
        .ok_or_else(|| noncanonical("package identity is invalid"))?;
    Ok(format!(
        "pkg/{}/{}/",
        escape_symbol_component(name),
        escape_symbol_component(version)
    ))
}

fn escape_symbol_component(component: &str) -> String {
    let mut output = String::with_capacity(1 + component.len() * 2);
    output.push('x');
    for byte in component.bytes() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing into String cannot fail");
    }
    output
}

fn is_type_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|first| {
        (first.is_ascii_alphabetic() || first == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

fn instruction_operand_count(instruction: &Instruction) -> Result<usize, ArtifactError> {
    let count = match instruction {
        Instruction::ListNew { elements, .. }
        | Instruction::TupleNew { elements, .. }
        | Instruction::EnumNew {
            payload: elements, ..
        }
        | Instruction::DirectCall {
            arguments: elements,
            ..
        }
        | Instruction::AsyncCall {
            arguments: elements,
            ..
        }
        | Instruction::ClosureNew {
            captures: elements, ..
        }
        | Instruction::ClosureCall {
            arguments: elements,
            ..
        }
        | Instruction::StringCall {
            arguments: elements,
            ..
        }
        | Instruction::CapabilityInspect {
            arguments: elements,
            ..
        }
        | Instruction::SafeCollectionCall {
            arguments: elements,
            ..
        }
        | Instruction::CheckedIntCall {
            arguments: elements,
            ..
        } => elements.len(),
        Instruction::MapNew { entries, .. } => entries
            .len()
            .checked_mul(2)
            .ok_or_else(|| limit("operands"))?,
        Instruction::RecordNew { fields, .. } => fields
            .len()
            .checked_mul(2)
            .ok_or_else(|| limit("operands"))?,
        Instruction::SwitchEnum { arms, .. } => arms.iter().try_fold(
            arms.len().checked_mul(2).ok_or_else(|| limit("operands"))?,
            |total, arm| {
                total
                    .checked_add(arm.bindings.len())
                    .ok_or_else(|| limit("operands"))
            },
        )?,
        _ => 0,
    };
    Ok(count)
}

fn artifact_value_types(artifact: &Artifact) -> Vec<&ValueType> {
    let mut types = Vec::new();
    for enum_type in &artifact.module.enum_types {
        for variant in &enum_type.variants {
            match &variant.payload {
                EnumPayloadType::Unit => {}
                EnumPayloadType::Tuple(elements) => types.extend(elements),
                EnumPayloadType::Record(fields) => {
                    types.extend(fields.iter().map(|field| &field.value_type));
                }
            }
        }
    }
    for function in &artifact.module.functions {
        types.extend(&function.registers);
        types.push(&function.return_type);
        for instruction in &function.code {
            if let Instruction::Narrow { target, .. } = instruction {
                types.push(target);
            }
        }
    }
    types
}

fn validate_type_depth(value_type: &ValueType, maximum: usize) -> Result<(), ArtifactError> {
    let mut pending = vec![(value_type, 0_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > maximum {
            return Err(limit("type depth"));
        }
        let next = depth.checked_add(1).ok_or_else(|| limit("type depth"))?;
        match value {
            ValueType::List(value)
            | ValueType::Option(value)
            | ValueType::Future(value)
            | ValueType::Task(value) => pending.push((value, next)),
            ValueType::Map(left, right) | ValueType::Result(left, right) => {
                pending.push((left, next));
                pending.push((right, next));
            }
            ValueType::Tuple(elements) => {
                pending.extend(elements.iter().map(|element| (element, next)));
            }
            ValueType::Record(fields) => {
                pending.extend(fields.iter().map(|field| (&field.value_type, next)));
            }
            ValueType::Function {
                parameters,
                return_type,
                ..
            } => {
                pending.extend(parameters.iter().map(|parameter| (parameter, next)));
                pending.push((return_type, next));
            }
            _ => {}
        }
    }
    Ok(())
}

fn encode_sections(
    artifact: &Artifact,
    strings: &[String],
    string_ids: &BTreeMap<&str, u32>,
    types: &[ValueType],
    type_ids: &BTreeMap<Vec<u8>, u32>,
) -> Result<Vec<Section>, ArtifactError> {
    let mut sections = vec![
        Section {
            id: SectionId::Strings,
            payload: encode_strings(strings)?,
        },
        Section {
            id: SectionId::Constants,
            payload: encode_constants(&artifact.module.constants, string_ids)?,
        },
        Section {
            id: SectionId::Types,
            payload: encode_type_section(types, &artifact.module.enum_types, string_ids, type_ids)?,
        },
        Section {
            id: SectionId::Functions,
            payload: encode_functions(
                &artifact.module.functions,
                &artifact.module.async_functions,
                string_ids,
                type_ids,
            )?,
        },
        Section {
            id: SectionId::Effects,
            payload: encode_effects(&artifact.module.effect_sets, string_ids)?,
        },
        Section {
            id: SectionId::Entries,
            payload: encode_entries(artifact, string_ids, type_ids)?,
        },
    ];
    sections.extend([
        Section {
            id: SectionId::Schemas,
            payload: encode_schemas(&artifact.schemas, string_ids, type_ids)?,
        },
        Section {
            id: SectionId::Imports,
            payload: encode_imports(&artifact.imports, string_ids)?,
        },
        Section {
            id: SectionId::ManifestContracts,
            payload: encode_manifest(
                artifact
                    .manifest
                    .as_ref()
                    .ok_or_else(|| noncanonical("artifacts require a manifest contract"))?,
                string_ids,
            )?,
        },
    ]);
    if let Some(debug) = &artifact.debug {
        sections.push(Section {
            id: SectionId::Debug,
            payload: encode_debug(debug, string_ids)?,
        });
    }
    Ok(sections)
}

/// Decode and structurally validate a canonical supported artifact.
///
/// # Errors
///
/// Returns a stable artifact error for malformed or over-limit input.
#[allow(clippy::too_many_lines)]
pub fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<DecodedArtifact, ArtifactError> {
    if bytes.len() > limits.artifact_bytes {
        return Err(ArtifactError::new(
            ArtifactErrorCode::ArtifactTooLarge,
            "artifact bytes exceed the decode limit",
        ));
    }
    if bytes.len() < HEADER_SIZE {
        return Err(truncated("artifact header"));
    }
    if bytes[..8] != ARTIFACT_MAGIC {
        return Err(ArtifactError::new(
            ArtifactErrorCode::InvalidMagic,
            "magic does not identify an ALLEN bytecode artifact",
        ));
    }
    let mut header = Reader::new(&bytes[8..HEADER_SIZE], limits);
    if header.u16()? as usize != HEADER_SIZE {
        return Err(ArtifactError::new(
            ArtifactErrorCode::InvalidHeader,
            "header size is not canonical",
        ));
    }
    let version = header.u16()?;
    if version != BYTECODE_VERSION {
        return Err(ArtifactError::new(
            ArtifactErrorCode::UnsupportedVersion,
            format!("bytecode version {version} is not supported"),
        ));
    }
    let language_version = header.version()?;
    if language_version != ArtifactMetadata::default().language_version {
        return Err(ArtifactError::new(
            ArtifactErrorCode::UnsupportedVersion,
            format!("language version {language_version} is not supported"),
        ));
    }
    let compiler_version = header.version()?;
    let target_profile = match header.u16()? {
        1 => TargetProfile::Portable,
        profile => {
            return Err(ArtifactError::new(
                ArtifactErrorCode::UnsupportedProfile,
                format!("target profile {profile} is not supported"),
            ));
        }
    };
    let section_count = header.u16()? as usize;
    if !(MANDATORY_SECTION_COUNT..=MAX_SECTION_COUNT).contains(&section_count) {
        return Err(ArtifactError::new(
            ArtifactErrorCode::MissingSection,
            "artifact must contain nine mandatory sections and optional debug",
        ));
    }
    if header.u32()? != 0 {
        return Err(ArtifactError::new(
            ArtifactErrorCode::InvalidHeader,
            "reserved header field must be zero",
        ));
    }
    let mut expected_digest = [0_u8; 32];
    expected_digest.copy_from_slice(header.take(32)?);
    header.finish()?;

    let body = &bytes[HEADER_SIZE..];
    let sections = split_sections(body, section_count, limits)?;
    let actual_digest = sha256(body);
    if actual_digest != expected_digest {
        return Err(ArtifactError::new(
            ArtifactErrorCode::DigestMismatch,
            "content digest does not match section bytes",
        ));
    }

    let model_budget = DecodeBudget::new(limits.decoded_model_bytes, limits.expanded_type_nodes);
    let strings = decode_strings(sections[0].1, limits, &model_budget)?;
    let constants = decode_constants(sections[1].1, &strings, limits, &model_budget)?;
    let (types, enum_types) = decode_type_section(sections[2].1, &strings, limits, &model_budget)?;
    let (functions, async_functions) =
        decode_functions(sections[3].1, &strings, &types, limits, &model_budget)?;
    let effect_sets = decode_effects(sections[4].1, &strings, limits, &model_budget)?;
    validate_canonical_effect_sets(&effect_sets)?;
    let manifest = decode_manifest(sections[8].1, &strings, limits, &model_budget)?;
    let (entry, entries) = decode_entries(sections[5].1, &strings, limits, &model_budget)?;
    let schemas = decode_schemas(sections[6].1, &types, limits, &model_budget)?;
    let imports = decode_imports(sections[7].1, &strings, limits, &model_budget)?;
    let debug = if section_count == MAX_SECTION_COUNT {
        Some(decode_debug(
            sections[9].1,
            &strings,
            limits,
            &model_budget,
        )?)
    } else {
        None
    };
    let artifact = Artifact {
        metadata: ArtifactMetadata {
            bytecode_version: version,
            language_version,
            compiler_version,
            target_profile,
        },
        module: Module {
            constants,
            enum_types,
            effect_sets,
            functions,
            async_functions,
            entry,
        },
        debug,
        schemas,
        entries,
        imports,
        manifest: Some(manifest),
    };
    validate_string_table(&artifact, &strings)?;
    validate_debug_shape(artifact.debug.as_ref())?;

    let decoded = DecodedArtifact {
        artifact,
        content_digest: actual_digest,
    };
    if decoded.canonical_bytes_with_limits(limits)?.as_slice() != bytes {
        return Err(ArtifactError::new(
            ArtifactErrorCode::NonCanonical,
            "artifact has a non-canonical representation",
        ));
    }
    Ok(decoded)
}

fn validate_canonical_effect_sets(effect_sets: &[Vec<String>]) -> Result<(), ArtifactError> {
    if !effect_sets
        .windows(2)
        .all(|pair| pair[0].as_slice() < pair[1].as_slice())
    {
        return Err(noncanonical(
            "effect-set table must be unique and sorted lexicographically",
        ));
    }
    for effect_set in effect_sets {
        if !effect_set
            .windows(2)
            .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
        {
            return Err(noncanonical(
                "effects must be unique and sorted by UTF-8 bytes",
            ));
        }
    }
    Ok(())
}

/// Decode, structurally validate, and independently verify an artifact.
///
/// # Errors
///
/// Returns a stable artifact error when decoding or bytecode verification fails.
pub fn decode_and_verify(
    bytes: &[u8],
    limits: &DecodeLimits,
) -> Result<VerifiedArtifact, ArtifactError> {
    let decoded = decode(bytes, limits)?;
    validate_debug_references(decoded.module(), decoded.debug())?;
    validate_verifier_complexity(decoded.module(), limits)?;
    let DecodedArtifact {
        artifact,
        content_digest,
    } = decoded;
    let Artifact {
        metadata,
        module: unverified_module,
        debug,
        schemas,
        entries,
        imports,
        manifest,
    } = artifact;
    let tool_contracts = manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .required_tools
                .iter()
                .map(|contract| {
                    let schema = |index: u32| {
                        schemas
                            .get(index as usize)
                            .map(|schema| schema.value_type.clone())
                            .ok_or_else(|| noncanonical("typed tool schema is out of range"))
                    };
                    Ok(ToolVerificationContract {
                        tool_name: contract.name.clone(),
                        input: schema(contract.input_schema)?,
                        output: schema(contract.output_schema)?,
                        declared_error: schema(contract.error_schema)?,
                    })
                })
                .collect::<Result<Vec<_>, ArtifactError>>()
        })
        .transpose()?
        .unwrap_or_default();
    let tool_contracts = manifest.as_ref().map(|_| tool_contracts.as_slice());
    let module = verify_internal(unverified_module, tool_contracts).map_err(|error| {
        ArtifactError::new(ArtifactErrorCode::VerificationFailed, error.to_string())
    })?;
    Ok(VerifiedArtifact {
        metadata,
        module,
        debug,
        schemas,
        entries,
        imports,
        manifest,
        content_digest,
    })
}

fn validate_verifier_complexity(
    module: &Module,
    limits: &DecodeLimits,
) -> Result<(), ArtifactError> {
    const OWNERSHIP_REGISTER_BYTES: usize = 16;
    const OWNERSHIP_STATE_OVERHEAD_BYTES: usize = 64;
    const SUB_AGENT_REGISTER_BYTES: usize = 16;
    const SUB_AGENT_SCOPE_BYTES: usize = 8;
    const SUB_AGENT_STATE_OVERHEAD_BYTES: usize = 64;
    const DOMINATOR_NODE_BYTES: usize = 192;
    const DOMINATOR_EDGE_BYTES: usize = 24;
    let mut total = 0_usize;
    for function in &module.functions {
        let initialization_bytes = function.registers.len().checked_add(7).map(|bits| bits / 8);
        let scope_count = function
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::TaskScopeEnter { .. }))
            .count();
        let ownership_bytes = function
            .registers
            .len()
            .checked_mul(OWNERSHIP_REGISTER_BYTES)
            .and_then(|bytes| {
                scope_count
                    .checked_mul(std::mem::size_of::<u32>())
                    .and_then(|scopes| bytes.checked_add(scopes))
            })
            .and_then(|bytes| bytes.checked_add(OWNERSHIP_STATE_OVERHEAD_BYTES));
        let mut state_bytes = initialization_bytes
            .and_then(|bytes| bytes.checked_add(ownership_bytes?))
            .and_then(|bytes| bytes.checked_mul(function.code.len()))
            .and_then(|bytes| {
                function
                    .code
                    .len()
                    .checked_mul(std::mem::size_of::<usize>())
                    .and_then(|worklist| bytes.checked_add(worklist))
            })
            .ok_or_else(|| limit("verifier state bytes"))?;
        let sub_agent_bytes = if function
            .registers
            .iter()
            .any(crate::contains_stored_sub_agent)
        {
            function
                .registers
                .len()
                .checked_mul(SUB_AGENT_REGISTER_BYTES)
                .and_then(|bytes| {
                    scope_count
                        .checked_mul(SUB_AGENT_SCOPE_BYTES)
                        .and_then(|scopes| bytes.checked_add(scopes))
                })
                .and_then(|bytes| bytes.checked_add(SUB_AGENT_STATE_OVERHEAD_BYTES))
                .and_then(|bytes| {
                    function
                        .code
                        .len()
                        .checked_add(2)
                        .and_then(|states| bytes.checked_mul(states))
                })
                .ok_or_else(|| limit("verifier state bytes"))?
        } else {
            0
        };
        state_bytes = state_bytes
            .checked_add(sub_agent_bytes)
            .ok_or_else(|| limit("verifier state bytes"))?;
        let edge_count = function
            .code
            .iter()
            .try_fold(0_usize, |total, instruction| {
                let successors = match instruction {
                    Instruction::Return { .. } | Instruction::Stop { .. } => 0,
                    Instruction::BranchBool { .. } => 2,
                    Instruction::SwitchEnum { arms, .. } => arms.len(),
                    _ => 1,
                };
                total.checked_add(successors)
            });
        let dominator_bytes = function
            .code
            .len()
            .checked_mul(DOMINATOR_NODE_BYTES)
            .and_then(|bytes| {
                edge_count?
                    .checked_mul(DOMINATOR_EDGE_BYTES)
                    .and_then(|edges| bytes.checked_add(edges))
            })
            .ok_or_else(|| limit("verifier state bytes"))?;
        state_bytes = state_bytes
            .checked_add(dominator_bytes)
            .ok_or_else(|| limit("verifier state bytes"))?;
        total = total
            .checked_add(state_bytes)
            .ok_or_else(|| limit("verifier state bytes"))?;
        if total > limits.verifier_state_bytes {
            return Err(limit("verifier state bytes"));
        }
    }
    Ok(())
}

fn split_sections<'a>(
    body: &'a [u8],
    section_count: usize,
    limits: &DecodeLimits,
) -> Result<Vec<(SectionId, &'a [u8])>, ArtifactError> {
    let mut reader = Reader::new(body, limits);
    let mut sections = Vec::with_capacity(section_count);
    let mut previous = 0_u16;
    for index in 0..section_count {
        let raw_id = reader.u16()?;
        let id = SectionId::from_raw(raw_id).ok_or_else(|| {
            ArtifactError::new(
                ArtifactErrorCode::UnknownSection,
                format!("section ID {raw_id} is unknown"),
            )
        })?;
        if raw_id == previous {
            return Err(ArtifactError::new(
                ArtifactErrorCode::DuplicateSection,
                format!("section ID {raw_id} appears more than once"),
            ));
        }
        if raw_id < previous {
            return Err(ArtifactError::new(
                ArtifactErrorCode::SectionOrder,
                "sections are not in canonical ID order",
            ));
        }
        let expected = if index < MANDATORY_SECTION_COUNT {
            SectionId::MANDATORY[index]
        } else {
            SectionId::Debug
        };
        if id != expected {
            return Err(ArtifactError::new(
                ArtifactErrorCode::MissingSection,
                format!("mandatory section ID {} is missing", expected as u16),
            ));
        }
        previous = raw_id;
        let byte_len = usize_from_u64(reader.u64()?, "section byte length")?;
        if byte_len > limits.section_bytes {
            return Err(ArtifactError::new(
                ArtifactErrorCode::SectionTooLarge,
                "section bytes exceed the decode limit",
            ));
        }
        let payload = reader.take(byte_len)?;
        sections.push((id, payload));
    }
    reader.finish()?;
    Ok(sections)
}

fn collect_strings(artifact: &Artifact) -> Vec<String> {
    let mut strings = BTreeSet::new();
    for constant in &artifact.module.constants {
        if let Constant::String(value) = constant {
            strings.insert(value.clone());
        }
    }
    for enum_type in &artifact.module.enum_types {
        strings.insert(enum_type.name.clone());
        for variant in &enum_type.variants {
            strings.insert(variant.name.clone());
            collect_payload_strings(&variant.payload, &mut strings);
        }
    }
    for effect_set in &artifact.module.effect_sets {
        strings.extend(effect_set.iter().cloned());
    }
    for function in &artifact.module.functions {
        strings.insert(function.name.clone());
        for value_type in function.registers.iter().chain([&function.return_type]) {
            collect_type_strings(value_type, &mut strings);
        }
        for instruction in &function.code {
            if let Instruction::Narrow { target, .. } = instruction {
                collect_type_strings(target, &mut strings);
            }
        }
    }
    if let Some(debug) = &artifact.debug {
        strings.extend(debug.sources.iter().cloned());
    }
    for schema in &artifact.schemas {
        collect_type_strings(&schema.value_type, &mut strings);
    }
    for entry in &artifact.entries {
        strings.insert(entry.name.clone());
    }
    for import in &artifact.imports {
        strings.extend([
            import.importer.clone(),
            import.alias.clone(),
            import.package.clone(),
            import.version.clone(),
            import.module.clone(),
        ]);
    }
    if let Some(manifest) = &artifact.manifest {
        strings.extend([
            manifest.package.clone(),
            manifest.version.clone(),
            manifest.language_requirement.clone(),
        ]);
        strings.extend(manifest.required_capabilities.iter().cloned());
        strings.extend(manifest.optional_capabilities.iter().cloned());
        strings.extend(manifest.limits.iter().map(|(name, _)| name.clone()));
        strings.extend(manifest.https_origins.iter().cloned());
        for tool in &manifest.required_tools {
            strings.extend([
                tool.name.clone(),
                tool.version.clone(),
                tool.version_requirement.clone(),
                tool.effect.clone(),
            ]);
        }
    }
    strings.into_iter().collect()
}

fn collect_types(artifact: &Artifact) -> Vec<ValueType> {
    let mut types = Vec::new();
    for enum_type in &artifact.module.enum_types {
        for variant in &enum_type.variants {
            match &variant.payload {
                EnumPayloadType::Unit => {}
                EnumPayloadType::Tuple(elements) => types.extend(elements.iter().cloned()),
                EnumPayloadType::Record(fields) => {
                    types.extend(fields.iter().map(|field| field.value_type.clone()));
                }
            }
        }
    }
    for function in &artifact.module.functions {
        types.extend(function.registers.iter().cloned());
        types.push(function.return_type.clone());
        for instruction in &function.code {
            if let Instruction::Narrow { target, .. } = instruction {
                types.push(target.clone());
            }
        }
    }
    types.extend(
        artifact
            .schemas
            .iter()
            .map(|schema| schema.value_type.clone()),
    );
    types
}

fn type_key(
    value_type: &ValueType,
    strings: &BTreeMap<&str, u32>,
) -> Result<Vec<u8>, ArtifactError> {
    let mut key = Vec::new();
    encode_type(&mut key, value_type, strings)?;
    Ok(key)
}

fn type_node_count(value_type: &ValueType) -> Result<usize, ArtifactError> {
    let children = match value_type {
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value) => type_node_count(value)?,
        ValueType::Map(left, right) | ValueType::Result(left, right) => type_node_count(left)?
            .checked_add(type_node_count(right)?)
            .ok_or_else(|| limit("expanded type nodes"))?,
        ValueType::Tuple(elements) => elements.iter().try_fold(0_usize, |total, element| {
            total
                .checked_add(type_node_count(element)?)
                .ok_or_else(|| limit("expanded type nodes"))
        })?,
        ValueType::Record(fields) => fields.iter().try_fold(0_usize, |total, field| {
            total
                .checked_add(type_node_count(&field.value_type)?)
                .ok_or_else(|| limit("expanded type nodes"))
        })?,
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => parameters
            .iter()
            .try_fold(type_node_count(return_type)?, |total, parameter| {
                total
                    .checked_add(type_node_count(parameter)?)
                    .ok_or_else(|| limit("expanded type nodes"))
            })?,
        _ => 0,
    };
    children
        .checked_add(1)
        .ok_or_else(|| limit("expanded type nodes"))
}

fn type_owned_bytes(value_type: &ValueType) -> Result<usize, ArtifactError> {
    let nested = match value_type {
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value) => type_owned_bytes(value)?,
        ValueType::Map(left, right) | ValueType::Result(left, right) => type_owned_bytes(left)?
            .checked_add(type_owned_bytes(right)?)
            .ok_or_else(|| limit("decoded model bytes"))?,
        ValueType::Tuple(elements) => elements.iter().try_fold(0_usize, |total, element| {
            total
                .checked_add(type_owned_bytes(element)?)
                .ok_or_else(|| limit("decoded model bytes"))
        })?,
        ValueType::Record(fields) => fields.iter().try_fold(0_usize, |total, field| {
            let value_bytes = type_owned_bytes(&field.value_type)?;
            let field_bytes = std::mem::size_of::<RecordField>()
                .checked_add(field.name.len())
                .and_then(|bytes| bytes.checked_add(value_bytes))
                .ok_or_else(|| limit("decoded model bytes"))?;
            total
                .checked_add(field_bytes)
                .ok_or_else(|| limit("decoded model bytes"))
        })?,
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => parameters
            .iter()
            .try_fold(type_owned_bytes(return_type)?, |total, parameter| {
                total
                    .checked_add(type_owned_bytes(parameter)?)
                    .ok_or_else(|| limit("decoded model bytes"))
            })?,
        _ => 0,
    };
    std::mem::size_of::<ValueType>()
        .checked_add(nested)
        .ok_or_else(|| limit("decoded model bytes"))
}

fn type_id(
    value_type: &ValueType,
    strings: &BTreeMap<&str, u32>,
    types: &BTreeMap<Vec<u8>, u32>,
) -> Result<u32, ArtifactError> {
    types
        .get(&type_key(value_type, strings)?)
        .copied()
        .ok_or_else(|| invalid_scalar("type descriptor reference"))
}

fn collect_payload_strings(payload: &EnumPayloadType, strings: &mut BTreeSet<String>) {
    match payload {
        EnumPayloadType::Unit => {}
        EnumPayloadType::Tuple(elements) => {
            for element in elements {
                collect_type_strings(element, strings);
            }
        }
        EnumPayloadType::Record(fields) => {
            for field in fields {
                strings.insert(field.name.clone());
                collect_type_strings(&field.value_type, strings);
            }
        }
    }
}

fn collect_type_strings(value_type: &ValueType, strings: &mut BTreeSet<String>) {
    match value_type {
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value) => collect_type_strings(value, strings),
        ValueType::Map(left, right) | ValueType::Result(left, right) => {
            collect_type_strings(left, strings);
            collect_type_strings(right, strings);
        }
        ValueType::Tuple(elements) => {
            for element in elements {
                collect_type_strings(element, strings);
            }
        }
        ValueType::Record(fields) => {
            for field in fields {
                strings.insert(field.name.clone());
                collect_type_strings(&field.value_type, strings);
            }
        }
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => {
            for parameter in parameters {
                collect_type_strings(parameter, strings);
            }
            collect_type_strings(return_type, strings);
        }
        ValueType::Int
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::Unit
        | ValueType::Never
        | ValueType::Enum(_)
        | ValueType::Workspace
        | ValueType::ExternalFsAccess
        | ValueType::SubAgent
        | ValueType::Unknown => {}
    }
}

fn encode_strings(strings: &[String]) -> Result<Vec<u8>, ArtifactError> {
    let mut output = Vec::new();
    put_count(&mut output, strings.len(), "string table")?;
    for value in strings {
        put_bytes(&mut output, value.as_bytes(), "string")?;
    }
    Ok(output)
}

fn encode_constants(
    constants: &[Constant],
    strings: &BTreeMap<&str, u32>,
) -> Result<Vec<u8>, ArtifactError> {
    let mut output = Vec::new();
    put_count(&mut output, constants.len(), "constant table")?;
    for constant in constants {
        match constant {
            Constant::Int(value) => {
                output.push(0);
                output.extend_from_slice(&value.to_le_bytes());
            }
            Constant::Bool(value) => {
                output.push(1);
                output.push(u8::from(*value));
            }
            Constant::Float(bits) => {
                if *bits != canonical_float_bits(*bits) {
                    return Err(noncanonical("float constant is not the canonical NaN"));
                }
                output.push(2);
                put_u64(&mut output, *bits);
            }
            Constant::String(value) => {
                output.push(3);
                put_u32(&mut output, string_id(strings, value)?);
            }
            Constant::Bytes(value) => {
                output.push(4);
                put_bytes(&mut output, value, "byte constant")?;
            }
            Constant::Unit => output.push(5),
            Constant::ExternalFsAccess(access) => {
                output.push(6);
                output.push(match access {
                    crate::ExternalFsAccess::Read => 0,
                    crate::ExternalFsAccess::Write => 1,
                    crate::ExternalFsAccess::ReadWrite => 2,
                });
            }
        }
    }
    Ok(output)
}

fn encode_type_section(
    types: &[ValueType],
    enum_types: &[EnumType],
    strings: &BTreeMap<&str, u32>,
    type_ids: &BTreeMap<Vec<u8>, u32>,
) -> Result<Vec<u8>, ArtifactError> {
    let mut output = Vec::new();
    encode_types(&mut output, types, strings)?;
    put_count(&mut output, enum_types.len(), "enum type table")?;
    for enum_type in enum_types {
        put_u32(&mut output, string_id(strings, &enum_type.name)?);
        put_count(&mut output, enum_type.variants.len(), "enum variant table")?;
        for variant in &enum_type.variants {
            put_u32(&mut output, string_id(strings, &variant.name)?);
            encode_payload(&mut output, &variant.payload, strings, type_ids)?;
        }
    }
    Ok(output)
}

fn encode_payload(
    output: &mut Vec<u8>,
    payload: &EnumPayloadType,
    strings: &BTreeMap<&str, u32>,
    type_ids: &BTreeMap<Vec<u8>, u32>,
) -> Result<(), ArtifactError> {
    match payload {
        EnumPayloadType::Unit => output.push(0),
        EnumPayloadType::Tuple(elements) => {
            output.push(1);
            put_count(output, elements.len(), "enum payload type references")?;
            for element in elements {
                put_u32(output, type_id(element, strings, type_ids)?);
            }
        }
        EnumPayloadType::Record(fields) => {
            output.push(2);
            put_count(output, fields.len(), "enum payload field table")?;
            for field in fields {
                put_u32(output, string_id(strings, &field.name)?);
                put_u32(output, type_id(&field.value_type, strings, type_ids)?);
            }
        }
    }
    Ok(())
}

fn encode_functions(
    functions: &[Function],
    async_functions: &[FunctionId],
    strings: &BTreeMap<&str, u32>,
    type_ids: &BTreeMap<Vec<u8>, u32>,
) -> Result<Vec<u8>, ArtifactError> {
    let mut output = Vec::new();
    put_count(&mut output, functions.len(), "function table")?;
    put_count(&mut output, async_functions.len(), "async function table")?;
    for function in async_functions {
        put_u32(&mut output, *function);
    }
    for function in functions {
        put_u32(&mut output, string_id(strings, &function.name)?);
        encode_registers(&mut output, &function.parameters)?;
        encode_registers(&mut output, &function.captures)?;
        put_count(
            &mut output,
            function.registers.len(),
            "register type references",
        )?;
        for register in &function.registers {
            put_u32(&mut output, type_id(register, strings, type_ids)?);
        }
        put_u32(
            &mut output,
            type_id(&function.return_type, strings, type_ids)?,
        );
        put_u32(&mut output, function.effects);
        put_count(&mut output, function.code.len(), "instruction table")?;
        for instruction in &function.code {
            encode_instruction(&mut output, instruction, strings, type_ids)?;
        }
    }
    Ok(output)
}

fn encode_effects(
    effect_sets: &[Vec<String>],
    strings: &BTreeMap<&str, u32>,
) -> Result<Vec<u8>, ArtifactError> {
    let mut output = Vec::new();
    put_count(&mut output, effect_sets.len(), "effect set table")?;
    for effect_set in effect_sets {
        put_count(&mut output, effect_set.len(), "effect table")?;
        for effect in effect_set {
            put_u32(&mut output, string_id(strings, effect)?);
        }
    }
    Ok(output)
}

fn encode_entries(
    artifact: &Artifact,
    strings: &BTreeMap<&str, u32>,
    _types: &BTreeMap<Vec<u8>, u32>,
) -> Result<Vec<u8>, ArtifactError> {
    let mut output = Vec::new();
    put_u32(
        &mut output,
        to_u32(artifact.entries.len(), "entry contracts")?,
    );
    for entry in &artifact.entries {
        put_u32(&mut output, string_id(strings, &entry.name)?);
        put_u32(&mut output, entry.function);
        put_u32(&mut output, entry.input_schema);
        put_u32(&mut output, entry.output_schema);
    }
    Ok(output)
}

fn encode_schemas(
    values: &[StrictSchema],
    strings: &BTreeMap<&str, u32>,
    types: &BTreeMap<Vec<u8>, u32>,
) -> Result<Vec<u8>, ArtifactError> {
    let mut o = Vec::new();
    put_u32(&mut o, to_u32(values.len(), "schemas")?);
    for value in values {
        put_u32(&mut o, type_id(&value.value_type, strings, types)?);
    }
    Ok(o)
}
fn encode_imports(
    values: &[ImportContract],
    strings: &BTreeMap<&str, u32>,
) -> Result<Vec<u8>, ArtifactError> {
    let mut o = Vec::new();
    put_u32(&mut o, to_u32(values.len(), "imports")?);
    for v in values {
        for s in [&v.importer, &v.alias, &v.package, &v.version, &v.module] {
            put_u32(&mut o, string_id(strings, s)?);
        }
        o.extend_from_slice(&v.content_digest);
    }
    Ok(o)
}
fn encode_manifest(
    value: &ManifestContract,
    strings: &BTreeMap<&str, u32>,
) -> Result<Vec<u8>, ArtifactError> {
    let mut o = Vec::new();
    put_u32(&mut o, 1);
    for s in [&value.package, &value.version, &value.language_requirement] {
        put_u32(&mut o, string_id(strings, s)?);
    }
    for list in [&value.required_capabilities, &value.optional_capabilities] {
        put_u32(&mut o, to_u32(list.len(), "capabilities")?);
        for s in list {
            put_u32(&mut o, string_id(strings, s)?);
        }
    }
    put_u32(&mut o, to_u32(value.limits.len(), "limits")?);
    for (key, limit) in &value.limits {
        put_u32(&mut o, string_id(strings, key)?);
        put_u64(&mut o, *limit);
    }
    put_u32(&mut o, to_u32(value.https_origins.len(), "HTTPS origins")?);
    for origin in &value.https_origins {
        put_u32(&mut o, string_id(strings, origin)?);
    }
    put_u32(
        &mut o,
        to_u32(value.required_tools.len(), "required tools")?,
    );
    for tool in &value.required_tools {
        for value in [
            &tool.name,
            &tool.version,
            &tool.version_requirement,
            &tool.effect,
        ] {
            put_u32(&mut o, string_id(strings, value)?);
        }
        for schema in [tool.input_schema, tool.output_schema, tool.error_schema] {
            put_u32(&mut o, schema);
        }
        o.extend_from_slice(&tool.input_digest);
        o.extend_from_slice(&tool.output_digest);
        o.extend_from_slice(&tool.error_digest);
    }
    o.extend_from_slice(&value.tool_contract_digest);
    Ok(o)
}

fn encode_debug(
    debug: &DebugInfo,
    strings: &BTreeMap<&str, u32>,
) -> Result<Vec<u8>, ArtifactError> {
    let mut output = Vec::new();
    put_count(&mut output, debug.sources.len(), "debug source table")?;
    for source in &debug.sources {
        put_u32(&mut output, string_id(strings, source)?);
    }
    put_count(&mut output, debug.locations.len(), "debug location table")?;
    for location in &debug.locations {
        put_u32(&mut output, location.function);
        put_u32(&mut output, location.instruction);
        put_u32(&mut output, location.source);
        put_u32(&mut output, location.start);
        put_u32(&mut output, location.end);
    }
    Ok(output)
}

fn encode_types(
    output: &mut Vec<u8>,
    types: &[ValueType],
    strings: &BTreeMap<&str, u32>,
) -> Result<(), ArtifactError> {
    put_count(output, types.len(), "type table")?;
    for value_type in types {
        encode_type(output, value_type, strings)?;
    }
    Ok(())
}

fn encode_fields(
    output: &mut Vec<u8>,
    fields: &[RecordField],
    strings: &BTreeMap<&str, u32>,
) -> Result<(), ArtifactError> {
    put_count(output, fields.len(), "record field table")?;
    for field in fields {
        put_u32(output, string_id(strings, &field.name)?);
        encode_type(output, &field.value_type, strings)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn encode_type(
    output: &mut Vec<u8>,
    value_type: &ValueType,
    strings: &BTreeMap<&str, u32>,
) -> Result<(), ArtifactError> {
    match value_type {
        ValueType::Int => output.push(0),
        ValueType::Bool => output.push(1),
        ValueType::Float => output.push(2),
        ValueType::String => output.push(3),
        ValueType::Bytes => output.push(4),
        ValueType::Unit => output.push(5),
        ValueType::Never => output.push(6),
        ValueType::List(element) => {
            output.push(7);
            encode_type(output, element, strings)?;
        }
        ValueType::Map(key, value) => {
            output.push(8);
            encode_type(output, key, strings)?;
            encode_type(output, value, strings)?;
        }
        ValueType::Tuple(elements) => {
            output.push(9);
            encode_types(output, elements, strings)?;
        }
        ValueType::Record(fields) => {
            output.push(10);
            encode_fields(output, fields, strings)?;
        }
        ValueType::Enum(id) => {
            output.push(11);
            put_u32(output, *id);
        }
        ValueType::Option(value) => {
            output.push(12);
            encode_type(output, value, strings)?;
        }
        ValueType::Result(ok, error) => {
            output.push(13);
            encode_type(output, ok, strings)?;
            encode_type(output, error, strings)?;
        }
        ValueType::Function {
            parameters,
            return_type,
            effects,
        } => {
            output.push(14);
            encode_types(output, parameters, strings)?;
            encode_type(output, return_type, strings)?;
            put_u32(output, *effects);
        }
        ValueType::Unknown => output.push(15),
        ValueType::Future(value) => {
            output.push(16);
            encode_type(output, value, strings)?;
        }
        ValueType::Task(value) => {
            output.push(17);
            encode_type(output, value, strings)?;
        }
        ValueType::Workspace => output.push(18),
        ValueType::ExternalFsAccess => output.push(19),
        ValueType::SubAgent => output.push(20),
    }
    Ok(())
}

fn encode_registers(output: &mut Vec<u8>, registers: &[Register]) -> Result<(), ArtifactError> {
    put_count(output, registers.len(), "register operand table")?;
    for register in registers {
        put_u16(output, *register);
    }
    Ok(())
}

fn encode_pairs(output: &mut Vec<u8>, pairs: &[(Register, Register)]) -> Result<(), ArtifactError> {
    put_count(output, pairs.len(), "pair operand table")?;
    for (left, right) in pairs {
        put_u16(output, *left);
        put_u16(output, *right);
    }
    Ok(())
}

fn encode_field_operands(
    output: &mut Vec<u8>,
    fields: &[(u32, Register)],
) -> Result<(), ArtifactError> {
    put_count(output, fields.len(), "field operand table")?;
    for (field, register) in fields {
        put_u32(output, *field);
        put_u16(output, *register);
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn encode_instruction(
    output: &mut Vec<u8>,
    instruction: &Instruction,
    strings: &BTreeMap<&str, u32>,
    type_ids: &BTreeMap<Vec<u8>, u32>,
) -> Result<(), ArtifactError> {
    match instruction {
        Instruction::Const {
            destination,
            constant,
        } => {
            output.push(0);
            put_u16(output, *destination);
            put_u32(output, *constant);
        }
        Instruction::Move {
            destination,
            source,
        } => {
            output.push(1);
            put_u16(output, *destination);
            put_u16(output, *source);
        }
        Instruction::IntBinary {
            destination,
            left,
            right,
            operation,
        } => {
            output.push(2);
            put_u16(output, *destination);
            put_u16(output, *left);
            put_u16(output, *right);
            output.push(numeric_op(*operation));
        }
        Instruction::IntRemainder {
            destination,
            left,
            right,
        } => {
            output.push(42);
            put_u16(output, *destination);
            put_u16(output, *left);
            put_u16(output, *right);
        }
        Instruction::FloatBinary {
            destination,
            left,
            right,
            operation,
        } => {
            output.push(3);
            put_u16(output, *destination);
            put_u16(output, *left);
            put_u16(output, *right);
            output.push(numeric_op(*operation));
        }
        Instruction::IntNegate {
            destination,
            source,
        } => {
            output.push(4);
            put_u16(output, *destination);
            put_u16(output, *source);
        }
        Instruction::FloatNegate {
            destination,
            source,
        } => {
            output.push(5);
            put_u16(output, *destination);
            put_u16(output, *source);
        }
        Instruction::Compare {
            destination,
            left,
            right,
            operation,
        } => {
            output.push(6);
            put_u16(output, *destination);
            put_u16(output, *left);
            put_u16(output, *right);
            output.push(compare_op(*operation));
        }
        Instruction::BoolNot {
            destination,
            source,
        } => {
            output.push(7);
            put_u16(output, *destination);
            put_u16(output, *source);
        }
        Instruction::BoolBinary {
            destination,
            left,
            right,
            operation,
        } => {
            output.push(8);
            put_u16(output, *destination);
            put_u16(output, *left);
            put_u16(output, *right);
            output.push(match operation {
                BoolBinaryOp::And => 0,
                BoolBinaryOp::Or => 1,
            });
        }
        Instruction::ListNew {
            destination,
            elements,
        } => {
            output.push(9);
            put_u16(output, *destination);
            encode_registers(output, elements)?;
        }
        Instruction::MapNew {
            destination,
            entries,
        } => {
            output.push(10);
            put_u16(output, *destination);
            encode_pairs(output, entries)?;
        }
        Instruction::TupleNew {
            destination,
            elements,
        } => {
            output.push(11);
            put_u16(output, *destination);
            encode_registers(output, elements)?;
        }
        Instruction::IndexGet {
            destination,
            collection,
            index,
        } => {
            output.push(12);
            put_u16(output, *destination);
            put_u16(output, *collection);
            put_u16(output, *index);
        }
        Instruction::TupleGet {
            destination,
            tuple,
            index,
        } => {
            output.push(13);
            put_u16(output, *destination);
            put_u16(output, *tuple);
            put_u32(output, *index);
        }
        Instruction::Convert {
            destination,
            source,
            conversion,
        } => {
            output.push(14);
            put_u16(output, *destination);
            put_u16(output, *source);
            output.push(match conversion {
                Conversion::IntToFloat => 0,
                Conversion::ToString => 1,
                Conversion::StringToBytes => 2,
            });
        }
        Instruction::RecordNew {
            destination,
            fields,
        } => {
            output.push(15);
            put_u16(output, *destination);
            encode_field_operands(output, fields)?;
        }
        Instruction::FieldGet {
            destination,
            record,
            field,
        } => {
            output.push(16);
            put_u16(output, *destination);
            put_u16(output, *record);
            put_u32(output, *field);
        }
        Instruction::EnumNew {
            destination,
            variant,
            payload,
        } => {
            output.push(17);
            put_u16(output, *destination);
            put_u32(output, *variant);
            encode_registers(output, payload)?;
        }
        Instruction::BranchBool {
            condition,
            true_target,
            false_target,
        } => {
            output.push(18);
            put_u16(output, *condition);
            put_u32(output, *true_target);
            put_u32(output, *false_target);
        }
        Instruction::SwitchEnum { source, arms } => {
            output.push(19);
            put_u16(output, *source);
            put_count(output, arms.len(), "switch arm table")?;
            for arm in arms {
                put_u32(output, arm.variant);
                put_u32(output, arm.target);
                encode_registers(output, &arm.bindings)?;
            }
        }
        Instruction::Jump { target } => {
            output.push(20);
            put_u32(output, *target);
        }
        Instruction::TryResult {
            destination,
            source,
        } => {
            output.push(21);
            put_u16(output, *destination);
            put_u16(output, *source);
        }
        Instruction::ToUnknown {
            destination,
            source,
        } => {
            output.push(22);
            put_u16(output, *destination);
            put_u16(output, *source);
        }
        Instruction::Narrow {
            destination,
            source,
            target,
        } => {
            output.push(23);
            put_u16(output, *destination);
            put_u16(output, *source);
            put_u32(output, type_id(target, strings, type_ids)?);
        }
        Instruction::DirectCall {
            destination,
            function,
            arguments,
        } => {
            output.push(24);
            put_u16(output, *destination);
            put_u32(output, *function);
            encode_registers(output, arguments)?;
        }
        Instruction::ClosureNew {
            destination,
            function,
            captures,
        } => {
            output.push(25);
            put_u16(output, *destination);
            put_u32(output, *function);
            encode_registers(output, captures)?;
        }
        Instruction::ClosureCall {
            destination,
            closure,
            arguments,
        } => {
            output.push(26);
            put_u16(output, *destination);
            put_u16(output, *closure);
            encode_registers(output, arguments)?;
        }
        Instruction::Return { source } => {
            output.push(27);
            put_u16(output, *source);
        }
        Instruction::AsyncCall {
            destination,
            function,
            arguments,
        } => {
            output.push(28);
            put_u16(output, *destination);
            put_u32(output, *function);
            encode_registers(output, arguments)?;
        }
        Instruction::Spawn {
            destination,
            future,
            scope,
        } => {
            output.push(29);
            put_u16(output, *destination);
            put_u16(output, *future);
            put_u32(output, *scope);
        }
        Instruction::Await {
            destination,
            source,
        } => {
            output.push(30);
            put_u16(output, *destination);
            put_u16(output, *source);
        }
        Instruction::TaskScopeEnter { scope } => {
            output.push(31);
            put_u32(output, *scope);
        }
        Instruction::TaskScopeExit { scope } => {
            output.push(32);
            put_u32(output, *scope);
        }
        Instruction::Stop { reason } => {
            output.push(33);
            put_u16(output, *reason);
        }
        Instruction::TaskSnapshot {
            destination,
            source,
        } => {
            output.push(34);
            put_u16(output, *destination);
            put_u16(output, *source);
        }
        Instruction::WorkspaceGet { destination } => {
            output.push(35);
            put_u16(output, *destination);
        }
        Instruction::EffectCall {
            destination,
            operation,
            arguments,
        } => {
            output.push(36);
            put_u16(output, *destination);
            output.push(match operation {
                crate::EffectOperation::ReadText => 0,
                crate::EffectOperation::ReadBytes => 1,
                crate::EffectOperation::WriteText => 2,
                crate::EffectOperation::WriteBytes => 3,
                crate::EffectOperation::List => 4,
                crate::EffectOperation::HttpGet => 5,
                crate::EffectOperation::PermissionRequestFile => 6,
                crate::EffectOperation::PermissionRequestDirectory => 7,
                crate::EffectOperation::AgentMessage => 8,
                crate::EffectOperation::AgentAsk => 9,
                crate::EffectOperation::AgentTranscript => 10,
                crate::EffectOperation::ModelRequest => 11,
                crate::EffectOperation::UserAsk => 12,
                crate::EffectOperation::SubAgentCreate => 13,
                crate::EffectOperation::SubAgentRun => 14,
                crate::EffectOperation::SubAgentMessage => 15,
                crate::EffectOperation::SubAgentAsk => 16,
                crate::EffectOperation::Search => 17,
            });
            encode_registers(output, arguments)?;
        }
        Instruction::StringCall {
            destination,
            operation,
            arguments,
        } => {
            output.push(43);
            put_u16(output, *destination);
            output.push(string_operation(*operation));
            encode_registers(output, arguments)?;
        }
        Instruction::CapabilityInspect {
            destination,
            operation,
            arguments,
        } => {
            output.push(44);
            put_u16(output, *destination);
            output.push(match operation {
                CapabilityOperation::IsGranted => 0,
                CapabilityOperation::Granted => 1,
            });
            encode_registers(output, arguments)?;
        }
        Instruction::SafeCollectionCall {
            destination,
            operation,
            arguments,
        } => {
            output.push(45);
            put_u16(output, *destination);
            output.push(match operation {
                SafeCollectionOperation::ListGet => 0,
                SafeCollectionOperation::ListTrySet => 1,
                SafeCollectionOperation::BytesGet => 2,
                SafeCollectionOperation::MapGet => 3,
            });
            encode_registers(output, arguments)?;
        }
        Instruction::CheckedIntCall {
            destination,
            operation,
            arguments,
        } => {
            output.push(46);
            put_u16(output, *destination);
            output.push(match operation {
                CheckedIntOperation::Add => 0,
                CheckedIntOperation::Subtract => 1,
                CheckedIntOperation::Multiply => 2,
                CheckedIntOperation::Divide => 3,
                CheckedIntOperation::Remainder => 4,
                CheckedIntOperation::Negate => 5,
            });
            encode_registers(output, arguments)?;
        }
        Instruction::ToolInvoke {
            destination,
            tool,
            input,
        } => {
            output.push(37);
            put_u16(output, *destination);
            put_u32(output, *tool);
            put_u16(output, *input);
        }
        Instruction::Length {
            destination,
            collection,
        } => {
            output.push(38);
            put_u16(output, *destination);
            put_u16(output, *collection);
        }
        Instruction::ListAppend {
            destination,
            values,
            value,
        } => {
            output.push(39);
            put_u16(output, *destination);
            put_u16(output, *values);
            put_u16(output, *value);
        }
        Instruction::ListSet {
            destination,
            values,
            index,
            value,
        } => {
            output.push(40);
            put_u16(output, *destination);
            put_u16(output, *values);
            put_u16(output, *index);
            put_u16(output, *value);
        }
        Instruction::MapEntryAt {
            destination,
            map,
            index,
        } => {
            output.push(41);
            put_u16(output, *destination);
            put_u16(output, *map);
            put_u16(output, *index);
        }
    }
    Ok(())
}

const fn numeric_op(operation: NumericBinaryOp) -> u8 {
    match operation {
        NumericBinaryOp::Add => 0,
        NumericBinaryOp::Subtract => 1,
        NumericBinaryOp::Multiply => 2,
        NumericBinaryOp::Divide => 3,
    }
}

const fn string_operation(operation: StringOperation) -> u8 {
    match operation {
        StringOperation::ByteLength => 0,
        StringOperation::Concat => 1,
        StringOperation::Get => 2,
        StringOperation::Slice => 3,
        StringOperation::Find => 4,
        StringOperation::Contains => 5,
        StringOperation::StartsWith => 6,
        StringOperation::EndsWith => 7,
        StringOperation::Split => 8,
        StringOperation::Join => 9,
        StringOperation::TrimAscii => 10,
        StringOperation::FromUtf8 => 11,
        StringOperation::TemplateConcat => 12,
    }
}

const fn compare_op(operation: CompareOp) -> u8 {
    match operation {
        CompareOp::Equal => 0,
        CompareOp::NotEqual => 1,
        CompareOp::Less => 2,
        CompareOp::LessEqual => 3,
        CompareOp::Greater => 4,
        CompareOp::GreaterEqual => 5,
    }
}

fn decode_strings(
    payload: &[u8],
    limits: &DecodeLimits,
    budget: &DecodeBudget,
) -> Result<Vec<String>, ArtifactError> {
    let mut reader = Reader::with_budget(payload, limits, budget);
    let count = reader.count(limits.table_entries, "table entries")?;
    reader.charge_items::<String>(count)?;
    let mut strings = Vec::with_capacity(count);
    let mut previous: Option<&[u8]> = None;
    for _ in 0..count {
        let byte_len = reader.count(limits.string_bytes, "string bytes")?;
        let bytes = reader.take(byte_len)?;
        if previous.is_some_and(|value| value >= bytes) {
            return Err(noncanonical(
                "strings must be unique and sorted by UTF-8 bytes",
            ));
        }
        let value = std::str::from_utf8(bytes).map_err(|_| {
            ArtifactError::new(ArtifactErrorCode::InvalidUtf8, "string is not valid UTF-8")
        })?;
        reader.charge_model_bytes(value.len())?;
        strings.push(value.to_owned());
        previous = Some(bytes);
    }
    reader.finish()?;
    Ok(strings)
}

fn decode_constants(
    payload: &[u8],
    strings: &[String],
    limits: &DecodeLimits,
    budget: &DecodeBudget,
) -> Result<Vec<Constant>, ArtifactError> {
    let mut reader = Reader::with_budget(payload, limits, budget);
    let count = reader.count(limits.table_entries, "table entries")?;
    reader.charge_items::<Constant>(count)?;
    let mut constants = Vec::with_capacity(count);
    for _ in 0..count {
        constants.push(match reader.u8()? {
            0 => Constant::Int(reader.i64()?),
            1 => Constant::Bool(reader.bool()?),
            2 => {
                let bits = reader.u64()?;
                if bits != canonical_float_bits(bits) {
                    return Err(noncanonical("float constant is not the canonical NaN"));
                }
                Constant::Float(bits)
            }
            3 => Constant::String(reader.string_ref(strings)?.to_owned()),
            4 => {
                let byte_len = reader.count(limits.string_bytes, "byte constant bytes")?;
                reader.charge_model_bytes(byte_len)?;
                Constant::Bytes(reader.take(byte_len)?.to_vec())
            }
            5 => Constant::Unit,
            6 => Constant::ExternalFsAccess(match reader.u8()? {
                0 => crate::ExternalFsAccess::Read,
                1 => crate::ExternalFsAccess::Write,
                2 => crate::ExternalFsAccess::ReadWrite,
                _ => return Err(invalid_scalar("external filesystem access")),
            }),
            _ => return Err(invalid_scalar("constant tag")),
        });
    }
    reader.finish()?;
    Ok(constants)
}

fn decode_type_section(
    payload: &[u8],
    strings: &[String],
    limits: &DecodeLimits,
    budget: &DecodeBudget,
) -> Result<(Vec<ValueType>, Vec<EnumType>), ArtifactError> {
    let mut reader = Reader::with_budget(payload, limits, budget);
    let types = reader.types(strings, 0)?;
    let expanded_nodes = types.iter().try_fold(0_usize, |total, value_type| {
        total
            .checked_add(type_node_count(value_type)?)
            .ok_or_else(|| limit("expanded type nodes"))
    })?;
    if expanded_nodes > limits.expanded_type_nodes {
        return Err(limit("expanded type nodes"));
    }
    let string_ids = strings
        .iter()
        .enumerate()
        .map(|(index, value)| {
            (
                value.as_str(),
                u32::try_from(index).expect("string ID fits"),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let encoded_keys = types
        .iter()
        .map(|value_type| type_key(value_type, &string_ids))
        .collect::<Result<Vec<_>, _>>()?;
    if !encoded_keys.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err(noncanonical(
            "type descriptors must be unique and sorted canonically",
        ));
    }
    let count = reader.count(limits.table_entries, "table entries")?;
    reader.charge_items::<EnumType>(count)?;
    let mut enum_types = Vec::with_capacity(count);
    for _ in 0..count {
        let name = reader.string_ref(strings)?.to_owned();
        let variant_count = reader.count(limits.table_entries, "table entries")?;
        reader.charge_items::<EnumVariant>(variant_count)?;
        let mut variants = Vec::with_capacity(variant_count);
        for _ in 0..variant_count {
            variants.push(EnumVariant {
                name: reader.string_ref(strings)?.to_owned(),
                payload: decode_payload(&mut reader, strings, &types)?,
            });
        }
        enum_types.push(EnumType { name, variants });
    }
    reader.finish()?;
    Ok((types, enum_types))
}

fn decode_payload(
    reader: &mut Reader<'_>,
    strings: &[String],
    types: &[ValueType],
) -> Result<EnumPayloadType, ArtifactError> {
    Ok(match reader.u8()? {
        0 => EnumPayloadType::Unit,
        1 => {
            let count = reader.count(reader.limits.table_entries, "table entries")?;
            reader.charge_items::<ValueType>(count)?;
            let mut elements = Vec::with_capacity(count);
            for _ in 0..count {
                elements.push(reader.type_ref(types)?);
            }
            EnumPayloadType::Tuple(elements)
        }
        2 => {
            let count = reader.count(reader.limits.table_entries, "table entries")?;
            reader.charge_items::<RecordField>(count)?;
            let mut fields = Vec::with_capacity(count);
            for _ in 0..count {
                fields.push(RecordField {
                    name: reader.string_ref(strings)?.to_owned(),
                    value_type: reader.type_ref(types)?,
                });
            }
            EnumPayloadType::Record(fields)
        }
        _ => return Err(invalid_scalar("enum payload tag")),
    })
}

fn decode_functions(
    payload: &[u8],
    strings: &[String],
    types: &[ValueType],
    limits: &DecodeLimits,
    budget: &DecodeBudget,
) -> Result<(Vec<Function>, Vec<FunctionId>), ArtifactError> {
    let mut reader = Reader::with_budget(payload, limits, budget);
    let count = reader.count(limits.functions, "functions")?;
    reader.charge_items::<Function>(count)?;
    let async_count = reader.count(limits.functions, "async functions")?;
    reader.charge_items::<FunctionId>(async_count)?;
    let mut async_functions = Vec::with_capacity(async_count);
    for _ in 0..async_count {
        async_functions.push(reader.u32()?);
    }
    if !async_functions.windows(2).all(|pair| pair[0] < pair[1])
        || async_functions
            .last()
            .is_some_and(|id| *id as usize >= count)
    {
        return Err(noncanonical(
            "async function IDs must be unique, sorted, and in range",
        ));
    }
    let mut functions = Vec::with_capacity(count);
    for _ in 0..count {
        let name = reader.string_ref(strings)?.to_owned();
        let parameters = reader.registers(limits.registers_per_function)?;
        let captures = reader.registers(limits.registers_per_function)?;
        let register_count = reader.count(limits.registers_per_function, "registers")?;
        reader.charge_items::<ValueType>(register_count)?;
        let mut registers = Vec::with_capacity(register_count);
        for _ in 0..register_count {
            registers.push(reader.type_ref(types)?);
        }
        let return_type = reader.type_ref(types)?;
        let effects = reader.u32()?;
        let instruction_count = reader.count(limits.instructions_per_function, "instructions")?;
        reader.charge_items::<Instruction>(instruction_count)?;
        let mut code = Vec::with_capacity(instruction_count);
        for _ in 0..instruction_count {
            code.push(reader.instruction(types)?);
        }
        functions.push(Function {
            name,
            parameters,
            captures,
            registers,
            return_type,
            effects,
            code,
        });
    }
    reader.finish()?;
    Ok((functions, async_functions))
}

fn decode_effects(
    payload: &[u8],
    strings: &[String],
    limits: &DecodeLimits,
    budget: &DecodeBudget,
) -> Result<Vec<Vec<String>>, ArtifactError> {
    let mut reader = Reader::with_budget(payload, limits, budget);
    let count = reader.count(limits.table_entries, "table entries")?;
    reader.charge_items::<Vec<String>>(count)?;
    let mut effect_sets = Vec::with_capacity(count);
    for _ in 0..count {
        let effect_count = reader.count(limits.table_entries, "table entries")?;
        reader.charge_items::<String>(effect_count)?;
        let mut effects = Vec::with_capacity(effect_count);
        for _ in 0..effect_count {
            effects.push(reader.string_ref(strings)?.to_owned());
        }
        effect_sets.push(effects);
    }
    reader.finish()?;
    Ok(effect_sets)
}

fn decode_entries(
    payload: &[u8],
    strings: &[String],
    limits: &DecodeLimits,
    budget: &DecodeBudget,
) -> Result<(u32, Vec<EntryContract>), ArtifactError> {
    let mut reader = Reader::with_budget(payload, limits, budget);
    let count = reader.count(limits.table_entries, "entry contracts")?;
    reader.charge_items::<EntryContract>(count)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push(EntryContract {
            name: reader.string_ref(strings)?.to_owned(),
            function: reader.u32()?,
            input_schema: reader.u32()?,
            output_schema: reader.u32()?,
        });
    }
    reader.finish()?;
    Ok((0, entries))
}

fn decode_schemas(
    payload: &[u8],
    types: &[ValueType],
    limits: &DecodeLimits,
    budget: &DecodeBudget,
) -> Result<Vec<StrictSchema>, ArtifactError> {
    let mut r = Reader::with_budget(payload, limits, budget);
    let n = r.count(limits.table_entries, "schemas")?;
    r.charge_items::<StrictSchema>(n)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(StrictSchema {
            value_type: r.type_ref(types)?,
        });
    }
    r.finish()?;
    Ok(v)
}
fn decode_imports(
    payload: &[u8],
    strings: &[String],
    limits: &DecodeLimits,
    budget: &DecodeBudget,
) -> Result<Vec<ImportContract>, ArtifactError> {
    let mut r = Reader::with_budget(payload, limits, budget);
    let n = r.count(limits.table_entries, "imports")?;
    r.charge_items::<ImportContract>(n)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let importer = r.string_ref(strings)?.to_owned();
        let alias = r.string_ref(strings)?.to_owned();
        let package = r.string_ref(strings)?.to_owned();
        let version = r.string_ref(strings)?.to_owned();
        let module = r.string_ref(strings)?.to_owned();
        let mut digest = [0; 32];
        digest.copy_from_slice(r.take(32)?);
        v.push(ImportContract {
            importer,
            alias,
            package,
            version,
            module,
            content_digest: digest,
        });
    }
    r.finish()?;
    Ok(v)
}
fn decode_manifest(
    payload: &[u8],
    strings: &[String],
    limits: &DecodeLimits,
    budget: &DecodeBudget,
) -> Result<ManifestContract, ArtifactError> {
    let mut r = Reader::with_budget(payload, limits, budget);
    let n = r.u32()?;
    if n != 1 {
        return Err(noncanonical(
            "artifact must contain exactly one manifest contract",
        ));
    }
    r.charge_items::<ManifestContract>(1)?;
    let package = r.string_ref(strings)?.to_owned();
    let ver = r.string_ref(strings)?.to_owned();
    let language_requirement = r.string_ref(strings)?.to_owned();
    let list = |r: &mut Reader<'_>| -> Result<Vec<String>, ArtifactError> {
        let n = r.count(limits.table_entries, "capabilities")?;
        r.charge_items::<String>(n)?;
        let mut a = Vec::with_capacity(n);
        for _ in 0..n {
            a.push(r.string_ref(strings)?.to_owned());
        }
        Ok(a)
    };
    let required_capabilities = list(&mut r)?;
    let optional_capabilities = list(&mut r)?;
    let n = r.count(limits.table_entries, "limits")?;
    r.charge_items::<(String, u64)>(n)?;
    let mut limitsv = Vec::with_capacity(n);
    for _ in 0..n {
        limitsv.push((r.string_ref(strings)?.to_owned(), r.u64()?));
    }
    let n = r.count(limits.table_entries, "HTTPS origins")?;
    r.charge_items::<String>(n)?;
    let mut https_origins = Vec::with_capacity(n);
    for _ in 0..n {
        https_origins.push(r.string_ref(strings)?.to_owned());
    }
    let n = r.count(limits.table_entries, "required tools")?;
    r.charge_items::<ToolContract>(n)?;
    let mut required_tools = Vec::with_capacity(n);
    for _ in 0..n {
        let name = r.string_ref(strings)?.to_owned();
        let version = r.string_ref(strings)?.to_owned();
        let version_requirement = r.string_ref(strings)?.to_owned();
        let effect = r.string_ref(strings)?.to_owned();
        let input_schema = r.u32()?;
        let output_schema = r.u32()?;
        let error_schema = r.u32()?;
        let mut input_digest = [0; 32];
        input_digest.copy_from_slice(r.take(32)?);
        let mut output_digest = [0; 32];
        output_digest.copy_from_slice(r.take(32)?);
        let mut error_digest = [0; 32];
        error_digest.copy_from_slice(r.take(32)?);
        required_tools.push(ToolContract {
            name,
            version,
            version_requirement,
            effect,
            input_schema,
            output_schema,
            error_schema,
            input_digest,
            output_digest,
            error_digest,
        });
    }
    let mut tool_contract_digest = [0; 32];
    tool_contract_digest.copy_from_slice(r.take(32)?);
    r.finish()?;
    Ok(ManifestContract {
        package,
        version: ver,
        language_requirement,
        required_capabilities,
        optional_capabilities,
        limits: limitsv,
        https_origins,
        required_tools,
        tool_contract_digest,
    })
}

fn decode_debug(
    payload: &[u8],
    strings: &[String],
    limits: &DecodeLimits,
    budget: &DecodeBudget,
) -> Result<DebugInfo, ArtifactError> {
    let mut reader = Reader::with_budget(payload, limits, budget);
    let source_count = reader.count(limits.debug_records, "debug records")?;
    reader.charge_items::<String>(source_count)?;
    let mut sources = Vec::with_capacity(source_count);
    for _ in 0..source_count {
        sources.push(reader.string_ref(strings)?.to_owned());
    }
    let location_count = reader.count(limits.debug_records, "debug records")?;
    reader.charge_items::<DebugLocation>(location_count)?;
    let mut locations = Vec::with_capacity(location_count);
    for _ in 0..location_count {
        locations.push(DebugLocation {
            function: reader.u32()?,
            instruction: reader.u32()?,
            source: reader.u32()?,
            start: reader.u32()?,
            end: reader.u32()?,
        });
    }
    reader.finish()?;
    Ok(DebugInfo { sources, locations })
}

fn validate_string_table(artifact: &Artifact, actual: &[String]) -> Result<(), ArtifactError> {
    if collect_strings(artifact) != actual {
        return Err(noncanonical(
            "string table contains an unused string or omits a used string",
        ));
    }
    Ok(())
}

fn validate_debug_shape(debug: Option<&DebugInfo>) -> Result<(), ArtifactError> {
    let Some(debug) = debug else {
        return Ok(());
    };
    if !debug
        .sources
        .windows(2)
        .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
    {
        return Err(ArtifactError::new(
            ArtifactErrorCode::InvalidDebug,
            "debug source paths must be unique and sorted by UTF-8 bytes",
        ));
    }
    for source in &debug.sources {
        if !(is_normalized_source_path(source) || is_pkg_source(source)) {
            return Err(ArtifactError::new(
                ArtifactErrorCode::InvalidDebug,
                "debug source path is not normalized and relative",
            ));
        }
    }
    let mut previous: Option<(u32, u32)> = None;
    for location in &debug.locations {
        let key = (location.function, location.instruction);
        if previous.is_some_and(|value| value >= key) {
            return Err(ArtifactError::new(
                ArtifactErrorCode::InvalidDebug,
                "debug locations must be unique and sorted by function and instruction",
            ));
        }
        if location.start > location.end {
            return Err(ArtifactError::new(
                ArtifactErrorCode::InvalidDebug,
                "debug source span start exceeds its end",
            ));
        }
        previous = Some(key);
    }
    Ok(())
}
fn is_pkg_source(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("pkg://") else {
        return false;
    };
    let Some((identity, path)) = rest.split_once("/src/") else {
        return false;
    };
    is_package_identity(identity) && is_normalized_utf8_source_path(path)
}

fn is_normalized_source_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path.as_bytes().ends_with(b".allen")
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn validate_debug_references(
    module: &Module,
    debug: Option<&DebugInfo>,
) -> Result<(), ArtifactError> {
    let Some(debug) = debug else {
        return Ok(());
    };
    for location in &debug.locations {
        let function = module
            .functions
            .get(location.function as usize)
            .ok_or_else(|| {
                ArtifactError::new(
                    ArtifactErrorCode::InvalidDebug,
                    "debug location function is out of range",
                )
            })?;
        if location.instruction as usize >= function.code.len() {
            return Err(ArtifactError::new(
                ArtifactErrorCode::InvalidDebug,
                "debug location instruction is out of range",
            ));
        }
        if location.source as usize >= debug.sources.len() {
            return Err(ArtifactError::new(
                ArtifactErrorCode::InvalidDebug,
                "debug location source is out of range",
            ));
        }
    }
    Ok(())
}

#[derive(Clone)]
struct DecodeBudget {
    remaining: Rc<Cell<usize>>,
    remaining_type_nodes: Rc<Cell<usize>>,
}

impl DecodeBudget {
    fn new(bytes: usize, type_nodes: usize) -> Self {
        Self {
            remaining: Rc::new(Cell::new(bytes)),
            remaining_type_nodes: Rc::new(Cell::new(type_nodes)),
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
    limits: DecodeLimits,
    model_budget: DecodeBudget,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], limits: &DecodeLimits) -> Self {
        Self {
            bytes,
            offset: 0,
            limits: *limits,
            model_budget: DecodeBudget {
                remaining: Rc::new(Cell::new(limits.decoded_model_bytes)),
                remaining_type_nodes: Rc::new(Cell::new(limits.expanded_type_nodes)),
            },
        }
    }

    fn with_budget(bytes: &'a [u8], limits: &DecodeLimits, budget: &DecodeBudget) -> Self {
        Self {
            bytes,
            offset: 0,
            limits: *limits,
            model_budget: budget.clone(),
        }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ArtifactError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| truncated("data"))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| truncated("data"))?;
        self.offset = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, ArtifactError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ArtifactError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("fixed length"),
        ))
    }

    fn u32(&mut self) -> Result<u32, ArtifactError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("fixed length"),
        ))
    }

    fn u64(&mut self) -> Result<u64, ArtifactError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("fixed length"),
        ))
    }

    fn i64(&mut self) -> Result<i64, ArtifactError> {
        Ok(i64::from_le_bytes(
            self.take(8)?.try_into().expect("fixed length"),
        ))
    }

    fn bool(&mut self) -> Result<bool, ArtifactError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid_scalar("boolean")),
        }
    }

    fn version(&mut self) -> Result<SemanticVersion, ArtifactError> {
        Ok(SemanticVersion::new(self.u16()?, self.u16()?, self.u16()?))
    }

    fn count(&mut self, maximum: usize, resource: &str) -> Result<usize, ArtifactError> {
        let count = usize::try_from(self.u32()?).map_err(|_| limit(resource))?;
        if count > maximum {
            return Err(limit(resource));
        }
        Ok(count)
    }

    fn string_ref<'s>(&mut self, strings: &'s [String]) -> Result<&'s str, ArtifactError> {
        let index = self.u32()? as usize;
        let value = strings.get(index).map(String::as_str).ok_or_else(|| {
            ArtifactError::new(
                ArtifactErrorCode::InvalidScalar,
                "string table reference is out of range",
            )
        })?;
        self.charge_model_bytes(value.len())?;
        Ok(value)
    }

    fn registers(&mut self, maximum: usize) -> Result<Vec<Register>, ArtifactError> {
        let count = self.count(
            maximum.min(self.limits.operands_per_instruction),
            "operands",
        )?;
        self.charge_items::<Register>(count)?;
        let mut registers = Vec::with_capacity(count);
        for _ in 0..count {
            registers.push(self.u16()?);
        }
        Ok(registers)
    }

    fn types(&mut self, strings: &[String], depth: usize) -> Result<Vec<ValueType>, ArtifactError> {
        let count = self.count(self.limits.table_entries, "table entries")?;
        self.charge_items::<ValueType>(count)?;
        let mut types = Vec::with_capacity(count);
        for _ in 0..count {
            types.push(self.value_type(strings, depth)?);
        }
        Ok(types)
    }

    fn fields(
        &mut self,
        strings: &[String],
        depth: usize,
    ) -> Result<Vec<RecordField>, ArtifactError> {
        let count = self.count(self.limits.table_entries, "table entries")?;
        self.charge_items::<RecordField>(count)?;
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            fields.push(RecordField {
                name: self.string_ref(strings)?.to_owned(),
                value_type: self.value_type(strings, depth)?,
            });
        }
        Ok(fields)
    }

    #[allow(clippy::too_many_lines)]
    fn value_type(&mut self, strings: &[String], depth: usize) -> Result<ValueType, ArtifactError> {
        if depth > self.limits.type_depth {
            return Err(limit("type depth"));
        }
        self.charge_type_node()?;
        Ok(match self.u8()? {
            0 => ValueType::Int,
            1 => ValueType::Bool,
            2 => ValueType::Float,
            3 => ValueType::String,
            4 => ValueType::Bytes,
            5 => ValueType::Unit,
            6 => ValueType::Never,
            7 => ValueType::List(Box::new(self.value_type(strings, depth + 1)?)),
            8 => ValueType::Map(
                Box::new(self.value_type(strings, depth + 1)?),
                Box::new(self.value_type(strings, depth + 1)?),
            ),
            9 => ValueType::Tuple(self.types(strings, depth + 1)?),
            10 => ValueType::Record(self.fields(strings, depth + 1)?),
            11 => ValueType::Enum(self.u32()?),
            12 => ValueType::Option(Box::new(self.value_type(strings, depth + 1)?)),
            13 => ValueType::Result(
                Box::new(self.value_type(strings, depth + 1)?),
                Box::new(self.value_type(strings, depth + 1)?),
            ),
            14 => ValueType::Function {
                parameters: self.types(strings, depth + 1)?,
                return_type: Box::new(self.value_type(strings, depth + 1)?),
                effects: self.u32()?,
            },
            15 => ValueType::Unknown,
            16 => ValueType::Future(Box::new(self.value_type(strings, depth + 1)?)),
            17 => ValueType::Task(Box::new(self.value_type(strings, depth + 1)?)),
            18 => ValueType::Workspace,
            19 => ValueType::ExternalFsAccess,
            20 => ValueType::SubAgent,
            _ => return Err(invalid_scalar("value type tag")),
        })
    }

    fn numeric_op(&mut self) -> Result<NumericBinaryOp, ArtifactError> {
        match self.u8()? {
            0 => Ok(NumericBinaryOp::Add),
            1 => Ok(NumericBinaryOp::Subtract),
            2 => Ok(NumericBinaryOp::Multiply),
            3 => Ok(NumericBinaryOp::Divide),
            _ => Err(invalid_scalar("numeric operation")),
        }
    }

    fn compare_op(&mut self) -> Result<CompareOp, ArtifactError> {
        match self.u8()? {
            0 => Ok(CompareOp::Equal),
            1 => Ok(CompareOp::NotEqual),
            2 => Ok(CompareOp::Less),
            3 => Ok(CompareOp::LessEqual),
            4 => Ok(CompareOp::Greater),
            5 => Ok(CompareOp::GreaterEqual),
            _ => Err(invalid_scalar("comparison operation")),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn instruction(&mut self, types: &[ValueType]) -> Result<Instruction, ArtifactError> {
        let mut operand_budget = self.limits.operands_per_instruction;
        Ok(match self.u8()? {
            0 => Instruction::Const {
                destination: self.u16()?,
                constant: self.u32()?,
            },
            1 => Instruction::Move {
                destination: self.u16()?,
                source: self.u16()?,
            },
            2 => Instruction::IntBinary {
                destination: self.u16()?,
                left: self.u16()?,
                right: self.u16()?,
                operation: self.numeric_op()?,
            },
            3 => Instruction::FloatBinary {
                destination: self.u16()?,
                left: self.u16()?,
                right: self.u16()?,
                operation: self.numeric_op()?,
            },
            42 => Instruction::IntRemainder {
                destination: self.u16()?,
                left: self.u16()?,
                right: self.u16()?,
            },
            43 => Instruction::StringCall {
                destination: self.u16()?,
                operation: match self.u8()? {
                    0 => StringOperation::ByteLength,
                    1 => StringOperation::Concat,
                    2 => StringOperation::Get,
                    3 => StringOperation::Slice,
                    4 => StringOperation::Find,
                    5 => StringOperation::Contains,
                    6 => StringOperation::StartsWith,
                    7 => StringOperation::EndsWith,
                    8 => StringOperation::Split,
                    9 => StringOperation::Join,
                    10 => StringOperation::TrimAscii,
                    11 => StringOperation::FromUtf8,
                    12 => StringOperation::TemplateConcat,
                    _ => return Err(invalid_scalar("String operation")),
                },
                arguments: self.operand_registers(&mut operand_budget)?,
            },
            44 => Instruction::CapabilityInspect {
                destination: self.u16()?,
                operation: match self.u8()? {
                    0 => CapabilityOperation::IsGranted,
                    1 => CapabilityOperation::Granted,
                    _ => return Err(invalid_scalar("capability inspection operation")),
                },
                arguments: self.operand_registers(&mut operand_budget)?,
            },
            45 => Instruction::SafeCollectionCall {
                destination: self.u16()?,
                operation: match self.u8()? {
                    0 => SafeCollectionOperation::ListGet,
                    1 => SafeCollectionOperation::ListTrySet,
                    2 => SafeCollectionOperation::BytesGet,
                    3 => SafeCollectionOperation::MapGet,
                    _ => return Err(invalid_scalar("safe collection operation")),
                },
                arguments: self.operand_registers(&mut operand_budget)?,
            },
            46 => Instruction::CheckedIntCall {
                destination: self.u16()?,
                operation: match self.u8()? {
                    0 => CheckedIntOperation::Add,
                    1 => CheckedIntOperation::Subtract,
                    2 => CheckedIntOperation::Multiply,
                    3 => CheckedIntOperation::Divide,
                    4 => CheckedIntOperation::Remainder,
                    5 => CheckedIntOperation::Negate,
                    _ => return Err(invalid_scalar("checked integer operation")),
                },
                arguments: self.operand_registers(&mut operand_budget)?,
            },
            4 => Instruction::IntNegate {
                destination: self.u16()?,
                source: self.u16()?,
            },
            5 => Instruction::FloatNegate {
                destination: self.u16()?,
                source: self.u16()?,
            },
            6 => Instruction::Compare {
                destination: self.u16()?,
                left: self.u16()?,
                right: self.u16()?,
                operation: self.compare_op()?,
            },
            7 => Instruction::BoolNot {
                destination: self.u16()?,
                source: self.u16()?,
            },
            8 => Instruction::BoolBinary {
                destination: self.u16()?,
                left: self.u16()?,
                right: self.u16()?,
                operation: match self.u8()? {
                    0 => BoolBinaryOp::And,
                    1 => BoolBinaryOp::Or,
                    _ => return Err(invalid_scalar("boolean operation")),
                },
            },
            9 => Instruction::ListNew {
                destination: self.u16()?,
                elements: self.operand_registers(&mut operand_budget)?,
            },
            10 => {
                let destination = self.u16()?;
                let count = self.count(self.limits.operands_per_instruction, "operands")?;
                Self::charge_operands(&mut operand_budget, count, 2)?;
                self.charge_items::<(Register, Register)>(count)?;
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    entries.push((self.u16()?, self.u16()?));
                }
                Instruction::MapNew {
                    destination,
                    entries,
                }
            }
            11 => Instruction::TupleNew {
                destination: self.u16()?,
                elements: self.operand_registers(&mut operand_budget)?,
            },
            12 => Instruction::IndexGet {
                destination: self.u16()?,
                collection: self.u16()?,
                index: self.u16()?,
            },
            13 => Instruction::TupleGet {
                destination: self.u16()?,
                tuple: self.u16()?,
                index: self.u32()?,
            },
            14 => Instruction::Convert {
                destination: self.u16()?,
                source: self.u16()?,
                conversion: match self.u8()? {
                    0 => Conversion::IntToFloat,
                    1 => Conversion::ToString,
                    2 => Conversion::StringToBytes,
                    _ => return Err(invalid_scalar("conversion")),
                },
            },
            15 => {
                let destination = self.u16()?;
                let count = self.count(self.limits.operands_per_instruction, "operands")?;
                Self::charge_operands(&mut operand_budget, count, 2)?;
                self.charge_items::<(u32, Register)>(count)?;
                let mut fields = Vec::with_capacity(count);
                for _ in 0..count {
                    fields.push((self.u32()?, self.u16()?));
                }
                Instruction::RecordNew {
                    destination,
                    fields,
                }
            }
            16 => Instruction::FieldGet {
                destination: self.u16()?,
                record: self.u16()?,
                field: self.u32()?,
            },
            17 => Instruction::EnumNew {
                destination: self.u16()?,
                variant: self.u32()?,
                payload: self.operand_registers(&mut operand_budget)?,
            },
            18 => Instruction::BranchBool {
                condition: self.u16()?,
                true_target: self.u32()?,
                false_target: self.u32()?,
            },
            19 => {
                let source = self.u16()?;
                let count = self.count(self.limits.operands_per_instruction, "operands")?;
                Self::charge_operands(&mut operand_budget, count, 2)?;
                self.charge_items::<EnumSwitchArm>(count)?;
                let mut arms = Vec::with_capacity(count);
                for _ in 0..count {
                    arms.push(EnumSwitchArm {
                        variant: self.u32()?,
                        target: self.u32()?,
                        bindings: self.operand_registers(&mut operand_budget)?,
                    });
                }
                Instruction::SwitchEnum { source, arms }
            }
            20 => Instruction::Jump {
                target: self.u32()?,
            },
            21 => Instruction::TryResult {
                destination: self.u16()?,
                source: self.u16()?,
            },
            22 => Instruction::ToUnknown {
                destination: self.u16()?,
                source: self.u16()?,
            },
            23 => Instruction::Narrow {
                destination: self.u16()?,
                source: self.u16()?,
                target: self.type_ref(types)?,
            },
            24 => Instruction::DirectCall {
                destination: self.u16()?,
                function: self.u32()?,
                arguments: self.operand_registers(&mut operand_budget)?,
            },
            25 => Instruction::ClosureNew {
                destination: self.u16()?,
                function: self.u32()?,
                captures: self.operand_registers(&mut operand_budget)?,
            },
            26 => Instruction::ClosureCall {
                destination: self.u16()?,
                closure: self.u16()?,
                arguments: self.operand_registers(&mut operand_budget)?,
            },
            27 => Instruction::Return {
                source: self.u16()?,
            },
            28 => Instruction::AsyncCall {
                destination: self.u16()?,
                function: self.u32()?,
                arguments: self.operand_registers(&mut operand_budget)?,
            },
            29 => Instruction::Spawn {
                destination: self.u16()?,
                future: self.u16()?,
                scope: self.u32()?,
            },
            30 => Instruction::Await {
                destination: self.u16()?,
                source: self.u16()?,
            },
            31 => Instruction::TaskScopeEnter { scope: self.u32()? },
            32 => Instruction::TaskScopeExit { scope: self.u32()? },
            33 => Instruction::Stop {
                reason: self.u16()?,
            },
            34 => Instruction::TaskSnapshot {
                destination: self.u16()?,
                source: self.u16()?,
            },
            35 => Instruction::WorkspaceGet {
                destination: self.u16()?,
            },
            36 => Instruction::EffectCall {
                destination: self.u16()?,
                operation: match self.u8()? {
                    0 => crate::EffectOperation::ReadText,
                    1 => crate::EffectOperation::ReadBytes,
                    2 => crate::EffectOperation::WriteText,
                    3 => crate::EffectOperation::WriteBytes,
                    4 => crate::EffectOperation::List,
                    5 => crate::EffectOperation::HttpGet,
                    6 => crate::EffectOperation::PermissionRequestFile,
                    7 => crate::EffectOperation::PermissionRequestDirectory,
                    8 => crate::EffectOperation::AgentMessage,
                    9 => crate::EffectOperation::AgentAsk,
                    10 => crate::EffectOperation::AgentTranscript,
                    11 => crate::EffectOperation::ModelRequest,
                    12 => crate::EffectOperation::UserAsk,
                    13 => crate::EffectOperation::SubAgentCreate,
                    14 => crate::EffectOperation::SubAgentRun,
                    15 => crate::EffectOperation::SubAgentMessage,
                    16 => crate::EffectOperation::SubAgentAsk,
                    17 => crate::EffectOperation::Search,
                    _ => return Err(invalid_scalar("effect operation")),
                },
                arguments: self.operand_registers(&mut operand_budget)?,
            },
            37 => Instruction::ToolInvoke {
                destination: self.u16()?,
                tool: self.u32()?,
                input: self.u16()?,
            },
            38 => Instruction::Length {
                destination: self.u16()?,
                collection: self.u16()?,
            },
            39 => Instruction::ListAppend {
                destination: self.u16()?,
                values: self.u16()?,
                value: self.u16()?,
            },
            40 => Instruction::ListSet {
                destination: self.u16()?,
                values: self.u16()?,
                index: self.u16()?,
                value: self.u16()?,
            },
            41 => Instruction::MapEntryAt {
                destination: self.u16()?,
                map: self.u16()?,
                index: self.u16()?,
            },
            _ => return Err(invalid_scalar("instruction opcode")),
        })
    }

    fn operand_registers(&mut self, remaining: &mut usize) -> Result<Vec<Register>, ArtifactError> {
        let count = self.count(self.limits.operands_per_instruction, "operands")?;
        Self::charge_operands(remaining, count, 1)?;
        self.charge_items::<Register>(count)?;
        let mut registers = Vec::with_capacity(count);
        for _ in 0..count {
            registers.push(self.u16()?);
        }
        Ok(registers)
    }

    fn charge_operands(
        remaining: &mut usize,
        count: usize,
        width: usize,
    ) -> Result<(), ArtifactError> {
        let charge = count.checked_mul(width).ok_or_else(|| limit("operands"))?;
        *remaining = remaining
            .checked_sub(charge)
            .ok_or_else(|| limit("operands"))?;
        Ok(())
    }

    fn type_ref(&mut self, types: &[ValueType]) -> Result<ValueType, ArtifactError> {
        let id = self.u32()?;
        let value = types
            .get(id as usize)
            .ok_or_else(|| invalid_scalar("type descriptor reference"))?;
        self.charge_model_bytes(type_owned_bytes(value)?)?;
        Ok(value.clone())
    }

    fn charge_model_bytes(&mut self, bytes: usize) -> Result<(), ArtifactError> {
        let remaining = self
            .model_budget
            .remaining
            .get()
            .checked_sub(bytes)
            .ok_or_else(|| limit("decoded model bytes"))?;
        self.model_budget.remaining.set(remaining);
        Ok(())
    }

    fn charge_items<T>(&mut self, count: usize) -> Result<(), ArtifactError> {
        let bytes = count
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| limit("decoded model bytes"))?;
        self.charge_model_bytes(bytes)
    }

    fn charge_type_node(&mut self) -> Result<(), ArtifactError> {
        let remaining = self
            .model_budget
            .remaining_type_nodes
            .get()
            .checked_sub(1)
            .ok_or_else(|| limit("expanded type nodes"))?;
        self.model_budget.remaining_type_nodes.set(remaining);
        self.charge_items::<ValueType>(1)
    }

    fn finish(&self) -> Result<(), ArtifactError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ArtifactError::new(
                ArtifactErrorCode::TrailingBytes,
                "section contains trailing bytes",
            ))
        }
    }
}

fn string_id(strings: &BTreeMap<&str, u32>, value: &str) -> Result<u32, ArtifactError> {
    strings.get(value).copied().ok_or_else(|| {
        ArtifactError::new(
            ArtifactErrorCode::InvalidScalar,
            "value is missing from the string table",
        )
    })
}

fn to_u32(value: usize, resource: &str) -> Result<u32, ArtifactError> {
    u32::try_from(value).map_err(|_| limit(resource))
}

fn usize_from_u64(value: u64, resource: &str) -> Result<usize, ArtifactError> {
    usize::try_from(value).map_err(|_| limit(resource))
}

fn put_count(output: &mut Vec<u8>, value: usize, resource: &str) -> Result<(), ArtifactError> {
    put_u32(output, to_u32(value, resource)?);
    Ok(())
}

fn put_bytes(output: &mut Vec<u8>, bytes: &[u8], resource: &str) -> Result<(), ArtifactError> {
    put_count(output, bytes.len(), resource)?;
    output.extend_from_slice(bytes);
    Ok(())
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_version(output: &mut Vec<u8>, version: SemanticVersion) {
    put_u16(output, version.major);
    put_u16(output, version.minor);
    put_u16(output, version.patch);
}

fn limit(resource: &str) -> ArtifactError {
    ArtifactError::new(
        ArtifactErrorCode::LimitExceeded,
        format!("{resource} exceeds the decode limit"),
    )
}

fn truncated(resource: &str) -> ArtifactError {
    ArtifactError::new(
        ArtifactErrorCode::Truncated,
        format!("{resource} is truncated"),
    )
}

fn invalid_scalar(resource: &str) -> ArtifactError {
    ArtifactError::new(
        ArtifactErrorCode::InvalidScalar,
        format!("{resource} has an invalid value"),
    )
}

fn noncanonical(message: impl Into<String>) -> ArtifactError {
    ArtifactError::new(ArtifactErrorCode::NonCanonical, message)
}

// FIPS 180-4 SHA-256. The artifact crate keeps the format primitive local so
// decoding does not depend on a serialization or cryptography framework.
#[allow(clippy::many_single_char_names, clippy::too_many_lines)]
fn sha256(input: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let padding = (64 - ((input.len() + 9) % 64)) % 64;
    let mut message = Vec::with_capacity(input.len() + 9 + padding);
    message.extend_from_slice(input);
    message.push(0x80);
    message.resize(input.len() + 1 + padding, 0);
    message.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for chunk in message.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, word) in words[..16].iter_mut().enumerate() {
            let start = index * 4;
            *word = u32::from_be_bytes(
                chunk[start..start + 4]
                    .try_into()
                    .expect("chunk word is fixed width"),
            );
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let upper_e = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(upper_e)
                .wrapping_add(choice)
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let upper_a = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = upper_a.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut digest = [0_u8; 32];
    for (index, value) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(debug: bool) -> Artifact {
        Artifact {
            metadata: ArtifactMetadata {
                bytecode_version: BYTECODE_VERSION,
                ..ArtifactMetadata::default()
            },
            module: Module {
                constants: vec![Constant::Unit],
                enum_types: Vec::new(),
                effect_sets: vec![Vec::new()],
                functions: vec![Function {
                    name: "pkg/x74657374/x302e312e30/x737263/x6d61696e.allen::main".to_owned(),
                    parameters: Vec::new(),
                    captures: Vec::new(),
                    registers: vec![ValueType::Unit],
                    return_type: ValueType::Unit,
                    effects: 0,
                    code: vec![
                        Instruction::Const {
                            destination: 0,
                            constant: 0,
                        },
                        Instruction::Return { source: 0 },
                    ],
                }],
                async_functions: vec![],
                entry: 0,
            },
            debug: debug.then(|| DebugInfo {
                sources: vec!["pkg://test@0.1.0/src/main.allen".to_owned()],
                locations: vec![DebugLocation {
                    function: 0,
                    instruction: 0,
                    source: 0,
                    start: 0,
                    end: 4,
                }],
            }),
            schemas: vec![StrictSchema {
                value_type: ValueType::Unit,
            }],
            entries: vec![EntryContract {
                name: "main".to_owned(),
                function: 0,
                input_schema: 0,
                output_schema: 0,
            }],
            imports: Vec::new(),
            manifest: Some(ManifestContract {
                package: "test".to_owned(),
                version: "0.1.0".to_owned(),
                language_requirement: "0.1".to_owned(),
                required_capabilities: Vec::new(),
                optional_capabilities: Vec::new(),
                limits: Vec::new(),
                https_origins: Vec::new(),
                required_tools: Vec::new(),
                tool_contract_digest: compute_tool_contract_digest(&[]),
            }),
        }
    }

    fn redigest(bytes: &mut [u8]) {
        let digest = sha256(&bytes[HEADER_SIZE..]);
        bytes[32..64].copy_from_slice(&digest);
    }

    fn section_payload_offset(bytes: &[u8], target: SectionId) -> usize {
        let mut offset = HEADER_SIZE;
        loop {
            let id = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
            let length = usize::try_from(u64::from_le_bytes(
                bytes[offset + 2..offset + 10].try_into().unwrap(),
            ))
            .unwrap();
            let payload = offset + 10;
            if id == target as u16 {
                return payload;
            }
            offset = payload + length;
        }
    }

    #[test]
    fn sha256_matches_fips_vector() {
        assert_eq!(
            sha256(b"abc"),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }

    #[test]
    fn canonical_round_trip_preserves_exact_bytes_and_metadata() {
        let model = artifact(true);
        let bytes = encode(&model).unwrap();
        assert_eq!(&bytes[..8], &ARTIFACT_MAGIC);
        assert_eq!(u16::from_le_bytes([bytes[10], bytes[11]]), BYTECODE_VERSION);
        let decoded = decode(&bytes, &DecodeLimits::default()).unwrap();
        assert_eq!(decoded.artifact(), &model);
        assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
        assert_eq!(decoded.content_digest(), &sha256(&bytes[HEADER_SIZE..]));
        assert_eq!(decoded.section_summaries().len(), 11);
        decode_and_verify(&bytes, &DecodeLimits::default()).unwrap();
    }

    #[test]
    fn artifact_codec_requires_exactly_one_manifest() {
        let mut missing = artifact(false);
        missing.manifest = None;
        let error = encode(&missing).unwrap_err();
        assert_eq!(error.code(), ArtifactErrorCode::NonCanonical);
        assert_eq!(
            error.message(),
            "artifacts require exactly one manifest contract"
        );

        for count in [0_u32, 2] {
            let mut bytes = encode(&artifact(false)).unwrap();
            let manifest = section_payload_offset(&bytes, SectionId::ManifestContracts);
            bytes[manifest..manifest + 4].copy_from_slice(&count.to_le_bytes());
            redigest(&mut bytes);
            let error = decode(&bytes, &DecodeLimits::default()).unwrap_err();
            assert_eq!(error.code(), ArtifactErrorCode::NonCanonical);
            assert_eq!(
                error.message(),
                "artifact must contain exactly one manifest contract"
            );
        }
    }

    #[test]
    fn omitting_debug_preserves_executable_module() {
        let with_debug = artifact(true);
        let without_debug = artifact(false);
        let first = decode(&encode(&with_debug).unwrap(), &DecodeLimits::default()).unwrap();
        let second = decode(&encode(&without_debug).unwrap(), &DecodeLimits::default()).unwrap();
        assert_eq!(first.module(), second.module());
        assert!(first.debug().is_some());
        assert!(second.debug().is_none());
    }

    #[test]
    fn every_one_byte_truncation_is_rejected() {
        let bytes = encode(&artifact(true)).unwrap();
        for length in 0..bytes.len() {
            assert!(
                decode(&bytes[..length], &DecodeLimits::default()).is_err(),
                "accepted truncation at byte {length}"
            );
        }
    }

    #[test]
    fn corruption_and_trailing_data_are_rejected() {
        let bytes = encode(&artifact(true)).unwrap();
        let mut corrupt = bytes.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert_eq!(
            decode(&corrupt, &DecodeLimits::default())
                .unwrap_err()
                .code(),
            ArtifactErrorCode::DigestMismatch
        );

        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            decode(&trailing, &DecodeLimits::default())
                .unwrap_err()
                .code(),
            ArtifactErrorCode::TrailingBytes
        );

        let mut unknown = bytes;
        unknown[HEADER_SIZE] = 11;
        redigest(&mut unknown);
        assert_eq!(
            decode(&unknown, &DecodeLimits::default())
                .unwrap_err()
                .code(),
            ArtifactErrorCode::UnknownSection
        );
    }

    #[test]
    fn artifact_profile_and_section_structure_are_rejected_stably() {
        let original = encode(&artifact(false)).unwrap();
        let mut unsupported_version = original.clone();
        unsupported_version[10..12].copy_from_slice(&14_u16.to_le_bytes());
        assert_eq!(
            decode(&unsupported_version, &DecodeLimits::default())
                .unwrap_err()
                .code(),
            ArtifactErrorCode::UnsupportedVersion
        );

        let mut unsupported_profile = original.clone();
        unsupported_profile[24..26].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            decode(&unsupported_profile, &DecodeLimits::default())
                .unwrap_err()
                .code(),
            ArtifactErrorCode::UnsupportedProfile
        );
        let mut unsupported_language = original.clone();
        unsupported_language[12..14].copy_from_slice(&1_u16.to_le_bytes());
        assert_eq!(
            decode(&unsupported_language, &DecodeLimits::default())
                .unwrap_err()
                .code(),
            ArtifactErrorCode::UnsupportedVersion
        );

        let first_payload_len = usize::try_from(u64::from_le_bytes(
            original[HEADER_SIZE + 2..HEADER_SIZE + 10]
                .try_into()
                .unwrap(),
        ))
        .unwrap();
        let second_id = HEADER_SIZE + 10 + first_payload_len;
        let mut duplicate = original.clone();
        duplicate[second_id..second_id + 2].copy_from_slice(&1_u16.to_le_bytes());
        redigest(&mut duplicate);
        assert_eq!(
            decode(&duplicate, &DecodeLimits::default())
                .unwrap_err()
                .code(),
            ArtifactErrorCode::DuplicateSection
        );

        let mut missing = original;
        missing[second_id..second_id + 2].copy_from_slice(&3_u16.to_le_bytes());
        redigest(&mut missing);
        assert_eq!(
            decode(&missing, &DecodeLimits::default())
                .unwrap_err()
                .code(),
            ArtifactErrorCode::MissingSection
        );

        let mut out_of_order = encode(&artifact(false)).unwrap();
        let second_payload_len = usize::try_from(u64::from_le_bytes(
            out_of_order[second_id + 2..second_id + 10]
                .try_into()
                .unwrap(),
        ))
        .unwrap();
        let third_id = second_id + 10 + second_payload_len;
        out_of_order[third_id..third_id + 2].copy_from_slice(&1_u16.to_le_bytes());
        redigest(&mut out_of_order);
        assert_eq!(
            decode(&out_of_order, &DecodeLimits::default())
                .unwrap_err()
                .code(),
            ArtifactErrorCode::SectionOrder
        );
    }

    #[test]
    fn noncanonical_effect_table_is_rejected_during_decode() {
        let mut model = artifact(false);
        model.module.effect_sets = vec![vec!["z.effect".to_owned()], Vec::new()];
        model.module.functions[0].effects = 1;
        let bytes = encode(&model).unwrap();
        assert_eq!(
            decode(&bytes, &DecodeLimits::default()).unwrap_err().code(),
            ArtifactErrorCode::NonCanonical
        );
    }

    #[test]
    fn malformed_utf8_and_noncanonical_scalar_are_rejected() {
        let mut bytes = encode(&artifact(false)).unwrap();
        // First section: 2-byte ID, 8-byte length, 4-byte count, 4-byte string length.
        bytes[HEADER_SIZE + 18] = 0xff;
        redigest(&mut bytes);
        assert_eq!(
            decode(&bytes, &DecodeLimits::default()).unwrap_err().code(),
            ArtifactErrorCode::InvalidUtf8
        );

        let mut bytes = encode(&artifact(false)).unwrap();
        bytes[30] = 1;
        assert_eq!(
            decode(&bytes, &DecodeLimits::default()).unwrap_err().code(),
            ArtifactErrorCode::InvalidHeader
        );
    }

    #[test]
    fn decoder_enforces_each_simple_limit_class() {
        let bytes = encode(&artifact(true)).unwrap();
        assert_eq!(
            decode(
                &bytes,
                &DecodeLimits {
                    artifact_bytes: bytes.len() - 1,
                    ..DecodeLimits::default()
                }
            )
            .unwrap_err()
            .code(),
            ArtifactErrorCode::ArtifactTooLarge
        );
        assert_eq!(
            decode(
                &bytes,
                &DecodeLimits {
                    section_bytes: 1,
                    ..DecodeLimits::default()
                }
            )
            .unwrap_err()
            .code(),
            ArtifactErrorCode::SectionTooLarge
        );
        let cases = [
            DecodeLimits {
                string_bytes: 3,
                ..DecodeLimits::default()
            },
            DecodeLimits {
                table_entries: 0,
                ..DecodeLimits::default()
            },
            DecodeLimits {
                functions: 0,
                ..DecodeLimits::default()
            },
            DecodeLimits {
                registers_per_function: 0,
                ..DecodeLimits::default()
            },
            DecodeLimits {
                instructions_per_function: 1,
                ..DecodeLimits::default()
            },
            DecodeLimits {
                debug_records: 0,
                ..DecodeLimits::default()
            },
            DecodeLimits {
                expanded_type_nodes: 0,
                ..DecodeLimits::default()
            },
            DecodeLimits {
                decoded_model_bytes: 0,
                ..DecodeLimits::default()
            },
        ];
        for limits in cases {
            assert_eq!(
                decode(&bytes, &limits).unwrap_err().code(),
                ArtifactErrorCode::LimitExceeded
            );
        }
        assert_eq!(
            decode_and_verify(
                &bytes,
                &DecodeLimits {
                    verifier_state_bytes: 0,
                    ..DecodeLimits::default()
                }
            )
            .unwrap_err()
            .code(),
            ArtifactErrorCode::LimitExceeded
        );
        assert_eq!(
            decode_and_verify(
                &bytes,
                &DecodeLimits {
                    verifier_state_bytes: 2,
                    ..DecodeLimits::default()
                }
            )
            .unwrap_err()
            .code(),
            ArtifactErrorCode::LimitExceeded
        );
    }

    #[test]
    fn verifier_state_limit_charges_full_affine_analysis_vectors() {
        let mut model = artifact(false);
        model.module.functions[0].registers = vec![ValueType::Unit; 4_096];
        let limits = DecodeLimits {
            verifier_state_bytes: 10_000,
            ..DecodeLimits::default()
        };
        assert_eq!(
            validate_verifier_complexity(&model.module, &limits)
                .unwrap_err()
                .code(),
            ArtifactErrorCode::LimitExceeded
        );
    }

    #[test]
    fn decoder_enforces_operand_and_type_depth_limits() {
        let mut model = artifact(false);
        model.module.functions[0].registers =
            vec![ValueType::Int, ValueType::Tuple(vec![ValueType::Int])];
        model.module.functions[0].code = vec![
            Instruction::TupleNew {
                destination: 1,
                elements: vec![0],
            },
            Instruction::Return { source: 1 },
        ];
        let bytes = encode(&model).unwrap();
        assert_eq!(
            decode(
                &bytes,
                &DecodeLimits {
                    operands_per_instruction: 0,
                    ..DecodeLimits::default()
                }
            )
            .unwrap_err()
            .code(),
            ArtifactErrorCode::LimitExceeded
        );
        assert_eq!(
            decode(
                &bytes,
                &DecodeLimits {
                    type_depth: 0,
                    ..DecodeLimits::default()
                }
            )
            .unwrap_err()
            .code(),
            ArtifactErrorCode::LimitExceeded
        );
    }

    #[test]
    fn independent_verification_rejects_invalid_module_and_debug_references() {
        let mut invalid = artifact(false);
        invalid.module.functions[0].code[1] = Instruction::Return { source: 7 };
        let error =
            decode_and_verify(&encode(&invalid).unwrap(), &DecodeLimits::default()).unwrap_err();
        assert_eq!(error.code(), ArtifactErrorCode::VerificationFailed);

        let mut invalid = artifact(true);
        invalid.debug.as_mut().unwrap().locations[0].function = 7;
        let error =
            decode_and_verify(&encode(&invalid).unwrap(), &DecodeLimits::default()).unwrap_err();
        assert_eq!(error.code(), ArtifactErrorCode::InvalidDebug);
    }
}
