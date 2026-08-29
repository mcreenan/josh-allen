//! Canonical version 0.1 compiler frontend.

use super::{Diagnostic, Span};
use allen_bytecode::{
    BoolBinaryOp, CapabilityOperation, CheckedIntOperation, CollectionOperation, CompareOp,
    Constant, Conversion, DebugInfo, DebugLocation, EffectOperation, EffectSetId,
    EntryValidatorPathSegment, EntryValidatorSite, EnumPayloadType, EnumType, EnumVariant,
    ExternalFsAccess, Function, FunctionId, Instruction, ListCombinator, MAX_VALUE_NESTING, Module,
    NumericBinaryOp, RecordField, RecordInvariantDefinition, Register, SafeCollectionOperation,
    StandardOperation, StringOperation, ValidatorExpr, ValueType, agent_error_type,
    external_directory_request_type, external_file_request_type, file_error_type,
    format_error_type, http_response_type, is_strict_schema_type, model_error_type,
    network_error_type, parse_error_type, permission_error_type, prompt_output_type, prompt_type,
    search_match_type, sub_agent_error_type, task_snapshot_type, time_error_type,
    transcript_message_type, transcript_part_enum_type, transcript_query_type,
    transcript_snapshot_type, user_error_type,
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
    BundleCompileContext, PreparedModule, ResolvedBundle, collect_effect_sets, is_canonical_effect,
    load_modules, lowered_type_spelling, rebase_tool_type, resolve_bundle,
    resolve_deferred_effect_sets,
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
    constants: Vec<LoweredConstDeclaration>,
    functions: Vec<LoweredFunction>,
    tests: Vec<LoweredTest>,
}

#[derive(Clone, Debug)]
struct LoweredTest {
    name: String,
    name_span: Span,
    offset: usize,
    declared_effects: Vec<String>,
    effects_span: Option<Span>,
    body: LoweredBody,
}

#[derive(Clone, Debug)]
struct LoweredConstDeclaration {
    exported: bool,
    name: String,
    name_span: Span,
    value_type: LoweredType,
    value: LoweredExpr,
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

/// One deterministically discovered source test.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceTest {
    pub module: String,
    pub name: String,
    pub offset: usize,
    pub effects: Vec<String>,
}

/// The checked and lowered artifact input for one isolated source test.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledSourceTest {
    pub test: SourceTest,
    pub compilation: Compilation,
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
        invariant: Option<LoweredExpr>,
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
    Newtype {
        exported: bool,
        name: String,
        name_span: Span,
        underlying: LoweredType,
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
            Self::Record { name, .. }
            | Self::Enum { name, .. }
            | Self::Alias { name, .. }
            | Self::Newtype { name, .. } => name,
        }
    }

    fn exported(&self) -> bool {
        match self {
            Self::Record { exported, .. }
            | Self::Enum { exported, .. }
            | Self::Alias { exported, .. }
            | Self::Newtype { exported, .. } => *exported,
        }
    }

    fn name_span(&self) -> Span {
        match self {
            Self::Record { name_span, .. }
            | Self::Enum { name_span, .. }
            | Self::Alias { name_span, .. }
            | Self::Newtype { name_span, .. } => *name_span,
        }
    }
}

#[derive(Clone, Debug)]
struct LoweredImport {
    extension: bool,
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
    parameter_defaults: Vec<Option<LoweredParameterDefault>>,
    return_type: LoweredType,
    declared_effects: Option<Vec<String>>,
    effects_span: Option<Span>,
    body: LoweredBody,
}

/// A declaration-owned default retained in both checked and canonical source
/// form. Later semantic and artifact phases decide how to validate, compile,
/// and digest it.
#[derive(Clone, Debug)]
struct LoweredParameterDefault {
    value: LoweredExpr,
    source_text: String,
    span: Span,
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
    Range(Box<Self>, Span),
    Sequence(Box<Self>, Span),
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
            | Self::Range(_, span)
            | Self::Sequence(_, span)
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
    LocalFunction(LoweredLocalFunction),
    Break(Span),
    Continue(Span),
}

