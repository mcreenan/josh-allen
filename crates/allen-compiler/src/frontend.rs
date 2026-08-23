//! Canonical version 0.1 compiler frontend.

use super::{Diagnostic, Span};
use allen_bytecode::{
    CapabilityOperation, CheckedIntOperation, CompareOp, Constant, Conversion, DebugInfo,
    DebugLocation, EffectOperation, EffectSetId, EnumPayloadType, EnumType, EnumVariant,
    ExternalFsAccess, Function, FunctionId, Instruction, MAX_VALUE_NESTING, Module,
    NumericBinaryOp, RecordField, Register, SafeCollectionOperation, StringOperation, ValueType,
    agent_error_type, external_directory_request_type, external_file_request_type, file_error_type,
    http_response_type, is_strict_schema_type, model_error_type, network_error_type,
    permission_error_type, prompt_output_type, prompt_type, search_match_type,
    sub_agent_error_type, task_snapshot_type, transcript_message_type, transcript_part_enum_type,
    transcript_query_type, transcript_snapshot_type, user_error_type,
};
use allen_schema::{FrozenCatalog, mangle_source_segment};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
use allen_bytecode::canonical_float_bits;
#[cfg(test)]
use allen_schema::ToolRequirement;

mod tool;

mod syntax_lowering;

pub use tool::{CompilerToolBinding, PreparedTools, ToolPreparationError, prepare_tools};

mod hir;
pub use hir::*;

mod mir;
pub use mir::*;

mod ir;
pub use ir::*;

mod resolution;
use resolution::{
    BundleCompileContext, PreparedModule, collect_effect_sets, is_canonical_effect, load_modules,
    lowered_type_spelling, rebase_tool_type, resolve_bundle, resolve_deferred_effect_sets,
};

mod checking;
use checking::{
    LocalBinding, SemanticType, concrete_type, contains_affine, contains_stored_sub_agent,
    contains_workspace,
};

mod bytecode_lowering;
use bytecode_lowering::{GlobalLowering, lower_one_function};

#[derive(Clone, Debug)]
struct LoweredModule {
    imports: Vec<LoweredImport>,
    types: Vec<LoweredTypeDeclaration>,
    functions: Vec<LoweredFunction>,
}

/// One source file parsed and lowered exactly once by the canonical frontend.
///
/// The lowered representation remains private. Callers may inspect the inline
/// manifest and then consume this value through one of the prepared compile
/// entry points without reparsing the source.
#[derive(Clone, Debug)]
pub struct PreparedSource {
    source: String,
    manifest: Option<InlineManifest>,
    module: LoweredModule,
}

impl PreparedSource {
    /// Returns the validated leading inline manifest, when present.
    #[must_use]
    pub const fn inline_manifest(&self) -> Option<&InlineManifest> {
        self.manifest.as_ref()
    }
}

#[derive(Clone, Debug)]
enum LoweredTypeDeclaration {
    Record {
        exported: bool,
        name: String,
        name_span: Span,
        fields: Vec<(String, LoweredType, Span)>,
    },
    Enum {
        exported: bool,
        name: String,
        name_span: Span,
        variants: Vec<LoweredEnumVariant>,
    },
    Alias {
        exported: bool,
        name: String,
        name_span: Span,
        target: LoweredType,
    },
}

#[derive(Clone, Debug)]
struct LoweredEnumVariant {
    name: String,
    span: Span,
    payload: LoweredEnumPayload,
}

#[derive(Clone, Debug)]
enum LoweredEnumPayload {
    Unit,
    Tuple(Vec<LoweredType>),
    Record(Vec<(String, LoweredType, Span)>),
}

impl LoweredTypeDeclaration {
    fn name(&self) -> &str {
        match self {
            Self::Record { name, .. } | Self::Enum { name, .. } | Self::Alias { name, .. } => name,
        }
    }

    fn exported(&self) -> bool {
        match self {
            Self::Record { exported, .. }
            | Self::Enum { exported, .. }
            | Self::Alias { exported, .. } => *exported,
        }
    }

