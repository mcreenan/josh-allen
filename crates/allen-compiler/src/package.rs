//! Catalog-aware package-to-artifact assembly shared by CLI and JOSH.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use allen_bytecode::{
    Artifact, ArtifactMetadata, BYTECODE_VERSION, EntryContract, ImportContract, Instruction,
    ManifestContract, StrictSchema, TemplateHole, TemplateMarker, TemplateResource, ValidatorExpr,
    ValueType, compute_entry_contract_digest, compute_entry_record_provenance,
    compute_template_digest, compute_tool_contract_digest, typed_response_output_type,
};
use allen_package::{
    LoadLimits, LoadedPackage, ManifestLimits, canonical_https_origin, load_verified_root_package,
    load_verified_root_package_with_resources,
};
use allen_schema::FrozenCatalog;

use crate::{
    Compilation, CompilerTemplateBinding, InlineManifest, PackageEntryPoint, PackageSourceBundle,
    PreparedTools, SourceTest, compile_inline_manifest_source_with_catalog,
    compile_package_bundle_with_prepared_tools_and_templates, prepare_tools, render_diagnostic,
};

/// Assemble an isolated source-test compilation into a genuine artifact.
///
/// The artifact contains one internal test entry contract bound to the
/// selected synthetic function. It is not returned by production compilation.
///
/// # Errors
///
/// Returns an error when the source-test compilation does not contain the
/// expected synthetic entry or cannot be packaged as a bytecode artifact.
pub fn assemble_source_test(compilation: Compilation) -> Result<Artifact, String> {
    let exported = compilation
        .exported_functions
        .iter()
        .find(|function| function.function == "test")
        .ok_or_else(|| "source test compilation has no synthetic entry".to_owned())?;
    let capabilities = exported
        .effects
        .iter()
        .filter(|effect| {
            !matches!(
                effect.as_str(),
                "task.spawn" | "debug.inspect" | "capability.inspect"
            )
        })
        .map(|effect| match effect.as_str() {
            "fs.read" | "fs.write" => format!("{effect}(workdir)"),
            _ => effect.clone(),
        })
        .collect();
    let package_identity = exported.module.strip_prefix("pkg://").and_then(|path| {
        let package = path.split('/').next()?;
        let (name, version) = package.rsplit_once('@')?;
        Some((name.to_owned(), version.to_owned()))
    });
    let mut artifact = assemble_standalone_compilation(
        &exported.module.clone(),
        InlineManifest {
            language: "0.1".to_owned(),
            entry: "test".to_owned(),
            capabilities,
            http_origins: Vec::new(),
            tools: Vec::new(),
        },
        compilation,
        PreparedTools::default(),
        package_identity.is_none(),
    )?
    .artifact;
    if let (Some((package, version)), Some(manifest)) =
        (package_identity, artifact.manifest.as_mut())
    {
        manifest.package = package;
        manifest.version = version;
    }
    Ok(artifact)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledPackage {
    pub artifact: Artifact,
    pub effects: Vec<String>,
    /// Canonical root-manifest command patterns for later host planning.
    pub requested_exec_commands: Vec<String>,
    /// Canonical root-manifest environment names for later host planning.
    pub requested_exec_environment: Vec<String>,
}

/// One selected package source test and its genuine verified-artifact input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledPackageSourceTest {
    pub test: SourceTest,
    pub package: CompiledPackage,
}

/// Compile one loose or inline-manifest source file into a verified package artifact.
///
/// A loose file receives a capability-free `inline` manifest with a `main` entry.
/// Inline manifests may request capabilities and tools from the supplied frozen
/// catalog. The manifest remains a request; execution grants are selected later.
///
/// # Errors
///
/// Rejects compiler diagnostics, missing entries, undeclared effects, invalid
/// boundary types, invalid HTTP origins, or unsatisfied tool requirements.
pub fn assemble_inline_source(
    source: &str,
    catalog: &FrozenCatalog,
) -> Result<CompiledPackage, String> {
    let compiled = compile_inline_manifest_source_with_catalog(source, catalog)
        .map_err(|diagnostics| render_inline_diagnostics(source, diagnostics))?;
    assemble_compiled_inline_source(compiled)
}