#[derive(Clone, Debug)]
struct LoweredLocalFunction {
    name: String,
    name_span: Span,
    parameters: Vec<(String, LoweredType, Span)>,
    parameter_defaults: Vec<Option<LoweredParameterDefault>>,
    return_type: LoweredType,
    declared_effects: Option<Vec<String>>,
    effects_span: Option<Span>,
    body: LoweredBody,
    span: Span,
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
    ListWithSpread(Vec<LoweredListItem>),
    Map(Vec<(LoweredExpr, LoweredExpr)>),
    MapWithSpread(Vec<LoweredMapItem>),
    Record {
        name: String,
        fields: Vec<(String, LoweredExpr, Span)>,
    },
    RecordUpdate {
        name: String,
        base: Box<LoweredExpr>,
        spread_span: Span,
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
    OptionalFieldGet {
        receiver: Box<LoweredExpr>,
        field: String,
        operator_span: Span,
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
    Compose {
        left: Box<LoweredExpr>,
        right: Box<LoweredExpr>,
        operator_span: Span,
    },
    Pipe {
        left: Box<LoweredExpr>,
        stage: Box<LoweredExpr>,
        operator_span: Span,
    },
    Range {
        start: Box<LoweredExpr>,
        end: Box<LoweredExpr>,
        inclusive: bool,
        operator_span: Span,
    },
    Index {
        collection: Box<LoweredExpr>,
        index: Box<LoweredExpr>,
    },
    Slice {
        collection: Box<LoweredExpr>,
        range: Box<LoweredExpr>,
        bracket_span: Span,
    },
    Call {
        callee: Box<LoweredExpr>,
        type_arguments: Vec<LoweredType>,
        arguments: Vec<LoweredCallArgument>,
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
    ShortClosure {
        parameters: Vec<(String, Span)>,
        body: Box<LoweredExpr>,
    },
}

/// A source-order call input retained until direct-call resolution validates
/// labels and reorders the resulting values to declaration order.
#[derive(Clone, Debug)]
struct LoweredCallArgument {
    label: Option<(String, Span)>,
    value: LoweredExpr,
    placeholder: bool,
    trailing: bool,
    preceding_call_span: Option<Span>,
    span: Span,
}

#[derive(Clone, Debug)]
struct LoweredListItem {
    spread: bool,
    value: LoweredExpr,
    span: Span,
}

#[derive(Clone, Debug)]
enum LoweredMapItem {
    Entry {
        key: LoweredExpr,
        value: LoweredExpr,
        span: Span,
    },
    Spread {
        value: LoweredExpr,
        span: Span,
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
    Binding {
        name: String,
        span: Span,
    },
    Wildcard,
    Bool(bool),
    Record {
        name: String,
        fields: Vec<(String, Span, Box<LoweredPattern>)>,
    },
    Enum {
        name: String,
        variant: String,
        patterns: Vec<LoweredPattern>,
        fields: Option<Vec<(String, Span, Box<LoweredPattern>)>>,
    },
    Option {
        some: bool,
        payload: Option<Box<LoweredPattern>>,
    },
    Result {
        ok: bool,
        payload: Box<LoweredPattern>,
    },
    Range {
        start: LoweredExpr,
        end: LoweredExpr,
        inclusive: bool,
        operator_span: Span,
    },
    Or {
        alternatives: Vec<LoweredPattern>,
        operator_spans: Vec<Span>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SourceWordContext {
    Identifier,
    MemberName,
}

fn is_forbidden_source_word(word: &str, context: SourceWordContext) -> bool {
    matches!(word, "undefined" | "null")
        || (word == "any" && context != SourceWordContext::MemberName)
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
    compile_bundle_with_import_targets(root, sources, &BTreeMap::new(), &[], &[], &[], &[])
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
            template_bindings: &[],
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
        &[],
    )
}

/// Discover every test declaration in a complete loose or package source bundle.
///
/// Discovery parses every supplied module, including modules not reachable from
/// a production entry, and returns canonical module/offset order.
///
/// # Errors
///
/// Returns diagnostics when any source module in the bundle fails to parse,
/// lower, resolve, or type-check for source-test discovery.
pub fn discover_source_tests(
    bundle: &PackageSourceBundle,
) -> Result<Vec<SourceTest>, Vec<Diagnostic>> {
    let modules = load_test_modules(bundle)?;
    Ok(source_tests(&modules))
}

/// Return the exact module import closure rooted at one selected source file.
///
/// This uses the ordinary loader and import map, so package test assembly can
/// discard unrelated dependency packages before preparing artifact metadata.
///
/// # Errors
///
/// Returns diagnostics when the requested module path is invalid or its import
/// closure cannot be loaded from the bundle.
pub fn reachable_source_modules(
    bundle: &PackageSourceBundle,
    module: &str,
) -> Result<Vec<String>, Vec<Diagnostic>> {
    let normalized = normalize_root(module).map_err(|diagnostic| vec![diagnostic])?;
    let modules = load_modules(
        std::slice::from_ref(&normalized),
        &bundle.sources,
        &bundle.import_targets,
        BTreeMap::new(),
    )
    .map_err(|diagnostic| vec![diagnostic])?;
    Ok(modules.into_keys().collect())
}

/// Compile exactly one selected source test as a private zero-argument `Void`
/// entry while reusing the ordinary resolver, checker, and bytecode lowerer.
///
/// # Errors
///
/// Returns diagnostics when the selected test is missing or the test module
/// fails the ordinary source compilation pipeline.
pub fn compile_source_test(
    bundle: &PackageSourceBundle,
    module: &str,
    name: &str,
) -> Result<CompiledSourceTest, Vec<Diagnostic>> {
    compile_source_test_with_bindings(bundle, module, name, &[], &[])
}

/// Compile one selected source test against the package's prepared tool and
/// template tables, then finalize the tool schemas exactly like production
/// package compilation.
///
/// # Errors
///
/// Returns diagnostics when the selected test cannot compile or prepared tool
/// finalization fails.
pub fn compile_source_test_with_prepared_tools_and_templates(
    bundle: &PackageSourceBundle,
    module: &str,
    name: &str,
    prepared: &mut PreparedTools,
    templates: &[CompilerTemplateBinding],
) -> Result<CompiledSourceTest, Vec<Diagnostic>> {
    let compiled =
        compile_source_test_with_bindings(bundle, module, name, &prepared.bindings, templates)?;
    let test = compiled.test;
    let compilation = finalize_prepared_tools(compiled.compilation, prepared)?;
    Ok(CompiledSourceTest { test, compilation })
}

fn compile_source_test_with_bindings(
    bundle: &PackageSourceBundle,
    module: &str,
    name: &str,
    tools: &[CompilerToolBinding],
    templates: &[CompilerTemplateBinding],
) -> Result<CompiledSourceTest, Vec<Diagnostic>> {
    let normalized = normalize_root(module).map_err(|diagnostic| vec![diagnostic])?;
    let mut modules = load_modules(
        std::slice::from_ref(&normalized),
        &bundle.sources,
        &bundle.import_targets,
        BTreeMap::new(),
    )
    .map_err(|diagnostic| vec![diagnostic])?;
    let selected = modules
        .get(&normalized)
        .and_then(|definition| definition.tests.iter().find(|test| test.name == name))
        .cloned()
        .ok_or_else(|| {
            vec![Diagnostic::new(
                "E3005",
                format!("source test {normalized}::{name:?} was not found"),
                Span { start: 0, end: 0 },
            )]
        })?;
    let test = SourceTest {
        module: normalized.clone(),
        name: selected.name.clone(),
        offset: selected.offset,
        effects: selected.declared_effects.clone(),
    };
    // `test` is reserved by the lexer and therefore cannot collide with a
    // user-declared function, while the module identity keeps it canonical.
    let synthetic_name = "test".to_owned();
    let definition = modules
        .get_mut(&normalized)
        .expect("selected module exists");
    definition.functions.push(LoweredFunction {
        exported: true,
        is_async: !selected.declared_effects.is_empty(),
        name: synthetic_name.clone(),
        name_span: selected.name_span,
        generics: Vec::new(),
        parameters: Vec::new(),
        parameter_defaults: Vec::new(),
        return_type: LoweredType::Named("Void".to_owned(), selected.name_span),
        declared_effects: Some(selected.declared_effects),
        effects_span: selected.effects_span,
        body: selected.body,
    });
    let mut compilation = compile_bundle_from_modules(
        &bundle.root,
        modules,
        &[PackageEntryPoint {
            module: normalized,
            function: synthetic_name,
        }],
        tools,
        templates,
    )?;
    // The exported bit is an internal entry-validation mechanism. Test
    // assembly consumes this boundary without exposing it to production APIs.
    compilation.effect_report.clear();
    Ok(CompiledSourceTest { test, compilation })
}

fn load_test_modules(
    bundle: &PackageSourceBundle,
) -> Result<BTreeMap<String, LoweredModule>, Vec<Diagnostic>> {
    let mut roots = Vec::with_capacity(bundle.sources.len());
    for path in bundle.sources.keys() {
        roots.push(normalize_root(path).map_err(|diagnostic| vec![diagnostic])?);
    }
    roots.sort();
    roots.dedup();
    load_modules(
        &roots,
        &bundle.sources,
        &bundle.import_targets,
        BTreeMap::new(),
    )
    .map_err(|diagnostic| vec![diagnostic])
}

fn source_tests(modules: &BTreeMap<String, LoweredModule>) -> Vec<SourceTest> {
    let mut tests = modules
        .iter()
        .flat_map(|(module, definition)| {
            definition.tests.iter().map(move |test| SourceTest {
                module: module.clone(),
                name: test.name.clone(),
                offset: test.offset,
                effects: test.declared_effects.clone(),
            })
        })
        .collect::<Vec<_>>();
    tests.sort_by(|left, right| {
        (&left.module, left.offset, &left.name).cmp(&(&right.module, right.offset, &right.name))
    });
    tests
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
    let compilation =
        compile_package_bundle_with_tools_and_templates(bundle, &prepared.bindings, &[])?;
    finalize_prepared_tools(compilation, prepared)
}

/// Compile a package against prepared tools and package-local template signatures.
///
/// # Errors
///
/// Returns deterministic diagnostics for invalid calls or bindings.
pub fn compile_package_bundle_with_prepared_tools_and_templates(
    bundle: &PackageSourceBundle,
    prepared: &mut PreparedTools,
    templates: &[CompilerTemplateBinding],
) -> Result<Compilation, Vec<Diagnostic>> {
    let compilation =
        compile_package_bundle_with_tools_and_templates(bundle, &prepared.bindings, templates)?;
    finalize_prepared_tools(compilation, prepared)
}

fn compile_package_bundle_with_tools_and_templates(
    bundle: &PackageSourceBundle,
    tools: &[CompilerToolBinding],
    templates: &[CompilerTemplateBinding],
) -> Result<Compilation, Vec<Diagnostic>> {
    compile_bundle_with_import_targets(
        &bundle.root,
        &bundle.sources,
        &bundle.import_targets,
        &bundle.entry_modules,
        &bundle.entry_points,
        tools,
        templates,
    )
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
    template_bindings: &[CompilerTemplateBinding],
) -> Result<Compilation, Vec<Diagnostic>> {
    compile_bundle_with_import_targets_and_prepared(
        root,
        sources,
        BundleCompileContext {
            import_targets,
            entry_modules,
            entry_points,
            tool_bindings,
            template_bindings,
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
        template_bindings,
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
    compile_bundle_from_modules(
        &root,
        modules,
        entry_points,
        tool_bindings,
        template_bindings,
    )
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
            template_bindings: &[],
            prepared: BTreeMap::from([(root, PreparedModule { source, module })]),
        },
    )
}

#[allow(clippy::too_many_lines)]
fn evaluate_constants(
    bundle: &ResolvedBundle,
    effect_sets: &[Vec<String>],
) -> Result<BTreeMap<u32, allen_vm::Value>, Diagnostic> {
    let constant_symbols = bundle
        .functions
        .iter()
        .filter(|function| function.is_const)
        .map(|function| function.symbol)
        .collect::<BTreeSet<_>>();
    if constant_symbols.is_empty() {
        return Ok(BTreeMap::new());
    }
    let function_count = bundle
        .functions
        .iter()
        .filter(|function| function.bytecode.is_some())
        .count();
    let mut lowering = GlobalLowering {
        bundle,
        effect_sets: effect_sets.to_vec(),
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
            Diagnostic::new(
                "E3005",
                "too many declared symbols",
                Span { start: 0, end: 0 },
            )
        })?,
        async_functions: BTreeSet::new(),
        constant_values: BTreeMap::new(),
        constant_evaluation: true,
    };
    for info in bundle
        .functions
        .iter()
        .filter(|function| function.bytecode.is_some())
    {
        let function_id = info.bytecode.expect("filtered bytecode function");
        let (function, _, _) = lower_one_function(
            &mut lowering,
            info.clone(),
            function_id,
            Vec::new(),
            BTreeMap::new(),
            BTreeSet::new(),
            &BTreeMap::new(),
        )
        .map_err(|diagnostic| diagnostic.with_source(&info.module))?;
        lowering.functions[function_id as usize] = Some(function);
    }
    let mut dependencies = BTreeMap::<u32, BTreeSet<u32>>::new();
    let bytecode_to_symbol = bundle
        .functions
        .iter()
        .filter_map(|function| function.bytecode.map(|id| (id, function.symbol)))
        .collect::<BTreeMap<_, _>>();
    for info in bundle.functions.iter().filter(|function| function.is_const) {
        let expression = info
            .lowered
            .body
            .tail
            .as_ref()
            .expect("constant has an initializer expression");
        if !is_constant_expression(expression) {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "constant '{}' uses a non-constant expression",
                    info.lowered.name
                ),
                expression.span,
            )
            .with_source(&info.module));
        }
        let value_type = concrete_type(&info.return_type, &BTreeMap::new(), effect_sets)
            .map_err(|diagnostic| diagnostic.with_source(&info.module))?;
        if matches!(
            value_type,
            ValueType::Never
                | ValueType::Unknown
                | ValueType::Function { .. }
                | ValueType::Future(_)
                | ValueType::Task(_)
                | ValueType::Workspace
                | ValueType::SubAgent
        ) || contains_affine(&value_type)
            || contains_workspace(&value_type)
            || contains_stored_sub_agent(&value_type)
        {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "constant '{}' requires a complete non-affine value type",
                    info.lowered.name
                ),
                info.lowered.return_type.span(),
            )
            .with_source(&info.module));
        }
        if !info.effects.is_empty() {
            return Err(Diagnostic::new(
                "E3011",
                format!("constant '{}' must be pure", info.lowered.name),
                info.lowered.name_span,
            )
            .with_source(&info.module));
        }
        let function = lowering.functions[info.bytecode.expect("constant bytecode") as usize]
            .as_ref()
            .expect("constant function lowered");
        let mut direct = BTreeSet::new();
        for instruction in &function.code {
            match instruction {
                Instruction::DirectCall { function, .. } => {
                    let symbol = bytecode_to_symbol[function];
                    if !constant_symbols.contains(&symbol) {
                        return Err(Diagnostic::new(
                            "E3011",
                            format!(
                                "constant '{}' cannot call runtime functions",
                                info.lowered.name
                            ),
                            info.lowered.name_span,
                        )
                        .with_source(&info.module));
                    }
                    direct.insert(symbol);
                }
                Instruction::ClosureNew { .. }
                | Instruction::ClosureCall { .. }
                | Instruction::AsyncCall { .. }
                | Instruction::Spawn { .. }
                | Instruction::Await { .. }
                | Instruction::TaskSnapshot { .. }
                | Instruction::WorkspaceGet { .. }
                | Instruction::EffectCall { .. }
                | Instruction::CapabilityInspect { .. }
                | Instruction::ListFold { .. }
                | Instruction::ToolInvoke { .. }
                | Instruction::TaskScopeEnter { .. }
                | Instruction::TaskScopeExit { .. }
                | Instruction::Stop { .. }
                | Instruction::Fail { .. } => {
                    return Err(Diagnostic::new(
                        "E3011",
                        format!(
                            "constant '{}' uses a runtime-only expression",
                            info.lowered.name
                        ),
                        info.lowered.name_span,
                    )
                    .with_source(&info.module));
                }
                _ => {}
            }
        }
        dependencies.insert(info.symbol, direct);
    }
    let mut ready = dependencies
        .iter()
        .filter_map(|(symbol, dependencies)| dependencies.is_empty().then_some(*symbol))
        .collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(dependencies.len());
    while let Some(symbol) = ready.pop_first() {
        order.push(symbol);
        for (candidate, required) in &mut dependencies {
            if required.remove(&symbol) && required.is_empty() && !order.contains(candidate) {
                ready.insert(*candidate);
            }
        }
    }
    if order.len() != dependencies.len() {
        let mut trail = Vec::new();
        let mut positions = BTreeMap::new();
        let mut current = *dependencies
            .iter()
            .find(|(_, required)| !required.is_empty())
            .map(|(symbol, _)| symbol)
            .expect("cycle has a member");
        let cycle_symbols = loop {
            if let Some(position) = positions.get(&current).copied() {
                break trail[position..].to_vec();
            }
            positions.insert(current, trail.len());
            trail.push(current);
            current = *dependencies[&current]
                .iter()
                .find(|dependency| !dependencies[*dependency].is_empty())
                .expect("unresolved dependency graph reaches a cycle");
        };
        let mut cycle = cycle_symbols
            .iter()
            .map(|symbol| {
                let info = &bundle.functions[*symbol as usize];
                format!("{}::{}", info.module, info.lowered.name)
            })
            .collect::<Vec<_>>();
        cycle.sort();
        let first_symbol = *cycle_symbols.iter().min().expect("cycle has a member");
        let first = &bundle.functions[first_symbol as usize];
        return Err(Diagnostic::new(
            "E3012",
            format!("constant dependency cycle: {}", cycle.join(", ")),
            first.lowered.name_span,
        )
        .with_source(&first.module));
    }
    let raw_module = Module {
        constants: lowering.constants,
        enum_types: bundle.enum_types.clone(),
        effect_sets: effect_sets.to_vec(),
        functions: lowering
            .functions
            .into_iter()
            .map(|function| function.expect("every preliminary function is lowered"))
            .collect(),
        async_functions: lowering.async_functions.into_iter().collect(),
        entry: 0,
    };
    let limits = allen_vm::ExecutionLimits {
        instructions: 100_000,
        allocation_bytes: 1_048_576,
        maximum_allocation_bytes: 1_048_576,
        call_depth: 128,
        wall_time: std::time::Duration::MAX,
        tasks: 0,
        concurrent_effects: 0,
        cleanup_instructions: 0,
    };
    let mut values = BTreeMap::new();
    for symbol in order {
        let info = &bundle.functions[symbol as usize];
        let mut module = raw_module.clone();
        module.entry = info.bytecode.expect("constant bytecode");
        let verified = allen_bytecode::verify(module).map_err(|error| {
            Diagnostic::new(
                "E3005",
                format!(
                    "constant '{}' produced invalid bytecode: {error}",
                    info.lowered.name
                ),
                info.lowered.name_span,
            )
            .with_source(&info.module)
        })?;
        let result = allen_vm::execute_with_limits(&verified, limits).map_err(|error| {
            Diagnostic::new(
                "E3011",
                format!(
                    "constant '{}' evaluation failed: {}",
                    info.lowered.name,
                    error.code()
                ),
                info.lowered.name_span,
            )
            .with_source(&info.module)
        })?;
        values.insert(symbol, result.value);
    }
    Ok(values)
}

