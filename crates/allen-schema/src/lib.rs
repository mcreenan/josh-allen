#![forbid(unsafe_code)]

//! Closed tool schemas, canonical tool identities, and frozen tool catalogs.

mod canonical;
mod catalog;
mod json;
mod name;
mod schema;
mod version;

pub use canonical::{canonical_json, digest_bytes};
pub use catalog::{
    CatalogError, CatalogLimits, FrozenCatalog, Idempotency, SelectedToolContract, ToolDefinition,
    ToolRequirement, selected_tool_contract_digest,
};
pub use json::{StrictJsonError, parse_json_strict};
pub use name::{
    GeneratedName, NameError, SchemaRole, ToolName, generated_tool_effect, mangle_effect_segment,
    mangle_source_segment, union_declarations, validate_generated_names,
};
pub use schema::{
    Descriptor, Field, SchemaError, SchemaErrorCode, SchemaLimits, ToolSchema, ValidationCode,
    ValidationIssue, Variant,
};
pub use version::{ExactVersion, VersionError, VersionRange};

/// The only tool schema profile in host-0.1.
pub const SCHEMA_PROFILE: &str = "allen.tool-schema/0.1";
/// The required JSON Schema dialect.
pub const SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";