fn render_inline_diagnostics(source: &str, diagnostics: Vec<crate::Diagnostic>) -> String {
    diagnostics
        .into_iter()
        .map(|diagnostic| {
            render_diagnostic(
                diagnostic.source.as_deref().unwrap_or("main.allen"),
                source,
                &diagnostic,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assemble_compiled_inline_source(
    compiled: (Option<InlineManifest>, Compilation, PreparedTools),
) -> Result<CompiledPackage, String> {
    let (manifest, compilation, prepared) = compiled;
    let manifest = manifest.unwrap_or_else(|| InlineManifest {
        language: "0.1".to_owned(),
        entry: "main".to_owned(),
        capabilities: Vec::new(),
        http_origins: Vec::new(),
        tools: Vec::new(),
    });
    assemble_inline_compilation(manifest, compilation, prepared)
}

/// Assemble already-compiled standalone source into a canonical package artifact.
///
/// # Errors
///
/// Rejects an invalid inline manifest, entry contract, capability request, or
/// serializable boundary.
#[allow(clippy::too_many_lines)]
pub fn assemble_inline_compilation(
    manifest: InlineManifest,
    compilation: Compilation,
    prepared: PreparedTools,
) -> Result<CompiledPackage, String> {
    assemble_standalone_compilation("main.allen", manifest, compilation, prepared, true)
}

/// Assemble an already-compiled loose source bundle into its canonical artifact.
///
/// The selected root module supplies the exported `main` entry. The synthesized
/// `inline` manifest requests no host capabilities. All loose module identities
/// are placed under that package without changing bundle relationships.
///
/// # Errors
///
/// Rejects a missing or invalid `main` entry, undeclared entry effects, or a
/// non-serializable entry boundary.
pub fn assemble_loose_compilation(
    root: &str,
    compilation: Compilation,
) -> Result<CompiledPackage, String> {
    let manifest = InlineManifest {
        language: "0.1".to_owned(),
        entry: "main".to_owned(),
        capabilities: Vec::new(),
        http_origins: Vec::new(),
        tools: Vec::new(),
    };
    assemble_standalone_compilation(root, manifest, compilation, PreparedTools::default(), true)
}

#[allow(clippy::too_many_lines)]
fn assemble_standalone_compilation(
    root: &str,
    manifest: InlineManifest,
    mut compilation: Compilation,
    mut prepared: PreparedTools,
    canonicalize_inline: bool,
) -> Result<CompiledPackage, String> {
    if manifest.language != "0.1" {
        return Err("inline manifest language must be exactly '0.1'".to_owned());
    }
    if manifest.capabilities.iter().any(|capability| {
        !matches!(
            capability.as_str(),
            "fs.read(workdir)"
                | "fs.write(workdir)"
                | "net.http_get"
                | "permission.request_external_fs"
                | "agent.message"
                | "agent.ask"
                | "agent.transcript"
                | "model.request"
                | "sub_agent.ask"
                | "sub_agent.create"
                | "sub_agent.message"
                | "sub_agent.run"
                | "user.ask"
        )
    }) {
        return Err("inline manifest contains an unsupported capability".to_owned());
    }
    if canonicalize_inline {
        canonicalize_inline_compilation(&mut compilation, &mut prepared.schemas);
    }
    let exported = compilation
        .exported_functions
        .iter()
        .find(|function| function.module == root && function.function == manifest.entry)
        .ok_or_else(|| "inline manifest entry does not name an exported function".to_owned())?;
    for origin in &manifest.http_origins {
        canonical_https_origin(origin).map_err(|error| error.to_string())?;
    }
    let requests_http = manifest
        .capabilities
        .iter()
        .any(|capability| capability == "net.http_get");
    if requests_http == manifest.http_origins.is_empty() {
        return Err(
            "inline manifest net.http_get and http_origins must be declared together".to_owned(),
        );
    }
    for effect in &exported.effects {
        let capability = match effect.as_str() {
            "fs.read" | "fs.write" => format!("{effect}(workdir)"),
            _ => effect.clone(),
        };
        let is_tool = prepared
            .contracts
            .iter()
            .any(|contract| contract.effect == *effect);
        if !matches!(
            effect.as_str(),
            "task.spawn" | "debug.inspect" | "capability.inspect"
        ) && !is_tool
            && !manifest
                .capabilities
                .iter()
                .any(|declared| declared == &capability)
        {
            return Err(format!(
                "inline manifest does not declare entry effect '{effect}'"
            ));
        }
    }
    if exported.parameter_types.len() > 1 {
        return Err("inline manifest entry must have zero or one parameter".to_owned());
    }
    let input = exported
        .parameter_types
        .first()
        .cloned()
        .unwrap_or(ValueType::Unit);
    reject_boundary_type(&input, &manifest.entry)?;
    reject_boundary_type(&exported.return_type, &manifest.entry)?;
    let mut schemas = prepared.schemas;
    let input_schema = push_schema(&mut schemas, input)?;
    let output_schema = push_schema(&mut schemas, exported.return_type.clone())?;
    let input_validators = exported.input_validators.clone();
    let output_validators = exported.output_validators.clone();
    let input_contract_digest = compute_entry_contract_digest(
        &schemas[input_schema as usize],
        &input_validators,
        &compilation.record_invariants,
    );
    let output_contract_digest = compute_entry_contract_digest(
        &schemas[output_schema as usize],
        &output_validators,
        &compilation.record_invariants,
    );
    let input_record_provenance = compute_entry_record_provenance(
        &schemas[input_schema as usize],
        &compilation.module.enum_types,
        &input_validators,
        &compilation.record_invariants,
    )
    .map_err(|error| error.to_string())?;
    let output_record_provenance = compute_entry_record_provenance(
        &schemas[output_schema as usize],
        &compilation.module.enum_types,
        &output_validators,
        &compilation.record_invariants,
    )
    .map_err(|error| error.to_string())?;
    push_typed_response_schemas(&mut schemas, &compilation)?;
    let mut capabilities = manifest
        .capabilities
        .into_iter()
        .map(|capability| {
            capability
                .split_once('(')
                .map_or(capability.clone(), |(name, _)| name.to_owned())
        })
        .collect::<Vec<_>>();
    capabilities.sort();
    capabilities.dedup();
    let mut module = compilation.module;
    module.entry = exported.function_id;
    let debug = compilation.debug;
    let effects = compilation
        .effect_report
        .iter()
        .map(|entry| format_effect_report_entry(&entry.module, &entry.function, &entry.effects))
        .collect();
    let tool_contract_digest = compute_tool_contract_digest(&prepared.contracts);
    let artifact = Artifact {
        metadata: ArtifactMetadata {
            bytecode_version: BYTECODE_VERSION,
            ..ArtifactMetadata::default()
        },
        module,
        debug: Some(debug),
        schemas,
        entries: vec![EntryContract {
            name: manifest.entry,
            function: exported.function_id,
            input_schema,
            output_schema,
            input_validators,
            output_validators,
            input_record_provenance,
            output_record_provenance,
            input_contract_digest,
            output_contract_digest,
        }],
        imports: Vec::new(),
        manifest: Some(ManifestContract {
            package: "inline".to_owned(),
            version: "0.1.0".to_owned(),
            language_requirement: manifest.language,
            required_capabilities: capabilities,
            optional_capabilities: Vec::new(),
            https_origins: manifest.http_origins,
            exec_commands: Vec::new(),
            exec_environment: Vec::new(),
            limits: Vec::new(),
            required_tools: prepared.contracts,
            tool_contract_digest,
        }),
        templates: Vec::new(),
        record_invariants: compilation.record_invariants,
    };
    Ok(CompiledPackage {
        artifact,
        effects,
        requested_exec_commands: Vec::new(),
        requested_exec_environment: Vec::new(),
    })
}

struct InlineIdentity {
    source: String,
    uri: String,
    symbol: String,
}

fn canonicalize_inline_compilation(compilation: &mut Compilation, schemas: &mut [StrictSchema]) {
    let identities = compilation
        .debug
        .sources
        .iter()
        .map(|source| InlineIdentity {
            source: source.clone(),
            uri: format!("pkg://inline@0.1.0/src/{source}"),
            symbol: inline_module_symbol(source),
        })
        .collect::<Vec<_>>();
    for function in &mut compilation.module.functions {
        if let Some(identity) = identities.iter().find(|identity| {
            function
                .name
                .strip_prefix(&identity.source)
                .is_some_and(|suffix| suffix.starts_with("::"))
        }) {
            let suffix = function
                .name
                .strip_prefix(&identity.source)
                .expect("the source prefix was matched");
            function.name = format!("{}{suffix}", identity.symbol);
        }
        for value_type in &mut function.registers {
            canonicalize_inline_value_type(value_type, &identities);
        }
        canonicalize_inline_value_type(&mut function.return_type, &identities);
        for instruction in &mut function.code {
            if let Instruction::Narrow { target, .. } | Instruction::Decode { target, .. } =
                instruction
            {
                canonicalize_inline_value_type(target, &identities);
            }
        }
    }
    for enum_type in &mut compilation.module.enum_types {
        if let Some(identity) = identities.iter().find(|identity| {
            enum_type
                .name
                .strip_prefix(&identity.source)
                .is_some_and(|suffix| suffix.starts_with("::"))
        }) {
            let suffix = enum_type
                .name
                .strip_prefix(&identity.source)
                .expect("the source prefix was matched");
            enum_type.name = format!("{}{suffix}", identity.uri);
        }
        for variant in &mut enum_type.variants {
            match &mut variant.payload {
                allen_bytecode::EnumPayloadType::Unit => {}
                allen_bytecode::EnumPayloadType::Tuple(values) => {
                    for value_type in values {
                        canonicalize_inline_value_type(value_type, &identities);
                    }
                }
                allen_bytecode::EnumPayloadType::Record(fields) => {
                    for field in fields {
                        canonicalize_inline_value_type(&mut field.value_type, &identities);
                    }
                }
            }
        }
    }
    for exported in &mut compilation.exported_functions {
        for value_type in &mut exported.parameter_types {
            canonicalize_inline_value_type(value_type, &identities);
        }
        canonicalize_inline_value_type(&mut exported.return_type, &identities);
    }
    for invariant in &mut compilation.record_invariants {
        if let Some(identity) = identities.iter().find(|identity| {
            invariant
                .identity
                .strip_prefix(&identity.source)
                .is_some_and(|suffix| suffix.starts_with("::"))
        }) {
            let suffix = invariant
                .identity
                .strip_prefix(&identity.source)
                .expect("the source prefix was matched");
            invariant.identity = format!("{}{suffix}", identity.uri);
        }
        for field in &mut invariant.fields {
            canonicalize_inline_value_type(&mut field.value_type, &identities);
        }
        canonicalize_inline_validator_expr(&mut invariant.predicate, &identities);
    }
    for schema in schemas {
        canonicalize_inline_value_type(&mut schema.value_type, &identities);
    }
    for source in &mut compilation.debug.sources {
        let identity = identities
            .iter()
            .find(|identity| identity.source == *source)
            .expect("every debug source has a canonical inline identity");
        source.clone_from(&identity.uri);
    }
}

fn canonicalize_inline_validator_expr(
    expression: &mut ValidatorExpr,
    identities: &[InlineIdentity],
) {
    match expression {
        ValidatorExpr::Field { value_type, .. } => {
            canonicalize_inline_value_type(value_type, identities);
        }
        ValidatorExpr::Not(value) => canonicalize_inline_validator_expr(value, identities),
        ValidatorExpr::BoolBinary { left, right, .. }
        | ValidatorExpr::Compare { left, right, .. } => {
            canonicalize_inline_validator_expr(left, identities);
            canonicalize_inline_validator_expr(right, identities);
        }
        ValidatorExpr::Bool(_) => {}
    }
}

fn canonicalize_inline_value_type(value_type: &mut ValueType, identities: &[InlineIdentity]) {
    match value_type {
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value)
        | ValueType::Sequence(value) => canonicalize_inline_value_type(value, identities),
        ValueType::Map(key, value) | ValueType::Result(key, value) => {
            canonicalize_inline_value_type(key, identities);
            canonicalize_inline_value_type(value, identities);
        }
        ValueType::Tuple(values) => {
            for value in values {
                canonicalize_inline_value_type(value, identities);
            }
        }
        ValueType::Record(fields) => {
            for field in fields {
                canonicalize_inline_value_type(&mut field.value_type, identities);
            }
        }
        ValueType::Newtype { name, underlying } => {
            if let Some(identity) = identities.iter().find(|identity| {
                name.strip_prefix(&identity.source)
                    .is_some_and(|suffix| suffix.starts_with("::"))
            }) {
                let suffix = name
                    .strip_prefix(&identity.source)
                    .expect("the source prefix was matched");
                *name = format!("{}{suffix}", identity.uri);
            }
            canonicalize_inline_value_type(underlying, identities);
        }
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => {
            for parameter in parameters {
                canonicalize_inline_value_type(parameter, identities);
            }
            canonicalize_inline_value_type(return_type, identities);
        }
        ValueType::Int
        | ValueType::Range
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

fn inline_module_symbol(source: &str) -> String {
    let mut components = vec![
        "pkg".to_owned(),
        escape_inline_symbol_component("inline"),
        escape_inline_symbol_component("0.1.0"),
        escape_inline_symbol_component("src"),
    ];
    let mut source_components = source.split('/').collect::<Vec<_>>();
    let file_name = source_components
        .pop()
        .expect("compiler debug sources are non-empty module identities");
    let file_stem = file_name
        .strip_suffix(".allen")
        .expect("compiler debug sources end in .allen");
    components.extend(
        source_components
            .into_iter()
            .map(escape_inline_symbol_component),
    );
    components.push(format!(
        "{}.allen",
        escape_inline_symbol_component(file_stem)
    ));
    components.join("/")
}

fn escape_inline_symbol_component(component: &str) -> String {
    let mut escaped = String::with_capacity(1 + component.len() * 2);
    escaped.push('x');
    for byte in component.bytes() {
        use std::fmt::Write as _;
        write!(&mut escaped, "{byte:02x}").expect("writing into String cannot fail");
    }
    escaped
}

/// Verify and assemble one normalized, dependency-free in-memory root package.
///
/// # Errors
///
/// Rejects unsafe source paths, dependency declarations, stale lock text,
/// unsatisfied tools, compiler diagnostics, and invalid artifact boundaries.
pub fn assemble_root_source_package(
    manifest_text: &str,
    sources: &BTreeMap<String, String>,
    lock_text: Option<&str>,
    catalog: Option<&FrozenCatalog>,
    limits: &LoadLimits,
) -> Result<CompiledPackage, String> {
    let loaded = load_verified_root_package(manifest_text, sources, lock_text, limits)
        .map_err(|error| error.to_string())?;
    assemble_loaded_package(&loaded, catalog)
}

/// Verify and assemble one dependency-free in-memory package with exact resources.
///
/// The resource map must exactly match the manifest's declared templates. Call
/// [`assemble_root_source_package`] when no external package resources exist.
///
/// # Errors
///
/// Rejects unsafe or incomplete resources, dependency declarations, stale lock
/// text, unsatisfied tools, compiler diagnostics, and invalid artifact boundaries.
pub fn assemble_root_source_package_with_resources(
    manifest_text: &str,
    sources: &BTreeMap<String, String>,
    resources: &BTreeMap<String, Vec<u8>>,
    lock_text: Option<&str>,
    catalog: Option<&FrozenCatalog>,
    limits: &LoadLimits,
) -> Result<CompiledPackage, String> {
    let loaded = load_verified_root_package_with_resources(
        manifest_text,
        sources,
        resources,
        lock_text,
        limits,
    )
    .map_err(|error| error.to_string())?;
    assemble_loaded_package(&loaded, catalog)
}

/// Compile one verified package graph into the canonical current artifact.
///
/// Required tools are selected from `catalog`. A package with no required
/// tools can be assembled without a catalog.
///
/// # Errors
///
/// Returns a safe package, catalog, compiler, boundary, or artifact assembly error.
pub fn assemble_loaded_package(
    loaded: &LoadedPackage,
    catalog: Option<&FrozenCatalog>,
) -> Result<CompiledPackage, String> {
    let (bundle, mut prepared, templates, resources) = prepare_loaded_package(loaded, catalog)?;
    let compilation = compile_package_bundle_with_prepared_tools_and_templates(
        &bundle,
        &mut prepared,
        &templates,
    )
    .map_err(|diagnostics| render_package_diagnostics(&bundle, diagnostics))?;
    finish_loaded_package(loaded, compilation, prepared, resources)
}

/// Build the canonical compiler bundle used for package source-test discovery.
///
/// The same verified package graph and import map as production compilation is
/// used, while entries are irrelevant because tests are selected separately.
///
/// # Errors
///
/// Returns an error when the loaded package graph cannot be converted into a
/// source-test discovery bundle.
pub fn prepare_loaded_source_tests(loaded: &LoadedPackage) -> Result<PackageSourceBundle, String> {
    source_test_discovery_bundle(loaded)
}

/// Compile and assemble one source test from a verified package graph.
///
/// The selected module's defining package becomes the test artifact root. Only
/// that package and its transitive dependencies contribute source, imports,
/// templates, tools, manifests, invariants, and digests. Assembly otherwise
/// follows the ordinary verified package path.
///
/// # Errors
///
/// Returns an error when the selected source test is missing or the scoped
/// package cannot be compiled, prepared, or assembled.
pub fn assemble_loaded_source_test(
    loaded: &LoadedPackage,
    catalog: Option<&FrozenCatalog>,
    module: &str,
    name: &str,
) -> Result<CompiledPackageSourceTest, String> {
    let discovery = source_test_discovery_bundle(loaded)?;
    let selected = crate::discover_source_tests(&discovery)
        .map_err(|diagnostics| render_package_diagnostics(&discovery, diagnostics))?
        .into_iter()
        .find(|test| test.module == module && test.name == name)
        .ok_or_else(|| format!("source test {module}::{name:?} was not found"))?;
    let scoped = scope_loaded_package(loaded, module, &selected.effects)?;
    let (bundle, mut prepared, templates, resources) =
        prepare_loaded_package_for_test(&scoped, catalog, module)?;
    let compiled = crate::compile_source_test_with_prepared_tools_and_templates(
        &bundle,
        module,
        name,
        &mut prepared,
        &templates,
    )
    .map_err(|diagnostics| render_package_diagnostics(&bundle, diagnostics))?;
    let test = compiled.test;
    let mut package = finish_loaded_package(&scoped, compiled.compilation, prepared, resources)?;
    package.requested_exec_commands.clear();
    package.requested_exec_environment.clear();
    Ok(CompiledPackageSourceTest { test, package })
}

fn source_test_discovery_bundle(loaded: &LoadedPackage) -> Result<PackageSourceBundle, String> {
    let sources = loaded
        .modules
        .iter()
        .map(|module| (module.identity.clone(), module.source.clone()))
        .collect::<BTreeMap<_, _>>();
    let root = sources
        .keys()
        .next()
        .cloned()
        .ok_or_else(|| "verified package graph has no source modules".to_owned())?;
    Ok(PackageSourceBundle {
        root,
        sources,
        import_targets: import_targets(loaded)?,
        entry_points: Vec::new(),
        entry_modules: Vec::new(),
    })
}

fn prepare_loaded_package_for_test(
    loaded: &LoadedPackage,
    catalog: Option<&FrozenCatalog>,
    module: &str,
) -> Result<
    (
        PackageSourceBundle,
        PreparedTools,
        Vec<CompilerTemplateBinding>,
        Vec<TemplateResource>,
    ),
    String,
> {
    let (mut bundle, prepared, templates, resources) = prepare_loaded_package(loaded, catalog)?;
    module.clone_into(&mut bundle.root);
    bundle.entry_modules = vec![module.to_owned()];
    bundle.entry_points = vec![PackageEntryPoint {
        module: module.to_owned(),
        function: "test".to_owned(),
    }];
    if !bundle.sources.contains_key(module) {
        return Err(format!("selected source-test module '{module}' is missing"));
    }
    Ok((bundle, prepared, templates, resources))
}

fn scope_loaded_package(
    loaded: &LoadedPackage,
    module: &str,
    effects: &[String],
) -> Result<LoadedPackage, String> {
    let discovery = source_test_discovery_bundle(loaded)?;
    let reachable_modules = crate::reachable_source_modules(&discovery, module)
        .map_err(|diagnostics| render_package_diagnostics(&discovery, diagnostics))?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let defining = loaded
        .modules
        .iter()
        .find(|candidate| candidate.identity == module)
        .ok_or_else(|| format!("selected source-test module '{module}' is not in the graph"))?
        .package
        .clone();
    let reachable = loaded
        .modules
        .iter()
        .filter(|source| reachable_modules.contains(&source.identity))
        .map(|source| source.package.clone())
        .collect::<BTreeSet<_>>();
    let mut scoped = loaded.clone();
    scoped.root = defining;
    scoped
        .packages
        .retain(|package| reachable.contains(&package.id));
    for package in &mut scoped.packages {
        package
            .dependencies
            .retain(|dependency| reachable.contains(&dependency.package));
        package
            .modules
            .retain(|source| reachable_modules.contains(&source.identity));
    }
    scoped
        .modules
        .retain(|source| reachable_modules.contains(&source.identity));
    let root = scoped
        .packages
        .iter_mut()
        .find(|package| package.id == scoped.root)
        .ok_or_else(|| "selected source-test package is missing".to_owned())?;
    let source = root
        .modules
        .iter()
        .find(|source| source.identity == module)
        .ok_or_else(|| "selected source-test module is missing from its package".to_owned())?;
    root.manifest.entries = vec![allen_package::Entry {
        name: "test".to_owned(),
        function: format!("{}::test", source.path),
        input: "Void".to_owned(),
        output: "Void".to_owned(),
    }];
    root.manifest.capabilities.required = effects
        .iter()
        .filter(|effect| {
            !effect.starts_with("tool.")
                && !matches!(
                    effect.as_str(),
                    "task.spawn" | "debug.inspect" | "capability.inspect"
                )
        })
        .cloned()
        .collect();
    root.manifest.capabilities.optional.clear();
    if !effects.iter().any(|effect| effect == "net.http_get") {
        root.manifest.network.http_get.origins.clear();
    }
    Ok(scoped)
}

#[allow(clippy::too_many_lines)]
fn prepare_loaded_package(
    loaded: &LoadedPackage,
    catalog: Option<&FrozenCatalog>,
) -> Result<
    (
        PackageSourceBundle,
        PreparedTools,
        Vec<CompilerTemplateBinding>,
        Vec<TemplateResource>,
    ),
    String,
> {
    let root_package = loaded
        .packages
        .iter()
        .find(|package| package.id == loaded.root)
        .ok_or_else(|| "verified graph has no root package".to_owned())?;
    let requirements = root_package
        .manifest
        .tools
        .required
        .iter()
        .map(|required| {
            allen_schema::ToolRequirement::parse(&required.name, &required.version)
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let prepared = match catalog {
        Some(catalog) => {
            prepare_tools(catalog, &requirements).map_err(|error| error.to_string())?
        }
        None if requirements.is_empty() => PreparedTools::default(),
        None => {
            return Err(
                "standalone compilation cannot resolve required tools without a frozen catalog"
                    .to_owned(),
            );
        }
    };
    let sources = loaded
        .modules
        .iter()
        .map(|module| (module.identity.clone(), module.source.clone()))
        .collect();
    let import_targets = import_targets(loaded)?;
    let entry_modules = root_package
        .manifest
        .entries
        .iter()
        .map(|entry| {
            entry
                .function
                .rsplit_once("::")
                .map(|(module, _)| {
                    canonical_module(&loaded.root.name, &loaded.root.version, module)
                })
                .ok_or_else(|| format!("entry '{}' is not module-qualified", entry.name))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let entry_points = root_package
        .manifest
        .entries
        .iter()
        .map(|entry| {
            let (module, function) = entry
                .function
                .rsplit_once("::")
                .ok_or_else(|| format!("entry '{}' is not module-qualified", entry.name))?;
            Ok(PackageEntryPoint {
                module: canonical_module(&loaded.root.name, &loaded.root.version, module),
                function: function.to_owned(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let root = entry_modules
        .first()
        .cloned()
        .ok_or_else(|| "root manifest has no entry".to_owned())?;
    let bundle = PackageSourceBundle {
        root,
        sources,
        import_targets,
        entry_points,
        entry_modules,
    };
    if !bundle.sources.contains_key(&bundle.root) {
        return Err("verified package entry source is missing".to_owned());
    }
    let (templates, resources) = prepare_templates(loaded)?;
    Ok((bundle, prepared, templates, resources))
}

fn render_package_diagnostics(
    bundle: &PackageSourceBundle,
    diagnostics: Vec<crate::Diagnostic>,
) -> String {
    let root_source = bundle
        .sources
        .get(&bundle.root)
        .expect("verified package entry source was checked during preparation");
    diagnostics
        .into_iter()
        .map(|diagnostic| {
            let module = diagnostic.source.as_deref().unwrap_or(&bundle.root);
            let source = bundle.sources.get(module).unwrap_or(root_source);
            render_diagnostic(module, source, &diagnostic)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn prepare_templates(
    loaded: &LoadedPackage,
) -> Result<(Vec<CompilerTemplateBinding>, Vec<TemplateResource>), String> {
    let mut pending = Vec::new();
    for package in &loaded.packages {
        let package_identity = package.id.canonical();
        for name in package.template_names() {
            let content = std::str::from_utf8(
                package
                    .template_content(name)
                    .ok_or_else(|| format!("template '{name}' has no content"))?,
            )
            .map_err(|_| format!("template '{name}' content is not UTF-8"))?
            .to_owned();
            let holes = package
                .template_holes(name)
                .ok_or_else(|| format!("template '{name}' has no signature"))?
                .iter()
                .map(|(hole, value_type)| {
                    let value_type = match value_type.as_str() {
                        "Bool" => ValueType::Bool,
                        "Int" => ValueType::Int,
                        "Float" => ValueType::Float,
                        "String" => ValueType::String,
                        _ => return Err(format!("template '{name}' has an invalid hole type")),
                    };
                    Ok(TemplateHole {
                        name: hole.clone(),
                        value_type,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let markers = package
                .template_markers(name)
                .ok_or_else(|| format!("template '{name}' has no marker table"))?
                .iter()
                .map(|(start, end, hole)| {
                    let hole = holes
                        .binary_search_by(|candidate| candidate.name.as_str().cmp(hole))
                        .map_err(|_| format!("template '{name}' marker has no declared hole"))?;
                    Ok(TemplateMarker {
                        start: u32::try_from(*start)
                            .map_err(|_| format!("template '{name}' marker is too large"))?,
                        end: u32::try_from(*end)
                            .map_err(|_| format!("template '{name}' marker is too large"))?,
                        hole: u32::try_from(hole)
                            .map_err(|_| format!("template '{name}' has too many holes"))?,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let identity = format!("pkg://{package_identity}/templates/{name}");
            let digest = compute_template_digest(&content, &holes);
            let mut digest_text = String::from("sha256:");
            for byte in digest {
                write!(&mut digest_text, "{byte:02x}")
                    .expect("writing a digest into String cannot fail");
            }
            if package.template_digest(name) != Some(digest_text.as_str()) {
                return Err(format!("template '{identity}' digest is inconsistent"));
            }
            pending.push((
                package_identity.clone(),
                name.to_owned(),
                TemplateResource {
                    identity,
                    content,
                    digest,
                    holes,
                    markers,
                },
            ));
        }
    }
    pending.sort_by(|left, right| left.2.identity.as_bytes().cmp(right.2.identity.as_bytes()));
    let mut bindings = Vec::with_capacity(pending.len());
    let mut resources = Vec::with_capacity(pending.len());
    for (index, (package, name, resource)) in pending.into_iter().enumerate() {
        bindings.push(CompilerTemplateBinding {
            package,
            name,
            template: u32::try_from(index).map_err(|_| "too many templates".to_owned())?,
            holes: resource
                .holes
                .iter()
                .map(|hole| (hole.name.clone(), hole.value_type.clone()))
                .collect(),
        });
        resources.push(resource);
    }
    Ok((bindings, resources))
}

#[allow(clippy::too_many_lines)]
fn finish_loaded_package(
    loaded: &LoadedPackage,
    compilation: Compilation,
    prepared: PreparedTools,
    templates: Vec<TemplateResource>,
) -> Result<CompiledPackage, String> {
    let root_package = loaded
        .packages
        .iter()
        .find(|package| package.id == loaded.root)
        .ok_or_else(|| "verified graph has no root package".to_owned())?;

    let mut schemas = prepared.schemas;
    let mut entries = Vec::new();
    for declared in &root_package.manifest.entries {
        let (module_path, function_name) = declared
            .function
            .rsplit_once("::")
            .ok_or_else(|| format!("entry '{}' is not module-qualified", declared.name))?;
        let module = canonical_module(&loaded.root.name, &loaded.root.version, module_path);
        let exported = compilation
            .exported_functions
            .iter()
            .find(|function| function.module == module && function.function == function_name)
            .ok_or_else(|| {
                format!(
                    "manifest.entry: entry '{}' does not name an exported function",
                    declared.name
                )
            })?;
        if exported.parameter_types.len() > 1 {
            return Err(format!(
                "manifest.entry: entry '{}' must have zero or one parameter",
                declared.name
            ));
        }
        let (input_type, input_spelling) = match exported.parameter_types.as_slice() {
            [] => (ValueType::Unit, "Void"),
            [input] => (input.clone(), exported.parameter_spellings[0].as_str()),
            _ => unreachable!("parameter count was checked"),
        };
        if declared.input != input_spelling || declared.output != exported.return_spelling {
            return Err(format!(
                "manifest.entry: entry '{}' declares {} -> {}, but the function is {} -> {}",
                declared.name,
                declared.input,
                declared.output,
                input_spelling,
                exported.return_spelling
            ));
        }
        reject_boundary_type(&input_type, &declared.name)?;
        reject_boundary_type(&exported.return_type, &declared.name)?;
        let input_schema = push_schema(&mut schemas, input_type)?;
        let output_schema = push_schema(&mut schemas, exported.return_type.clone())?;
        let input_validators = exported.input_validators.clone();
        let output_validators = exported.output_validators.clone();
        let input_record_provenance = compute_entry_record_provenance(
            &schemas[input_schema as usize],
            &compilation.module.enum_types,
            &input_validators,
            &compilation.record_invariants,
        )
        .map_err(|error| error.to_string())?;
        let output_record_provenance = compute_entry_record_provenance(
            &schemas[output_schema as usize],
            &compilation.module.enum_types,
            &output_validators,
            &compilation.record_invariants,
        )
        .map_err(|error| error.to_string())?;
        entries.push(EntryContract {
            name: declared.name.clone(),
            function: exported.function_id,
            input_schema,
            output_schema,
            input_contract_digest: compute_entry_contract_digest(
                &schemas[input_schema as usize],
                &input_validators,
                &compilation.record_invariants,
            ),
            output_contract_digest: compute_entry_contract_digest(
                &schemas[output_schema as usize],
                &output_validators,
                &compilation.record_invariants,
            ),
            input_validators,
            output_validators,
            input_record_provenance,
            output_record_provenance,
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));

    for function in &compilation.module.functions {
        for instruction in &function.code {
            if let Some(output) = typed_response_output_type(function, instruction) {
                push_schema(&mut schemas, output.clone())?;
            }
        }
    }

    let mut module = compilation.module;
    if let Some(first) = entries.first() {
        module.entry = first.function;
    }
    let effects = compilation
        .effect_report
        .iter()
        .map(|entry| format_effect_report_entry(&entry.module, &entry.function, &entry.effects))
        .collect();
    let tool_contract_digest = compute_tool_contract_digest(&prepared.contracts);
    let artifact = Artifact {
        metadata: ArtifactMetadata {
            bytecode_version: BYTECODE_VERSION,
            ..ArtifactMetadata::default()
        },
        module,
        debug: Some(compilation.debug),
        schemas,
        entries,
        imports: import_contracts(loaded)?,
        manifest: Some(ManifestContract {
            package: root_package.id.name.clone(),
            version: root_package.id.version.clone(),
            language_requirement: root_package.manifest.package.language.clone(),
            required_capabilities: root_package.manifest.capabilities.required.clone(),
            optional_capabilities: root_package.manifest.capabilities.optional.clone(),
            https_origins: root_package.manifest.network.http_get.origins.clone(),
            exec_commands: root_package.manifest.exec_commands().to_vec(),
            exec_environment: root_package.manifest.exec_environment().to_vec(),
            limits: manifest_limits(&root_package.manifest.limits),
            required_tools: prepared.contracts,
            tool_contract_digest,
        }),
        templates,
        record_invariants: compilation.record_invariants,
    };
    Ok(CompiledPackage {
        artifact,
        effects,
        requested_exec_commands: root_package.manifest.exec_commands().to_vec(),
        requested_exec_environment: root_package.manifest.exec_environment().to_vec(),
    })
}

fn format_effect_report_entry(module: &str, function: &str, effects: &[String]) -> String {
    let name = format!("{module}::{function}");
    if effects.is_empty() {
        name
    } else {
        format!("{name} effects [{}]", effects.join(", "))
    }
}

fn canonical_module(name: &str, version: &str, path: &str) -> String {
    format!("pkg://{name}@{version}/{path}")
}

fn import_targets(loaded: &LoadedPackage) -> Result<BTreeMap<(String, String), String>, String> {
    let packages = loaded
        .packages
        .iter()
        .map(|package| (package.id.clone(), package))
        .collect::<BTreeMap<_, _>>();
    let mut targets = BTreeMap::new();
    for package in &loaded.packages {
        for dependency in &package.dependencies {
            let target = packages.get(&dependency.package).ok_or_else(|| {
                format!("locked dependency '{}' has no package", dependency.alias)
            })?;
            for source in &package.modules {
                for target_module in &target.modules {
                    targets.insert(
                        (
                            source.identity.clone(),
                            format!("{}/{}", dependency.alias, target_module.path),
                        ),
                        target_module.identity.clone(),
                    );
                }
            }
        }
    }
    Ok(targets)
}

fn import_contracts(loaded: &LoadedPackage) -> Result<Vec<ImportContract>, String> {
    let packages = loaded
        .packages
        .iter()
        .map(|package| (package.id.clone(), package))
        .collect::<BTreeMap<_, _>>();
    let mut contracts = Vec::new();
    for package in &loaded.packages {
        for dependency in &package.dependencies {
            let target = packages.get(&dependency.package).ok_or_else(|| {
                format!("locked dependency '{}' has no package", dependency.alias)
            })?;
            let digest = decode_digest(&target.digest)?;
            for module in &target.modules {
                contracts.push(ImportContract {
                    importer: package.id.canonical(),
                    alias: dependency.alias.clone(),
                    package: target.id.name.clone(),
                    version: target.id.version.clone(),
                    module: module.path.clone(),
                    content_digest: digest,
                });
            }
        }
    }
    contracts.sort_by(|left, right| {
        (
            &left.importer,
            &left.alias,
            &left.package,
            &left.version,
            &left.module,
        )
            .cmp(&(
                &right.importer,
                &right.alias,
                &right.package,
                &right.version,
                &right.module,
            ))
    });
    contracts.dedup();
    Ok(contracts)
}

fn decode_digest(value: &str) -> Result<[u8; 32], String> {
    let hex = value
        .strip_prefix("sha256:")
        .ok_or_else(|| "locked digest is not SHA-256".to_owned())?;
    if hex.len() != 64 {
        return Err("locked SHA-256 digest has the wrong length".to_owned());
    }
    let mut output = [0_u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| "locked SHA-256 digest is malformed".to_owned())?;
    }
    Ok(output)
}

fn push_schema(schemas: &mut Vec<StrictSchema>, value_type: ValueType) -> Result<u32, String> {
    if let Some(index) = schemas
        .iter()
        .position(|schema| schema.value_type == value_type)
    {
        return u32::try_from(index).map_err(|_| "too many schemas".to_owned());
    }
    let index = u32::try_from(schemas.len()).map_err(|_| "too many schemas".to_owned())?;
    schemas.push(StrictSchema { value_type });
    Ok(index)
}

fn push_typed_response_schemas(
    schemas: &mut Vec<StrictSchema>,
    compilation: &Compilation,
) -> Result<(), String> {
    for function in &compilation.module.functions {
        for instruction in &function.code {
            if let Some(output) = typed_response_output_type(function, instruction) {
                push_schema(schemas, output.clone())?;
            }
        }
    }
    Ok(())
}

fn reject_boundary_type(value: &ValueType, entry: &str) -> Result<(), String> {
    let invalid = match value {
        ValueType::Function { .. }
        | ValueType::Future(_)
        | ValueType::Task(_)
        | ValueType::Workspace
        | ValueType::ExternalFsAccess
        | ValueType::SubAgent
        | ValueType::Range
        | ValueType::Sequence(_)
        | ValueType::Unknown
        | ValueType::Never => true,
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Newtype {
            underlying: value, ..
        } => reject_boundary_type(value, entry).is_err(),
        ValueType::Map(key, value) | ValueType::Result(key, value) => {
            reject_boundary_type(key, entry).is_err() || reject_boundary_type(value, entry).is_err()
        }
        ValueType::Tuple(values) => values
            .iter()
            .any(|value| reject_boundary_type(value, entry).is_err()),
        ValueType::Record(fields) => fields
            .iter()
            .any(|field| reject_boundary_type(&field.value_type, entry).is_err()),
        ValueType::Enum(_)
        | ValueType::Int
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::Unit => false,
    };
    if invalid {
        Err(format!(
            "manifest.entry: entry '{entry}' uses a non-serializable boundary type"
        ))
    } else {
        Ok(())
    }
}

fn manifest_limits(limits: &ManifestLimits) -> Vec<(String, u64)> {
    let mut values = Vec::new();
    macro_rules! add {
        ($name:literal, $value:expr) => {
            if let Some(value) = $value {
                values.push(($name.to_owned(), u64::from(value)));
            }
        };
    }
    add!("wall_ms", limits.wall_ms);
    add!("instructions", limits.instructions);
    add!("heap_bytes", limits.heap_bytes);
    add!("maximum_allocation_bytes", limits.maximum_allocation_bytes);
    add!("call_depth", limits.call_depth);
    add!("tasks", limits.tasks);
    add!("concurrent_effects", limits.concurrent_effects);
    add!("response_attempts", limits.response_attempts);
    add!("effects", limits.effects);
    add!("cleanup_instructions", limits.cleanup_instructions);
    add!("input_bytes", limits.input_bytes);
    add!("output_bytes", limits.output_bytes);
    add!("fs_operations", limits.fs_operations);
    add!("fs_read_bytes", limits.fs_read_bytes);
    add!("fs_write_bytes", limits.fs_write_bytes);
    add!("fs_file_bytes", limits.fs_file_bytes);
    add!("fs_entries", limits.fs_entries);
    add!("http_requests", limits.http_requests);
    add!("http_redirects", limits.http_redirects);
    add!("http_dns_addresses", limits.http_dns_addresses);
    add!("http_response_headers", limits.http_response_headers);
    add!(
        "http_response_header_bytes",
        limits.http_response_header_bytes
    );
    add!("http_compressed_bytes", limits.http_compressed_bytes);
    add!("http_decoded_bytes", limits.http_decoded_bytes);
    add!("http_decompression_ratio", limits.http_decompression_ratio);
    add!("http_connect_ms", limits.http_connect_ms);
    add!("http_first_byte_ms", limits.http_first_byte_ms);
    add!("http_idle_ms", limits.http_idle_ms);
    add!("http_total_ms", limits.http_total_ms);
    values.sort_by(|left, right| left.0.cmp(&right.0));
    values
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new() -> Self {
            let nonce = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "allen-compiler-root-package-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(path.join("src")).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn collect_newtype_names<'a>(
        value_type: &'a ValueType,
        names: &mut std::collections::BTreeSet<&'a str>,
    ) {
        match value_type {
            ValueType::List(value)
            | ValueType::Option(value)
            | ValueType::Future(value)
            | ValueType::Task(value)
            | ValueType::Sequence(value) => collect_newtype_names(value, names),
            ValueType::Map(key, value) | ValueType::Result(key, value) => {
                collect_newtype_names(key, names);
                collect_newtype_names(value, names);
            }
            ValueType::Tuple(values) => {
                for value in values {
                    collect_newtype_names(value, names);
                }
            }
            ValueType::Record(fields) => {
                for field in fields {
                    collect_newtype_names(&field.value_type, names);
                }
            }
            ValueType::Newtype { name, underlying } => {
                names.insert(name);
                collect_newtype_names(underlying, names);
            }
            ValueType::Function {
                parameters,
                return_type,
                ..
            } => {
                for parameter in parameters {
                    collect_newtype_names(parameter, names);
                }
                collect_newtype_names(return_type, names);
            }
            ValueType::Int
            | ValueType::Range
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

    #[test]
    fn memory_and_filesystem_root_packages_produce_identical_artifact_bytes() {
        let manifest = r#"[package]
name = "memory-parity"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "String"

[capabilities]
required = []
optional = []
"#;
        let source = r#"export fn main() returns String { "same" }
"#;
        let fixture = Fixture::new();
        fs::write(fixture.0.join("allen.toml"), manifest).unwrap();
        fs::write(fixture.0.join("src/main.allen"), source).unwrap();
        let limits = LoadLimits::default();
        let lock = allen_package::generate_lock(&fixture.0, &limits).unwrap();
        fs::write(fixture.0.join("allen.lock"), &lock).unwrap();
        let loaded = allen_package::load_verified_package(&fixture.0, &lock, &limits).unwrap();
        let filesystem = assemble_loaded_package(&loaded, None).unwrap();
        let memory = assemble_root_source_package(
            manifest,
            &BTreeMap::from([("src/main.allen".to_owned(), source.to_owned())]),
            Some(&lock),
            None,
            &limits,
        )
        .unwrap();
        assert_eq!(
            memory.artifact.metadata.bytecode_version,
            allen_bytecode::BYTECODE_VERSION
        );
        let filesystem_bytes = allen_bytecode::encode(&filesystem.artifact).unwrap();
        let memory_bytes = allen_bytecode::encode(&memory.artifact).unwrap();
        assert_eq!(memory_bytes, filesystem_bytes);
        let decoded =
            allen_bytecode::decode(&memory_bytes, &allen_bytecode::DecodeLimits::default())
                .unwrap();
        assert_eq!(decoded.artifact(), &memory.artifact);
        allen_bytecode::decode_and_verify(&memory_bytes, &allen_bytecode::DecodeLimits::default())
            .unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn filesystem_templates_compile_embed_verify_and_render_without_runtime_files() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("templates")).unwrap();
        fs::create_dir_all(fixture.0.join("packages/words/src")).unwrap();
        fs::create_dir_all(fixture.0.join("packages/words/templates")).unwrap();
        fs::write(
            fixture.0.join("allen.toml"),
            r#"[package]
name = "template-app"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "String"

[[templates]]
name = "notice"
path = "templates/notice.txt"
holes = { count = "Int", enabled = "Bool", name = "String", ratio = "Float" }

[dependencies.words]
path = "packages/words"
version = "1.0.0"
"#,
        )
        .unwrap();
        fs::write(
            fixture.0.join("src/main.allen"),
            r#"import { unused } from "words/src/words.allen";

newtype Count = Int

export fn main() returns String {
  let dependency_render = unused();
  templates.notice.render({ name: "Zoë", count: Count(7), enabled: true, ratio: 1.5 })
}
"#,
        )
        .unwrap();
        fs::write(
            fixture.0.join("templates/notice.txt"),
            "hé {{name}}/{{name}} count={{count}} enabled={{enabled}} ratio={{ratio}}",
        )
        .unwrap();
        fs::write(
            fixture.0.join("packages/words/allen.toml"),
            r#"[package]
name = "words"
version = "1.0.0"
language = ">=0.1.0, <0.2.0"

[[templates]]
name = "notice"
path = "templates/notice.txt"
holes = { name = "String" }
"#,
        )
        .unwrap();
        fs::write(
            fixture.0.join("packages/words/src/words.allen"),
            "export fn unused() returns String { templates.notice.render({ name: \"dep\" }) }\n",
        )
        .unwrap();
        fs::write(
            fixture.0.join("packages/words/templates/notice.txt"),
            "dependency {{name}}",
        )
        .unwrap();

        let limits = LoadLimits::default();
        let lock = allen_package::generate_lock(&fixture.0, &limits).unwrap();
        let loaded = allen_package::load_verified_package(&fixture.0, &lock, &limits).unwrap();
        let package = assemble_loaded_package(&loaded, None).unwrap();
        assert_eq!(
            package
                .artifact
                .templates
                .iter()
                .map(|template| template.identity.as_str())
                .collect::<Vec<_>>(),
            [
                "pkg://template-app@0.1.0/templates/notice",
                "pkg://words@1.0.0/templates/notice",
            ]
        );
        assert!(package.artifact.module.functions.iter().any(|function| {
            function.code.iter().any(|instruction| {
                matches!(instruction, Instruction::TemplateRender { template: 0, arguments, .. } if arguments.len() == 4)
            })
        }));
        assert!(package.artifact.module.functions.iter().any(|function| {
            function.code.iter().any(|instruction| {
                matches!(instruction, Instruction::TemplateRender { template: 1, arguments, .. } if arguments.len() == 1)
            })
        }));
        let bytes = allen_bytecode::encode(&package.artifact).unwrap();
        let verified =
            allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
                .unwrap();
        fs::remove_file(fixture.0.join("templates/notice.txt")).unwrap();
        fs::remove_file(fixture.0.join("packages/words/templates/notice.txt")).unwrap();
        let result = allen_vm::execute_verified_artifact(&verified).unwrap();
        assert_eq!(
            result.value,
            allen_vm::Value::String("hé Zoë/Zoë count=7 enabled=true ratio=1.5".into())
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn package_source_tests_keep_reachable_imports_and_package_local_templates() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("templates")).unwrap();
        fs::create_dir_all(fixture.0.join("packages/words/src")).unwrap();
        fs::create_dir_all(fixture.0.join("packages/words/templates")).unwrap();
        fs::create_dir_all(fixture.0.join("packages/unused/src")).unwrap();
        fs::write(
            fixture.0.join("allen.toml"),
            r#"[package]
name = "test-app"
version = "0.1.0"
language = "^0.1"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Void"

[[templates]]
name = "notice"
path = "templates/notice.txt"
holes = { name = "String" }

[dependencies.words]
path = "packages/words"
version = "=1.0.0"

[dependencies.unused]
path = "packages/unused"
version = "=1.0.0"
"#,
        )
        .unwrap();
        fs::write(
            fixture.0.join("src/main.allen"),
            r#"import { dependency_notice } from "words/src/words.allen";
export fn main() returns Void { () }
test "root templates and import" {
  let local = templates.notice.render({ name: "root" });
  let dependency = dependency_notice();
  if (local == "root root" && dependency == "dependency dep") { () } else { fail("wrong template") }
}
"#,
        )
        .unwrap();
        fs::write(fixture.0.join("templates/notice.txt"), "root {{name}}").unwrap();
        fs::write(
            fixture.0.join("packages/words/allen.toml"),
            r#"[package]
name = "words"
version = "1.0.0"
language = "^0.1"

[[templates]]
name = "notice"
path = "templates/notice.txt"
holes = { name = "String" }
"#,
        )
        .unwrap();
        fs::write(
            fixture.0.join("packages/words/src/words.allen"),
            r#"export fn dependency_notice() returns String { templates.notice.render({ name: "dep" }) }
test "dependency template" {
  if (dependency_notice() == "dependency dep") { () } else { fail("wrong dependency template") }
}
"#,
        )
        .unwrap();
        fs::write(
            fixture.0.join("packages/words/templates/notice.txt"),
            "dependency {{name}}",
        )
        .unwrap();
        fs::write(
            fixture.0.join("packages/unused/allen.toml"),
            "[package]\nname = \"unused\"\nversion = \"1.0.0\"\nlanguage = \"^0.1\"\n",
        )
        .unwrap();
        fs::write(
            fixture.0.join("packages/unused/src/unused.allen"),
            "export fn unused() returns Int { 0 }\n",
        )
        .unwrap();

        let limits = LoadLimits::default();
        let lock = allen_package::generate_lock(&fixture.0, &limits).unwrap();
        let loaded = allen_package::load_verified_package(&fixture.0, &lock, &limits).unwrap();
        let discovery = prepare_loaded_source_tests(&loaded).unwrap();
        let tests = crate::discover_source_tests(&discovery).unwrap();
        assert_eq!(tests.len(), 2);

        for test in tests {
            let assembled =
                assemble_loaded_source_test(&loaded, None, &test.module, &test.name).unwrap();
            let bytes = allen_bytecode::encode(&assembled.package.artifact).unwrap();
            let verified =
                allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
                    .unwrap();
            assert_eq!(verified.entries()[0].name, "test");
            if test.name == "root templates and import" {
                assert_eq!(verified.imports().len(), 1);
                assert_eq!(verified.templates().len(), 2);
            } else {
                assert!(verified.imports().is_empty());
                assert_eq!(verified.templates().len(), 1);
                assert!(
                    verified.templates()[0]
                        .identity
                        .starts_with("pkg://words@1.0.0/")
                );
            }
        }
    }

    #[test]
    fn inline_artifact_canonicalizes_nested_newtype_identities_before_schema_digests() {
        let source = r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
}

newtype Count = Int
enum Payload { Empty Values(List<Option<Count>>) }

fn z_echo(value: Option<Count>) returns Option<Count> { value }

export fn main(input: List<Option<Count>>) returns (Count, Payload) {
  (Count(7), Payload.Values(input))
}
"#;
        let catalog =
            FrozenCatalog::freeze(Vec::new(), &allen_schema::CatalogLimits::default()).unwrap();
        let package = assemble_inline_source(source, &catalog).unwrap();
        let expected_identity = "pkg://inline@0.1.0/src/main.allen::Count";
        let mut identities = std::collections::BTreeSet::new();

        for schema in &package.artifact.schemas {
            collect_newtype_names(&schema.value_type, &mut identities);
        }
        for function in &package.artifact.module.functions {
            for value_type in &function.registers {
                collect_newtype_names(value_type, &mut identities);
            }
            collect_newtype_names(&function.return_type, &mut identities);
        }
        for enum_type in &package.artifact.module.enum_types {
            for variant in &enum_type.variants {
                match &variant.payload {
                    allen_bytecode::EnumPayloadType::Unit => {}
                    allen_bytecode::EnumPayloadType::Tuple(values) => {
                        for value_type in values {
                            collect_newtype_names(value_type, &mut identities);
                        }
                    }
                    allen_bytecode::EnumPayloadType::Record(fields) => {
                        for field in fields {
                            collect_newtype_names(&field.value_type, &mut identities);
                        }
                    }
                }
            }
        }
        assert_eq!(
            identities,
            std::collections::BTreeSet::from([expected_identity])
        );

        let entry = &package.artifact.entries[0];
        let input_schema = &package.artifact.schemas[entry.input_schema as usize];
        assert_eq!(
            input_schema.value_type,
            ValueType::List(Box::new(ValueType::Option(Box::new(ValueType::Newtype {
                name: expected_identity.to_owned(),
                underlying: Box::new(ValueType::Int),
            }))))
        );
        let local_schema = StrictSchema {
            value_type: ValueType::List(Box::new(ValueType::Option(Box::new(
                ValueType::Newtype {
                    name: "main.allen::Count".to_owned(),
                    underlying: Box::new(ValueType::Int),
                },
            )))),
        };
        assert_ne!(
            allen_bytecode::compute_strict_schema_digest(input_schema),
            allen_bytecode::compute_strict_schema_digest(&local_schema)
        );

        let bytes = allen_bytecode::encode(&package.artifact).unwrap();
        let decoded =
            allen_bytecode::decode(&bytes, &allen_bytecode::DecodeLimits::default()).unwrap();
        assert_eq!(decoded.artifact(), &package.artifact);
        assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
        allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
            .unwrap();
    }

    #[test]
    fn inline_record_invariant_rebases_definition_and_newtype_metadata_before_digests() {
        let source = r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
}

newtype Count = Int
record Range { max: Count, min: Count } where { min <= max }
export fn main(input: Range) returns Range { input }
"#;
        let catalog =
            FrozenCatalog::freeze(Vec::new(), &allen_schema::CatalogLimits::default()).unwrap();
        let package = assemble_inline_source(source, &catalog).unwrap();
        let invariant = package.artifact.record_invariants.first().unwrap();
        let range_identity = "pkg://inline@0.1.0/src/main.allen::Range";
        let count_identity = "pkg://inline@0.1.0/src/main.allen::Count";
        assert_eq!(invariant.identity, range_identity);
        assert!(invariant.fields.iter().all(|field| {
            matches!(
                &field.value_type,
                ValueType::Newtype { name, underlying }
                    if name == count_identity && underlying.as_ref() == &ValueType::Int
            )
        }));
        let ValidatorExpr::Compare { left, right, .. } = &invariant.predicate else {
            panic!("expected comparison invariant")
        };
        for expression in [left.as_ref(), right.as_ref()] {
            assert!(matches!(
                expression,
                ValidatorExpr::Field {
                    value_type: ValueType::Newtype { name, underlying },
                    ..
                } if name == count_identity && underlying.as_ref() == &ValueType::Int
            ));
        }

        let entry = &package.artifact.entries[0];
        assert_eq!(
            entry.input_contract_digest,
            compute_entry_contract_digest(
                &package.artifact.schemas[entry.input_schema as usize],
                &entry.input_validators,
                &package.artifact.record_invariants,
            )
        );
        assert_eq!(
            entry.output_contract_digest,
            compute_entry_contract_digest(
                &package.artifact.schemas[entry.output_schema as usize],
                &entry.output_validators,
                &package.artifact.record_invariants,
            )
        );
        let bytes = allen_bytecode::encode(&package.artifact).unwrap();
        let verified =
            allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
                .unwrap();
        assert_eq!(
            verified.record_invariants(),
            package.artifact.record_invariants
        );
        assert_eq!(verified.entries(), package.artifact.entries);
    }

    #[test]
    fn inline_scalar_record_invariant_has_canonical_definition_identity() {
        let source = r#"manifest {
  language: "0.1"
  entry: main
  capabilities: []
}

record Range { max: Int, min: Int } where { min <= max }
export fn main(input: Range) returns Range { input }
"#;
        let catalog =
            FrozenCatalog::freeze(Vec::new(), &allen_schema::CatalogLimits::default()).unwrap();
        let package = assemble_inline_source(source, &catalog).unwrap();
        assert_eq!(
            package.artifact.record_invariants[0].identity,
            "pkg://inline@0.1.0/src/main.allen::Range"
        );
        allen_bytecode::decode_and_verify(
            &allen_bytecode::encode(&package.artifact).unwrap(),
            &allen_bytecode::DecodeLimits::default(),
        )
        .unwrap();
    }

    #[test]
    fn compiled_package_carries_only_canonical_root_exec_requests() {
        let manifest = r#"[package]
name = "exec-package"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Void"

[exec]
commands = ["git status", "aws cloudwatch *", "git status"]
environment = ["HOME", "AWS_REGION", "HOME"]
"#;
        let package = assemble_root_source_package(
            manifest,
            &BTreeMap::from([(
                "src/main.allen".to_owned(),
                "export fn main() returns Void { () }".to_owned(),
            )]),
            None,
            None,
            &LoadLimits::default(),
        )
        .unwrap();

        assert_eq!(
            package.requested_exec_commands,
            ["aws cloudwatch *", "git status"]
        );
        assert_eq!(package.requested_exec_environment, ["AWS_REGION", "HOME"]);
    }

    #[test]
    fn memory_root_rejects_dependencies_and_unsafe_source_paths() {
        let manifest = r#"[package]
name = "memory-negative"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Void"

[dependencies.other]
path = "packages/other"
version = "=1.0.0"
"#;
        let sources = BTreeMap::from([(
            "src/main.allen".to_owned(),
            "export fn main() returns Void { () }".to_owned(),
        )]);
        assert!(
            assemble_root_source_package(manifest, &sources, None, None, &LoadLimits::default())
                .is_err()
        );

        let manifest = manifest.split("[dependencies.other]").next().unwrap();
        let unsafe_sources = BTreeMap::from([(
            "src/../main.allen".to_owned(),
            "export fn main() returns Void { () }".to_owned(),
        )]);
        assert!(
            assemble_root_source_package(
                manifest,
                &unsafe_sources,
                None,
                None,
                &LoadLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn agent_package_emits_a_verified_current_artifact() {
        let manifest = r#"[package]
name = "agent-package"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Result<String, AgentError>"

[capabilities]
required = ["agent.ask"]
optional = []
"#;
        let source = r#"export async fn main() returns Result<String, AgentError> effects [agent.ask] {
  await agent.ask(prompt { system: "continue?", output: String })
}
"#;
        let package = assemble_root_source_package(
            manifest,
            &BTreeMap::from([("src/main.allen".to_owned(), source.to_owned())]),
            None,
            None,
            &LoadLimits::default(),
        )
        .unwrap();
        assert_eq!(
            package.artifact.metadata.bytecode_version,
            allen_bytecode::BYTECODE_VERSION
        );
        assert!(package.artifact.module.functions.iter().any(|function| {
            function.code.iter().any(|instruction| {
                matches!(
                    instruction,
                    allen_bytecode::Instruction::EffectCall {
                        operation: allen_bytecode::EffectOperation::AgentAsk,
                        ..
                    }
                )
            })
        }));
        let bytes = allen_bytecode::encode(&package.artifact).unwrap();
        allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
            .unwrap();
    }

    #[test]
    fn typed_prompt_package_embeds_its_private_response_schema() {
        let manifest = r#"[package]
name = "typed-prompt-package"
version = "0.1.0"
language = ">=0.1.0, <0.2.0"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "Void"
output = "Void"

[capabilities]
required = ["agent.ask"]
optional = []

[limits]
response_attempts = 2
"#;
        let source = r#"record Review { approved: Bool, reason: String }
export async fn main() returns Void effects [agent.ask] {
  let review = await agent.ask<Review>(prompt {
    system: "Review."
    data: { change: "bounded" }
    output: Review
    policy: { max_attempts: 2 }
  });
  ()
}
"#;
        let package = assemble_root_source_package(
            manifest,
            &BTreeMap::from([("src/main.allen".to_owned(), source.to_owned())]),
            None,
            None,
            &LoadLimits::default(),
        )
        .unwrap();
        let review = ValueType::Record(vec![
            allen_bytecode::RecordField {
                name: "approved".to_owned(),
                value_type: ValueType::Bool,
            },
            allen_bytecode::RecordField {
                name: "reason".to_owned(),
                value_type: ValueType::String,
            },
        ]);
        assert!(
            package
                .artifact
                .schemas
                .iter()
                .any(|schema| schema.value_type == review)
        );
        assert!(package.artifact.manifest.as_ref().is_some_and(|manifest| {
            manifest.limits == vec![("response_attempts".to_owned(), 2)]
        }));
        let bytes = allen_bytecode::encode(&package.artifact).unwrap();
        allen_bytecode::decode_and_verify(&bytes, &allen_bytecode::DecodeLimits::default())
            .unwrap();
    }
}