fn is_constant_expression(expression: &LoweredExpr) -> bool {
    match &expression.kind {
        LoweredExprKind::Unit
        | LoweredExprKind::Int(_)
        | LoweredExprKind::Float(_)
        | LoweredExprKind::Bool(_)
        | LoweredExprKind::String(_)
        | LoweredExprKind::Bytes(_)
        | LoweredExprKind::Variable(_) => true,
        LoweredExprKind::Template(parts) => {
            template_interpolations(parts).all(is_constant_expression)
        }
        LoweredExprKind::List(values) | LoweredExprKind::Tuple(values) => {
            values.iter().all(is_constant_expression)
        }
        LoweredExprKind::ListWithSpread(items) => {
            items.iter().all(|item| is_constant_expression(&item.value))
        }
        LoweredExprKind::Map(entries) => entries
            .iter()
            .all(|(key, value)| is_constant_expression(key) && is_constant_expression(value)),
        LoweredExprKind::MapWithSpread(items) => items.iter().all(|item| match item {
            LoweredMapItem::Entry { key, value, .. } => {
                is_constant_expression(key) && is_constant_expression(value)
            }
            LoweredMapItem::Spread { value, .. } => is_constant_expression(value),
        }),
        LoweredExprKind::Record { fields, .. } => fields
            .iter()
            .all(|(_, value, _)| is_constant_expression(value)),
        LoweredExprKind::RecordUpdate { base, fields, .. } => {
            is_constant_expression(base)
                && fields
                    .iter()
                    .all(|(_, value, _)| is_constant_expression(value))
        }
        LoweredExprKind::Enum { payload, .. } => match payload {
            LoweredEnumValuePayload::Unit => true,
            LoweredEnumValuePayload::Tuple(values) => values.iter().all(is_constant_expression),
            LoweredEnumValuePayload::Record(fields) => fields
                .iter()
                .all(|(_, value, _)| is_constant_expression(value)),
        },
        LoweredExprKind::FieldGet { record, .. }
        | LoweredExprKind::Unary {
            operand: record, ..
        } => is_constant_expression(record),
        LoweredExprKind::Binary { left, right, .. } => {
            is_constant_expression(left) && is_constant_expression(right)
        }
        LoweredExprKind::Index { collection, index } => {
            is_constant_expression(collection) && is_constant_expression(index)
        }
        LoweredExprKind::Slice {
            collection, range, ..
        } => is_constant_expression(collection) && is_constant_expression(range),
        LoweredExprKind::Call {
            callee, arguments, ..
        } => {
            is_constant_expression(callee)
                && arguments
                    .iter()
                    .all(|argument| is_constant_expression(&argument.value))
        }
        LoweredExprKind::Range { .. }
        | LoweredExprKind::Prompt { .. }
        | LoweredExprKind::OptionalFieldGet { .. }
        | LoweredExprKind::Compose { .. }
        | LoweredExprKind::Pipe { .. }
        | LoweredExprKind::Try(_)
        | LoweredExprKind::Match { .. }
        | LoweredExprKind::If { .. }
        | LoweredExprKind::Spawn(_)
        | LoweredExprKind::Await(_)
        | LoweredExprKind::AwaitBlock(_)
        | LoweredExprKind::Closure { .. }
        | LoweredExprKind::ShortClosure { .. } => false,
    }
}