    fn name_span(&self) -> Span {
        match self {
            Self::Record { name_span, .. }
            | Self::Enum { name_span, .. }
            | Self::Alias { name_span, .. } => *name_span,
        }
    }
}

#[derive(Clone, Debug)]
struct LoweredImport {
    names: Vec<(String, String, Span)>,
    path: String,
    resolved_path: Option<String>,
    span: Span,
}

#[derive(Clone, Debug)]
struct LoweredFunction {
    exported: bool,
    is_async: bool,
    name: String,
    name_span: Span,
    generics: Vec<(String, Span)>,
    parameters: Vec<(String, LoweredType, Span)>,
    return_type: LoweredType,
    declared_effects: Option<Vec<String>>,
    effects_span: Option<Span>,
    body: LoweredBody,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LoweredType {
    Named(String, Span),
    Tuple(Vec<Self>, Span),
    Record(Vec<(String, Self, Span)>, Span),
    List(Box<Self>, Span),
    Map(Box<Self>, Box<Self>, Span),
    Option(Box<Self>, Span),
    Result(Box<Self>, Box<Self>, Span),
    Future(Box<Self>, Span),
    Task(Box<Self>, Span),
    Prompt(Box<Self>, Span),
    Function {
        parameters: Vec<Self>,
        return_type: Box<Self>,
        effects: Vec<String>,
        span: Span,
    },
}

impl LoweredType {
    fn span(&self) -> Span {
        match self {
            Self::Named(_, span)
            | Self::Tuple(_, span)
            | Self::Record(_, span)
            | Self::List(_, span)
            | Self::Map(_, _, span)
            | Self::Option(_, span)
            | Self::Result(_, _, span)
            | Self::Future(_, span)
            | Self::Task(_, span)
            | Self::Prompt(_, span)
            | Self::Function { span, .. } => *span,
        }
    }
}

#[derive(Clone, Debug)]
struct LoweredBody {
    statements: Vec<LoweredStatement>,
    tail: Option<LoweredExpr>,
    span: Span,
}

#[derive(Clone, Debug)]
enum LoweredStatement {
    Let {
        name: String,
        name_span: Span,
        mutable: bool,
        annotation: Option<LoweredType>,
        value: LoweredExpr,
    },
    Assignment {
        name: String,
        name_span: Span,
        operation: Option<Binary>,
        value: LoweredExpr,
    },
    ControlFlow(LoweredExpr),
    Return(Option<LoweredExpr>, Span),
    While {
        condition: LoweredExpr,
        body: LoweredBody,
        span: Span,
    },
    Loop {
        body: LoweredBody,
        span: Span,
    },
    For {
        binding: LoweredLoopBinding,
        source: LoweredForSource,
        body: LoweredBody,
        span: Span,
    },
    Break(Span),
    Continue(Span),
}

#[derive(Clone, Debug)]
struct LoweredLoopBinding {
    elements: Vec<LoweredLoopBindingElement>,
    tuple: bool,
    span: Span,
}

#[derive(Clone, Debug)]
struct LoweredLoopBindingElement {
    name: Option<String>,
    span: Span,
}

#[derive(Clone, Debug)]
enum LoweredForSource {
    Iterable(LoweredExpr),
    Range {
        start: LoweredExpr,
        end: LoweredExpr,
    },
}

#[derive(Clone, Debug)]
struct LoweredExpr {
    kind: LoweredExprKind,
    span: Span,
}

#[derive(Clone, Debug)]
enum LoweredExprKind {
    Unit,
    Int(i64),
    Float(u64),
    Bool(bool),
    String(String),
    Template(Vec<LoweredTemplatePart>),
    Bytes(Vec<u8>),
    Variable(String),
    List(Vec<LoweredExpr>),
    Map(Vec<(LoweredExpr, LoweredExpr)>),
    Record {
        name: String,
        fields: Vec<(String, LoweredExpr, Span)>,
    },
    Prompt {
        system: Box<LoweredExpr>,
        context: Option<Box<LoweredExpr>>,
        data: Option<Box<LoweredExpr>>,
        output: LoweredType,
        max_attempts: u32,
    },
    Enum {
        name: String,
        variant: String,
        payload: LoweredEnumValuePayload,
    },
    FieldGet {
        record: Box<LoweredExpr>,
        field: String,
        field_span: Span,
    },
    Try(Box<LoweredExpr>),
    Match {
        source: Box<LoweredExpr>,
        arms: Vec<(LoweredPattern, LoweredExpr, Span)>,
    },
    If {
        condition: Box<LoweredExpr>,
        then_body: Box<LoweredBody>,
        else_branch: Option<LoweredElse>,
    },
    Tuple(Vec<LoweredExpr>),
    Unary {
        operation: Unary,
        operand: Box<LoweredExpr>,
    },
    Binary {
        operation: Binary,
        left: Box<LoweredExpr>,
        right: Box<LoweredExpr>,
    },
    Index {
        collection: Box<LoweredExpr>,
        index: Box<LoweredExpr>,
    },
    Call {
        callee: Box<LoweredExpr>,
        type_arguments: Vec<LoweredType>,
        arguments: Vec<LoweredExpr>,
    },
    Spawn(Box<LoweredExpr>),
    Await(Box<LoweredExpr>),
    AwaitBlock(Box<LoweredBody>),
    Closure {
        parameters: Vec<(String, LoweredType, Span)>,
        return_type: LoweredType,
        declared_effects: Option<Vec<String>>,
        body: Box<LoweredBody>,
    },
}

#[derive(Clone, Debug)]
enum LoweredTemplatePart {
    Literal { value: String, span: Span },
    Interpolation(LoweredExpr),
}

fn template_interpolations(parts: &[LoweredTemplatePart]) -> impl Iterator<Item = &LoweredExpr> {
    parts.iter().filter_map(|part| match part {
        LoweredTemplatePart::Literal { .. } => None,
        LoweredTemplatePart::Interpolation(expression) => Some(expression),
    })
}

#[derive(Clone, Debug)]
enum LoweredElse {
    Body(Box<LoweredBody>),
    If(Box<LoweredExpr>),
}

#[derive(Clone, Debug)]
enum LoweredEnumValuePayload {
    Unit,
    Tuple(Vec<LoweredExpr>),
    Record(Vec<(String, LoweredExpr, Span)>),
}

#[derive(Clone, Debug)]
enum LoweredPattern {
    Wildcard,
    Bool(bool),
    Record {
        name: String,
        fields: Vec<(String, Span, Option<String>)>,
    },
    Enum {
        name: String,
        variant: String,
        bindings: Vec<Option<String>>,
        fields: Option<Vec<(String, Span, Option<String>)>>,
    },
    Option {
        some: bool,
        binding: Option<String>,
    },
    Result {
        ok: bool,
        binding: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Unary {
    Not,
    Negate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Binary {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    And,
    Or,
}

fn is_forbidden_source_word(word: &str) -> bool {
    matches!(word, "any" | "undefined" | "null")
}

/// Parse the optional inline manifest through the canonical syntax frontend.
///
/// The second return value preserves the public API shape but contains the
/// original source. Equal-length manifest blanking is no longer performed.
/// Diagnostics and semantic failures after a valid leading manifest do not
/// affect extraction; full compilation validates the complete module.
///
/// # Errors
///
/// Returns a stable, source-qualified diagnostic for a malformed or invalid
/// leading manifest, or for a source that exceeds frontend resource limits.
pub fn extract_inline_manifest(
    source: &str,
) -> Result<(Option<InlineManifest>, String), Diagnostic> {
    let manifest = syntax_lowering::extract_manifest("inline.allen", source)?;
    Ok((manifest, source.to_owned()))
}

/// Parse and fully lower one source exactly once for later compilation.
///
/// `path` is retained only in diagnostics; bundle compilation supplies the
/// canonical module identity when this value is consumed.
///
/// # Errors
///
/// Returns a stable, source-qualified syntax or checked-lowering diagnostic.
pub fn prepare_source(path: &str, source: &str) -> Result<PreparedSource, Diagnostic> {
    let checked = syntax_lowering::lower_source(path, source)?;
    Ok(PreparedSource {
        source: source.to_owned(),
        manifest: checked.manifest,
        module: checked.module,
    })
}

/// Compile standalone source after parsing its optional inline manifest.
///
/// The manifest is returned for package preflight; it is not an authority
/// grant by itself.
///
/// # Errors
///
/// Returns inline-manifest or compiler diagnostics.
pub fn compile_inline_manifest_source(
    source: &str,
) -> Result<(Option<InlineManifest>, Compilation), Vec<Diagnostic>> {
    let prepared = prepare_source("main.allen", source).map_err(|diagnostic| vec![diagnostic])?;
    compile_prepared_inline_manifest_source(prepared)
}

/// Compile a previously prepared standalone source without parsing it again.
///
/// # Errors
///
/// Returns the same deterministic diagnostics as [`compile_inline_manifest_source`].
pub fn compile_prepared_inline_manifest_source(
    prepared: PreparedSource,
) -> Result<(Option<InlineManifest>, Compilation), Vec<Diagnostic>> {
    let PreparedSource {
        source,
        manifest,
        module,
    } = prepared;
    let entry = manifest.as_ref().map_or("main", |manifest| &manifest.entry);
    let bundle = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([("main.allen".to_owned(), source.clone())]),
        import_targets: BTreeMap::new(),
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: entry.to_owned(),
        }],
        entry_modules: Vec::new(),
    };
    let compilation = compile_package_bundle_with_prepared_root(&bundle, source, module, &[])?;
    Ok((manifest, compilation))
}

/// Compile inline-manifest source against one frozen tool catalog.
///
/// # Errors
///
/// Returns strict manifest, catalog-selection, source, type, or effect diagnostics.
pub fn compile_inline_manifest_source_with_catalog(
    source: &str,
    catalog: &FrozenCatalog,
) -> Result<(Option<InlineManifest>, Compilation, PreparedTools), Vec<Diagnostic>> {
    let checked = syntax_lowering::lower_source("main.allen", source)
        .map_err(|diagnostic| vec![diagnostic])?;
    let manifest = checked.manifest;
    let requirements = manifest
        .as_ref()
        .map_or(&[][..], |manifest| manifest.tools.as_slice());
    let mut prepared = prepare_tools(catalog, requirements).map_err(|_| {
        vec![Diagnostic::new(
            "E3005",
            "inline manifest tools do not match the frozen catalog",
            Span { start: 0, end: 0 },
        )]
    })?;
    let entry = manifest.as_ref().map_or("main", |manifest| &manifest.entry);
    let bundle = PackageSourceBundle {
        root: "main.allen".to_owned(),
        sources: BTreeMap::from([("main.allen".to_owned(), source.to_owned())]),
        import_targets: BTreeMap::new(),
        entry_points: vec![PackageEntryPoint {
            module: "main.allen".to_owned(),
            function: entry.to_owned(),
        }],
        entry_modules: Vec::new(),
    };
    let compilation = compile_package_bundle_with_prepared_root(
        &bundle,
        source.to_owned(),
        checked.module,
        &prepared.bindings,
    )?;
    let compilation = finalize_prepared_tools(compilation, &mut prepared)?;
    Ok((manifest, compilation, prepared))
}

/// Compile a complete in-memory source bundle.
///
/// # Errors
///
/// Returns deterministic module, parse, type, effect, or lowering diagnostics.
///
/// # Panics
///
/// Panics only if an internal symbol or function table invariant is broken.
#[allow(clippy::too_many_lines)]
pub fn compile_bundle(
    root: &str,
    sources: &BTreeMap<String, String>,
) -> Result<Compilation, Vec<Diagnostic>> {
    compile_bundle_with_import_targets(root, sources, &BTreeMap::new(), &[], &[], &[])
}

/// Compile a bundle while reusing its already prepared root source.
///
/// The prepared text must exactly match `sources[root]`. Every other reachable
/// module is parsed and lowered once during this call.
///
/// # Errors
///
/// Returns the same deterministic diagnostics as [`compile_bundle`], plus a
/// source-qualified mismatch diagnostic if the prepared source is stale.
pub fn compile_bundle_with_prepared_source(
    root: &str,
    sources: &BTreeMap<String, String>,
    prepared: PreparedSource,
) -> Result<Compilation, Vec<Diagnostic>> {
    let normalized = normalize_root(root).map_err(|diagnostic| vec![diagnostic])?;
    let PreparedSource {
        source,
        manifest: _,
        module,
    } = prepared;
    compile_bundle_with_import_targets_and_prepared(
        root,
        sources,
        BundleCompileContext {
            import_targets: &BTreeMap::new(),
            entry_modules: &[],
            entry_points: &[],
            tool_bindings: &[],
            prepared: BTreeMap::from([(normalized, PreparedModule { source, module })]),
        },
    )
}

/// Compile one package-resolved source bundle with canonical module identities.
///
/// # Errors
///
/// Returns the same deterministic diagnostics as [`compile_bundle`].
pub fn compile_package_bundle(
    bundle: &PackageSourceBundle,
) -> Result<Compilation, Vec<Diagnostic>> {
    compile_package_bundle_with_tools(bundle, &[])
}

/// Compile a package source bundle against one frozen, manifest-selected tool table.
///
/// # Errors
///
/// Returns deterministic diagnostics for invalid bindings or source calls.
pub fn compile_package_bundle_with_tools(
    bundle: &PackageSourceBundle,
    tools: &[CompilerToolBinding],
) -> Result<Compilation, Vec<Diagnostic>> {
    compile_bundle_with_import_targets(
        &bundle.root,
        &bundle.sources,
        &bundle.import_targets,
        &bundle.entry_modules,
        &bundle.entry_points,
        tools,
    )
}

/// Compile with prepared tool tables and finalize their enum IDs for the module.
///
/// # Errors
///
/// Returns deterministic compiler diagnostics. The prepared tables are changed
/// only after compilation succeeds.
pub fn compile_package_bundle_with_prepared_tools(
    bundle: &PackageSourceBundle,
    prepared: &mut PreparedTools,
) -> Result<Compilation, Vec<Diagnostic>> {
    let compilation = compile_package_bundle_with_tools(bundle, &prepared.bindings)?;
    finalize_prepared_tools(compilation, prepared)
}

fn finalize_prepared_tools(
    compilation: Compilation,
    prepared: &mut PreparedTools,
) -> Result<Compilation, Vec<Diagnostic>> {
    let generated_count = prepared
        .bindings
        .iter()
        .map(|binding| binding.enum_types.len())
        .sum::<usize>();
    let source_count = compilation
        .module
        .enum_types
        .len()
        .checked_sub(generated_count)
        .ok_or_else(|| {
            vec![Diagnostic::new(
                "E3005",
                "generated tool enum table is inconsistent",
                Span { start: 0, end: 0 },
            )]
        })?;
    let mut schemas = prepared.schemas.clone();
    let mut generated_before = 0usize;
    for (binding, contract) in prepared.bindings.iter().zip(&prepared.contracts) {
        let base = u32::try_from(source_count + generated_before).map_err(|_| {
            vec![Diagnostic::new(
                "E3005",
                "too many nominal enum types",
                Span { start: 0, end: 0 },
            )]
        })?;
        for schema in [
            contract.input_schema,
            contract.output_schema,
            contract.error_schema,
        ] {
            let schema = schemas.get_mut(schema as usize).ok_or_else(|| {
                vec![Diagnostic::new(
                    "E3005",
                    "prepared tool schema index is invalid",
                    Span { start: 0, end: 0 },
                )]
            })?;
            rebase_tool_type(&mut schema.value_type, base, binding.enum_types.len())
                .map_err(|diagnostic| vec![diagnostic])?;
        }
        generated_before += binding.enum_types.len();
    }
    let mut unique = Vec::new();
    let mut remap = Vec::with_capacity(schemas.len());
    for schema in schemas {
        let index = if let Some(index) = unique.iter().position(|existing| existing == &schema) {
            index
        } else {
            let index = unique.len();
            unique.push(schema);
            index
        };
        remap.push(u32::try_from(index).map_err(|_| {
            vec![Diagnostic::new(
                "E3005",
                "too many tool schemas",
                Span { start: 0, end: 0 },
            )]
        })?);
    }
    let mut contracts = prepared.contracts.clone();
    for contract in &mut contracts {
        contract.input_schema = remap[contract.input_schema as usize];
        contract.output_schema = remap[contract.output_schema as usize];
        contract.error_schema = remap[contract.error_schema as usize];
    }
    prepared.schemas = unique;
    prepared.contracts = contracts;
    Ok(compilation)
}

#[allow(clippy::too_many_lines)]
fn compile_bundle_with_import_targets(
    root: &str,
    sources: &BTreeMap<String, String>,
    import_targets: &BTreeMap<(String, String), String>,
    entry_modules: &[String],
    entry_points: &[PackageEntryPoint],
    tool_bindings: &[CompilerToolBinding],
) -> Result<Compilation, Vec<Diagnostic>> {
    compile_bundle_with_import_targets_and_prepared(
        root,
        sources,
        BundleCompileContext {
            import_targets,
            entry_modules,
            entry_points,
            tool_bindings,
            prepared: BTreeMap::new(),
        },
    )
}

#[allow(clippy::too_many_lines)]
fn compile_bundle_with_import_targets_and_prepared(
    root: &str,
    sources: &BTreeMap<String, String>,
    context: BundleCompileContext<'_>,
) -> Result<Compilation, Vec<Diagnostic>> {
    let BundleCompileContext {
        import_targets,
        entry_modules,
        entry_points,
        tool_bindings,
        prepared,
    } = context;
    let root = normalize_root(root).map_err(|diagnostic| vec![diagnostic])?;
    if !sources.contains_key(&root) {
        return Err(vec![Diagnostic::new(
            "E3003",
            format!("root module '{root}' is not in the source bundle"),
            Span { start: 0, end: 0 },
        )]);
    }
    let mut roots = vec![root.clone()];
    for entry_module in entry_modules {
        let entry_module = normalize_root(entry_module).map_err(|diagnostic| vec![diagnostic])?;
        roots.push(entry_module);
    }
    for entry_point in entry_points {
        let entry_module =
            normalize_root(&entry_point.module).map_err(|diagnostic| vec![diagnostic])?;
        roots.push(entry_module);
    }
    let modules = load_modules(&roots, sources, import_targets, prepared)
        .map_err(|diagnostic| vec![diagnostic])?;
    compile_bundle_from_modules(&root, modules, entry_points, tool_bindings)
}

fn compile_package_bundle_with_prepared_root(
    bundle: &PackageSourceBundle,
    source: String,
    module: LoweredModule,
    tool_bindings: &[CompilerToolBinding],
) -> Result<Compilation, Vec<Diagnostic>> {
    let root = normalize_root(&bundle.root).map_err(|diagnostic| vec![diagnostic])?;
    compile_bundle_with_import_targets_and_prepared(
        &bundle.root,
        &bundle.sources,
        BundleCompileContext {
            import_targets: &bundle.import_targets,
            entry_modules: &bundle.entry_modules,
            entry_points: &bundle.entry_points,
            tool_bindings,
            prepared: BTreeMap::from([(root, PreparedModule { source, module })]),
        },
    )
}

#[allow(clippy::too_many_lines)]
fn compile_bundle_from_modules(
    root: &str,
    modules: BTreeMap<String, LoweredModule>,
    entry_points: &[PackageEntryPoint],
    tool_bindings: &[CompilerToolBinding],
) -> Result<Compilation, Vec<Diagnostic>> {
    let mut bundle =
        resolve_bundle(modules, tool_bindings).map_err(|diagnostic| vec![diagnostic])?;
    let (entry_module, entry_name, package_entry) = if let Some(entry) = entry_points.first() {
        (
            normalize_root(&entry.module).map_err(|diagnostic| vec![diagnostic])?,
            entry.function.clone(),
            true,
        )
    } else {
        (root.to_owned(), "main".to_owned(), false)
    };
    let entry_symbol = bundle
        .names
        .get(&(entry_module.clone(), entry_name.clone()))
        .copied()
        .ok_or_else(|| {
            vec![Diagnostic::new(
                "E3005",
                if package_entry {
                    format!("package entry '{entry_module}::{entry_name}' is not declared")
                } else {
                    "root module must export main".to_owned()
                },
                Span { start: 0, end: 0 },
            )]
        })?;
    let entry_info = &bundle.functions[entry_symbol as usize];
    let valid_parameter_count = if package_entry {
        entry_info.lowered.parameters.len() <= 1
    } else {
        entry_info.lowered.parameters.is_empty()
    };
    if !entry_info.lowered.exported || !valid_parameter_count || entry_info.bytecode.is_none() {
        return Err(vec![Diagnostic::new(
            "E3005",
            if package_entry {
                "package entry must be an exported, nongeneric, zero- or one-argument function"
                    .to_owned()
            } else {
                "entry main must be an exported, nongeneric, zero-argument function".to_owned()
            },
            entry_info.lowered.name_span,
        )]);
    }
    if matches!(&entry_info.return_type, SemanticType::Value(value) if contains_affine(value)) {
        return Err(vec![Diagnostic::new(
            "E3011",
            if package_entry {
                "package entry cannot return Future or Task"
            } else {
                "entry main cannot return Future or Task"
            },
            entry_info.lowered.return_type.span(),
        )]);
    }
    if matches!(&entry_info.return_type, SemanticType::Value(value) if contains_workspace(value)) {
        return Err(vec![Diagnostic::new(
            "E3011",
            if package_entry {
                "package entry cannot return Workspace"
            } else {
                "entry main cannot return Workspace"
            },
            entry_info.lowered.return_type.span(),
        )]);
    }
    if matches!(&entry_info.return_type, SemanticType::Value(value) if contains_stored_sub_agent(value))
    {
        return Err(vec![Diagnostic::new(
            "E3011",
            "entry cannot return SubAgent",
            entry_info.lowered.return_type.span(),
        )]);
    }
    if package_entry
        && entry_info.parameters.iter().any(|parameter| {
            matches!(parameter, SemanticType::Value(value) if contains_workspace(value))
        })
    {
        return Err(vec![Diagnostic::new(
            "E3011",
            "package entry cannot accept Workspace",
            entry_info.lowered.name_span,
        )]);
    }
    if package_entry
        && entry_info.parameters.iter().any(|parameter| {
            matches!(parameter, SemanticType::Value(value) if contains_stored_sub_agent(value))
        })
    {
        return Err(vec![Diagnostic::new(
            "E3011",
            "package entry cannot accept SubAgent",
            entry_info.lowered.name_span,
        )]);
    }
    let entry = entry_info.bytecode.expect("checked entry bytecode");
    let effect_sets = collect_effect_sets(&bundle);
    resolve_deferred_effect_sets(&mut bundle, &effect_sets);
    let mut exported_functions = Vec::new();
    for function in bundle
        .functions
        .iter()
        .filter(|function| function.lowered.exported && function.bytecode.is_some())
    {
        let parameter_types = function
            .parameters
            .iter()
            .map(|parameter| concrete_type(parameter, &BTreeMap::new(), &effect_sets))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|diagnostic| vec![diagnostic])?;
        let return_type = concrete_type(&function.return_type, &BTreeMap::new(), &effect_sets)
            .map_err(|diagnostic| vec![diagnostic])?;
        exported_functions.push(ExportedFunction {
            function_id: function.bytecode.expect("filtered bytecode function"),
            module: function.module.clone(),
            function: function.lowered.name.clone(),
            parameter_types,
            return_type,
            parameter_spellings: function
                .lowered
                .parameters
                .iter()
                .map(|(_, value_type, _)| lowered_type_spelling(value_type))
                .collect(),
            return_spelling: lowered_type_spelling(&function.lowered.return_type),
            effects: function.effects.clone(),
        });
    }
    exported_functions.sort_by(|left, right| {
        (&left.module, &left.function).cmp(&(&right.module, &right.function))
    });
    let function_count = bundle
        .functions
        .iter()
        .filter(|function| function.bytecode.is_some())
        .count();
    let mut lowering = GlobalLowering {
        bundle: &bundle,
        effect_sets,
        constants: Vec::new(),
        functions: vec![None; function_count],
        monomorphs: Vec::new(),
        hir_modules: BTreeMap::new(),
        mir_functions: Vec::new(),
        types: Vec::new(),
        spans: Vec::new(),
        debug_sources: bundle.modules.keys().cloned().collect(),
        debug_locations: Vec::new(),
        next_symbol: u32::try_from(bundle.functions.len()).map_err(|_| {
            vec![Diagnostic::new(
                "E3005",
                "too many declared symbols",
                Span { start: 0, end: 0 },
            )]
        })?,
        async_functions: BTreeSet::new(),
    };
    for info in bundle
        .functions
        .iter()
        .filter(|function| function.bytecode.is_some())
    {
        let function_id = info.bytecode.expect("filtered bytecode function");
        let (function, hir, mir) = lower_one_function(
            &mut lowering,
            info.clone(),
            function_id,
            Vec::new(),
            &BTreeMap::new(),
        )
        .map_err(|diagnostic| vec![diagnostic.with_source(&info.module)])?;
        lowering.functions[function_id as usize] = Some(function);
        lowering
            .hir_modules
            .entry(info.module.clone())
            .or_default()
            .push(hir);
        lowering.mir_functions.push(mir);
    }
    let effect_report = bundle
        .functions
        .iter()
        .filter(|function| !function.lowered.exported)
        .map(|function| EffectReportEntry {
            module: function.module.clone(),
            function: function.lowered.name.clone(),
            effects: function.effects.clone(),
        })
        .collect();
    let hir = HirBundle {
        types: lowering.types,
        spans: lowering.spans,
        modules: lowering
            .hir_modules
            .into_iter()
            .map(|(path, mut functions)| {
                functions.sort_by(|left, right| left.name.cmp(&right.name));
                HirModule { path, functions }
            })
            .collect(),
    };
    lowering
        .mir_functions
        .sort_by(|left, right| left.name.cmp(&right.name));
    lowering.debug_locations.sort_by_key(|location| {
        (
            location.function,
            location.instruction,
            location.source,
            location.start,
            location.end,
        )
    });
    let module = Module {
        constants: lowering.constants,
        enum_types: bundle.enum_types.clone(),
        effect_sets: lowering.effect_sets,
        functions: lowering
            .functions
            .into_iter()
            .map(|function| function.expect("every allocated function is lowered"))
            .collect(),
        async_functions: lowering.async_functions.into_iter().collect(),
        entry,
    };
    Ok(Compilation {
        module,
        debug: DebugInfo {
            sources: lowering.debug_sources,
            locations: lowering.debug_locations,
        },
        hir,
        mir: MirBundle {
            functions: lowering.mir_functions,
        },
        effect_report,
        exported_functions,
    })
}

/// Compile one canonical version 0.1 source as `main.allen`.
///
/// # Errors
///
/// Returns the same diagnostics as [`compile_bundle`].
pub fn compile_source(source: &str) -> Result<Compilation, Vec<Diagnostic>> {
    compile_bundle(
        "main.allen",
        &BTreeMap::from([("main.allen".to_owned(), source.to_owned())]),
    )
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn normalize_root(path: &str) -> Result<String, Diagnostic> {
    let valid_package_identity = path.strip_prefix("pkg://").is_some_and(|suffix| {
        let components = suffix.split('/').collect::<Vec<_>>();
        components.len() >= 3
            && components[0].contains('@')
            && components[1] == "src"
            && components
                .iter()
                .all(|part| !part.is_empty() && *part != "." && *part != "..")
    });
    let valid_source_path = !path.is_empty()
        && !path.starts_with('/')
        && path.ends_with(".allen")
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if !(valid_package_identity || valid_source_path) || !path.ends_with(".allen") {
        return Err(Diagnostic::new(
            "E3003",
            format!("invalid module path '{path}'"),
            Span { start: 0, end: 0 },
        ));
    }
    Ok(path.to_owned())
}

#[cfg(test)]
#[rustfmt::skip]
mod tests;
