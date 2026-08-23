//! Catalog-aware package-to-artifact assembly shared by CLI and JOSH.

use std::collections::BTreeMap;

use allen_bytecode::{
    Artifact, ArtifactMetadata, BYTECODE_VERSION, EntryContract, ImportContract, ManifestContract,
    StrictSchema, ValueType, compute_tool_contract_digest, typed_response_output_type,
};
use allen_package::{
    LoadLimits, LoadedPackage, ManifestLimits, canonical_https_origin, load_verified_root_package,
};
use allen_schema::FrozenCatalog;

use crate::{
    Compilation, InlineManifest, PackageEntryPoint, PackageSourceBundle, PreparedTools,
    compile_inline_manifest_source_with_catalog, compile_package_bundle_with_prepared_tools,
    prepare_tools, render_diagnostic,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledPackage {
    pub artifact: Artifact,
    pub effects: Vec<String>,
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
    assemble_standalone_compilation("main.allen", manifest, compilation, prepared)
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
    assemble_standalone_compilation(root, manifest, compilation, PreparedTools::default())
}

#[allow(clippy::too_many_lines)]
fn assemble_standalone_compilation(
    root: &str,
    manifest: InlineManifest,
    compilation: Compilation,
    prepared: PreparedTools,
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
    let mut debug = compilation.debug;
    canonicalize_inline_modules(&mut module, &mut debug);
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
        }],
        imports: Vec::new(),
        manifest: Some(ManifestContract {
            package: "inline".to_owned(),
            version: "0.1.0".to_owned(),
            language_requirement: manifest.language,
            required_capabilities: capabilities,
            optional_capabilities: Vec::new(),
            https_origins: manifest.http_origins,
            limits: Vec::new(),
            required_tools: prepared.contracts,
            tool_contract_digest,
        }),
    };
    Ok(CompiledPackage { artifact, effects })
}

fn canonicalize_inline_modules(
    module: &mut allen_bytecode::Module,
    debug: &mut allen_bytecode::DebugInfo,
) {
    let identities = debug
        .sources
        .iter()
        .map(|source| {
            (
                source.clone(),
                format!("pkg://inline@0.1.0/src/{source}"),
                inline_module_symbol(source),
            )
        })
        .collect::<Vec<_>>();
    for function in &mut module.functions {
        if let Some((source, _, symbol)) = identities.iter().find(|(source, _, _)| {
            function
                .name
                .strip_prefix(source)
                .is_some_and(|suffix| suffix.starts_with("::"))
        }) {
            let suffix = function
                .name
                .strip_prefix(source)
                .expect("the source prefix was matched");
            function.name = format!("{symbol}{suffix}");
        }
    }
    for enum_type in &mut module.enum_types {
        if let Some((source, uri, _)) = identities.iter().find(|(source, _, _)| {
            enum_type
                .name
                .strip_prefix(source)
                .is_some_and(|suffix| suffix.starts_with("::"))
        }) {
            let suffix = enum_type
                .name
                .strip_prefix(source)
                .expect("the source prefix was matched");
            enum_type.name = format!("{uri}{suffix}");
        }
    }
    for source in &mut debug.sources {
        let (_, uri, _) = identities
            .iter()
            .find(|(original, _, _)| original == source)
            .expect("every debug source has a canonical inline identity");
        source.clone_from(uri);
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
    let (bundle, mut prepared) = prepare_loaded_package(loaded, catalog)?;
    let compilation = compile_package_bundle_with_prepared_tools(&bundle, &mut prepared)
        .map_err(|diagnostics| render_package_diagnostics(&bundle, diagnostics))?;
    finish_loaded_package(loaded, compilation, prepared)
}

#[allow(clippy::too_many_lines)]
fn prepare_loaded_package(
    loaded: &LoadedPackage,
    catalog: Option<&FrozenCatalog>,
) -> Result<(PackageSourceBundle, PreparedTools), String> {
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
    Ok((bundle, prepared))
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

#[allow(clippy::too_many_lines)]
fn finish_loaded_package(
    loaded: &LoadedPackage,
    compilation: Compilation,
    prepared: PreparedTools,
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
        entries.push(EntryContract {
            name: declared.name.clone(),
            function: exported.function_id,
            input_schema,
            output_schema,
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
            limits: manifest_limits(&root_package.manifest.limits),
            required_tools: prepared.contracts,
            tool_contract_digest,
        }),
    };
    Ok(CompiledPackage { artifact, effects })
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
        | ValueType::Unknown
        | ValueType::Never => true,
        ValueType::List(value) | ValueType::Option(value) => {
            reject_boundary_type(value, entry).is_err()
        }
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