#[allow(clippy::too_many_lines)]
fn compile_record_invariants(
    bundle: &ResolvedBundle,
) -> Result<(Vec<RecordInvariantDefinition>, BTreeMap<String, u32>), Diagnostic> {
    let mut definitions = Vec::new();
    for (module, source) in &bundle.modules {
        for declaration in &source.types {
            let LoweredTypeDeclaration::Record {
                name,
                invariant: Some(source_predicate),
                ..
            } = declaration
            else {
                continue;
            };
            let identity = format!("{module}::{name}");
            let ValueType::Record(fields) = &bundle.types[&(module.clone(), name.clone())] else {
                unreachable!("record declaration resolves to a structural record")
            };
            let mut nodes = 0usize;
            let (predicate, value_type) =
                compile_validator_expr(source_predicate, fields, &mut nodes)
                    .map_err(|diagnostic| diagnostic.with_source(module))?;
            if value_type != ValueType::Bool {
                return Err(Diagnostic::new(
                    "E3013",
                    "record invariant predicate must have type Bool",
                    source_predicate.span,
                )
                .with_source(module));
            }
            definitions.push(RecordInvariantDefinition {
                identity,
                fields: fields.clone(),
                predicate,
            });
        }
    }
    definitions.sort_by(|left, right| left.identity.as_bytes().cmp(right.identity.as_bytes()));
    let indexes = definitions
        .iter()
        .enumerate()
        .map(|(index, definition)| {
            (
                definition.identity.clone(),
                u32::try_from(index).expect("invariant table index fits"),
            )
        })
        .collect();
    Ok((definitions, indexes))
}

