use std::collections::BTreeMap;
use std::fmt;

use crate::canonical::{canonical_json, digest_bytes};
use crate::name::{NameError, ToolName, generated_tool_effect, validate_generated_names};
use crate::{
    ExactVersion, SCHEMA_DIALECT, SCHEMA_PROFILE, SchemaError, SchemaLimits, ToolSchema,
    VersionRange,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogLimits {
    pub tools: usize,
    pub decoded_schema_bytes: usize,
    pub schema: SchemaLimits,
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self {
            tools: 256,
            decoded_schema_bytes: 3 * 1024 * 1024,
            schema: SchemaLimits::default(),
        }
    }
}

impl CatalogLimits {
    #[must_use]
    pub fn bounded_by(self, host: Self) -> Self {
        Self {
            tools: self.tools.min(host.tools),
            decoded_schema_bytes: self.decoded_schema_bytes.min(host.decoded_schema_bytes),
            schema: self.schema.bounded_by(host.schema),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Idempotency {
    Unknown,
    Idempotent,
    NonIdempotent,
}

impl Idempotency {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Idempotent => "idempotent",
            Self::NonIdempotent => "non_idempotent",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    pub name: ToolName,
    pub version: ExactVersion,
    pub input_schema: ToolSchema,
    pub output_schema: ToolSchema,
    pub error_schema: ToolSchema,
    pub effects: Vec<String>,
    pub idempotency: Idempotency,
}

impl ToolDefinition {
    /// Parse all externally controlled fields for one catalog definition.
    ///
    /// # Errors
    ///
    /// Rejects any noncanonical identity, schema, effect list, or zero-major tool.
    #[allow(clippy::too_many_arguments)]
    pub fn parse(
        name: &str,
        version: &str,
        input_schema: &str,
        output_schema: &str,
        error_schema: &str,
        effects: Vec<String>,
        idempotency: Idempotency,
        limits: &SchemaLimits,
    ) -> Result<Self, CatalogError> {
        let definition = Self {
            name: ToolName::parse(name)?,
            version: ExactVersion::parse(version).map_err(|_| CatalogError::Version)?,
            input_schema: ToolSchema::parse(input_schema, limits)?,
            output_schema: ToolSchema::parse(output_schema, limits)?,
            error_schema: ToolSchema::parse(error_schema, limits)?,
            effects,
            idempotency,
        };
        definition.validate()?;
        Ok(definition)
    }

    /// Recheck invariants after constructing a definition from typed fields.
    ///
    /// # Errors
    ///
    /// Rejects invalid effect metadata and a zero-major version.
    pub fn validate(&self) -> Result<(), CatalogError> {
        generated_tool_effect(&self.name, self.version)?;
        if self.effects.len() > 32
            || self
                .effects
                .windows(2)
                .any(|pair| pair[0].as_bytes() >= pair[1].as_bytes())
            || self
                .effects
                .iter()
                .any(|effect| !is_canonical_effect(effect))
        {
            return Err(CatalogError::Effect);
        }
        Ok(())
    }

    fn digest_value(&self) -> serde_json::Value {
        serde_json::json!({
            "error_schema": self.error_schema.digest(),
            "effects": self.effects,
            "idempotency": self.idempotency.as_str(),
            "input_schema": self.input_schema.digest(),
            "name": self.name.as_str(),
            "output_schema": self.output_schema.digest(),
            "version": self.version.to_string()
        })
    }
}

#[derive(Clone, Debug)]
pub struct FrozenCatalog {
    tools: Vec<ToolDefinition>,
    by_name: BTreeMap<ToolName, usize>,
    digest: String,
}

impl FrozenCatalog {
    /// Validate and freeze one complete connection catalog.
    ///
    /// # Errors
    ///
    /// The input must already be in strict canonical-name byte order.
    pub fn freeze(
        tools: Vec<ToolDefinition>,
        limits: &CatalogLimits,
    ) -> Result<Self, CatalogError> {
        Self::freeze_with_dialect(SCHEMA_DIALECT, tools, limits)
    }

    /// Freeze a catalog only for the exact 2020-12 dialect URI.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Dialect`] for every other dialect.
    pub fn freeze_with_dialect(
        dialect: &str,
        tools: Vec<ToolDefinition>,
        limits: &CatalogLimits,
    ) -> Result<Self, CatalogError> {
        if dialect != SCHEMA_DIALECT {
            return Err(CatalogError::Dialect);
        }
        if tools.len() > limits.tools.min(256) {
            return Err(CatalogError::Limit);
        }
        if tools
            .windows(2)
            .any(|pair| pair[0].name.as_str().as_bytes() >= pair[1].name.as_str().as_bytes())
        {
            return Err(CatalogError::Order);
        }
        let mut schema_bytes = 0usize;
        for tool in &tools {
            tool.validate()?;
            if !tool.input_schema.fits_limits(&limits.schema)
                || !tool.output_schema.fits_limits(&limits.schema)
                || !tool.error_schema.fits_limits(&limits.schema)
            {
                return Err(CatalogError::Limit);
            }
            schema_bytes = schema_bytes
                .checked_add(tool.input_schema.source_bytes())
                .and_then(|value| value.checked_add(tool.output_schema.source_bytes()))
                .and_then(|value| value.checked_add(tool.error_schema.source_bytes()))
                .ok_or(CatalogError::Limit)?;
            if schema_bytes > limits.decoded_schema_bytes.min(3 * 1024 * 1024) {
                return Err(CatalogError::Limit);
            }
        }
        validate_generated_names(&tools)?;
        let value = serde_json::json!({
            "schema_dialect": SCHEMA_DIALECT,
            "tools": tools.iter().map(ToolDefinition::digest_value).collect::<Vec<_>>()
        });
        let digest = digest_bytes(&canonical_json(&value));
        let by_name = tools
            .iter()
            .enumerate()
            .map(|(index, tool)| (tool.name.clone(), index))
            .collect();
        Ok(Self {
            tools,
            by_name,
            digest,
        })
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
    #[must_use]
    pub const fn schema_profile(&self) -> &'static str {
        SCHEMA_PROFILE
    }
    #[must_use]
    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }
    #[must_use]
    pub fn get(&self, name: &ToolName) -> Option<&ToolDefinition> {
        self.by_name.get(name).map(|index| &self.tools[*index])
    }

    /// Select the exact required tool contracts for a program.
    ///
    /// # Errors
    ///
    /// Requirements must be unique, sorted, present, and contain the selected version.
    pub fn select(
        &self,
        requirements: &[ToolRequirement],
    ) -> Result<Vec<SelectedToolContract>, CatalogError> {
        if requirements
            .windows(2)
            .any(|pair| pair[0].name.as_str().as_bytes() >= pair[1].name.as_str().as_bytes())
        {
            return Err(CatalogError::Order);
        }
        requirements
            .iter()
            .map(|requirement| {
                let tool = self
                    .get(&requirement.name)
                    .ok_or(CatalogError::MissingTool)?;
                if !requirement.version.contains(tool.version) {
                    return Err(CatalogError::VersionUnsatisfied);
                }
                Ok(SelectedToolContract {
                    name: tool.name.clone(),
                    version_requirement: requirement.version,
                    version: tool.version,
                    effect: generated_tool_effect(&tool.name, tool.version)?,
                    input_schema: tool.input_schema.digest().to_owned(),
                    output_schema: tool.output_schema.digest().to_owned(),
                    error_schema: tool.error_schema.digest().to_owned(),
                })
            })
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRequirement {
    pub name: ToolName,
    pub version: VersionRange,
}

impl ToolRequirement {
    /// Parse one required manifest entry.
    ///
    /// # Errors
    ///
    /// Rejects a noncanonical tool name or non-exact bounded range.
    pub fn parse(name: &str, version: &str) -> Result<Self, CatalogError> {
        Ok(Self {
            name: ToolName::parse(name)?,
            version: VersionRange::parse(version).map_err(|_| CatalogError::Version)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedToolContract {
    pub name: ToolName,
    pub version_requirement: VersionRange,
    pub version: ExactVersion,
    pub effect: String,
    pub input_schema: String,
    pub output_schema: String,
    pub error_schema: String,
}

impl SelectedToolContract {
    fn canonical_value(&self) -> serde_json::Value {
        serde_json::json!({
            "effect": self.effect,
            "error_schema": self.error_schema,
            "input_schema": self.input_schema,
            "name": self.name.as_str(),
            "output_schema": self.output_schema,
            "version": self.version.to_string(),
            "version_requirement": self.version_requirement.to_string()
        })
    }
}

/// Digest one already sorted selected-tool contract set.
///
/// # Errors
///
/// Rejects duplicate, unsorted, malformed, or cross-contract inconsistent entries.
pub fn selected_tool_contract_digest(
    contracts: &[SelectedToolContract],
) -> Result<String, CatalogError> {
    if contracts
        .windows(2)
        .any(|pair| pair[0].name.as_str().as_bytes() >= pair[1].name.as_str().as_bytes())
    {
        return Err(CatalogError::Order);
    }
    for contract in contracts {
        if !contract.version_requirement.contains(contract.version)
            || contract.effect != generated_tool_effect(&contract.name, contract.version)?
            || !is_digest(&contract.input_schema)
            || !is_digest(&contract.output_schema)
            || !is_digest(&contract.error_schema)
        {
            return Err(CatalogError::Contract);
        }
    }
    let value = serde_json::json!({"tools": contracts.iter().map(SelectedToolContract::canonical_value).collect::<Vec<_>>()});
    Ok(digest_bytes(&canonical_json(&value)))
}

#[derive(Debug)]
pub enum CatalogError {
    Dialect,
    Name(NameError),
    Version,
    Schema(SchemaError),
    Effect,
    Order,
    Limit,
    MissingTool,
    VersionUnsatisfied,
    Contract,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Dialect => "catalog schema dialect is invalid",
            Self::Name(_) => "catalog generated name is invalid",
            Self::Version => "catalog tool version is invalid",
            Self::Schema(_) => "catalog tool schema is invalid",
            Self::Effect => "catalog tool effect metadata is invalid",
            Self::Order => "catalog tools are not in canonical order",
            Self::Limit => "catalog limit exceeded",
            Self::MissingTool => "required tool is missing",
            Self::VersionUnsatisfied => "required tool version is unsatisfied",
            Self::Contract => "selected tool contract is inconsistent",
        })
    }
}
impl std::error::Error for CatalogError {}
impl From<NameError> for CatalogError {
    fn from(error: NameError) -> Self {
        Self::Name(error)
    }
}
impl From<SchemaError> for CatalogError {
    fn from(error: SchemaError) -> Self {
        Self::Schema(error)
    }
}

fn is_canonical_effect(effect: &str) -> bool {
    let (base, version) = effect
        .rsplit_once('@')
        .map_or((effect, None), |(base, version)| (base, Some(version)));
    if version.is_some_and(|version| {
        version.is_empty()
            || version.starts_with('0')
            || !version.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return false;
    }
    !base.is_empty()
        && base.split('.').all(|segment| {
            let mut bytes = segment.bytes();
            bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
                && bytes
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
}
fn is_digest(value: &str) -> bool {
    value.len() == 71
        && value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(name: &str, version: &str) -> ToolDefinition {
        ToolDefinition::parse(
            name, version,
            r#"{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}"#,
            r#"{"type":"boolean"}"#,
            r#"{"type":"string","enum":["denied"]}"#,
            vec!["external.write".to_owned()], Idempotency::Unknown, &SchemaLimits::default(),
        ).unwrap()
    }

    #[test]
    fn catalog_and_selected_contract_digests_are_stable_golden_values() {
        let catalog = FrozenCatalog::freeze(
            vec![definition("github.create_issue", "2.1.3")],
            &CatalogLimits::default(),
        )
        .unwrap();
        assert_eq!(catalog.schema_profile(), "allen.tool-schema/0.1");
        assert_eq!(
            catalog.digest(),
            "sha256:f4d8eeb98978b1a24b2e8486dbe0dbe77cc58e9b37483c2ea262cc2a7ccd137c"
        );
        let selected = catalog
            .select(&[ToolRequirement::parse("github.create_issue", ">=2.0.0, <3.0.0").unwrap()])
            .unwrap();
        assert_eq!(
            selected_tool_contract_digest(&selected).unwrap(),
            "sha256:51af0e84dc0e3bcbbec98ef70322d7696794f966077f75f60abbb4450690310c"
        );
        assert_eq!(
            selected_tool_contract_digest(&[]).unwrap(),
            "sha256:fe2f3b4ef49492d81cb350fb689bf9f9dff6cfd1817d72d6ff9fe3350e3d5e6a"
        );
    }

    #[test]
    fn order_effect_versions_and_mangling_collisions_fail_before_freeze() {
        assert!(matches!(
            FrozenCatalog::freeze(
                vec![definition("z", "1.0.0"), definition("a", "1.0.0")],
                &CatalogLimits::default()
            ),
            Err(CatalogError::Order)
        ));
        assert!(
            ToolDefinition::parse(
                "a",
                "0.1.0",
                r#"{"type":"boolean"}"#,
                r#"{"type":"boolean"}"#,
                r#"{"type":"boolean"}"#,
                vec![],
                Idempotency::Unknown,
                &SchemaLimits::default()
            )
            .is_err()
        );
        assert!(
            ToolDefinition::parse(
                "a",
                "1.0.0",
                r#"{"type":"boolean"}"#,
                r#"{"type":"boolean"}"#,
                r#"{"type":"boolean"}"#,
                vec!["Bad".to_owned()],
                Idempotency::Unknown,
                &SchemaLimits::default()
            )
            .is_err()
        );
        assert!(
            FrozenCatalog::freeze(
                vec![definition("a-b", "1.0.0"), definition("a_x2D_b", "1.0.0")],
                &CatalogLimits::default()
            )
            .is_err()
        );
        assert!(
            FrozenCatalog::freeze(
                vec![definition("a", "1.0.0"), definition("a.call", "1.0.0")],
                &CatalogLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn catalog_rechecks_host_lowered_schema_limits() {
        let limits = CatalogLimits {
            schema: SchemaLimits {
                properties: 0,
                ..SchemaLimits::default()
            },
            ..CatalogLimits::default()
        };
        assert!(matches!(
            FrozenCatalog::freeze(vec![definition("a", "1.0.0")], &limits),
            Err(CatalogError::Limit)
        ));
    }

    #[test]
    fn catalog_count_and_decoded_schema_byte_limits_are_enforced() {
        let cases = [
            (
                vec![definition("a", "1.0.0"), definition("b", "1.0.0")],
                CatalogLimits {
                    tools: 1,
                    ..CatalogLimits::default()
                },
            ),
            (
                vec![definition("a", "1.0.0")],
                CatalogLimits {
                    decoded_schema_bytes: 1,
                    ..CatalogLimits::default()
                },
            ),
        ];
        for (tools, limits) in cases {
            assert!(matches!(
                FrozenCatalog::freeze(tools, &limits),
                Err(CatalogError::Limit)
            ));
        }
    }

    #[test]
    fn requirement_selection_checks_presence_range_and_contract_digests() {
        let catalog =
            FrozenCatalog::freeze(vec![definition("a", "2.0.0")], &CatalogLimits::default())
                .unwrap();
        assert!(
            catalog
                .select(&[ToolRequirement::parse("missing", ">=1.0.0, <2.0.0").unwrap()])
                .is_err()
        );
        assert!(
            catalog
                .select(&[ToolRequirement::parse("a", ">=1.0.0, <2.0.0").unwrap()])
                .is_err()
        );
        let mut selected = catalog
            .select(&[ToolRequirement::parse("a", ">=2.0.0, <3.0.0").unwrap()])
            .unwrap();
        selected[0].input_schema = format!("sha256:{}", "0".repeat(64));
        assert!(selected_tool_contract_digest(&selected).is_ok());
        selected[0].effect = "tool.wrong@2".to_owned();
        assert!(selected_tool_contract_digest(&selected).is_err());
    }
}
