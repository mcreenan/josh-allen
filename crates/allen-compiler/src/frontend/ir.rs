//! Stable compiler entry, package, manifest, and compilation result contracts.

use super::{HirBundle, MirBundle};
use allen_bytecode::{
    DebugInfo, EntryValidatorSite, FunctionId, Module, RecordInvariantDefinition, ValueType,
};
use allen_schema::ToolRequirement;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Compilation {
    pub module: Module,
    pub debug: DebugInfo,
    pub hir: HirBundle,
    pub mir: MirBundle,
    pub effect_report: Vec<EffectReportEntry>,
    pub exported_functions: Vec<ExportedFunction>,
    pub record_invariants: Vec<RecordInvariantDefinition>,
}

/// One source-level exported function boundary after exact type resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportedFunction {
    /// The stable bytecode function table ID for this source boundary.
    pub function_id: FunctionId,
    pub module: String,
    pub function: String,
    pub parameter_types: Vec<ValueType>,
    pub return_type: ValueType,
    pub parameter_spellings: Vec<String>,
    pub return_spelling: String,
    pub effects: Vec<String>,
    pub input_validators: Vec<EntryValidatorSite>,
    pub output_validators: Vec<EntryValidatorSite>,
}

/// One manifest-selected package entry source boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageEntryPoint {
    pub module: String,
    pub function: String,
}

/// One package-local template signature bound to its artifact table index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompilerTemplateBinding {
    pub package: String,
    pub name: String,
    pub template: u32,
    pub holes: Vec<(String, ValueType)>,
}

/// Canonical package-module inputs for the compiler.
///
/// `sources` uses canonical module identities. `import_targets` maps a source
/// module and its package-qualified import spelling to the target identity.
/// The package loader owns manifest, lockfile, alias, and hash validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageSourceBundle {
    pub root: String,
    pub sources: BTreeMap<String, String>,
    pub import_targets: BTreeMap<(String, String), String>,
    /// Manifest-selected source boundaries. The first is the package entry.
    pub entry_points: Vec<PackageEntryPoint>,
    /// Additional canonical module identities to compile.
    ///
    /// These roots are not entry selectors. Every entry-point module is loaded
    /// whether or not it also appears here.
    pub entry_modules: Vec<String>,
}

/// The manifest embedded at the start of one standalone source file.
///
/// This is only the source-level form. Package validation owns capability
/// grants and conflicts with an `allen.toml` manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlineManifest {
    pub language: String,
    pub entry: String,
    pub capabilities: Vec<String>,
    pub http_origins: Vec<String>,
    pub tools: Vec<ToolRequirement>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectReportEntry {
    pub module: String,
    pub function: String,
    pub effects: Vec<String>,
}