#[allow(clippy::too_many_lines)]
fn compile_validator_expr(
    expression: &LoweredExpr,
    fields: &[RecordField],
    nodes: &mut usize,
) -> Result<(ValidatorExpr, ValueType), Diagnostic> {
    *nodes = nodes.saturating_add(1);
    if *nodes > 256 {
        return Err(Diagnostic::new(
            "E3013",
            "record invariant exceeds 256 AST nodes",
            expression.span,
        ));
    }
    Ok(match &expression.kind {
        LoweredExprKind::Bool(value) => (ValidatorExpr::Bool(*value), ValueType::Bool),
        LoweredExprKind::Variable(name) => {
            let (index, field) = fields
                .iter()
                .enumerate()
                .find(|(_, field)| field.name == *name)
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E3013",
                        format!("record invariant cannot reference '{name}'"),
                        expression.span,
                    )
                })?;
            if !validator_scalar_type(&field.value_type) {
                return Err(Diagnostic::new(
                    "E3013",
                    "record invariant fields must be scalar or newtype scalar values",
                    expression.span,
                ));
            }
            (
                ValidatorExpr::Field {
                    field: u32::try_from(index).expect("record field index fits"),
                    value_type: field.value_type.clone(),
                },
                field.value_type.clone(),
            )
        }
        LoweredExprKind::Unary {
            operation: Unary::Not,
            operand,
        } => {
            let (operand, value_type) = compile_validator_expr(operand, fields, nodes)?;
            if value_type != ValueType::Bool {
                return Err(Diagnostic::new(
                    "E3013",
                    "record invariant `!` requires Bool",
                    expression.span,
                ));
            }
            (ValidatorExpr::Not(Box::new(operand)), ValueType::Bool)
        }
        LoweredExprKind::Binary {
            operation,
            left,
            right,
        } => {
            let (left_expr, left_type) = compile_validator_expr(left, fields, nodes)?;
            let (right_expr, right_type) = compile_validator_expr(right, fields, nodes)?;
            match operation {
                Binary::And | Binary::Or => {
                    if left_type != ValueType::Bool || right_type != ValueType::Bool {
                        return Err(Diagnostic::new(
                            "E3013",
                            "record invariant boolean operators require Bool",
                            expression.span,
                        ));
                    }
                    (
                        ValidatorExpr::BoolBinary {
                            operation: if *operation == Binary::And {
                                BoolBinaryOp::And
                            } else {
                                BoolBinaryOp::Or
                            },
                            left: Box::new(left_expr),
                            right: Box::new(right_expr),
                        },
                        ValueType::Bool,
                    )
                }
                Binary::Equal
                | Binary::NotEqual
                | Binary::Less
                | Binary::LessEqual
                | Binary::Greater
                | Binary::GreaterEqual => {
                    let ordered = matches!(
                        operation,
                        Binary::Less | Binary::LessEqual | Binary::Greater | Binary::GreaterEqual
                    );
                    if left_type != right_type
                        || !validator_scalar_type(&left_type)
                        || ordered && !left_type.is_ordered()
                    {
                        return Err(Diagnostic::new(
                            "E3013",
                            "record invariant comparison types are invalid",
                            expression.span,
                        ));
                    }
                    let operation = match operation {
                        Binary::Equal => CompareOp::Equal,
                        Binary::NotEqual => CompareOp::NotEqual,
                        Binary::Less => CompareOp::Less,
                        Binary::LessEqual => CompareOp::LessEqual,
                        Binary::Greater => CompareOp::Greater,
                        Binary::GreaterEqual => CompareOp::GreaterEqual,
                        _ => unreachable!(),
                    };
                    (
                        ValidatorExpr::Compare {
                            operation,
                            left: Box::new(left_expr),
                            right: Box::new(right_expr),
                        },
                        ValueType::Bool,
                    )
                }
                _ => {
                    return Err(Diagnostic::new(
                        "E3013",
                        "record invariant permits only `!`, `&&`, `||`, equality, and ordering comparisons",
                        expression.span,
                    ));
                }
            }
        }
        _ => {
            return Err(Diagnostic::new(
                "E3013",
                "record invariant permits only Bool literals and direct immutable scalar field references",
                expression.span,
            ));
        }
    })
}

fn validator_scalar_type(value_type: &ValueType) -> bool {
    match value_type {
        ValueType::Bool
        | ValueType::Int
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes => true,
        ValueType::Newtype { underlying, .. } => validator_scalar_type(underlying),
        _ => false,
    }
}

fn resolved_type_declaration<'a>(
    bundle: &'a ResolvedBundle,
    module: &str,
    name: &str,
    span: Span,
) -> Result<Option<(String, &'a LoweredTypeDeclaration)>, Diagnostic> {
    if let Some(declaration) = bundle.modules[module]
        .types
        .iter()
        .find(|declaration| declaration.name() == name)
    {
        return Ok(Some((module.to_owned(), declaration)));
    }
    for import in &bundle.modules[module].imports {
        let Some((imported, _, _)) = import.names.iter().find(|(_, local, _)| local == name) else {
            continue;
        };
        let target = resolution::resolve_import_path(module, import)?;
        let declaration = bundle.modules[&target]
            .types
            .iter()
            .find(|declaration| declaration.name() == imported)
            .ok_or_else(|| {
                Diagnostic::new(
                    "E3003",
                    format!("module '{target}' does not export type '{imported}'"),
                    span,
                )
            })?;
        return Ok(Some((target, declaration)));
    }
    Ok(None)
}

#[allow(clippy::too_many_lines)]
fn collect_entry_validator_sites(
    value_type: &LoweredType,
    module: &str,
    bundle: &ResolvedBundle,
    indexes: &BTreeMap<String, u32>,
    path: &mut Vec<EntryValidatorPathSegment>,
    sites: &mut Vec<EntryValidatorSite>,
) -> Result<(), Diagnostic> {
    match value_type {
        LoweredType::Named(name, span) => {
            let Some((definition_module, declaration)) =
                resolved_type_declaration(bundle, module, name, *span)?
            else {
                return Ok(());
            };
            match declaration {
                LoweredTypeDeclaration::Alias { target, .. } => {
                    collect_entry_validator_sites(
                        target,
                        &definition_module,
                        bundle,
                        indexes,
                        path,
                        sites,
                    )?;
                }
                LoweredTypeDeclaration::Newtype { underlying, .. } => {
                    path.push(EntryValidatorPathSegment::NewtypeValue);
                    collect_entry_validator_sites(
                        underlying,
                        &definition_module,
                        bundle,
                        indexes,
                        path,
                        sites,
                    )?;
                    path.pop();
                }
                LoweredTypeDeclaration::Record { name, fields, .. } => {
                    let identity = format!("{definition_module}::{name}");
                    if let Some(invariant) = indexes.get(&identity) {
                        sites.push(EntryValidatorSite {
                            path: path.clone(),
                            invariant: *invariant,
                        });
                    }
                    let ValueType::Record(layout) =
                        &bundle.types[&(definition_module.clone(), name.clone())]
                    else {
                        unreachable!()
                    };
                    for (field_name, field_type, _) in fields {
                        let index = layout
                            .iter()
                            .position(|field| field.name == *field_name)
                            .expect("resolved record field");
                        path.push(EntryValidatorPathSegment::Field(
                            u32::try_from(index).expect("field index fits"),
                        ));
                        collect_entry_validator_sites(
                            field_type,
                            &definition_module,
                            bundle,
                            indexes,
                            path,
                            sites,
                        )?;
                        path.pop();
                    }
                }
                LoweredTypeDeclaration::Enum { name, variants, .. } => {
                    let ValueType::Enum(id) =
                        bundle.types[&(definition_module.clone(), name.clone())]
                    else {
                        unreachable!()
                    };
                    let enumeration = &bundle.enum_types[id as usize];
                    for (variant_index, variant) in variants.iter().enumerate() {
                        match &variant.payload {
                            LoweredEnumPayload::Unit => {}
                            LoweredEnumPayload::Tuple(values) => {
                                for (element, value) in values.iter().enumerate() {
                                    path.push(EntryValidatorPathSegment::EnumPayload {
                                        variant: u32::try_from(variant_index)
                                            .expect("enum variant index fits"),
                                        element: u32::try_from(element)
                                            .expect("enum payload element index fits"),
                                    });
                                    collect_entry_validator_sites(
                                        value,
                                        &definition_module,
                                        bundle,
                                        indexes,
                                        path,
                                        sites,
                                    )?;
                                    path.pop();
                                }
                            }
                            LoweredEnumPayload::Record(fields) => {
                                let EnumPayloadType::Record(layout) =
                                    &enumeration.variants[variant_index].payload
                                else {
                                    unreachable!()
                                };
                                for (field_name, value, _) in fields {
                                    let element = layout
                                        .iter()
                                        .position(|field| field.name == *field_name)
                                        .expect("resolved enum field");
                                    path.push(EntryValidatorPathSegment::EnumPayload {
                                        variant: u32::try_from(variant_index)
                                            .expect("enum variant index fits"),
                                        element: u32::try_from(element)
                                            .expect("enum payload element index fits"),
                                    });
                                    collect_entry_validator_sites(
                                        value,
                                        &definition_module,
                                        bundle,
                                        indexes,
                                        path,
                                        sites,
                                    )?;
                                    path.pop();
                                }
                            }
                        }
                    }
                }
            }
        }
        LoweredType::List(value, _) => {
            path.push(EntryValidatorPathSegment::ListElement);
            collect_entry_validator_sites(value, module, bundle, indexes, path, sites)?;
            path.pop();
        }
        LoweredType::Map(key, value, _) => {
            for (segment, child) in [
                (EntryValidatorPathSegment::MapKey, key.as_ref()),
                (EntryValidatorPathSegment::MapValue, value.as_ref()),
            ] {
                path.push(segment);
                collect_entry_validator_sites(child, module, bundle, indexes, path, sites)?;
                path.pop();
            }
        }
        LoweredType::Tuple(values, _) => {
            for (index, value) in values.iter().enumerate() {
                path.push(EntryValidatorPathSegment::TupleElement(
                    u32::try_from(index).expect("tuple element index fits"),
                ));
                collect_entry_validator_sites(value, module, bundle, indexes, path, sites)?;
                path.pop();
            }
        }
        LoweredType::Record(fields, _) => {
            let mut names = fields.iter().map(|(name, _, _)| name).collect::<Vec<_>>();
            names.sort();
            for (name, value, _) in fields {
                let index = names.binary_search(&name).expect("anonymous record field");
                path.push(EntryValidatorPathSegment::Field(
                    u32::try_from(index).expect("record field index fits"),
                ));
                collect_entry_validator_sites(value, module, bundle, indexes, path, sites)?;
                path.pop();
            }
        }
        LoweredType::Option(value, _) => {
            path.push(EntryValidatorPathSegment::OptionSome);
            collect_entry_validator_sites(value, module, bundle, indexes, path, sites)?;
            path.pop();
        }
        LoweredType::Result(ok, error, _) => {
            for (segment, child) in [
                (EntryValidatorPathSegment::ResultOk, ok.as_ref()),
                (EntryValidatorPathSegment::ResultError, error.as_ref()),
            ] {
                path.push(segment);
                collect_entry_validator_sites(child, module, bundle, indexes, path, sites)?;
                path.pop();
            }
        }
        LoweredType::Future(value, _)
        | LoweredType::Task(value, _)
        | LoweredType::Prompt(value, _)
        | LoweredType::Range(value, _)
        | LoweredType::Sequence(value, _) => {
            collect_entry_validator_sites(value, module, bundle, indexes, path, sites)?;
        }
        LoweredType::Function { .. } => {}
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn compile_bundle_from_modules(
    root: &str,
    modules: BTreeMap<String, LoweredModule>,
    entry_points: &[PackageEntryPoint],
    tool_bindings: &[CompilerToolBinding],
    template_bindings: &[CompilerTemplateBinding],
) -> Result<Compilation, Vec<Diagnostic>> {
    let mut bundle = resolve_bundle(modules, tool_bindings, template_bindings)
        .map_err(|diagnostic| vec![diagnostic])?;
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
    if entry_info.is_const
        || !entry_info.lowered.exported
        || !valid_parameter_count
        || entry_info.bytecode.is_none()
    {
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
    let effect_sets = collect_effect_sets(&bundle);
    resolve_deferred_effect_sets(&mut bundle, &effect_sets);
    let constant_values =
        evaluate_constants(&bundle, &effect_sets).map_err(|diagnostic| vec![diagnostic])?;
    let (record_invariants, invariant_indexes) =
        compile_record_invariants(&bundle).map_err(|diagnostic| vec![diagnostic])?;
    let mut next_runtime_function = 0_u32;
    for function in &mut bundle.functions {
        if function.is_const {
            function.bytecode = None;
        } else if function.bytecode.is_some() {
            function.bytecode = Some(next_runtime_function);
            next_runtime_function = next_runtime_function.checked_add(1).ok_or_else(|| {
                vec![Diagnostic::new(
                    "E3005",
                    "too many runtime functions",
                    function.lowered.name_span,
                )]
            })?;
        }
    }
    let entry = bundle.functions[entry_symbol as usize]
        .bytecode
        .expect("checked runtime entry bytecode");
    let mut exported_functions = Vec::new();
    for function in bundle.functions.iter().filter(|function| {
        !function.is_const && function.lowered.exported && function.bytecode.is_some()
    }) {
        let parameter_types = function
            .parameters
            .iter()
            .map(|parameter| concrete_type(parameter, &BTreeMap::new(), &effect_sets))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|diagnostic| vec![diagnostic])?;
        let return_type = concrete_type(&function.return_type, &BTreeMap::new(), &effect_sets)
            .map_err(|diagnostic| vec![diagnostic])?;
        let mut input_validators = Vec::new();
        if let Some((_, input, _)) = function.lowered.parameters.first() {
            collect_entry_validator_sites(
                input,
                &function.module,
                &bundle,
                &invariant_indexes,
                &mut Vec::new(),
                &mut input_validators,
            )
            .map_err(|diagnostic| vec![diagnostic.with_source(&function.module)])?;
        }
        let mut output_validators = Vec::new();
        collect_entry_validator_sites(
            &function.lowered.return_type,
            &function.module,
            &bundle,
            &invariant_indexes,
            &mut Vec::new(),
            &mut output_validators,
        )
        .map_err(|diagnostic| vec![diagnostic.with_source(&function.module)])?;
        input_validators.sort();
        input_validators.dedup();
        output_validators.sort();
        output_validators.dedup();
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
            input_validators,
            output_validators,
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
        constant_values,
        constant_evaluation: false,
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
            BTreeMap::new(),
            BTreeSet::new(),
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
    let mut hir_constants = BTreeMap::<String, Vec<HirConstant>>::new();
    let mut mir_constants = Vec::new();
    for info in bundle.functions.iter().filter(|function| function.is_const) {
        let value_type = concrete_type(&info.return_type, &BTreeMap::new(), &lowering.effect_sets)
            .map_err(|diagnostic| vec![diagnostic.with_source(&info.module)])?;
        let type_id = lowering.intern_type(value_type);
        hir_constants
            .entry(info.module.clone())
            .or_default()
            .push(HirConstant {
                symbol: info.symbol,
                name: info.lowered.name.clone(),
                value_type: type_id,
            });
        mir_constants.push(MirConstant {
            symbol: info.symbol,
            name: format!("{}::{}", info.module, info.lowered.name),
            value_type: type_id,
        });
    }
    let effect_report = bundle
        .functions
        .iter()
        .filter(|function| !function.is_const && !function.lowered.exported)
        .map(|function| EffectReportEntry {
            module: function.module.clone(),
            function: function.lowered.name.clone(),
            effects: function.effects.clone(),
        })
        .collect();
    for module in hir_constants.keys() {
        lowering.hir_modules.entry(module.clone()).or_default();
    }
    let hir = HirBundle {
        types: lowering.types,
        spans: lowering.spans,
        modules: lowering
            .hir_modules
            .into_iter()
            .map(|(path, mut functions)| {
                functions.sort_by(|left, right| left.name.cmp(&right.name));
                HirModule {
                    constants: hir_constants.remove(&path).unwrap_or_default(),
                    path,
                    functions,
                }
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
            constants: mir_constants,
            functions: lowering.mir_functions,
        },
        effect_report,
        exported_functions,
        record_invariants,
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
