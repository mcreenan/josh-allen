//! Module/name resolution, effect discovery, and exact semantic type resolution.

use super::checking::{
    SemanticType, contains_affine, contains_stored_sub_agent, contains_sub_agent,
    contains_workspace, effect_id, valid_newtype_underlying, validate_declared_type_shapes,
};
use super::{
    BTreeMap, BTreeSet, CapabilityOperation, CheckedIntOperation, CollectionOperation,
    CompilerTemplateBinding, CompilerToolBinding, Diagnostic, EffectOperation, EffectSetId,
    EnumPayloadType, EnumType, EnumVariant, FunctionId, ListCombinator, LoweredBody, LoweredElse,
    LoweredEnumPayload, LoweredEnumValuePayload, LoweredExpr, LoweredExprKind, LoweredForSource,
    LoweredFunction, LoweredImport, LoweredLoopBinding, LoweredModule, LoweredPattern,
    LoweredStatement, LoweredType, LoweredTypeDeclaration, MAX_VALUE_NESTING, PackageEntryPoint,
    RecordField, SafeCollectionOperation, Span, StandardOperation, StringOperation, SymbolId,
    ValueType, agent_error_type, external_directory_request_type, external_file_request_type,
    file_error_type, http_response_type, is_strict_schema_type, mangle_source_segment,
    model_error_type, network_error_type, normalize_root, permission_error_type, prompt_type,
    search_match_type, sub_agent_error_type, syntax_lowering, template_interpolations,
    transcript_message_type, transcript_part_enum_type, transcript_query_type,
    transcript_snapshot_type, user_error_type,
};

mod type_aliases;
pub(super) use type_aliases::resolve_named_type;
use type_aliases::{
    builtin_semantic_type, has_pending_alias_dependency, resolve_alias_target,
    resolve_named_semantic_type, resolve_record_layout, validate_alias_cycles,
};

pub(super) fn is_canonical_effect(effect: &str) -> bool {
    let (name, version) = effect
        .rsplit_once('@')
        .map_or((effect, None), |(name, version)| (name, Some(version)));
    if version.is_some_and(|version| {
        version.is_empty()
            || version.starts_with('0')
            || !version.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return false;
    }
    !name.is_empty()
        && name.split('.').all(|segment| {
            let mut bytes = segment.bytes();
            bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
                && bytes
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
}

#[derive(Clone, Debug)]
pub(super) struct FunctionInfo {
    pub(super) module: String,
    pub(super) symbol: SymbolId,
    pub(super) bytecode: Option<FunctionId>,
    pub(super) lowered: LoweredFunction,
    pub(super) parameters: Vec<SemanticType>,
    pub(super) return_type: SemanticType,
    pub(super) effects: Vec<String>,
    pub(super) is_const: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedBundle {
    pub(super) modules: BTreeMap<String, LoweredModule>,
    pub(super) functions: Vec<FunctionInfo>,
    pub(super) names: BTreeMap<(String, String), SymbolId>,
    pub(super) types: BTreeMap<(String, String), ValueType>,
    pub(super) enum_types: Vec<EnumType>,
    pub(super) transcript_part: Option<u32>,
    pub(super) tools: BTreeMap<Vec<String>, CompilerToolBinding>,
    pub(super) templates: BTreeMap<(String, String), CompilerTemplateBinding>,
    pub(super) deferred_effect_sets: Vec<Vec<String>>,
}

const DEFERRED_EFFECT_SET_BASE: EffectSetId = 1 << 31;

pub(super) fn collect_type_effect_sets(value_type: &LoweredType, sets: &mut BTreeSet<Vec<String>>) {
    match value_type {
        LoweredType::Tuple(elements, _) => {
            for element in elements {
                collect_type_effect_sets(element, sets);
            }
        }
        LoweredType::Record(fields, _) => {
            for (_, field, _) in fields {
                collect_type_effect_sets(field, sets);
            }
        }
        LoweredType::List(value, _)
        | LoweredType::Option(value, _)
        | LoweredType::Future(value, _)
        | LoweredType::Task(value, _)
        | LoweredType::Prompt(value, _)
        | LoweredType::Range(value, _)
        | LoweredType::Sequence(value, _) => collect_type_effect_sets(value, sets),
        LoweredType::Map(key, value, _) | LoweredType::Result(key, value, _) => {
            collect_type_effect_sets(key, sets);
            collect_type_effect_sets(value, sets);
        }
        LoweredType::Function {
            parameters,
            return_type,
            effects,
            ..
        } => {
            sets.insert(effects.clone());
            for parameter in parameters {
                collect_type_effect_sets(parameter, sets);
            }
            collect_type_effect_sets(return_type, sets);
        }
        LoweredType::Named(_, _) => {}
    }
}

pub(super) fn deferred_effect_set_id(
    effect_sets: &[Vec<String>],
    effects: &[String],
) -> EffectSetId {
    let index = effect_sets
        .binary_search_by(|candidate| candidate.as_slice().cmp(effects))
        .expect("record callback effect set was collected");
    DEFERRED_EFFECT_SET_BASE + u32::try_from(index).expect("deferred effect set index fits")
}

#[allow(clippy::too_many_lines)]
pub(super) fn record_field_value_type(
    lowered: &LoweredType,
    module: &str,
    modules: &BTreeMap<String, LoweredModule>,
    types: &BTreeMap<(String, String), ValueType>,
    deferred_effect_sets: &[Vec<String>],
) -> Result<ValueType, Diagnostic> {
    Ok(match lowered {
        LoweredType::Named(name, span) => {
            if let SemanticType::Value(value_type) =
                resolve_named_semantic_type(modules, types, module, name, *span)?
            {
                return Ok(value_type);
            }
            let mut alias_module = module.to_owned();
            let mut alias_target = lowered;
            while let LoweredType::Named(alias_name, alias_span) = alias_target {
                let Some((definition_module, target)) =
                    resolve_alias_target(modules, &alias_module, alias_name, *alias_span)?
                else {
                    return Err(Diagnostic::new(
                        "E3005",
                        "record field type must be concrete",
                        lowered.span(),
                    ));
                };
                alias_module = definition_module;
                alias_target = target;
            }
            return record_field_value_type(
                alias_target,
                &alias_module,
                modules,
                types,
                deferred_effect_sets,
            );
        }
        LoweredType::Tuple(elements, _) => ValueType::Tuple(
            elements
                .iter()
                .map(|element| {
                    record_field_value_type(element, module, modules, types, deferred_effect_sets)
                })
                .collect::<Result<_, _>>()?,
        ),
        LoweredType::Record(fields, _) => {
            let mut seen = BTreeSet::new();
            let mut layout = fields
                .iter()
                .map(|(name, field, span)| {
                    if !seen.insert(name.clone()) {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!("duplicate record type field '{name}'"),
                            *span,
                        ));
                    }
                    Ok(RecordField {
                        name: name.clone(),
                        value_type: record_field_value_type(
                            field,
                            module,
                            modules,
                            types,
                            deferred_effect_sets,
                        )?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            layout.sort_by(|left, right| left.name.cmp(&right.name));
            ValueType::Record(layout)
        }
        LoweredType::List(value, _) => ValueType::List(Box::new(record_field_value_type(
            value,
            module,
            modules,
            types,
            deferred_effect_sets,
        )?)),
        LoweredType::Map(key, value, _) => {
            let key = record_field_value_type(key, module, modules, types, deferred_effect_sets)?;
            if !key.is_map_key() {
                return Err(Diagnostic::new(
                    "E3011",
                    "Map requires a scalar key",
                    lowered.span(),
                ));
            }
            ValueType::Map(
                Box::new(key),
                Box::new(record_field_value_type(
                    value,
                    module,
                    modules,
                    types,
                    deferred_effect_sets,
                )?),
            )
        }
        LoweredType::Option(value, _) => ValueType::Option(Box::new(record_field_value_type(
            value,
            module,
            modules,
            types,
            deferred_effect_sets,
        )?)),
        LoweredType::Result(ok, error, _) => ValueType::Result(
            Box::new(record_field_value_type(
                ok,
                module,
                modules,
                types,
                deferred_effect_sets,
            )?),
            Box::new(record_field_value_type(
                error,
                module,
                modules,
                types,
                deferred_effect_sets,
            )?),
        ),
        LoweredType::Future(value, _) => ValueType::Future(Box::new(record_field_value_type(
            value,
            module,
            modules,
            types,
            deferred_effect_sets,
        )?)),
        LoweredType::Task(value, _) => ValueType::Task(Box::new(record_field_value_type(
            value,
            module,
            modules,
            types,
            deferred_effect_sets,
        )?)),
        LoweredType::Range(value, _) => {
            let value =
                record_field_value_type(value, module, modules, types, deferred_effect_sets)?;
            if value != ValueType::Int {
                return Err(Diagnostic::new(
                    "E3011",
                    "Range requires Int bounds",
                    lowered.span(),
                ));
            }
            ValueType::Range
        }
        LoweredType::Sequence(value, _) => ValueType::Sequence(Box::new(record_field_value_type(
            value,
            module,
            modules,
            types,
            deferred_effect_sets,
        )?)),
        LoweredType::Prompt(value, _) => {
            let value =
                record_field_value_type(value, module, modules, types, deferred_effect_sets)?;
            if !is_strict_schema_type(&value) {
                return Err(Diagnostic::new(
                    "E3011",
                    "Prompt response type is not supported by the strict schema profile",
                    lowered.span(),
                ));
            }
            prompt_type(value)
        }
        LoweredType::Function {
            parameters,
            return_type,
            effects,
            ..
        } => ValueType::Function {
            parameters: parameters
                .iter()
                .map(|parameter| {
                    record_field_value_type(parameter, module, modules, types, deferred_effect_sets)
                })
                .collect::<Result<_, _>>()?,
            return_type: Box::new(record_field_value_type(
                return_type,
                module,
                modules,
                types,
                deferred_effect_sets,
            )?),
            effects: deferred_effect_set_id(deferred_effect_sets, effects),
        },
    })
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub(super) fn resolve_import_path(
    module: &str,
    import: &LoweredImport,
) -> Result<String, Diagnostic> {
    if let Some(path) = &import.resolved_path {
        return Ok(path.clone());
    }
    if !(import.path.starts_with("./") || import.path.starts_with("../"))
        || !import.path.ends_with(".allen")
    {
        return Err(Diagnostic::new(
            "E3003",
            "import path must be relative and end with '.allen'",
            import.span,
        ));
    }
    if let Some(package_path) = module.strip_prefix("pkg://") {
        let mut components = package_path.split('/').collect::<Vec<_>>();
        if components.len() < 3 || components[0].is_empty() || components[1] != "src" {
            return Err(Diagnostic::new(
                "E3003",
                format!("invalid canonical package module path '{module}'"),
                import.span,
            ));
        }
        components.pop();
        for component in import.path.split('/') {
            match component {
                "." => {}
                ".." => {
                    if components.len() <= 2 {
                        return Err(Diagnostic::new(
                            "E3003",
                            "package-local import leaves the package source directory",
                            import.span,
                        ));
                    }
                    components.pop();
                }
                "" => {
                    return Err(Diagnostic::new(
                        "E3003",
                        "import path has an empty component",
                        import.span,
                    ));
                }
                value => components.push(value),
            }
        }
        return Ok(format!("pkg://{}", components.join("/")));
    }
    let mut components = module.split('/').collect::<Vec<_>>();
    components.pop();
    for component in import.path.split('/') {
        match component {
            "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(Diagnostic::new(
                        "E3003",
                        "import path leaves the source bundle",
                        import.span,
                    ));
                }
            }
            "" => {
                return Err(Diagnostic::new(
                    "E3003",
                    "import path has an empty component",
                    import.span,
                ));
            }
            value => components.push(value),
        }
    }
    Ok(components.join("/"))
}

pub(super) struct PreparedModule {
    pub(super) source: String,
    pub(super) module: LoweredModule,
}

pub(super) struct BundleCompileContext<'a> {
    pub(super) import_targets: &'a BTreeMap<(String, String), String>,
    pub(super) entry_modules: &'a [String],
    pub(super) entry_points: &'a [PackageEntryPoint],
    pub(super) tool_bindings: &'a [CompilerToolBinding],
    pub(super) template_bindings: &'a [CompilerTemplateBinding],
    pub(super) prepared: BTreeMap<String, PreparedModule>,
}

pub(super) fn load_modules(
    roots: &[String],
    sources: &BTreeMap<String, String>,
    import_targets: &BTreeMap<(String, String), String>,
    mut prepared: BTreeMap<String, PreparedModule>,
) -> Result<BTreeMap<String, LoweredModule>, Diagnostic> {
    pub(super) fn visit(
        path: &str,
        sources: &BTreeMap<String, String>,
        import_targets: &BTreeMap<(String, String), String>,
        prepared: &mut BTreeMap<String, PreparedModule>,
        modules: &mut BTreeMap<String, LoweredModule>,
        active: &mut Vec<String>,
    ) -> Result<(), Diagnostic> {
        if let Some(position) = active.iter().position(|item| item == path) {
            let mut cycle = active[position..].to_vec();
            cycle.push(path.to_owned());
            return Err(Diagnostic::new(
                "E3002",
                format!("module import cycle: {}", cycle.join(" -> ")),
                Span { start: 0, end: 0 },
            ));
        }
        if modules.contains_key(path) {
            return Ok(());
        }
        let source = sources.get(path).ok_or_else(|| {
            Diagnostic::new(
                "E3003",
                format!("imported module '{path}' is not in the source bundle"),
                Span { start: 0, end: 0 },
            )
        })?;
        let mut module = if let Some(prepared) = prepared.remove(path) {
            if prepared.source != *source {
                return Err(Diagnostic::new(
                    "E3005",
                    "prepared source does not match the source bundle",
                    Span { start: 0, end: 0 },
                )
                .with_source(path));
            }
            prepared.module
        } else {
            syntax_lowering::parse_module(path, source)?
        };
        active.push(path.to_owned());
        for import in &mut module.imports {
            if !(import.path.starts_with("./") || import.path.starts_with("../")) {
                let key = (path.to_owned(), import.path.clone());
                let target = import_targets.get(&key).cloned().ok_or_else(|| {
                    Diagnostic::new(
                        "E3003",
                        format!(
                            "package import '{}' is not in the canonical source map",
                            import.path
                        ),
                        import.span,
                    )
                    .with_source(path)
                })?;
                import.resolved_path = Some(
                    normalize_root(&target).map_err(|diagnostic| diagnostic.with_source(path))?,
                );
            }
            let target = resolve_import_path(path, import)
                .map_err(|diagnostic| diagnostic.with_source(path))?;
            visit(&target, sources, import_targets, prepared, modules, active)?;
        }
        active.pop();
        modules.insert(path.to_owned(), module);
        Ok(())
    }

    let mut modules = BTreeMap::new();
    for root in roots {
        visit(
            root,
            sources,
            import_targets,
            &mut prepared,
            &mut modules,
            &mut Vec::new(),
        )?;
    }
    if let Some((path, _)) = prepared.first_key_value() {
        return Err(Diagnostic::new(
            "E3005",
            "prepared source is not reachable from the compilation roots",
            Span { start: 0, end: 0 },
        )
        .with_source(path));
    }
    Ok(modules)
}

#[allow(clippy::too_many_lines)]
pub(super) fn semantic_type(
    lowered: &LoweredType,
    generics: &BTreeSet<String>,
    module: &str,
    modules: &BTreeMap<String, LoweredModule>,
    types: &BTreeMap<(String, String), ValueType>,
) -> Result<SemanticType, Diagnostic> {
    match lowered {
        LoweredType::Named(name, span) => {
            if let Some(builtin) = builtin_semantic_type(name) {
                Ok(builtin)
            } else if generics.contains(name) {
                Ok(SemanticType::Generic(name.clone()))
            } else {
                resolve_named_semantic_type(modules, types, module, name, *span)
            }
        }
        LoweredType::Tuple(elements, _) => {
            if elements.is_empty() {
                Ok(SemanticType::Value(ValueType::Unit))
            } else {
                let mut values = Vec::new();
                for element in elements {
                    let SemanticType::Value(value) =
                        semantic_type(element, generics, module, modules, types)?
                    else {
                        return Err(Diagnostic::new(
                            "E3007",
                            "generic values cannot be nested in a tuple type in version 0.1",
                            element.span(),
                        ));
                    };
                    values.push(value);
                }
                if values.iter().any(contains_affine) || values.iter().any(contains_sub_agent) {
                    return Err(Diagnostic::new(
                        "E3011",
                        "future, task, or SubAgent values cannot be stored in a tuple",
                        lowered.span(),
                    ));
                }
                if values.iter().any(contains_workspace) {
                    return Err(Diagnostic::new(
                        "E3011",
                        "Workspace cannot be stored in a tuple",
                        lowered.span(),
                    ));
                }
                Ok(SemanticType::Value(ValueType::Tuple(values)))
            }
        }
        LoweredType::Record(fields, span) => {
            let mut seen = BTreeSet::new();
            let mut layout = Vec::with_capacity(fields.len());
            for (name, value, name_span) in fields {
                if !seen.insert(name.clone()) {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("duplicate record type field '{name}'"),
                        *name_span,
                    ));
                }
                let SemanticType::Value(value_type) =
                    semantic_type(value, generics, module, modules, types)?
                else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "record field type must be concrete",
                        value.span(),
                    ));
                };
                if contains_affine(&value_type)
                    || contains_stored_sub_agent(&value_type)
                    || contains_workspace(&value_type)
                {
                    return Err(Diagnostic::new(
                        "E3011",
                        "record cannot store future, task, SubAgent, or Workspace values",
                        *span,
                    ));
                }
                layout.push(RecordField {
                    name: name.clone(),
                    value_type,
                });
            }
            layout.sort_by(|left, right| left.name.cmp(&right.name));
            Ok(SemanticType::Value(ValueType::Record(layout)))
        }
        LoweredType::List(value, _) => {
            let SemanticType::Value(value) =
                semantic_type(value, generics, module, modules, types)?
            else {
                return Err(Diagnostic::new(
                    "E3007",
                    "List element type must be concrete",
                    value.span(),
                ));
            };
            if contains_affine(&value) || contains_stored_sub_agent(&value) {
                return Err(Diagnostic::new(
                    "E3011",
                    "future, task, or SubAgent values cannot be stored in List",
                    lowered.span(),
                ));
            }
            if contains_workspace(&value) {
                return Err(Diagnostic::new(
                    "E3011",
                    "Workspace cannot be stored in List",
                    lowered.span(),
                ));
            }
            Ok(SemanticType::Value(ValueType::List(Box::new(value))))
        }
        LoweredType::Map(key, value, _) => {
            let SemanticType::Value(key) = semantic_type(key, generics, module, modules, types)?
            else {
                return Err(Diagnostic::new(
                    "E3007",
                    "Map key type must be concrete",
                    lowered.span(),
                ));
            };
            let SemanticType::Value(value) =
                semantic_type(value, generics, module, modules, types)?
            else {
                return Err(Diagnostic::new(
                    "E3007",
                    "Map value type must be concrete",
                    lowered.span(),
                ));
            };
            if !key.is_map_key()
                || contains_affine(&key)
                || contains_affine(&value)
                || contains_stored_sub_agent(&key)
                || contains_stored_sub_agent(&value)
            {
                return Err(Diagnostic::new(
                    "E3011",
                    "Map cannot contain future, task, or SubAgent values and requires a scalar key",
                    lowered.span(),
                ));
            }
            if contains_workspace(&key) || contains_workspace(&value) {
                return Err(Diagnostic::new(
                    "E3011",
                    "Workspace cannot be stored in Map",
                    lowered.span(),
                ));
            }
            Ok(SemanticType::Value(ValueType::Map(
                Box::new(key),
                Box::new(value),
            )))
        }
        LoweredType::Option(value, _) => {
            let SemanticType::Value(value) =
                semantic_type(value, generics, module, modules, types)?
            else {
                return Err(Diagnostic::new(
                    "E3007",
                    "Option element type must be concrete",
                    value.span(),
                ));
            };
            if contains_affine(&value) || contains_stored_sub_agent(&value) {
                return Err(Diagnostic::new(
                    "E3011",
                    "future, task, or SubAgent values cannot be stored in Option",
                    lowered.span(),
                ));
            }
            if contains_workspace(&value) {
                return Err(Diagnostic::new(
                    "E3011",
                    "Workspace cannot be stored in Option",
                    lowered.span(),
                ));
            }
            Ok(SemanticType::Value(ValueType::Option(Box::new(value))))
        }
        LoweredType::Result(ok, error, _) => {
            let SemanticType::Value(ok_type) = semantic_type(ok, generics, module, modules, types)?
            else {
                return Err(Diagnostic::new(
                    "E3007",
                    "Result success type must be concrete",
                    ok.span(),
                ));
            };
            let SemanticType::Value(error_type) =
                semantic_type(error, generics, module, modules, types)?
            else {
                return Err(Diagnostic::new(
                    "E3007",
                    "Result error type must be concrete",
                    error.span(),
                ));
            };
            let is_sub_agent_result =
                ok_type == ValueType::SubAgent && error_type == sub_agent_error_type();
            if contains_affine(&ok_type)
                || contains_affine(&error_type)
                || contains_stored_sub_agent(&ok_type) && !is_sub_agent_result
                || contains_stored_sub_agent(&error_type)
            {
                return Err(Diagnostic::new(
                    "E3011",
                    "future, task, or SubAgent values cannot be stored in Result",
                    lowered.span(),
                ));
            }
            let is_permission_result =
                ok_type == ValueType::Workspace && error_type == permission_error_type();
            if (contains_workspace(&ok_type) || contains_workspace(&error_type))
                && !is_permission_result
            {
                return Err(Diagnostic::new(
                    "E3011",
                    "Workspace can be stored only in Result<Workspace, FileError>",
                    lowered.span(),
                ));
            }
            Ok(SemanticType::Value(ValueType::Result(
                Box::new(ok_type),
                Box::new(error_type),
            )))
        }
        LoweredType::Future(value, _) | LoweredType::Task(value, _) => {
            let SemanticType::Value(value_type) =
                semantic_type(value, generics, module, modules, types)?
            else {
                return Err(Diagnostic::new(
                    "E3007",
                    "asynchronous element type must be concrete",
                    value.span(),
                ));
            };
            Ok(SemanticType::Value(
                if matches!(lowered, LoweredType::Future(_, _)) {
                    ValueType::Future(Box::new(value_type))
                } else {
                    ValueType::Task(Box::new(value_type))
                },
            ))
        }
        LoweredType::Prompt(value, _) => {
            let SemanticType::Value(value_type) =
                semantic_type(value, generics, module, modules, types)?
            else {
                return Err(Diagnostic::new(
                    "E3007",
                    "Prompt response type must be concrete",
                    value.span(),
                ));
            };
            if !is_strict_schema_type(&value_type) {
                return Err(Diagnostic::new(
                    "E3011",
                    "Prompt response type is not supported by the strict schema profile",
                    lowered.span(),
                ));
            }
            Ok(SemanticType::Value(prompt_type(value_type)))
        }
        LoweredType::Range(value, _) => {
            let SemanticType::Value(value_type) =
                semantic_type(value, generics, module, modules, types)?
            else {
                return Err(Diagnostic::new(
                    "E3007",
                    "Range element type must be concrete",
                    value.span(),
                ));
            };
            if value_type != ValueType::Int {
                return Err(Diagnostic::new(
                    "E3011",
                    "Range requires Int bounds",
                    value.span(),
                ));
            }
            Ok(SemanticType::Value(ValueType::Range))
        }
        LoweredType::Sequence(value, _) => {
            let SemanticType::Value(value_type) =
                semantic_type(value, generics, module, modules, types)?
            else {
                return Err(Diagnostic::new(
                    "E3007",
                    "Sequence element type must be concrete",
                    value.span(),
                ));
            };
            if contains_affine(&value_type)
                || contains_stored_sub_agent(&value_type)
                || contains_workspace(&value_type)
            {
                return Err(Diagnostic::new(
                    "E3011",
                    "Sequence cannot contain affine, SubAgent, or Workspace values",
                    lowered.span(),
                ));
            }
            Ok(SemanticType::Value(ValueType::Sequence(Box::new(
                value_type,
            ))))
        }
        LoweredType::Function {
            parameters,
            return_type,
            effects,
            ..
        } => Ok(SemanticType::Function {
            parameters: parameters
                .iter()
                .map(|value| semantic_type(value, generics, module, modules, types))
                .collect::<Result<_, _>>()?,
            return_type: Box::new(semantic_type(
                return_type,
                generics,
                module,
                modules,
                types,
            )?),
            effects: effects.clone(),
        }),
    }
}

pub(super) fn lowered_type_spelling(value: &LoweredType) -> String {
    match value {
        LoweredType::Named(name, _) => name.clone(),
        LoweredType::Tuple(values, _) => format!(
            "({})",
            values
                .iter()
                .map(lowered_type_spelling)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        LoweredType::Record(fields, _) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(name, value, _)| format!("{name}: {}", lowered_type_spelling(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        LoweredType::List(value, _) => format!("List<{}>", lowered_type_spelling(value)),
        LoweredType::Map(key, value, _) => format!(
            "Map<{}, {}>",
            lowered_type_spelling(key),
            lowered_type_spelling(value)
        ),
        LoweredType::Option(value, _) => format!("Option<{}>", lowered_type_spelling(value)),
        LoweredType::Result(ok, error, _) => format!(
            "Result<{}, {}>",
            lowered_type_spelling(ok),
            lowered_type_spelling(error)
        ),
        LoweredType::Future(value, _) => format!("Future<{}>", lowered_type_spelling(value)),
        LoweredType::Task(value, _) => format!("Task<{}>", lowered_type_spelling(value)),
        LoweredType::Prompt(value, _) => format!("Prompt<{}>", lowered_type_spelling(value)),
        LoweredType::Range(value, _) => format!("Range<{}>", lowered_type_spelling(value)),
        LoweredType::Sequence(value, _) => format!("Sequence<{}>", lowered_type_spelling(value)),
        LoweredType::Function {
            parameters,
            return_type,
            effects,
            ..
        } => format!(
            "fn({}) returns {} effects [{}]",
            parameters
                .iter()
                .map(lowered_type_spelling)
                .collect::<Vec<_>>()
                .join(", "),
            lowered_type_spelling(return_type),
            effects.join(", ")
        ),
    }
}

pub(super) fn resolve_function_name(
    bundle: &ResolvedBundle,
    module: &str,
    name: &str,
) -> Result<Option<SymbolId>, Diagnostic> {
    if let Some(symbol) = bundle.names.get(&(module.to_owned(), name.to_owned())) {
        return Ok(Some(*symbol));
    }
    let definition = &bundle.modules[module];
    for import in &definition.imports {
        if import.extension {
            continue;
        }
        if let Some((imported, _, _)) = import.names.iter().find(|(_, local, _)| local == name) {
            let target = resolve_import_path(module, import)?;
            let symbol = bundle
                .names
                .get(&(target.clone(), imported.clone()))
                .copied();
            if symbol.is_none()
                && bundle.modules[&target]
                    .types
                    .iter()
                    .any(|declaration| declaration.name() == imported && declaration.exported())
            {
                return Ok(None);
            }
            let symbol = symbol.ok_or_else(|| {
                Diagnostic::new(
                    "E3003",
                    format!("module '{target}' does not export '{imported}'"),
                    import.span,
                )
            })?;
            if !bundle.functions[symbol as usize].lowered.exported {
                return Err(Diagnostic::new(
                    "E3003",
                    format!("function '{imported}' is private to module '{target}'"),
                    import.span,
                ));
            }
            return Ok(Some(symbol));
        }
    }
    Ok(None)
}

pub(super) fn resolve_extension_functions(
    bundle: &ResolvedBundle,
    module: &str,
    member: &str,
    receiver: &ValueType,
) -> Result<Vec<SymbolId>, Diagnostic> {
    let mut candidates = Vec::new();
    for import in &bundle.modules[module].imports {
        if !import.extension {
            continue;
        }
        let target_module = resolve_import_path(module, import)?;
        for (imported, local, _) in &import.names {
            if local != member {
                continue;
            }
            let Some(symbol) = bundle
                .names
                .get(&(target_module.clone(), imported.clone()))
                .copied()
            else {
                continue;
            };
            let function = &bundle.functions[symbol as usize];
            let Some(first) = function.parameters.first() else {
                continue;
            };
            let exact = match first {
                SemanticType::Value(value_type) => value_type == receiver,
                SemanticType::Generic(_) => true,
                SemanticType::Function { .. } => false,
            };
            if exact {
                candidates.push(symbol);
            }
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    Ok(candidates)
}

pub(super) fn default_helper_name(function: &str, parameter: usize) -> String {
    format!("$default@{function}@{parameter}")
}

#[allow(clippy::too_many_lines)]
fn expression_references_any(expression: &LoweredExpr, names: &BTreeSet<&str>) -> Option<Span> {
    match &expression.kind {
        LoweredExprKind::Variable(name) => names.contains(name.as_str()).then_some(expression.span),
        LoweredExprKind::Template(parts) => parts.iter().find_map(|part| match part {
            super::LoweredTemplatePart::Literal { .. } => None,
            super::LoweredTemplatePart::Interpolation(value) => {
                expression_references_any(value, names)
            }
        }),
        LoweredExprKind::List(values) | LoweredExprKind::Tuple(values) => values
            .iter()
            .find_map(|value| expression_references_any(value, names)),
        LoweredExprKind::ListWithSpread(items) => items
            .iter()
            .find_map(|item| expression_references_any(&item.value, names)),
        LoweredExprKind::Map(entries) => entries.iter().find_map(|(key, value)| {
            expression_references_any(key, names)
                .or_else(|| expression_references_any(value, names))
        }),
        LoweredExprKind::MapWithSpread(items) => items.iter().find_map(|item| match item {
            super::LoweredMapItem::Entry { key, value, .. } => {
                expression_references_any(key, names)
                    .or_else(|| expression_references_any(value, names))
            }
            super::LoweredMapItem::Spread { value, .. } => expression_references_any(value, names),
        }),
        LoweredExprKind::Record { fields, .. } => fields
            .iter()
            .find_map(|(_, value, _)| expression_references_any(value, names)),
        LoweredExprKind::RecordUpdate { base, fields, .. } => {
            expression_references_any(base, names).or_else(|| {
                fields
                    .iter()
                    .find_map(|(_, value, _)| expression_references_any(value, names))
            })
        }
        LoweredExprKind::Prompt {
            system,
            context,
            data,
            ..
        } => expression_references_any(system, names)
            .or_else(|| {
                context
                    .as_deref()
                    .and_then(|value| expression_references_any(value, names))
            })
            .or_else(|| {
                data.as_deref()
                    .and_then(|value| expression_references_any(value, names))
            }),
        LoweredExprKind::Enum { payload, .. } => match payload {
            LoweredEnumValuePayload::Unit => None,
            LoweredEnumValuePayload::Tuple(values) => values
                .iter()
                .find_map(|value| expression_references_any(value, names)),
            LoweredEnumValuePayload::Record(fields) => fields
                .iter()
                .find_map(|(_, value, _)| expression_references_any(value, names)),
        },
        LoweredExprKind::FieldGet { record, .. } => expression_references_any(record, names),
        LoweredExprKind::OptionalFieldGet { receiver, .. } => {
            expression_references_any(receiver, names)
        }
        LoweredExprKind::Try(value)
        | LoweredExprKind::Spawn(value)
        | LoweredExprKind::Await(value)
        | LoweredExprKind::Unary { operand: value, .. } => expression_references_any(value, names),
        LoweredExprKind::Match { source, arms } => expression_references_any(source, names)
            .or_else(|| {
                arms.iter()
                    .find_map(|(_, value, _)| expression_references_any(value, names))
            }),
        LoweredExprKind::If {
            condition,
            then_body,
            else_branch,
        } => expression_references_any(condition, names)
            .or_else(|| body_references_any(then_body, names))
            .or_else(|| match else_branch {
                Some(LoweredElse::Body(body)) => body_references_any(body, names),
                Some(LoweredElse::If(value)) => expression_references_any(value, names),
                None => None,
            }),
        LoweredExprKind::Binary { left, right, .. }
        | LoweredExprKind::Compose { left, right, .. }
        | LoweredExprKind::Range {
            start: left,
            end: right,
            ..
        } => expression_references_any(left, names)
            .or_else(|| expression_references_any(right, names)),
        LoweredExprKind::Pipe { left, stage, .. } => expression_references_any(left, names)
            .or_else(|| expression_references_any(stage, names)),
        LoweredExprKind::Index { collection, index }
        | LoweredExprKind::Slice {
            collection,
            range: index,
            ..
        } => expression_references_any(collection, names)
            .or_else(|| expression_references_any(index, names)),
        LoweredExprKind::Call {
            callee, arguments, ..
        } => expression_references_any(callee, names).or_else(|| {
            arguments
                .iter()
                .find_map(|argument| expression_references_any(&argument.value, names))
        }),
        LoweredExprKind::AwaitBlock(body) | LoweredExprKind::Closure { body, .. } => {
            body_references_any(body, names)
        }
        LoweredExprKind::ShortClosure { body, .. } => expression_references_any(body, names),
        LoweredExprKind::Unit
        | LoweredExprKind::Int(_)
        | LoweredExprKind::Float(_)
        | LoweredExprKind::Bool(_)
        | LoweredExprKind::String(_)
        | LoweredExprKind::Bytes(_) => None,
    }
}

fn body_references_any(body: &LoweredBody, names: &BTreeSet<&str>) -> Option<Span> {
    body.statements
        .iter()
        .find_map(|statement| match statement {
            LoweredStatement::Let { value, .. }
            | LoweredStatement::Assignment { value, .. }
            | LoweredStatement::ControlFlow(value) => expression_references_any(value, names),
            LoweredStatement::Return(value, _) => value
                .as_ref()
                .and_then(|value| expression_references_any(value, names)),
            LoweredStatement::While {
                condition, body, ..
            } => expression_references_any(condition, names)
                .or_else(|| body_references_any(body, names)),
            LoweredStatement::Loop { body, .. } => body_references_any(body, names),
            LoweredStatement::For { source, body, .. } => {
                let source = match source {
                    LoweredForSource::Iterable(value) => expression_references_any(value, names),
                };
                source.or_else(|| body_references_any(body, names))
            }
            LoweredStatement::LocalFunction(function) => body_references_any(&function.body, names),
            LoweredStatement::Break(_) | LoweredStatement::Continue(_) => None,
        })
        .or_else(|| {
            body.tail
                .as_ref()
                .and_then(|value| expression_references_any(value, names))
        })
}

#[allow(clippy::type_complexity)]
pub(super) fn resolve_lowered_record_fields<'a>(
    bundle: &'a ResolvedBundle,
    module: &str,
    name: &str,
) -> Option<(String, &'a [(String, LoweredType, Span)])> {
    if let Some(LoweredTypeDeclaration::Record { fields, .. }) = bundle.modules[module]
        .types
        .iter()
        .find(|declaration| declaration.name() == name)
    {
        return Some((module.to_owned(), fields));
    }
    for import in &bundle.modules[module].imports {
        if import.extension {
            continue;
        }
        let Some((imported, _, _)) = import.names.iter().find(|(_, local, _)| local == name) else {
            continue;
        };
        let target = resolve_import_path(module, import).ok()?;
        if let Some(LoweredTypeDeclaration::Record { fields, .. }) = bundle.modules[&target]
            .types
            .iter()
            .find(|declaration| declaration.name() == imported)
        {
            return Some((target, fields));
        }
    }
    None
}

pub(super) fn is_task_snapshot_callee(expression: &LoweredExpr) -> bool {
    let LoweredExprKind::FieldGet {
        record: task_namespace,
        field: operation,
        ..
    } = &expression.kind
    else {
        return false;
    };
    if operation != "task_snapshot" {
        return false;
    }
    let LoweredExprKind::FieldGet {
        record: allen_namespace,
        field: namespace,
        ..
    } = &task_namespace.kind
    else {
        return false;
    };
    namespace == "internal"
        && matches!(&allen_namespace.kind, LoweredExprKind::Variable(name) if name == "allen")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StandardBuiltin {
    Workspace,
    Operation(EffectOperation),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CollectionBuiltin {
    Length,
    ListAppend,
    ListSet,
    Operation(CollectionOperation),
    ListFold,
    ListCombinator(ListCombinator),
    Safe(SafeCollectionOperation),
    CheckedInt(CheckedIntOperation),
    Sequence(SequenceBuiltin),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SequenceBuiltin {
    FromList,
    Map,
    Filter,
    Take,
    Find,
    Any,
    All,
    Fold,
    ToList,
}

pub(super) fn string_builtin_callee(expression: &LoweredExpr) -> Option<StringOperation> {
    let LoweredExprKind::FieldGet { record, field, .. } = &expression.kind else {
        return None;
    };
    let LoweredExprKind::Variable(namespace) = &record.kind else {
        return None;
    };
    if namespace != "string" {
        return None;
    }
    Some(match field.as_str() {
        "byte_length" => StringOperation::ByteLength,
        "concat" => StringOperation::Concat,
        "get" => StringOperation::Get,
        "slice" => StringOperation::Slice,
        "find" => StringOperation::Find,
        "contains" => StringOperation::Contains,
        "starts_with" => StringOperation::StartsWith,
        "ends_with" => StringOperation::EndsWith,
        "split" => StringOperation::Split,
        "join" => StringOperation::Join,
        "trim_ascii" => StringOperation::TrimAscii,
        "from_utf8" => StringOperation::FromUtf8,
        "replace" => StringOperation::Replace,
        _ => return None,
    })
}

pub(super) fn standard_operation_callee(expression: &LoweredExpr) -> Option<StandardOperation> {
    let LoweredExprKind::FieldGet { record, field, .. } = &expression.kind else {
        return None;
    };
    let LoweredExprKind::Variable(namespace) = &record.kind else {
        return None;
    };
    match (namespace.as_str(), field.as_str()) {
        ("float", "format") => Some(StandardOperation::FloatFormat),
        ("time", "format_utc") => Some(StandardOperation::TimeFormatUtc),
        ("time", "parse_utc") => Some(StandardOperation::TimeParseUtc),
        ("time", "bucket") => Some(StandardOperation::TimeBucket),
        _ => None,
    }
}

pub(super) fn capability_builtin_callee(expression: &LoweredExpr) -> Option<CapabilityOperation> {
    let LoweredExprKind::FieldGet { record, field, .. } = &expression.kind else {
        return None;
    };
    let LoweredExprKind::Variable(namespace) = &record.kind else {
        return None;
    };
    if namespace != "capability" {
        return None;
    }
    match field.as_str() {
        "is_granted" => Some(CapabilityOperation::IsGranted),
        "granted" => Some(CapabilityOperation::Granted),
        _ => None,
    }
}

pub(super) fn collection_builtin_callee(expression: &LoweredExpr) -> Option<CollectionBuiltin> {
    match &expression.kind {
        LoweredExprKind::Variable(name) if name == "length" => Some(CollectionBuiltin::Length),
        LoweredExprKind::Variable(name) if name == "zip" => {
            Some(CollectionBuiltin::Operation(CollectionOperation::Zip))
        }
        LoweredExprKind::FieldGet { record, field, .. } => {
            let LoweredExprKind::Variable(namespace) = &record.kind else {
                return None;
            };
            match (namespace.as_str(), field.as_str()) {
                ("list", "append") => Some(CollectionBuiltin::ListAppend),
                ("list", "set") => Some(CollectionBuiltin::ListSet),
                ("list", "min") => Some(CollectionBuiltin::Operation(CollectionOperation::ListMin)),
                ("list", "max") => Some(CollectionBuiltin::Operation(CollectionOperation::ListMax)),
                ("list", "sum") => Some(CollectionBuiltin::Operation(
                    CollectionOperation::ListSumInt,
                )),
                ("list", "fold") => Some(CollectionBuiltin::ListFold),
                ("list", "map") => Some(CollectionBuiltin::ListCombinator(ListCombinator::Map)),
                ("list", "filter") => {
                    Some(CollectionBuiltin::ListCombinator(ListCombinator::Filter))
                }
                ("list", "flat_map") => {
                    Some(CollectionBuiltin::ListCombinator(ListCombinator::FlatMap))
                }
                ("list", "filter_map") => {
                    Some(CollectionBuiltin::ListCombinator(ListCombinator::FilterMap))
                }
                ("list", "find") => Some(CollectionBuiltin::ListCombinator(ListCombinator::Find)),
                ("list", "any") => Some(CollectionBuiltin::ListCombinator(ListCombinator::Any)),
                ("list", "all") => Some(CollectionBuiltin::ListCombinator(ListCombinator::All)),
                ("list", "partition") => {
                    Some(CollectionBuiltin::ListCombinator(ListCombinator::Partition))
                }
                ("list", "scan") => Some(CollectionBuiltin::ListCombinator(ListCombinator::Scan)),
                ("list", "get") => Some(CollectionBuiltin::Safe(SafeCollectionOperation::ListGet)),
                ("list", "try_set") => {
                    Some(CollectionBuiltin::Safe(SafeCollectionOperation::ListTrySet))
                }
                ("bytes", "get") => {
                    Some(CollectionBuiltin::Safe(SafeCollectionOperation::BytesGet))
                }
                ("map", "get") => Some(CollectionBuiltin::Safe(SafeCollectionOperation::MapGet)),
                ("map", "insert") => {
                    Some(CollectionBuiltin::Safe(SafeCollectionOperation::MapInsert))
                }
                ("map", "remove") => {
                    Some(CollectionBuiltin::Safe(SafeCollectionOperation::MapRemove))
                }
                ("map", "keys") => Some(CollectionBuiltin::Safe(SafeCollectionOperation::MapKeys)),
                ("seq", "from_list") => {
                    Some(CollectionBuiltin::Sequence(SequenceBuiltin::FromList))
                }
                ("seq", "map") => Some(CollectionBuiltin::Sequence(SequenceBuiltin::Map)),
                ("seq", "filter") => Some(CollectionBuiltin::Sequence(SequenceBuiltin::Filter)),
                ("seq", "take") => Some(CollectionBuiltin::Sequence(SequenceBuiltin::Take)),
                ("seq", "find") => Some(CollectionBuiltin::Sequence(SequenceBuiltin::Find)),
                ("seq", "any") => Some(CollectionBuiltin::Sequence(SequenceBuiltin::Any)),
                ("seq", "all") => Some(CollectionBuiltin::Sequence(SequenceBuiltin::All)),
                ("seq", "fold") => Some(CollectionBuiltin::Sequence(SequenceBuiltin::Fold)),
                ("seq", "to_list") => Some(CollectionBuiltin::Sequence(SequenceBuiltin::ToList)),
                ("int", "checked_add") => {
                    Some(CollectionBuiltin::CheckedInt(CheckedIntOperation::Add))
                }
                ("int", "checked_sub") => {
                    Some(CollectionBuiltin::CheckedInt(CheckedIntOperation::Subtract))
                }
                ("int", "checked_mul") => {
                    Some(CollectionBuiltin::CheckedInt(CheckedIntOperation::Multiply))
                }
                ("int", "checked_div") => {
                    Some(CollectionBuiltin::CheckedInt(CheckedIntOperation::Divide))
                }
                ("int", "checked_rem") => Some(CollectionBuiltin::CheckedInt(
                    CheckedIntOperation::Remainder,
                )),
                ("int", "checked_neg") => {
                    Some(CollectionBuiltin::CheckedInt(CheckedIntOperation::Negate))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

pub(super) fn standard_builtin_callee(expression: &LoweredExpr) -> Option<StandardBuiltin> {
    let LoweredExprKind::FieldGet { record, field, .. } = &expression.kind else {
        return None;
    };
    let LoweredExprKind::Variable(namespace) = &record.kind else {
        return None;
    };
    match (namespace.as_str(), field.as_str()) {
        ("fs", "workspace") => Some(StandardBuiltin::Workspace),
        ("fs", "read_text") => Some(StandardBuiltin::Operation(EffectOperation::ReadText)),
        ("fs", "read_bytes") => Some(StandardBuiltin::Operation(EffectOperation::ReadBytes)),
        ("fs", "write_text") => Some(StandardBuiltin::Operation(EffectOperation::WriteText)),
        ("fs", "write_bytes") => Some(StandardBuiltin::Operation(EffectOperation::WriteBytes)),
        ("fs", "list") => Some(StandardBuiltin::Operation(EffectOperation::List)),
        ("fs", "search") => Some(StandardBuiltin::Operation(EffectOperation::Search)),
        ("http", "get") => Some(StandardBuiltin::Operation(EffectOperation::HttpGet)),
        ("exec", "run") => Some(StandardBuiltin::Operation(EffectOperation::ExecRun)),
        ("permission", "request_file") => Some(StandardBuiltin::Operation(
            EffectOperation::PermissionRequestFile,
        )),
        ("permission", "request_directory") => Some(StandardBuiltin::Operation(
            EffectOperation::PermissionRequestDirectory,
        )),
        ("agent", "message") => Some(StandardBuiltin::Operation(EffectOperation::AgentMessage)),
        ("agent", "ask") => Some(StandardBuiltin::Operation(EffectOperation::AgentAsk)),
        ("agent", "transcript") => {
            Some(StandardBuiltin::Operation(EffectOperation::AgentTranscript))
        }
        ("model", "request") => Some(StandardBuiltin::Operation(EffectOperation::ModelRequest)),
        ("user", "ask") => Some(StandardBuiltin::Operation(EffectOperation::UserAsk)),
        ("sub_agent", "create") => {
            Some(StandardBuiltin::Operation(EffectOperation::SubAgentCreate))
        }
        ("sub_agent", "run") => Some(StandardBuiltin::Operation(EffectOperation::SubAgentRun)),
        ("sub_agent", "message") => {
            Some(StandardBuiltin::Operation(EffectOperation::SubAgentMessage))
        }
        ("sub_agent", "ask") => Some(StandardBuiltin::Operation(EffectOperation::SubAgentAsk)),
        _ => None,
    }
}

pub(super) fn tool_callee(expression: &LoweredExpr) -> Option<Vec<String>> {
    pub(super) fn components(expression: &LoweredExpr, output: &mut Vec<String>) -> bool {
        match &expression.kind {
            LoweredExprKind::Variable(name) => {
                output.push(name.clone());
                true
            }
            LoweredExprKind::FieldGet { record, field, .. } => {
                if !components(record, output) {
                    return false;
                }
                output.push(field.clone());
                true
            }
            _ => false,
        }
    }

    let mut path = Vec::new();
    if !components(expression, &mut path)
        || path.first().is_none_or(|value| value != "tools")
        || path.last().is_none_or(|value| value != "call")
        || path.len() < 3
    {
        return None;
    }
    Some(path[1..path.len() - 1].to_vec())
}

pub(super) fn template_callee(expression: &LoweredExpr) -> Option<&str> {
    let LoweredExprKind::FieldGet {
        record,
        field: render,
        ..
    } = &expression.kind
    else {
        return None;
    };
    let LoweredExprKind::FieldGet {
        record: namespace,
        field: name,
        ..
    } = &record.kind
    else {
        return None;
    };
    matches!(&namespace.kind, LoweredExprKind::Variable(value) if value == "templates")
        .then_some(())
        .filter(|()| render == "render")
        .map(|()| name.as_str())
}

pub(super) fn template_binding<'a>(
    bundle: &'a ResolvedBundle,
    module: &str,
    name: &str,
) -> Option<&'a CompilerTemplateBinding> {
    let package = module.strip_prefix("pkg://")?.split_once('/')?.0;
    bundle.templates.get(&(package.to_owned(), name.to_owned()))
}

#[allow(clippy::too_many_lines)]
pub(super) fn effect_operation_signature(
    operation: EffectOperation,
    transcript_part: Option<u32>,
) -> Option<(Vec<ValueType>, ValueType, &'static str, &'static str)> {
    let workspace = ValueType::Workspace;
    let path = ValueType::String;
    Some(match operation {
        EffectOperation::ReadText => (
            vec![workspace, path],
            ValueType::Result(Box::new(ValueType::String), Box::new(file_error_type())),
            "fs.read",
            "filesystem operation",
        ),
        EffectOperation::ReadBytes => (
            vec![workspace, path],
            ValueType::Result(Box::new(ValueType::Bytes), Box::new(file_error_type())),
            "fs.read",
            "filesystem operation",
        ),
        EffectOperation::WriteText => (
            vec![workspace, path.clone(), ValueType::String],
            ValueType::Result(Box::new(ValueType::Unit), Box::new(file_error_type())),
            "fs.write",
            "filesystem operation",
        ),
        EffectOperation::WriteBytes => (
            vec![workspace, path, ValueType::Bytes],
            ValueType::Result(Box::new(ValueType::Unit), Box::new(file_error_type())),
            "fs.write",
            "filesystem operation",
        ),
        EffectOperation::List => (
            vec![workspace, path],
            ValueType::Result(
                Box::new(ValueType::List(Box::new(ValueType::String))),
                Box::new(file_error_type()),
            ),
            "fs.read",
            "filesystem operation",
        ),
        EffectOperation::Search => (
            vec![workspace, path, ValueType::String],
            ValueType::Result(
                Box::new(ValueType::List(Box::new(search_match_type()))),
                Box::new(file_error_type()),
            ),
            "fs.read",
            "filesystem operation",
        ),
        EffectOperation::HttpGet => (
            vec![ValueType::String],
            ValueType::Result(
                Box::new(http_response_type()),
                Box::new(network_error_type()),
            ),
            "net.http_get",
            "http.get",
        ),
        EffectOperation::ExecRun => (
            vec![
                ValueType::List(Box::new(ValueType::String)),
                ValueType::Option(Box::new(ValueType::Bytes)),
            ],
            ValueType::Result(
                Box::new(allen_bytecode::exec_response_type()),
                Box::new(allen_bytecode::exec_error_type()),
            ),
            "exec.run",
            "exec.run",
        ),
        EffectOperation::PermissionRequestFile => (
            vec![external_file_request_type()],
            ValueType::Result(
                Box::new(ValueType::Workspace),
                Box::new(permission_error_type()),
            ),
            "permission.request_external_fs",
            "permission.request_file",
        ),
        EffectOperation::PermissionRequestDirectory => (
            vec![external_directory_request_type()],
            ValueType::Result(
                Box::new(ValueType::Workspace),
                Box::new(permission_error_type()),
            ),
            "permission.request_external_fs",
            "permission.request_directory",
        ),
        EffectOperation::AgentMessage => (
            vec![ValueType::String],
            ValueType::Result(Box::new(ValueType::Unit), Box::new(agent_error_type())),
            "agent.message",
            "agent.message",
        ),
        EffectOperation::AgentAsk => (
            vec![ValueType::String],
            ValueType::Result(Box::new(ValueType::String), Box::new(agent_error_type())),
            "agent.ask",
            "agent.ask",
        ),
        EffectOperation::AgentTranscript => (
            vec![transcript_query_type()],
            ValueType::Result(
                Box::new(transcript_snapshot_type(transcript_part?)),
                Box::new(agent_error_type()),
            ),
            "agent.transcript",
            "agent.transcript",
        ),
        EffectOperation::ModelRequest
        | EffectOperation::UserAsk
        | EffectOperation::SubAgentCreate
        | EffectOperation::SubAgentRun
        | EffectOperation::SubAgentMessage
        | EffectOperation::SubAgentAsk => return None,
    })
}

pub(super) fn sub_agent_projection_type() -> ValueType {
    ValueType::Record(vec![
        RecordField {
            name: "capabilities".to_owned(),
            value_type: ValueType::List(Box::new(ValueType::String)),
        },
        RecordField {
            name: "limits".to_owned(),
            value_type: ValueType::Map(Box::new(ValueType::String), Box::new(ValueType::Int)),
        },
        RecordField {
            name: "tools".to_owned(),
            value_type: ValueType::List(Box::new(ValueType::String)),
        },
    ])
}

#[allow(clippy::too_many_lines)]
pub(super) fn required_body_effects(
    bundle: &ResolvedBundle,
    function: &FunctionInfo,
    current: &[Vec<String>],
) -> Result<Vec<String>, Diagnostic> {
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum EffectShape {
        Function {
            effects: Vec<String>,
            result: Box<EffectShape>,
            local: bool,
        },
        List(Box<EffectShape>),
        Map(Box<EffectShape>, Box<EffectShape>),
        Tuple(Vec<EffectShape>),
        Record(BTreeMap<String, EffectShape>),
        Never,
        Other,
    }

    pub(super) fn type_shape(
        value_type: &LoweredType,
        bundle: &ResolvedBundle,
        module: &str,
    ) -> EffectShape {
        fn resolve(
            value_type: &LoweredType,
            bundle: &ResolvedBundle,
            module: &str,
            active: &mut BTreeSet<(String, String)>,
        ) -> EffectShape {
            match value_type {
                LoweredType::Function {
                    return_type,
                    effects,
                    ..
                } => EffectShape::Function {
                    effects: effects.clone(),
                    result: Box::new(resolve(return_type, bundle, module, active)),
                    local: false,
                },
                LoweredType::List(value, _) => {
                    EffectShape::List(Box::new(resolve(value, bundle, module, active)))
                }
                LoweredType::Map(key, value, _) => EffectShape::Map(
                    Box::new(resolve(key, bundle, module, active)),
                    Box::new(resolve(value, bundle, module, active)),
                ),
                LoweredType::Tuple(elements, _) => EffectShape::Tuple(
                    elements
                        .iter()
                        .map(|element| resolve(element, bundle, module, active))
                        .collect(),
                ),
                LoweredType::Record(fields, _) => EffectShape::Record(
                    fields
                        .iter()
                        .map(|(name, field, _)| {
                            (name.clone(), resolve(field, bundle, module, active))
                        })
                        .collect(),
                ),
                LoweredType::Named(name, span) => {
                    if name == "Never" {
                        return EffectShape::Never;
                    }
                    if let Ok(Some((definition_module, target))) =
                        resolve_alias_target(&bundle.modules, module, name, *span)
                    {
                        let key = (definition_module.clone(), name.clone());
                        if !active.insert(key.clone()) {
                            return EffectShape::Other;
                        }
                        let shape = resolve(target, bundle, &definition_module, active);
                        active.remove(&key);
                        return shape;
                    }
                    let Some((definition_module, fields)) =
                        resolve_lowered_record_fields(bundle, module, name)
                    else {
                        return EffectShape::Other;
                    };
                    let key = (definition_module.clone(), name.clone());
                    if !active.insert(key.clone()) {
                        return EffectShape::Other;
                    }
                    let shape = EffectShape::Record(
                        fields
                            .iter()
                            .map(|(name, field, _)| {
                                (
                                    name.clone(),
                                    resolve(field, bundle, &definition_module, active),
                                )
                            })
                            .collect(),
                    );
                    active.remove(&key);
                    shape
                }
                LoweredType::Option(_, _)
                | LoweredType::Result(_, _, _)
                | LoweredType::Future(_, _)
                | LoweredType::Task(_, _)
                | LoweredType::Prompt(_, _)
                | LoweredType::Range(_, _)
                | LoweredType::Sequence(_, _) => EffectShape::Other,
            }
        }

        resolve(value_type, bundle, module, &mut BTreeSet::new())
    }

    pub(super) fn merge_shapes(shapes: impl IntoIterator<Item = EffectShape>) -> EffectShape {
        let mut merged = None;
        let mut saw_never = false;
        for shape in shapes {
            if shape == EffectShape::Never {
                saw_never = true;
                continue;
            }
            if merged.as_ref().is_some_and(|existing| existing != &shape) {
                return EffectShape::Other;
            }
            merged = Some(shape);
        }
        merged.unwrap_or(if saw_never {
            EffectShape::Never
        } else {
            EffectShape::Other
        })
    }

    fn local_function_shape(
        function: &super::LoweredLocalFunction,
        bundle: &ResolvedBundle,
        module: &str,
    ) -> EffectShape {
        EffectShape::Function {
            effects: function.declared_effects.clone().unwrap_or_default(),
            result: Box::new(type_shape(&function.return_type, bundle, module)),
            local: true,
        }
    }

    pub(super) fn expression_shape(
        expression: &LoweredExpr,
        bundle: &ResolvedBundle,
        module: &str,
        callbacks: &BTreeMap<String, EffectShape>,
        current: &[Vec<String>],
    ) -> Result<EffectShape, Diagnostic> {
        Ok(match &expression.kind {
            LoweredExprKind::Variable(name) => {
                if let Some(shape) = callbacks.get(name) {
                    shape.clone()
                } else if let Some(symbol) = resolve_function_name(bundle, module, name)? {
                    EffectShape::Function {
                        effects: current[symbol as usize].clone(),
                        result: Box::new(type_shape(
                            &bundle.functions[symbol as usize].lowered.return_type,
                            bundle,
                            &bundle.functions[symbol as usize].module,
                        )),
                        local: false,
                    }
                } else {
                    EffectShape::Other
                }
            }
            LoweredExprKind::Closure {
                parameters,
                return_type,
                declared_effects,
                body,
            } => {
                let mut nested = callbacks.clone();
                for (name, value_type, _) in parameters {
                    nested.insert(name.clone(), type_shape(value_type, bundle, module));
                }
                let effects = if let Some(effects) = declared_effects {
                    effects.clone()
                } else {
                    let mut effects = BTreeSet::new();
                    check_body(body, bundle, module, &nested, current, &mut effects)?;
                    effects.into_iter().collect()
                };
                EffectShape::Function {
                    effects,
                    result: Box::new(type_shape(return_type, bundle, module)),
                    local: false,
                }
            }
            LoweredExprKind::List(values) => EffectShape::List(Box::new(merge_shapes(
                values
                    .iter()
                    .map(|value| expression_shape(value, bundle, module, callbacks, current))
                    .collect::<Result<Vec<_>, _>>()?,
            ))),
            LoweredExprKind::Map(entries) => EffectShape::Map(
                Box::new(merge_shapes(
                    entries
                        .iter()
                        .map(|(key, _)| expression_shape(key, bundle, module, callbacks, current))
                        .collect::<Result<Vec<_>, _>>()?,
                )),
                Box::new(merge_shapes(
                    entries
                        .iter()
                        .map(|(_, value)| {
                            expression_shape(value, bundle, module, callbacks, current)
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                )),
            ),
            LoweredExprKind::Tuple(values) => EffectShape::Tuple(
                values
                    .iter()
                    .map(|value| expression_shape(value, bundle, module, callbacks, current))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            LoweredExprKind::Record { fields, .. } => EffectShape::Record(
                fields
                    .iter()
                    .map(|(name, value, _)| {
                        Ok((
                            name.clone(),
                            expression_shape(value, bundle, module, callbacks, current)?,
                        ))
                    })
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            LoweredExprKind::FieldGet { record, field, .. } => {
                match expression_shape(record, bundle, module, callbacks, current)? {
                    EffectShape::Record(fields) => {
                        fields.get(field).cloned().unwrap_or(EffectShape::Other)
                    }
                    _ => EffectShape::Other,
                }
            }
            LoweredExprKind::Index { collection, index } => {
                match expression_shape(collection, bundle, module, callbacks, current)? {
                    EffectShape::List(value) | EffectShape::Map(_, value) => *value,
                    EffectShape::Tuple(elements) => {
                        if let LoweredExprKind::Int(index) = &index.kind {
                            usize::try_from(*index)
                                .ok()
                                .and_then(|index| elements.get(index).cloned())
                                .unwrap_or(EffectShape::Other)
                        } else {
                            EffectShape::Other
                        }
                    }
                    _ => EffectShape::Other,
                }
            }
            LoweredExprKind::Call {
                callee, arguments, ..
            } if matches!(&callee.kind, LoweredExprKind::Variable(name) if name == "stop" || (name == "fail" && arguments.len() == 1)) => {
                EffectShape::Never
            }
            LoweredExprKind::Call { callee, .. } => {
                match expression_shape(callee, bundle, module, callbacks, current)? {
                    EffectShape::Function { result, .. } => *result,
                    EffectShape::Never => EffectShape::Never,
                    _ => EffectShape::Other,
                }
            }
            LoweredExprKind::Match { source, arms } => {
                let source_shape = expression_shape(source, bundle, module, callbacks, current)?;
                merge_shapes(
                    arms.iter()
                        .map(|(pattern, value, _)| {
                            let mut nested = callbacks.clone();
                            bind_match_shape(pattern, &source_shape, &mut nested);
                            expression_shape(value, bundle, module, &nested, current)
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                )
            }
            LoweredExprKind::If {
                then_body,
                else_branch,
                ..
            } => {
                let mut shapes = vec![body_shape(then_body, bundle, module, callbacks, current)?];
                match else_branch {
                    Some(LoweredElse::Body(body)) => {
                        shapes.push(body_shape(body, bundle, module, callbacks, current)?);
                    }
                    Some(LoweredElse::If(expression)) => shapes.push(expression_shape(
                        expression, bundle, module, callbacks, current,
                    )?),
                    None => shapes.push(EffectShape::Other),
                }
                merge_shapes(shapes)
            }
            LoweredExprKind::AwaitBlock(body) => {
                body_shape(body, bundle, module, callbacks, current)?
            }
            LoweredExprKind::ListWithSpread(items) => EffectShape::List(Box::new(merge_shapes(
                items
                    .iter()
                    .map(|item| expression_shape(&item.value, bundle, module, callbacks, current))
                    .collect::<Result<Vec<_>, _>>()?,
            ))),
            LoweredExprKind::MapWithSpread(items) => {
                let mut keys = Vec::new();
                let mut values = Vec::new();
                for item in items {
                    match item {
                        super::LoweredMapItem::Entry { key, value, .. } => {
                            keys.push(expression_shape(key, bundle, module, callbacks, current)?);
                            values
                                .push(expression_shape(value, bundle, module, callbacks, current)?);
                        }
                        super::LoweredMapItem::Spread { value, .. } => {
                            if let EffectShape::Map(key, value) =
                                expression_shape(value, bundle, module, callbacks, current)?
                            {
                                keys.push(*key);
                                values.push(*value);
                            }
                        }
                    }
                }
                EffectShape::Map(Box::new(merge_shapes(keys)), Box::new(merge_shapes(values)))
            }
            LoweredExprKind::RecordUpdate { base, fields, .. } => {
                match expression_shape(base, bundle, module, callbacks, current)? {
                    EffectShape::Record(mut shape) => {
                        for (name, value, _) in fields {
                            shape.insert(
                                name.clone(),
                                expression_shape(value, bundle, module, callbacks, current)?,
                            );
                        }
                        EffectShape::Record(shape)
                    }
                    _ => EffectShape::Other,
                }
            }
            LoweredExprKind::OptionalFieldGet {
                receiver, field, ..
            } => match expression_shape(receiver, bundle, module, callbacks, current)? {
                EffectShape::Record(fields) => {
                    fields.get(field).cloned().unwrap_or(EffectShape::Other)
                }
                _ => EffectShape::Other,
            },
            // These operators have no additional callback shape until their
            // call lowering is selected, but their nested operands are still
            // visited by check_expression below.
            LoweredExprKind::Unit
            | LoweredExprKind::Int(_)
            | LoweredExprKind::Float(_)
            | LoweredExprKind::Bool(_)
            | LoweredExprKind::String(_)
            | LoweredExprKind::Template(_)
            | LoweredExprKind::Bytes(_)
            | LoweredExprKind::Prompt { .. }
            | LoweredExprKind::Enum { .. }
            | LoweredExprKind::Try(_)
            | LoweredExprKind::Unary { .. }
            | LoweredExprKind::Binary { .. }
            | LoweredExprKind::ShortClosure { .. }
            | LoweredExprKind::Spawn(_)
            | LoweredExprKind::Await(_)
            | LoweredExprKind::Compose { .. }
            | LoweredExprKind::Pipe { .. }
            | LoweredExprKind::Range { .. }
            | LoweredExprKind::Slice { .. } => EffectShape::Other,
        })
    }

    pub(super) fn body_shape(
        body: &LoweredBody,
        bundle: &ResolvedBundle,
        module: &str,
        callbacks: &BTreeMap<String, EffectShape>,
        current: &[Vec<String>],
    ) -> Result<EffectShape, Diagnostic> {
        let mut scoped = callbacks.clone();
        for statement in &body.statements {
            match statement {
                LoweredStatement::Let {
                    name,
                    annotation,
                    value,
                    ..
                } => {
                    let shape = if let Some(annotation) = annotation {
                        type_shape(annotation, bundle, module)
                    } else {
                        expression_shape(value, bundle, module, &scoped, current)?
                    };
                    scoped.insert(name.clone(), shape);
                }
                LoweredStatement::LocalFunction(function) => {
                    scoped.insert(
                        function.name.clone(),
                        local_function_shape(function, bundle, module),
                    );
                }
                _ => {}
            }
        }
        if let Some(tail) = &body.tail {
            return expression_shape(tail, bundle, module, &scoped, current);
        }
        match body.statements.last() {
            Some(
                LoweredStatement::Return(_, _)
                | LoweredStatement::Break(_)
                | LoweredStatement::Continue(_),
            ) => Ok(EffectShape::Never),
            Some(
                LoweredStatement::Let { value, .. }
                | LoweredStatement::Assignment { value, .. }
                | LoweredStatement::ControlFlow(value),
            ) if expression_shape(value, bundle, module, &scoped, current)?
                == EffectShape::Never =>
            {
                Ok(EffectShape::Never)
            }
            _ => Ok(EffectShape::Other),
        }
    }

    pub(super) fn bind_loop_shape(
        binding: &LoweredLoopBinding,
        yielded: EffectShape,
        scope: &mut BTreeMap<String, EffectShape>,
    ) {
        if binding.tuple {
            let EffectShape::Tuple(elements) = yielded else {
                return;
            };
            if elements.len() != binding.elements.len() {
                return;
            }
            for (binding, shape) in binding.elements.iter().zip(elements) {
                if let Some(name) = &binding.name {
                    scope.insert(name.clone(), shape);
                }
            }
        } else if let Some(name) = &binding.elements[0].name {
            scope.insert(name.clone(), yielded);
        }
    }

    pub(super) fn bind_match_shape(
        pattern: &LoweredPattern,
        source: &EffectShape,
        scope: &mut BTreeMap<String, EffectShape>,
    ) {
        let (LoweredPattern::Record { fields, .. }, EffectShape::Record(source_fields)) =
            (pattern, source)
        else {
            return;
        };
        for (field, _, binding) in fields {
            if let (LoweredPattern::Binding { name, .. }, Some(shape)) =
                (binding.as_ref(), source_fields.get(field))
            {
                scope.insert(name.clone(), shape.clone());
            }
        }
    }

    pub(super) fn iterable_shape(shape: EffectShape) -> EffectShape {
        match shape {
            EffectShape::List(value) => *value,
            EffectShape::Map(key, value) => EffectShape::Tuple(vec![*key, *value]),
            EffectShape::Function { .. }
            | EffectShape::Tuple(_)
            | EffectShape::Record(_)
            | EffectShape::Never
            | EffectShape::Other => EffectShape::Other,
        }
    }

    pub(super) fn check_expression(
        expression: &LoweredExpr,
        bundle: &ResolvedBundle,
        module: &str,
        callbacks: &BTreeMap<String, EffectShape>,
        current: &[Vec<String>],
        effects: &mut BTreeSet<String>,
    ) -> Result<(), Diagnostic> {
        match &expression.kind {
            LoweredExprKind::Template(parts) => {
                for interpolation in template_interpolations(parts) {
                    check_expression(interpolation, bundle, module, callbacks, current, effects)?;
                }
            }
            LoweredExprKind::List(values) | LoweredExprKind::Tuple(values) => {
                for value in values {
                    check_expression(value, bundle, module, callbacks, current, effects)?;
                }
            }
            LoweredExprKind::ListWithSpread(items) => {
                for item in items {
                    check_expression(&item.value, bundle, module, callbacks, current, effects)?;
                }
            }
            LoweredExprKind::Map(entries) => {
                for (key, value) in entries {
                    check_expression(key, bundle, module, callbacks, current, effects)?;
                    check_expression(value, bundle, module, callbacks, current, effects)?;
                }
            }
            LoweredExprKind::MapWithSpread(items) => {
                for item in items {
                    match item {
                        super::LoweredMapItem::Entry { key, value, .. } => {
                            check_expression(key, bundle, module, callbacks, current, effects)?;
                            check_expression(value, bundle, module, callbacks, current, effects)?;
                        }
                        super::LoweredMapItem::Spread { value, .. } => {
                            check_expression(value, bundle, module, callbacks, current, effects)?;
                        }
                    }
                }
            }
            LoweredExprKind::RecordUpdate { base, fields, .. } => {
                check_expression(base, bundle, module, callbacks, current, effects)?;
                for (_, value, _) in fields {
                    check_expression(value, bundle, module, callbacks, current, effects)?;
                }
            }
            LoweredExprKind::Record { fields, .. }
            | LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Record(fields),
                ..
            } => {
                for (_, value, _) in fields {
                    check_expression(value, bundle, module, callbacks, current, effects)?;
                }
            }
            LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Tuple(values),
                ..
            } => {
                for value in values {
                    check_expression(value, bundle, module, callbacks, current, effects)?;
                }
            }
            LoweredExprKind::Prompt {
                system,
                context,
                data,
                ..
            } => {
                check_expression(system, bundle, module, callbacks, current, effects)?;
                if let Some(context) = context {
                    check_expression(context, bundle, module, callbacks, current, effects)?;
                }
                if let Some(data) = data {
                    check_expression(data, bundle, module, callbacks, current, effects)?;
                }
            }
            LoweredExprKind::Binary { left, right, .. }
            | LoweredExprKind::Range {
                start: left,
                end: right,
                ..
            } => {
                check_expression(left, bundle, module, callbacks, current, effects)?;
                check_expression(right, bundle, module, callbacks, current, effects)?;
            }
            LoweredExprKind::Compose { left, right, .. } => {
                check_expression(left, bundle, module, callbacks, current, effects)?;
                check_expression(right, bundle, module, callbacks, current, effects)?;
            }
            LoweredExprKind::Pipe { left, stage, .. } => {
                check_expression(left, bundle, module, callbacks, current, effects)?;
                check_expression(stage, bundle, module, callbacks, current, effects)?;
            }
            LoweredExprKind::Index {
                collection: left,
                index: right,
            }
            | LoweredExprKind::Slice {
                collection: left,
                range: right,
                ..
            } => {
                check_expression(left, bundle, module, callbacks, current, effects)?;
                check_expression(right, bundle, module, callbacks, current, effects)?;
            }
            LoweredExprKind::FieldGet { record, .. }
            | LoweredExprKind::OptionalFieldGet {
                receiver: record, ..
            }
            | LoweredExprKind::Try(record)
            | LoweredExprKind::Unary {
                operand: record, ..
            } => {
                check_expression(record, bundle, module, callbacks, current, effects)?;
            }
            LoweredExprKind::Spawn(value) => {
                effects.insert("task.spawn".to_owned());
                check_expression(value, bundle, module, callbacks, current, effects)?;
            }
            LoweredExprKind::Await(value) => {
                check_expression(value, bundle, module, callbacks, current, effects)?;
            }
            LoweredExprKind::AwaitBlock(body) => {
                check_body(body, bundle, module, callbacks, current, effects)?;
            }
            LoweredExprKind::Match { source, arms } => {
                check_expression(source, bundle, module, callbacks, current, effects)?;
                let source_shape = expression_shape(source, bundle, module, callbacks, current)?;
                for (pattern, value, _) in arms {
                    let mut nested = callbacks.clone();
                    bind_match_shape(pattern, &source_shape, &mut nested);
                    check_expression(value, bundle, module, &nested, current, effects)?;
                }
            }
            LoweredExprKind::If {
                condition,
                then_body,
                else_branch,
            } => {
                check_expression(condition, bundle, module, callbacks, current, effects)?;
                check_body(then_body, bundle, module, callbacks, current, effects)?;
                match else_branch {
                    Some(LoweredElse::Body(body)) => {
                        check_body(body, bundle, module, callbacks, current, effects)?;
                    }
                    Some(LoweredElse::If(expression)) => {
                        check_expression(expression, bundle, module, callbacks, current, effects)?;
                    }
                    None => {}
                }
            }
            LoweredExprKind::Call {
                callee, arguments, ..
            } => {
                if is_task_snapshot_callee(callee) {
                    effects.insert("debug.inspect".to_owned());
                } else if let Some(name) = template_callee(callee) {
                    if template_binding(bundle, module, name).is_none() {
                        return Err(Diagnostic::new(
                            "E3012",
                            format!("template '{name}' is not declared in this package"),
                            callee.span,
                        ));
                    }
                } else if let Some(path) = tool_callee(callee) {
                    let binding = bundle.tools.get(&path).ok_or_else(|| {
                        Diagnostic::new(
                            "E3005",
                            "tool call is not in the frozen catalog",
                            callee.span,
                        )
                    })?;
                    effects.insert(binding.effect.clone());
                } else if let Some(StandardBuiltin::Operation(operation)) =
                    standard_builtin_callee(callee)
                {
                    effects.insert(operation.required_effect().to_owned());
                } else if capability_builtin_callee(callee).is_some() {
                    effects.insert("capability.inspect".to_owned());
                } else if let EffectShape::Function {
                    effects: callback_effects,
                    ..
                } = expression_shape(callee, bundle, module, callbacks, current)?
                {
                    effects.extend(callback_effects);
                }
                for argument in arguments {
                    check_expression(&argument.value, bundle, module, callbacks, current, effects)?;
                }
            }
            LoweredExprKind::Closure {
                parameters,
                declared_effects,
                body,
                ..
            } => {
                let mut nested = callbacks.clone();
                for (name, value_type, _) in parameters {
                    nested.insert(name.clone(), type_shape(value_type, bundle, module));
                }
                let mut required = BTreeSet::new();
                check_body(body, bundle, module, &nested, current, &mut required)?;
                if let Some(declared) = declared_effects {
                    effects.extend(declared.iter().cloned());
                } else {
                    effects.extend(required);
                }
            }
            LoweredExprKind::Variable(name) => {
                if let Some(EffectShape::Function {
                    effects: local_effects,
                    local: true,
                    ..
                }) = callbacks.get(name)
                {
                    effects.extend(local_effects.iter().cloned());
                }
            }
            LoweredExprKind::Unit
            | LoweredExprKind::Int(_)
            | LoweredExprKind::Float(_)
            | LoweredExprKind::Bool(_)
            | LoweredExprKind::String(_)
            | LoweredExprKind::Bytes(_)
            | LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Unit,
                ..
            }
            | LoweredExprKind::ShortClosure { .. } => {}
        }
        Ok(())
    }

    pub(super) fn check_body(
        body: &LoweredBody,
        bundle: &ResolvedBundle,
        module: &str,
        callbacks: &BTreeMap<String, EffectShape>,
        current: &[Vec<String>],
        effects: &mut BTreeSet<String>,
    ) -> Result<(), Diagnostic> {
        let mut scoped = callbacks.clone();
        for statement in &body.statements {
            let (name, value) = match statement {
                LoweredStatement::Let { name, value, .. } => (Some(name), Some(value)),
                LoweredStatement::Assignment { value, .. }
                | LoweredStatement::ControlFlow(value) => (None, Some(value)),
                LoweredStatement::Return(value, _) => (None, value.as_ref()),
                LoweredStatement::While {
                    condition, body, ..
                } => {
                    check_expression(condition, bundle, module, &scoped, current, effects)?;
                    check_body(body, bundle, module, &scoped, current, effects)?;
                    continue;
                }
                LoweredStatement::Loop { body, .. } => {
                    check_body(body, bundle, module, &scoped, current, effects)?;
                    continue;
                }
                LoweredStatement::For {
                    binding,
                    source,
                    body,
                    ..
                } => {
                    let yielded = match source {
                        LoweredForSource::Iterable(value) => {
                            check_expression(value, bundle, module, &scoped, current, effects)?;
                            iterable_shape(expression_shape(
                                value, bundle, module, &scoped, current,
                            )?)
                        }
                    };
                    let mut nested = scoped.clone();
                    bind_loop_shape(binding, yielded, &mut nested);
                    check_body(body, bundle, module, &nested, current, effects)?;
                    continue;
                }
                LoweredStatement::LocalFunction(function) => {
                    scoped.insert(
                        function.name.clone(),
                        local_function_shape(function, bundle, module),
                    );
                    continue;
                }
                LoweredStatement::Break(_) | LoweredStatement::Continue(_) => continue,
            };
            let Some(value) = value else { continue };
            check_expression(value, bundle, module, &scoped, current, effects)?;
            if let Some(name) = name {
                let shape = match statement {
                    LoweredStatement::Let {
                        annotation: Some(annotation),
                        ..
                    } => type_shape(annotation, bundle, module),
                    _ => expression_shape(value, bundle, module, &scoped, current)?,
                };
                scoped.insert(name.clone(), shape);
            }
        }
        if let Some(tail) = &body.tail {
            check_expression(tail, bundle, module, &scoped, current, effects)?;
        }
        Ok(())
    }

    let callbacks = function
        .lowered
        .parameters
        .iter()
        .map(|(name, value_type, _)| {
            (
                name.clone(),
                type_shape(value_type, bundle, &function.module),
            )
        })
        .collect();
    let mut effects = BTreeSet::new();
    check_body(
        &function.lowered.body,
        bundle,
        &function.module,
        &callbacks,
        current,
        &mut effects,
    )?;
    Ok(effects.into_iter().collect())
}

#[allow(clippy::too_many_lines)]
pub(super) fn expression_uses_agent_transcript(expression: &LoweredExpr) -> bool {
    match &expression.kind {
        LoweredExprKind::Template(parts) => {
            template_interpolations(parts).any(expression_uses_agent_transcript)
        }
        LoweredExprKind::Call {
            callee, arguments, ..
        } => {
            standard_builtin_callee(callee)
                == Some(StandardBuiltin::Operation(EffectOperation::AgentTranscript))
                || expression_uses_agent_transcript(callee)
                || arguments
                    .iter()
                    .any(|argument| expression_uses_agent_transcript(&argument.value))
        }
        LoweredExprKind::List(values) | LoweredExprKind::Tuple(values) => {
            values.iter().any(expression_uses_agent_transcript)
        }
        LoweredExprKind::ListWithSpread(items) => items
            .iter()
            .any(|item| expression_uses_agent_transcript(&item.value)),
        LoweredExprKind::Map(entries) => entries.iter().any(|(key, value)| {
            expression_uses_agent_transcript(key) || expression_uses_agent_transcript(value)
        }),
        LoweredExprKind::MapWithSpread(items) => items.iter().any(|item| match item {
            super::LoweredMapItem::Entry { key, value, .. } => {
                expression_uses_agent_transcript(key) || expression_uses_agent_transcript(value)
            }
            super::LoweredMapItem::Spread { value, .. } => expression_uses_agent_transcript(value),
        }),
        LoweredExprKind::RecordUpdate { base, fields, .. } => {
            expression_uses_agent_transcript(base)
                || fields
                    .iter()
                    .any(|(_, value, _)| expression_uses_agent_transcript(value))
        }
        LoweredExprKind::Record { fields, .. }
        | LoweredExprKind::Enum {
            payload: LoweredEnumValuePayload::Record(fields),
            ..
        } => fields
            .iter()
            .any(|(_, value, _)| expression_uses_agent_transcript(value)),
        LoweredExprKind::Enum {
            payload: LoweredEnumValuePayload::Tuple(values),
            ..
        } => values.iter().any(expression_uses_agent_transcript),
        LoweredExprKind::Prompt {
            system,
            context,
            data,
            ..
        } => {
            expression_uses_agent_transcript(system)
                || context
                    .as_deref()
                    .is_some_and(expression_uses_agent_transcript)
                || data
                    .as_deref()
                    .is_some_and(expression_uses_agent_transcript)
        }
        LoweredExprKind::Binary { left, right, .. }
        | LoweredExprKind::Compose { left, right, .. }
        | LoweredExprKind::Range {
            start: left,
            end: right,
            ..
        } => expression_uses_agent_transcript(left) || expression_uses_agent_transcript(right),
        LoweredExprKind::Pipe { left, stage, .. } => {
            expression_uses_agent_transcript(left) || expression_uses_agent_transcript(stage)
        }
        LoweredExprKind::Index { collection, index }
        | LoweredExprKind::Slice {
            collection,
            range: index,
            ..
        } => {
            expression_uses_agent_transcript(collection) || expression_uses_agent_transcript(index)
        }
        LoweredExprKind::FieldGet { record, .. }
        | LoweredExprKind::OptionalFieldGet {
            receiver: record, ..
        }
        | LoweredExprKind::Try(record)
        | LoweredExprKind::Unary {
            operand: record, ..
        }
        | LoweredExprKind::Spawn(record)
        | LoweredExprKind::Await(record) => expression_uses_agent_transcript(record),
        LoweredExprKind::AwaitBlock(body) | LoweredExprKind::Closure { body, .. } => {
            body_uses_agent_transcript(body)
        }
        LoweredExprKind::Match { source, arms } => {
            expression_uses_agent_transcript(source)
                || arms
                    .iter()
                    .any(|(_, value, _)| expression_uses_agent_transcript(value))
        }
        LoweredExprKind::If {
            condition,
            then_body,
            else_branch,
        } => {
            expression_uses_agent_transcript(condition)
                || body_uses_agent_transcript(then_body)
                || match else_branch {
                    Some(LoweredElse::Body(body)) => body_uses_agent_transcript(body),
                    Some(LoweredElse::If(expression)) => {
                        expression_uses_agent_transcript(expression)
                    }
                    None => false,
                }
        }
        LoweredExprKind::ShortClosure { .. }
        | LoweredExprKind::Unit
        | LoweredExprKind::Int(_)
        | LoweredExprKind::Float(_)
        | LoweredExprKind::Bool(_)
        | LoweredExprKind::String(_)
        | LoweredExprKind::Bytes(_)
        | LoweredExprKind::Variable(_)
        | LoweredExprKind::Enum {
            payload: LoweredEnumValuePayload::Unit,
            ..
        } => false,
    }
}

pub(super) fn body_uses_agent_transcript(body: &LoweredBody) -> bool {
    body.statements.iter().any(|statement| match statement {
        LoweredStatement::Let { value, .. }
        | LoweredStatement::Assignment { value, .. }
        | LoweredStatement::ControlFlow(value) => expression_uses_agent_transcript(value),
        LoweredStatement::Return(value, _) => {
            value.as_ref().is_some_and(expression_uses_agent_transcript)
        }
        LoweredStatement::While {
            condition, body, ..
        } => expression_uses_agent_transcript(condition) || body_uses_agent_transcript(body),
        LoweredStatement::Loop { body, .. } => body_uses_agent_transcript(body),
        LoweredStatement::For { source, body, .. } => {
            let source_uses = match source {
                LoweredForSource::Iterable(value) => expression_uses_agent_transcript(value),
            };
            source_uses || body_uses_agent_transcript(body)
        }
        LoweredStatement::LocalFunction(function) => body_uses_agent_transcript(&function.body),
        LoweredStatement::Break(_) | LoweredStatement::Continue(_) => false,
    }) || body
        .tail
        .as_ref()
        .is_some_and(expression_uses_agent_transcript)
}

#[allow(clippy::too_many_lines)]
pub(super) fn direct_capability_inspection_span(expression: &LoweredExpr) -> Option<Span> {
    match &expression.kind {
        LoweredExprKind::Template(parts) => {
            template_interpolations(parts).find_map(direct_capability_inspection_span)
        }
        LoweredExprKind::Call {
            callee, arguments, ..
        } => capability_builtin_callee(callee)
            .map(|_| expression.span)
            .or_else(|| direct_capability_inspection_span(callee))
            .or_else(|| {
                arguments
                    .iter()
                    .find_map(|argument| direct_capability_inspection_span(&argument.value))
            }),
        LoweredExprKind::List(values) | LoweredExprKind::Tuple(values) => {
            values.iter().find_map(direct_capability_inspection_span)
        }
        LoweredExprKind::ListWithSpread(items) => items
            .iter()
            .find_map(|item| direct_capability_inspection_span(&item.value)),
        LoweredExprKind::Map(entries) => entries.iter().find_map(|(key, value)| {
            direct_capability_inspection_span(key)
                .or_else(|| direct_capability_inspection_span(value))
        }),
        LoweredExprKind::MapWithSpread(items) => items.iter().find_map(|item| match item {
            super::LoweredMapItem::Entry { key, value, .. } => {
                direct_capability_inspection_span(key)
                    .or_else(|| direct_capability_inspection_span(value))
            }
            super::LoweredMapItem::Spread { value, .. } => direct_capability_inspection_span(value),
        }),
        LoweredExprKind::RecordUpdate { base, fields, .. } => {
            direct_capability_inspection_span(base).or_else(|| {
                fields
                    .iter()
                    .find_map(|(_, value, _)| direct_capability_inspection_span(value))
            })
        }
        LoweredExprKind::Record { fields, .. }
        | LoweredExprKind::Enum {
            payload: LoweredEnumValuePayload::Record(fields),
            ..
        } => fields
            .iter()
            .find_map(|(_, value, _)| direct_capability_inspection_span(value)),
        LoweredExprKind::Enum {
            payload: LoweredEnumValuePayload::Tuple(values),
            ..
        } => values.iter().find_map(direct_capability_inspection_span),
        LoweredExprKind::Prompt {
            system,
            context,
            data,
            ..
        } => direct_capability_inspection_span(system)
            .or_else(|| {
                context
                    .as_deref()
                    .and_then(direct_capability_inspection_span)
            })
            .or_else(|| data.as_deref().and_then(direct_capability_inspection_span)),
        LoweredExprKind::Binary { left, right, .. }
        | LoweredExprKind::Compose { left, right, .. }
        | LoweredExprKind::Range {
            start: left,
            end: right,
            ..
        } => direct_capability_inspection_span(left)
            .or_else(|| direct_capability_inspection_span(right)),
        LoweredExprKind::Pipe { left, stage, .. } => direct_capability_inspection_span(left)
            .or_else(|| direct_capability_inspection_span(stage)),
        LoweredExprKind::Index { collection, index }
        | LoweredExprKind::Slice {
            collection,
            range: index,
            ..
        } => direct_capability_inspection_span(collection)
            .or_else(|| direct_capability_inspection_span(index)),
        LoweredExprKind::FieldGet { record, .. }
        | LoweredExprKind::OptionalFieldGet {
            receiver: record, ..
        }
        | LoweredExprKind::Try(record)
        | LoweredExprKind::Unary {
            operand: record, ..
        }
        | LoweredExprKind::Spawn(record)
        | LoweredExprKind::Await(record) => direct_capability_inspection_span(record),
        LoweredExprKind::AwaitBlock(body) => direct_capability_inspection_body_span(body),
        LoweredExprKind::Match { source, arms } => direct_capability_inspection_span(source)
            .or_else(|| {
                arms.iter()
                    .find_map(|(_, value, _)| direct_capability_inspection_span(value))
            }),
        LoweredExprKind::If {
            condition,
            then_body,
            else_branch,
        } => direct_capability_inspection_span(condition)
            .or_else(|| direct_capability_inspection_body_span(then_body))
            .or_else(|| match else_branch {
                Some(LoweredElse::Body(body)) => direct_capability_inspection_body_span(body),
                Some(LoweredElse::If(expression)) => direct_capability_inspection_span(expression),
                None => None,
            }),
        LoweredExprKind::Closure { .. }
        | LoweredExprKind::ShortClosure { .. }
        | LoweredExprKind::Unit
        | LoweredExprKind::Int(_)
        | LoweredExprKind::Float(_)
        | LoweredExprKind::Bool(_)
        | LoweredExprKind::String(_)
        | LoweredExprKind::Bytes(_)
        | LoweredExprKind::Variable(_)
        | LoweredExprKind::Enum {
            payload: LoweredEnumValuePayload::Unit,
            ..
        } => None,
    }
}

pub(super) fn direct_capability_inspection_body_span(body: &LoweredBody) -> Option<Span> {
    body.statements
        .iter()
        .find_map(|statement| match statement {
            LoweredStatement::Let { value, .. }
            | LoweredStatement::Assignment { value, .. }
            | LoweredStatement::ControlFlow(value) => direct_capability_inspection_span(value),
            LoweredStatement::Return(value, _) => {
                value.as_ref().and_then(direct_capability_inspection_span)
            }
            LoweredStatement::While {
                condition, body, ..
            } => direct_capability_inspection_span(condition)
                .or_else(|| direct_capability_inspection_body_span(body)),
            LoweredStatement::Loop { body, .. } => direct_capability_inspection_body_span(body),
            LoweredStatement::For { source, body, .. } => {
                let source_span = match source {
                    LoweredForSource::Iterable(value) => direct_capability_inspection_span(value),
                };
                source_span.or_else(|| direct_capability_inspection_body_span(body))
            }
            LoweredStatement::LocalFunction(function) => {
                direct_capability_inspection_body_span(&function.body)
            }
            LoweredStatement::Break(_) | LoweredStatement::Continue(_) => None,
        })
        .or_else(|| {
            body.tail
                .as_ref()
                .and_then(direct_capability_inspection_span)
        })
}

#[allow(clippy::too_many_lines)]
pub(super) fn resolve_bundle(
    modules: BTreeMap<String, LoweredModule>,
    tool_bindings: &[CompilerToolBinding],
    template_bindings: &[CompilerTemplateBinding],
) -> Result<ResolvedBundle, Diagnostic> {
    let uses_agent_transcript = modules.values().any(|module| {
        module
            .functions
            .iter()
            .any(|function| body_uses_agent_transcript(&function.body))
    });
    let mut type_declarations = Vec::new();
    for (module, definition) in &modules {
        let mut seen = BTreeSet::new();
        for declaration in &definition.types {
            if matches!(
                declaration,
                LoweredTypeDeclaration::Alias { .. } | LoweredTypeDeclaration::Newtype { .. }
            ) && builtin_semantic_type(declaration.name()).is_some()
            {
                let kind = if matches!(declaration, LoweredTypeDeclaration::Alias { .. }) {
                    "type alias"
                } else {
                    "newtype"
                };
                return Err(Diagnostic::new(
                    "E3005",
                    format!(
                        "{kind} '{}' conflicts with built-in type '{}'",
                        declaration.name(),
                        declaration.name()
                    ),
                    declaration.name_span(),
                )
                .with_source(module));
            }
            if !seen.insert(declaration.name().to_owned()) {
                return Err(Diagnostic::new(
                    "E3005",
                    format!("duplicate type '{}'", declaration.name()),
                    declaration.name_span(),
                )
                .with_source(module));
            }
            type_declarations.push((
                module.clone(),
                declaration.name().to_owned(),
                declaration.clone(),
            ));
        }
    }
    type_declarations.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    let alias_order = validate_alias_cycles(&modules)?;
    let mut deferred_effect_sets = BTreeSet::new();
    for (_, _, declaration) in &type_declarations {
        match declaration {
            LoweredTypeDeclaration::Record { fields, .. } => {
                for (_, field, _) in fields {
                    collect_type_effect_sets(field, &mut deferred_effect_sets);
                }
            }
            LoweredTypeDeclaration::Alias { target, .. } => {
                collect_type_effect_sets(target, &mut deferred_effect_sets);
            }
            LoweredTypeDeclaration::Newtype { underlying, .. } => {
                collect_type_effect_sets(underlying, &mut deferred_effect_sets);
            }
            LoweredTypeDeclaration::Enum { .. } => {}
        }
    }
    let deferred_effect_sets = deferred_effect_sets.into_iter().collect::<Vec<_>>();
    let mut types = BTreeMap::new();
    let mut enum_types = Vec::new();
    let mut enum_spans = Vec::new();
    for (module, name, declaration) in &type_declarations {
        if let LoweredTypeDeclaration::Enum { variants, .. } = declaration {
            let enum_id = u32::try_from(enum_types.len()).map_err(|_| {
                Diagnostic::new(
                    "E3005",
                    "too many nominal enum types",
                    declaration.name_span(),
                )
                .with_source(module)
            })?;
            let mut variant_names = BTreeSet::new();
            for variant in variants {
                if !variant_names.insert(variant.name.clone()) {
                    return Err(Diagnostic::new(
                        "E3005",
                        format!("duplicate enum variant '{}'", variant.name),
                        variant.span,
                    )
                    .with_source(module));
                }
            }
            types.insert((module.clone(), name.clone()), ValueType::Enum(enum_id));
            enum_types.push(EnumType {
                name: format!("{module}::{name}"),
                variants: Vec::new(),
            });
            enum_spans.push((module.clone(), declaration.name_span()));
        }
    }
    let transcript_part = if uses_agent_transcript {
        if type_declarations
            .iter()
            .any(|(_, name, _)| name == "TranscriptPart")
        {
            return Err(Diagnostic::new(
                "E3005",
                "TranscriptPart is a reserved standard type",
                Span { start: 0, end: 0 },
            ));
        }
        let id = u32::try_from(enum_types.len()).map_err(|_| {
            Diagnostic::new(
                "E3005",
                "too many nominal enum types",
                Span { start: 0, end: 0 },
            )
        })?;
        enum_types.push(transcript_part_enum_type());
        enum_spans.push(("<synthetic>".to_owned(), Span { start: 0, end: 0 }));
        for module in modules.keys() {
            types.insert(
                (module.clone(), "TranscriptPart".to_owned()),
                ValueType::Enum(id),
            );
            types.insert(
                (module.clone(), "TranscriptMessage".to_owned()),
                transcript_message_type(id),
            );
            types.insert(
                (module.clone(), "TranscriptSnapshot".to_owned()),
                transcript_snapshot_type(id),
            );
        }
        Some(id)
    } else {
        None
    };
    let alias_declarations = type_declarations
        .iter()
        .filter_map(|(module, name, declaration)| {
            matches!(declaration, LoweredTypeDeclaration::Alias { .. })
                .then_some(((module.clone(), name.clone()), declaration))
        })
        .collect::<BTreeMap<_, _>>();
    let mut pending_aliases = alias_declarations.keys().cloned().collect::<BTreeSet<_>>();
    let mut pending_newtypes = type_declarations
        .iter()
        .filter_map(|(module, name, declaration)| {
            matches!(declaration, LoweredTypeDeclaration::Newtype { .. })
                .then_some(((module.clone(), name.clone()), declaration))
        })
        .collect::<BTreeMap<_, _>>();
    let mut pending_records = type_declarations
        .iter()
        .filter_map(|(module, name, declaration)| {
            matches!(declaration, LoweredTypeDeclaration::Record { .. })
                .then_some(((module.clone(), name.clone()), declaration))
        })
        .collect::<BTreeMap<_, _>>();
    let mut alias_shapes = Vec::new();
    let mut newtype_shapes = Vec::new();
    let mut record_shapes = Vec::new();
    while !pending_aliases.is_empty() || !pending_newtypes.is_empty() || !pending_records.is_empty()
    {
        let mut progress = false;
        let mut first_unresolved = None;
        let pending_nominals = pending_aliases
            .iter()
            .cloned()
            .chain(pending_newtypes.keys().cloned())
            .collect::<BTreeSet<_>>();

        for key in &alias_order {
            if !pending_aliases.contains(key) {
                continue;
            }
            let declaration = alias_declarations[key];
            let LoweredTypeDeclaration::Alias { target, .. } = declaration else {
                unreachable!("pending alias declaration is an alias")
            };
            if has_pending_alias_dependency(&modules, &key.0, target, &pending_nominals)? {
                continue;
            }
            match semantic_type(target, &BTreeSet::new(), &key.0, &modules, &types)
                .map_err(|diagnostic| diagnostic.with_source(&key.0))
            {
                Ok(SemanticType::Value(value_type)) => {
                    alias_shapes.push((value_type.clone(), key.0.clone(), declaration.name_span()));
                    types.insert(key.clone(), value_type);
                    pending_aliases.remove(key);
                    progress = true;
                }
                Ok(SemanticType::Function { .. } | SemanticType::Generic(_)) => {
                    pending_aliases.remove(key);
                    progress = true;
                }
                Err(diagnostic)
                    if diagnostic.code == "E3005"
                        && diagnostic.message.starts_with("unknown type '") =>
                {
                    first_unresolved.get_or_insert(diagnostic);
                }
                Err(diagnostic) => return Err(diagnostic),
            }
        }

        let newtype_keys = pending_newtypes.keys().cloned().collect::<Vec<_>>();
        for key in newtype_keys {
            let declaration = pending_newtypes[&key];
            let LoweredTypeDeclaration::Newtype { underlying, .. } = declaration else {
                unreachable!("pending newtype declaration is a newtype")
            };
            if has_pending_alias_dependency(&modules, &key.0, underlying, &pending_nominals)? {
                continue;
            }
            match semantic_type(underlying, &BTreeSet::new(), &key.0, &modules, &types)
                .map_err(|diagnostic| diagnostic.with_source(&key.0))
            {
                Ok(SemanticType::Value(value_type)) => {
                    if !valid_newtype_underlying(&value_type) {
                        return Err(Diagnostic::new(
                            "E3011",
                            "newtype underlying type must be complete, inhabited, non-affine, and non-callable",
                            underlying.span(),
                        )
                        .with_source(&key.0));
                    }
                    let value_type = ValueType::Newtype {
                        name: format!("{}::{}", key.0, key.1),
                        underlying: Box::new(value_type),
                    };
                    newtype_shapes.push((
                        value_type.clone(),
                        key.0.clone(),
                        declaration.name_span(),
                    ));
                    types.insert(key.clone(), value_type);
                    pending_newtypes.remove(&key);
                    progress = true;
                }
                Ok(SemanticType::Function { .. } | SemanticType::Generic(_)) => {
                    return Err(Diagnostic::new(
                        "E3011",
                        "newtype underlying type must be a complete value type",
                        underlying.span(),
                    )
                    .with_source(&key.0));
                }
                Err(diagnostic)
                    if diagnostic.code == "E3005"
                        && diagnostic.message.starts_with("unknown type '") =>
                {
                    first_unresolved.get_or_insert(diagnostic);
                }
                Err(diagnostic) => return Err(diagnostic),
            }
        }

        let record_keys = pending_records.keys().cloned().collect::<Vec<_>>();
        for key in record_keys {
            let declaration = pending_records[&key];
            let LoweredTypeDeclaration::Record { fields, .. } = declaration else {
                unreachable!("pending type declaration is a record")
            };
            if fields.iter().try_fold(false, |pending, (_, field, _)| {
                Ok::<_, Diagnostic>(
                    pending
                        || has_pending_alias_dependency(
                            &modules,
                            &key.0,
                            field,
                            &pending_nominals,
                        )?,
                )
            })? {
                continue;
            }
            match resolve_record_layout(fields, &key.0, &modules, &types, &deferred_effect_sets) {
                Ok(layout) => {
                    record_shapes.push((
                        ValueType::Record(layout.clone()),
                        key.0.clone(),
                        declaration.name_span(),
                    ));
                    types.insert(key.clone(), ValueType::Record(layout));
                    pending_records.remove(&key);
                    progress = true;
                }
                Err(diagnostic)
                    if diagnostic.code == "E3005"
                        && diagnostic.message.starts_with("unknown type '") =>
                {
                    first_unresolved.get_or_insert(diagnostic);
                }
                Err(diagnostic) => return Err(diagnostic),
            }
        }

        if !progress {
            if let Some((key, declaration)) = pending_newtypes.first_key_value() {
                return Err(Diagnostic::new(
                    "E2012",
                    format!("recursive or unresolved newtype '{}'", key.1),
                    declaration.name_span(),
                )
                .with_source(&key.0));
            }
            if let Some(diagnostic) = first_unresolved {
                return Err(diagnostic);
            }
            let key = pending_records
                .keys()
                .next()
                .expect("pending aliases depend on an unresolved record");
            return Err(Diagnostic::new(
                "E3005",
                format!("unknown type dependency for record '{}'", key.1),
                pending_records[key].name_span(),
            )
            .with_source(&key.0));
        }
    }
    for (module, name, declaration) in &type_declarations {
        let LoweredTypeDeclaration::Enum { variants, .. } = declaration else {
            continue;
        };
        let ValueType::Enum(enum_id) = types[&(module.clone(), name.clone())] else {
            unreachable!("enum declaration has a nominal type ID")
        };
        let mut metadata = Vec::with_capacity(variants.len());
        for variant in variants {
            let payload = match &variant.payload {
                LoweredEnumPayload::Unit => EnumPayloadType::Unit,
                LoweredEnumPayload::Tuple(values) => EnumPayloadType::Tuple(
                    values
                        .iter()
                        .map(|value| {
                            let SemanticType::Value(value_type) =
                                semantic_type(value, &BTreeSet::new(), module, &modules, &types)
                                    .map_err(|diagnostic| diagnostic.with_source(module))?
                            else {
                                return Err(Diagnostic::new(
                                    "E3007",
                                    "enum payload type must be concrete",
                                    value.span(),
                                )
                                .with_source(module));
                            };
                            Ok(value_type)
                        })
                        .collect::<Result<_, Diagnostic>>()?,
                ),
                LoweredEnumPayload::Record(fields) => {
                    let mut seen = BTreeSet::new();
                    let mut lowered = Vec::with_capacity(fields.len());
                    for (field, value, span) in fields {
                        if !seen.insert(field.clone()) {
                            return Err(Diagnostic::new(
                                "E3005",
                                format!("duplicate enum record field '{field}'"),
                                *span,
                            )
                            .with_source(module));
                        }
                        let SemanticType::Value(value_type) =
                            semantic_type(value, &BTreeSet::new(), module, &modules, &types)
                                .map_err(|diagnostic| diagnostic.with_source(module))?
                        else {
                            return Err(Diagnostic::new(
                                "E3007",
                                "enum payload type must be concrete",
                                value.span(),
                            )
                            .with_source(module));
                        };
                        lowered.push(RecordField {
                            name: field.clone(),
                            value_type,
                        });
                    }
                    lowered.sort_by(|left, right| left.name.cmp(&right.name));
                    EnumPayloadType::Record(lowered)
                }
            };
            metadata.push(EnumVariant {
                name: variant.name.clone(),
                payload,
            });
        }
        enum_types[enum_id as usize].variants = metadata;
    }
    validate_declared_type_shapes(
        &enum_types,
        &enum_spans,
        &record_shapes,
        &alias_shapes,
        &newtype_shapes,
    )?;
    let mut next_tool_enum = u32::try_from(enum_types.len()).map_err(|_| {
        Diagnostic::new(
            "E3005",
            "too many nominal enum types",
            Span { start: 0, end: 0 },
        )
    })?;
    for binding in tool_bindings {
        let local_enum_count = binding.enum_types.len();
        let mut input = binding.input.clone();
        let mut output = binding.output.clone();
        let mut declared_error = binding.declared_error.clone();
        let mut error = binding.error.clone();
        rebase_tool_type(&mut input, next_tool_enum, local_enum_count)?;
        rebase_tool_type(&mut output, next_tool_enum, local_enum_count)?;
        rebase_tool_type(&mut declared_error, next_tool_enum, local_enum_count)?;
        rebase_tool_type(&mut error, next_tool_enum, local_enum_count)?;
        let namespace = format!("tools.{}", binding.source_path.join("."));
        for module in modules.keys() {
            types.insert(
                (module.clone(), format!("{namespace}.Input")),
                input.clone(),
            );
            types.insert(
                (module.clone(), format!("{namespace}.Output")),
                output.clone(),
            );
            types.insert(
                (module.clone(), format!("{namespace}.DeclaredError")),
                declared_error.clone(),
            );
            types.insert(
                (module.clone(), format!("{namespace}.Error")),
                error.clone(),
            );
        }
        next_tool_enum = next_tool_enum
            .checked_add(u32::try_from(local_enum_count).map_err(|_| {
                Diagnostic::new(
                    "E3005",
                    "too many nominal enum types",
                    Span { start: 0, end: 0 },
                )
            })?)
            .ok_or_else(|| {
                Diagnostic::new(
                    "E3005",
                    "too many nominal enum types",
                    Span { start: 0, end: 0 },
                )
            })?;
    }
    let mut keys = Vec::new();
    for (module, definition) in &modules {
        let mut seen = definition
            .types
            .iter()
            .filter(|declaration| {
                matches!(
                    declaration,
                    LoweredTypeDeclaration::Record { .. } | LoweredTypeDeclaration::Newtype { .. }
                )
            })
            .map(|declaration| declaration.name().to_owned())
            .collect::<BTreeSet<_>>();
        for function in &definition.functions {
            if !seen.insert(function.name.clone()) {
                return Err(Diagnostic::new(
                    "E3005",
                    format!("duplicate value '{}'", function.name),
                    function.name_span,
                )
                .with_source(module));
            }
            if function.parameter_defaults.len() != function.parameters.len() {
                return Err(Diagnostic::new(
                    "E3010",
                    format!(
                        "function '{}' has inconsistent parameter default metadata",
                        function.name
                    ),
                    function.name_span,
                )
                .with_source(module));
            }
            let mut saw_default = false;
            for (parameter_index, default) in function.parameter_defaults.iter().enumerate() {
                let Some(default) = default else {
                    if saw_default {
                        let (parameter, _, parameter_span) = &function.parameters[parameter_index];
                        return Err(Diagnostic::new(
                            "E3010",
                            format!(
                                "required parameter '{parameter}' cannot follow a default parameter"
                            ),
                            *parameter_span,
                        )
                        .with_source(module));
                    }
                    continue;
                };
                saw_default = true;
                let forbidden = function.parameters[parameter_index..]
                    .iter()
                    .map(|(name, _, _)| name.as_str())
                    .collect::<BTreeSet<_>>();
                if let Some(reference_span) = expression_references_any(&default.value, &forbidden)
                {
                    return Err(Diagnostic::new(
                        "E3010",
                        "a parameter default can reference only constants and earlier parameters",
                        reference_span,
                    )
                    .with_label(default.span, "default declared here")
                    .with_source(module));
                }
                let (_, parameter_type, _) = &function.parameters[parameter_index];
                let helper_name = default_helper_name(&function.name, parameter_index);
                keys.push((
                    module.clone(),
                    helper_name.clone(),
                    LoweredFunction {
                        exported: false,
                        is_async: false,
                        name: helper_name,
                        name_span: default.span,
                        generics: function.generics.clone(),
                        parameters: function.parameters[..parameter_index].to_vec(),
                        parameter_defaults: vec![None; parameter_index],
                        return_type: parameter_type.clone(),
                        declared_effects: Some(Vec::new()),
                        effects_span: Some(default.span),
                        body: LoweredBody {
                            statements: Vec::new(),
                            tail: Some(default.value.clone()),
                            span: default.span,
                        },
                    },
                    false,
                ));
            }
            keys.push((
                module.clone(),
                function.name.clone(),
                function.clone(),
                false,
            ));
        }
        for constant in &definition.constants {
            if !seen.insert(constant.name.clone()) {
                return Err(Diagnostic::new(
                    "E3005",
                    format!("duplicate value '{}'", constant.name),
                    constant.name_span,
                )
                .with_source(module));
            }
            keys.push((
                module.clone(),
                constant.name.clone(),
                LoweredFunction {
                    exported: constant.exported,
                    is_async: false,
                    name: constant.name.clone(),
                    name_span: constant.name_span,
                    generics: Vec::new(),
                    parameters: Vec::new(),
                    parameter_defaults: Vec::new(),
                    return_type: constant.value_type.clone(),
                    declared_effects: Some(Vec::new()),
                    effects_span: None,
                    body: LoweredBody {
                        statements: Vec::new(),
                        tail: Some(constant.value.clone()),
                        span: constant.value.span,
                    },
                },
                true,
            ));
        }
    }
    keys.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    let mut names = BTreeMap::new();
    let mut functions = Vec::new();
    let mut next_bytecode = 0_u32;
    for (index, (module, name, lowered, is_const)) in keys.into_iter().enumerate() {
        let symbol = u32::try_from(index).map_err(|_| {
            Diagnostic::new("E3005", "too many function declarations", lowered.name_span)
                .with_source(&module)
        })?;
        names.insert((module.clone(), name), symbol);
        let generics = lowered
            .generics
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>();
        let parameters = lowered
            .parameters
            .iter()
            .map(|(_, value_type, _)| {
                semantic_type(value_type, &generics, &module, &modules, &types)
            })
            .collect::<Result<_, _>>()
            .map_err(|diagnostic| diagnostic.with_source(&module))?;
        let return_type = semantic_type(&lowered.return_type, &generics, &module, &modules, &types)
            .map_err(|diagnostic| diagnostic.with_source(&module))?;
        let bytecode = if lowered.generics.is_empty() {
            let value = next_bytecode;
            next_bytecode = next_bytecode.checked_add(1).ok_or_else(|| {
                Diagnostic::new("E3005", "too many functions", lowered.name_span)
                    .with_source(&module)
            })?;
            Some(value)
        } else {
            None
        };
        let effects = lowered.declared_effects.clone().unwrap_or_default();
        functions.push(FunctionInfo {
            module,
            symbol,
            bytecode,
            lowered,
            parameters,
            return_type,
            effects,
            is_const,
        });
    }
    let mut tools = BTreeMap::new();
    for binding in tool_bindings {
        let enum_base = u32::try_from(enum_types.len()).map_err(|_| {
            Diagnostic::new(
                "E3005",
                "too many nominal enum types",
                Span { start: 0, end: 0 },
            )
        })?;
        let mut binding = binding.clone();
        let local_enum_count = binding.enum_types.len();
        rebase_tool_type(&mut binding.input, enum_base, local_enum_count)?;
        rebase_tool_type(&mut binding.output, enum_base, local_enum_count)?;
        rebase_tool_type(&mut binding.declared_error, enum_base, local_enum_count)?;
        rebase_tool_type(&mut binding.error, enum_base, local_enum_count)?;
        for enum_type in &mut binding.enum_types {
            for variant in &mut enum_type.variants {
                match &mut variant.payload {
                    EnumPayloadType::Unit => {}
                    EnumPayloadType::Tuple(values) => {
                        for value in values {
                            rebase_tool_type(value, enum_base, local_enum_count)?;
                        }
                    }
                    EnumPayloadType::Record(fields) => {
                        for field in fields {
                            rebase_tool_type(&mut field.value_type, enum_base, local_enum_count)?;
                        }
                    }
                }
            }
            let owner = modules.keys().next().ok_or_else(|| {
                Diagnostic::new(
                    "E3005",
                    "tool binding requires a source module",
                    Span { start: 0, end: 0 },
                )
            })?;
            enum_type.name = format!("{owner}::_tool_{}", mangle_source_segment(&enum_type.name));
        }
        enum_types.extend(binding.enum_types.iter().cloned());
        if binding.source_path.is_empty()
            || binding.source_path.iter().any(String::is_empty)
            || tools.insert(binding.source_path.clone(), binding).is_some()
        {
            return Err(Diagnostic::new(
                "E3005",
                "tool source binding is invalid or duplicated",
                Span { start: 0, end: 0 },
            ));
        }
    }
    let mut templates = BTreeMap::new();
    let mut template_indexes = BTreeSet::new();
    for binding in template_bindings {
        if binding.package.is_empty()
            || !is_template_name(&binding.name)
            || !template_indexes.insert(binding.template)
            || templates
                .insert(
                    (binding.package.clone(), binding.name.clone()),
                    binding.clone(),
                )
                .is_some()
        {
            return Err(Diagnostic::new(
                "E3012",
                "template source binding is invalid or duplicated",
                Span { start: 0, end: 0 },
            ));
        }
    }
    let mut bundle = ResolvedBundle {
        modules,
        functions,
        names,
        types,
        enum_types,
        transcript_part,
        tools,
        templates,
        deferred_effect_sets,
    };

    for (module, definition) in &bundle.modules {
        let local_values = definition
            .functions
            .iter()
            .map(|function| function.name.clone())
            .chain(
                definition
                    .constants
                    .iter()
                    .map(|constant| constant.name.clone()),
            )
            .chain(
                definition
                    .types
                    .iter()
                    .filter(|declaration| {
                        matches!(
                            declaration,
                            LoweredTypeDeclaration::Record { .. }
                                | LoweredTypeDeclaration::Newtype { .. }
                        )
                    })
                    .map(|declaration| declaration.name().to_owned()),
            )
            .collect::<BTreeSet<_>>();
        let mut bindings = BTreeSet::new();
        for import in &definition.imports {
            let target = resolve_import_path(module, import)
                .map_err(|diagnostic| diagnostic.with_source(module))?;
            for (imported, local, span) in &import.names {
                if !import.extension && !bindings.insert(local.clone()) {
                    return Err(Diagnostic::new(
                        "E3003",
                        format!("duplicate import binding '{local}'"),
                        *span,
                    )
                    .with_source(module));
                }
                let function = bundle
                    .names
                    .get(&(target.clone(), imported.clone()))
                    .copied();
                let exported_function = function
                    .is_some_and(|symbol| bundle.functions[symbol as usize].lowered.exported);
                let imported_declaration = bundle.modules[&target]
                    .types
                    .iter()
                    .find(|declaration| declaration.name() == imported && declaration.exported());
                let imported_type = imported_declaration.is_some();
                if !exported_function && (!imported_type || import.extension) {
                    let message = function.map_or_else(
                        || format!("unknown import '{imported}'"),
                        |_| format!("function '{imported}' is private to module '{target}'"),
                    );
                    return Err(Diagnostic::new("E3003", message, *span).with_source(module));
                }
                let imported_value = !import.extension
                    && (exported_function
                        || imported_declaration.is_some_and(|declaration| {
                            matches!(
                                declaration,
                                LoweredTypeDeclaration::Record { .. }
                                    | LoweredTypeDeclaration::Newtype { .. }
                            )
                        }));
                if imported_value && local_values.contains(local) {
                    return Err(Diagnostic::new(
                        "E3003",
                        format!("import binding '{local}' conflicts with a local value"),
                        *span,
                    )
                    .with_source(module));
                }
            }
        }
    }

    let mut current = bundle
        .functions
        .iter()
        .map(|function| {
            function
                .lowered
                .declared_effects
                .clone()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    loop {
        let mut changed = false;
        for index in 0..bundle.functions.len() {
            let module = bundle.functions[index].module.clone();
            if let Some(call_span) =
                direct_capability_inspection_body_span(&bundle.functions[index].lowered.body)
            {
                let declared = bundle.functions[index].lowered.declared_effects.as_deref();
                if !declared.is_some_and(|effects| {
                    effects
                        .binary_search_by(|effect| effect.as_str().cmp("capability.inspect"))
                        .is_ok()
                }) {
                    return Err(Diagnostic::new(
                        "E2403",
                        format!(
                            "function '{}' directly inspects capabilities and must explicitly declare effect 'capability.inspect'",
                            bundle.functions[index].lowered.name
                        ),
                        bundle.functions[index]
                            .lowered
                            .effects_span
                            .unwrap_or(call_span),
                    )
                    .with_source(module));
                }
            }
            let required = required_body_effects(&bundle, &bundle.functions[index], &current)
                .map_err(|diagnostic| diagnostic.with_source(&module))?;
            if let Some(declared) = &bundle.functions[index].lowered.declared_effects {
                if required
                    .iter()
                    .any(|effect| declared.binary_search(effect).is_err())
                {
                    return Err(Diagnostic::new(
                        "E2403",
                        format!(
                            "function '{}' requires undeclared effects [{}]",
                            bundle.functions[index].lowered.name,
                            required.join(", ")
                        ),
                        bundle.functions[index]
                            .lowered
                            .effects_span
                            .unwrap_or(bundle.functions[index].lowered.name_span),
                    )
                    .with_source(module));
                }
            } else if current[index] != required {
                current[index] = required;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for (function, effects) in bundle.functions.iter_mut().zip(current) {
        function.effects = effects;
    }
    Ok(bundle)
}

fn is_template_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|first| {
        (first.is_ascii_alphabetic() || first == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

pub(super) fn rebase_tool_type(
    value_type: &mut ValueType,
    base: u32,
    local_enum_count: usize,
) -> Result<(), Diagnostic> {
    match value_type {
        ValueType::Enum(id) => {
            if *id as usize >= local_enum_count {
                return Err(Diagnostic::new(
                    "E3005",
                    "tool binding references an invalid generated enum",
                    Span { start: 0, end: 0 },
                ));
            }
            *id = id.checked_add(base).ok_or_else(|| {
                Diagnostic::new(
                    "E3005",
                    "too many nominal enum types",
                    Span { start: 0, end: 0 },
                )
            })?;
        }
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value)
        | ValueType::Sequence(value)
        | ValueType::Newtype {
            underlying: value, ..
        } => {
            rebase_tool_type(value, base, local_enum_count)?;
        }
        ValueType::Map(left, right) | ValueType::Result(left, right) => {
            rebase_tool_type(left, base, local_enum_count)?;
            rebase_tool_type(right, base, local_enum_count)?;
        }
        ValueType::Tuple(values) => {
            for value in values {
                rebase_tool_type(value, base, local_enum_count)?;
            }
        }
        ValueType::Record(fields) => {
            for field in fields {
                rebase_tool_type(&mut field.value_type, base, local_enum_count)?;
            }
        }
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => {
            for parameter in parameters {
                rebase_tool_type(parameter, base, local_enum_count)?;
            }
            rebase_tool_type(return_type, base, local_enum_count)?;
        }
        ValueType::Int
        | ValueType::Range
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::Unit
        | ValueType::Never
        | ValueType::Workspace
        | ValueType::ExternalFsAccess
        | ValueType::SubAgent
        | ValueType::Unknown => {}
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn collect_effect_sets(bundle: &ResolvedBundle) -> Vec<Vec<String>> {
    pub(super) fn type_effects(value_type: &LoweredType, sets: &mut BTreeSet<Vec<String>>) {
        match value_type {
            LoweredType::Tuple(elements, _) => {
                for element in elements {
                    type_effects(element, sets);
                }
            }
            LoweredType::Record(fields, _) => {
                for (_, value, _) in fields {
                    type_effects(value, sets);
                }
            }
            LoweredType::List(value, _)
            | LoweredType::Option(value, _)
            | LoweredType::Future(value, _)
            | LoweredType::Task(value, _)
            | LoweredType::Prompt(value, _)
            | LoweredType::Range(value, _)
            | LoweredType::Sequence(value, _) => {
                type_effects(value, sets);
            }
            LoweredType::Map(key, value, _) => {
                type_effects(key, sets);
                type_effects(value, sets);
            }
            LoweredType::Result(ok, error, _) => {
                type_effects(ok, sets);
                type_effects(error, sets);
            }
            LoweredType::Function {
                parameters,
                return_type,
                effects,
                ..
            } => {
                sets.insert(effects.clone());
                for parameter in parameters {
                    type_effects(parameter, sets);
                }
                type_effects(return_type, sets);
            }
            LoweredType::Named(_, _) => {}
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum CollectedEffectShape {
        Function {
            effects: Vec<String>,
            result: Box<CollectedEffectShape>,
        },
        List(Box<CollectedEffectShape>),
        Map(Box<CollectedEffectShape>, Box<CollectedEffectShape>),
        Tuple(Vec<CollectedEffectShape>),
        Record(BTreeMap<String, CollectedEffectShape>),
        Never,
        Other,
    }

    pub(super) fn type_shape(
        value_type: &LoweredType,
        bundle: &ResolvedBundle,
        module: &str,
    ) -> CollectedEffectShape {
        fn resolve(
            value_type: &LoweredType,
            bundle: &ResolvedBundle,
            module: &str,
            active: &mut BTreeSet<(String, String)>,
        ) -> CollectedEffectShape {
            match value_type {
                LoweredType::Function {
                    return_type,
                    effects,
                    ..
                } => CollectedEffectShape::Function {
                    effects: effects.clone(),
                    result: Box::new(resolve(return_type, bundle, module, active)),
                },
                LoweredType::List(value, _) => {
                    CollectedEffectShape::List(Box::new(resolve(value, bundle, module, active)))
                }
                LoweredType::Map(key, value, _) => CollectedEffectShape::Map(
                    Box::new(resolve(key, bundle, module, active)),
                    Box::new(resolve(value, bundle, module, active)),
                ),
                LoweredType::Tuple(elements, _) => CollectedEffectShape::Tuple(
                    elements
                        .iter()
                        .map(|element| resolve(element, bundle, module, active))
                        .collect(),
                ),
                LoweredType::Record(fields, _) => CollectedEffectShape::Record(
                    fields
                        .iter()
                        .map(|(name, field, _)| {
                            (name.clone(), resolve(field, bundle, module, active))
                        })
                        .collect(),
                ),
                LoweredType::Named(name, span) => {
                    if name == "Never" {
                        return CollectedEffectShape::Never;
                    }
                    if let Ok(Some((definition_module, target))) =
                        resolve_alias_target(&bundle.modules, module, name, *span)
                    {
                        let key = (definition_module.clone(), name.clone());
                        if !active.insert(key.clone()) {
                            return CollectedEffectShape::Other;
                        }
                        let shape = resolve(target, bundle, &definition_module, active);
                        active.remove(&key);
                        return shape;
                    }
                    let Some((definition_module, fields)) =
                        resolve_lowered_record_fields(bundle, module, name)
                    else {
                        return CollectedEffectShape::Other;
                    };
                    let key = (definition_module.clone(), name.clone());
                    if !active.insert(key.clone()) {
                        return CollectedEffectShape::Other;
                    }
                    let shape = CollectedEffectShape::Record(
                        fields
                            .iter()
                            .map(|(name, field, _)| {
                                (
                                    name.clone(),
                                    resolve(field, bundle, &definition_module, active),
                                )
                            })
                            .collect(),
                    );
                    active.remove(&key);
                    shape
                }
                LoweredType::Option(_, _)
                | LoweredType::Result(_, _, _)
                | LoweredType::Future(_, _)
                | LoweredType::Task(_, _)
                | LoweredType::Prompt(_, _)
                | LoweredType::Range(_, _)
                | LoweredType::Sequence(_, _) => CollectedEffectShape::Other,
            }
        }

        resolve(value_type, bundle, module, &mut BTreeSet::new())
    }

    pub(super) fn shape_type(shape: &CollectedEffectShape, span: Span) -> LoweredType {
        match shape {
            CollectedEffectShape::Function { effects, result } => LoweredType::Function {
                parameters: Vec::new(),
                return_type: Box::new(shape_type(result, span)),
                effects: effects.clone(),
                span,
            },
            CollectedEffectShape::List(value) => {
                LoweredType::List(Box::new(shape_type(value, span)), span)
            }
            CollectedEffectShape::Map(key, value) => LoweredType::Map(
                Box::new(shape_type(key, span)),
                Box::new(shape_type(value, span)),
                span,
            ),
            CollectedEffectShape::Tuple(elements) => LoweredType::Tuple(
                elements
                    .iter()
                    .map(|element| shape_type(element, span))
                    .collect(),
                span,
            ),
            CollectedEffectShape::Record(fields) => LoweredType::Record(
                fields
                    .iter()
                    .map(|(name, field)| (name.clone(), shape_type(field, span), span))
                    .collect(),
                span,
            ),
            CollectedEffectShape::Never => LoweredType::Named("Never".to_owned(), span),
            CollectedEffectShape::Other => LoweredType::Named("Void".to_owned(), span),
        }
    }

    pub(super) fn merge_shapes(
        shapes: impl IntoIterator<Item = CollectedEffectShape>,
    ) -> CollectedEffectShape {
        let mut merged = None;
        let mut saw_never = false;
        for shape in shapes {
            if shape == CollectedEffectShape::Never {
                saw_never = true;
                continue;
            }
            if merged.as_ref().is_some_and(|existing| existing != &shape) {
                return CollectedEffectShape::Other;
            }
            merged = Some(shape);
        }
        merged.unwrap_or(if saw_never {
            CollectedEffectShape::Never
        } else {
            CollectedEffectShape::Other
        })
    }

    fn local_function_shape(
        function: &super::LoweredLocalFunction,
        bundle: &ResolvedBundle,
        module: &str,
    ) -> CollectedEffectShape {
        CollectedEffectShape::Function {
            effects: function.declared_effects.clone().unwrap_or_default(),
            result: Box::new(type_shape(&function.return_type, bundle, module)),
        }
    }

    pub(super) fn bind_loop_shape(
        binding: &LoweredLoopBinding,
        yielded: CollectedEffectShape,
        scope: &mut BTreeMap<String, CollectedEffectShape>,
    ) {
        if binding.tuple {
            let CollectedEffectShape::Tuple(elements) = yielded else {
                return;
            };
            if elements.len() != binding.elements.len() {
                return;
            }
            for (binding, shape) in binding.elements.iter().zip(elements) {
                if let Some(name) = &binding.name {
                    scope.insert(name.clone(), shape);
                }
            }
        } else if let Some(name) = &binding.elements[0].name {
            scope.insert(name.clone(), yielded);
        }
    }

    pub(super) fn bind_match_shape(
        pattern: &LoweredPattern,
        source: &CollectedEffectShape,
        scope: &mut BTreeMap<String, CollectedEffectShape>,
    ) {
        let (LoweredPattern::Record { fields, .. }, CollectedEffectShape::Record(source_fields)) =
            (pattern, source)
        else {
            return;
        };
        for (field, _, binding) in fields {
            if let (LoweredPattern::Binding { name, .. }, Some(shape)) =
                (binding.as_ref(), source_fields.get(field))
            {
                scope.insert(name.clone(), shape.clone());
            }
        }
    }

    pub(super) fn iterable_shape(shape: CollectedEffectShape) -> CollectedEffectShape {
        match shape {
            CollectedEffectShape::List(value) => *value,
            CollectedEffectShape::Map(key, value) => {
                CollectedEffectShape::Tuple(vec![*key, *value])
            }
            CollectedEffectShape::Function { .. }
            | CollectedEffectShape::Tuple(_)
            | CollectedEffectShape::Record(_)
            | CollectedEffectShape::Never
            | CollectedEffectShape::Other => CollectedEffectShape::Other,
        }
    }

    pub(super) fn body_effects(
        body: &LoweredBody,
        bundle: &ResolvedBundle,
        module: &str,
        shapes: &BTreeMap<String, CollectedEffectShape>,
        current: &[Vec<String>],
    ) -> Result<Vec<String>, Diagnostic> {
        let lowered = LoweredFunction {
            exported: false,
            is_async: false,
            name: "$effect-body".to_owned(),
            name_span: body.span,
            generics: Vec::new(),
            parameters: shapes
                .iter()
                .map(|(name, shape)| (name.clone(), shape_type(shape, body.span), body.span))
                .collect(),
            parameter_defaults: Vec::new(),
            return_type: LoweredType::Named("Void".to_owned(), body.span),
            declared_effects: None,
            effects_span: None,
            body: body.clone(),
        };
        required_body_effects(
            bundle,
            &FunctionInfo {
                is_const: false,
                module: module.to_owned(),
                symbol: 0,
                bytecode: None,
                lowered,
                parameters: Vec::new(),
                return_type: SemanticType::Value(ValueType::Unit),
                effects: Vec::new(),
            },
            current,
        )
    }

    pub(super) fn expression_effects(
        expression: &LoweredExpr,
        bundle: &ResolvedBundle,
        module: &str,
        shapes: &BTreeMap<String, CollectedEffectShape>,
        current: &[Vec<String>],
    ) -> Result<Vec<String>, Diagnostic> {
        body_effects(
            &LoweredBody {
                statements: Vec::new(),
                tail: Some(expression.clone()),
                span: expression.span,
            },
            bundle,
            module,
            shapes,
            current,
        )
    }

    pub(super) fn expression_shape(
        expression: &LoweredExpr,
        bundle: &ResolvedBundle,
        module: &str,
        shapes: &BTreeMap<String, CollectedEffectShape>,
        current: &[Vec<String>],
    ) -> Result<CollectedEffectShape, Diagnostic> {
        Ok(match &expression.kind {
            LoweredExprKind::Variable(name) => {
                if let Some(shape) = shapes.get(name) {
                    shape.clone()
                } else if let Some(symbol) = resolve_function_name(bundle, module, name)? {
                    CollectedEffectShape::Function {
                        effects: current[symbol as usize].clone(),
                        result: Box::new(type_shape(
                            &bundle.functions[symbol as usize].lowered.return_type,
                            bundle,
                            &bundle.functions[symbol as usize].module,
                        )),
                    }
                } else {
                    CollectedEffectShape::Other
                }
            }
            LoweredExprKind::Closure {
                parameters,
                return_type,
                declared_effects,
                body,
            } => {
                let mut nested = shapes.clone();
                for (name, value_type, _) in parameters {
                    nested.insert(name.clone(), type_shape(value_type, bundle, module));
                }
                let effects = if let Some(effects) = declared_effects {
                    effects.clone()
                } else {
                    body_effects(body, bundle, module, &nested, current)?
                };
                CollectedEffectShape::Function {
                    effects,
                    result: Box::new(type_shape(return_type, bundle, module)),
                }
            }
            LoweredExprKind::List(values) => CollectedEffectShape::List(Box::new(merge_shapes(
                values
                    .iter()
                    .map(|value| expression_shape(value, bundle, module, shapes, current))
                    .collect::<Result<Vec<_>, _>>()?,
            ))),
            LoweredExprKind::Map(entries) => CollectedEffectShape::Map(
                Box::new(merge_shapes(
                    entries
                        .iter()
                        .map(|(key, _)| expression_shape(key, bundle, module, shapes, current))
                        .collect::<Result<Vec<_>, _>>()?,
                )),
                Box::new(merge_shapes(
                    entries
                        .iter()
                        .map(|(_, value)| expression_shape(value, bundle, module, shapes, current))
                        .collect::<Result<Vec<_>, _>>()?,
                )),
            ),
            LoweredExprKind::Tuple(values) => CollectedEffectShape::Tuple(
                values
                    .iter()
                    .map(|value| expression_shape(value, bundle, module, shapes, current))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            LoweredExprKind::Record { fields, .. } => CollectedEffectShape::Record(
                fields
                    .iter()
                    .map(|(name, value, _)| {
                        Ok((
                            name.clone(),
                            expression_shape(value, bundle, module, shapes, current)?,
                        ))
                    })
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            LoweredExprKind::FieldGet { record, field, .. } => {
                match expression_shape(record, bundle, module, shapes, current)? {
                    CollectedEffectShape::Record(fields) => fields
                        .get(field)
                        .cloned()
                        .unwrap_or(CollectedEffectShape::Other),
                    _ => CollectedEffectShape::Other,
                }
            }
            LoweredExprKind::Index { collection, index } => {
                match expression_shape(collection, bundle, module, shapes, current)? {
                    CollectedEffectShape::List(value) | CollectedEffectShape::Map(_, value) => {
                        *value
                    }
                    CollectedEffectShape::Tuple(elements) => {
                        if let LoweredExprKind::Int(index) = &index.kind {
                            usize::try_from(*index)
                                .ok()
                                .and_then(|index| elements.get(index).cloned())
                                .unwrap_or(CollectedEffectShape::Other)
                        } else {
                            CollectedEffectShape::Other
                        }
                    }
                    _ => CollectedEffectShape::Other,
                }
            }
            LoweredExprKind::Call {
                callee, arguments, ..
            } if matches!(&callee.kind, LoweredExprKind::Variable(name) if name == "stop" || (name == "fail" && arguments.len() == 1)) => {
                CollectedEffectShape::Never
            }
            LoweredExprKind::Call { callee, .. } => {
                match expression_shape(callee, bundle, module, shapes, current)? {
                    CollectedEffectShape::Function { result, .. } => *result,
                    CollectedEffectShape::Never => CollectedEffectShape::Never,
                    _ => CollectedEffectShape::Other,
                }
            }
            LoweredExprKind::Match { source, arms } => {
                let source_shape = expression_shape(source, bundle, module, shapes, current)?;
                merge_shapes(
                    arms.iter()
                        .map(|(pattern, value, _)| {
                            let mut nested = shapes.clone();
                            bind_match_shape(pattern, &source_shape, &mut nested);
                            expression_shape(value, bundle, module, &nested, current)
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                )
            }
            LoweredExprKind::If {
                then_body,
                else_branch,
                ..
            } => {
                let mut branch_shapes =
                    vec![body_shape(then_body, bundle, module, shapes, current)?];
                match else_branch {
                    Some(LoweredElse::Body(body)) => {
                        branch_shapes.push(body_shape(body, bundle, module, shapes, current)?);
                    }
                    Some(LoweredElse::If(expression)) => branch_shapes.push(expression_shape(
                        expression, bundle, module, shapes, current,
                    )?),
                    None => branch_shapes.push(CollectedEffectShape::Other),
                }
                merge_shapes(branch_shapes)
            }
            LoweredExprKind::AwaitBlock(body) => body_shape(body, bundle, module, shapes, current)?,
            LoweredExprKind::ListWithSpread(items) => {
                CollectedEffectShape::List(Box::new(merge_shapes(
                    items
                        .iter()
                        .map(|item| expression_shape(&item.value, bundle, module, shapes, current))
                        .collect::<Result<Vec<_>, _>>()?,
                )))
            }
            LoweredExprKind::MapWithSpread(items) => {
                let mut keys = Vec::new();
                let mut values = Vec::new();
                for item in items {
                    match item {
                        super::LoweredMapItem::Entry { key, value, .. } => {
                            keys.push(expression_shape(key, bundle, module, shapes, current)?);
                            values.push(expression_shape(value, bundle, module, shapes, current)?);
                        }
                        super::LoweredMapItem::Spread { value, .. } => {
                            if let CollectedEffectShape::Map(key, value) =
                                expression_shape(value, bundle, module, shapes, current)?
                            {
                                keys.push(*key);
                                values.push(*value);
                            }
                        }
                    }
                }
                CollectedEffectShape::Map(
                    Box::new(merge_shapes(keys)),
                    Box::new(merge_shapes(values)),
                )
            }
            LoweredExprKind::RecordUpdate { base, fields, .. } => {
                match expression_shape(base, bundle, module, shapes, current)? {
                    CollectedEffectShape::Record(mut shape) => {
                        for (name, value, _) in fields {
                            shape.insert(
                                name.clone(),
                                expression_shape(value, bundle, module, shapes, current)?,
                            );
                        }
                        CollectedEffectShape::Record(shape)
                    }
                    _ => CollectedEffectShape::Other,
                }
            }
            LoweredExprKind::OptionalFieldGet {
                receiver, field, ..
            } => match expression_shape(receiver, bundle, module, shapes, current)? {
                CollectedEffectShape::Record(fields) => fields
                    .get(field)
                    .cloned()
                    .unwrap_or(CollectedEffectShape::Other),
                _ => CollectedEffectShape::Other,
            },
            // These operators have no additional callback shape until their
            // call lowering is selected, but their nested operands are still
            // visited by check_expression below.
            LoweredExprKind::Unit
            | LoweredExprKind::Int(_)
            | LoweredExprKind::Float(_)
            | LoweredExprKind::Bool(_)
            | LoweredExprKind::String(_)
            | LoweredExprKind::Template(_)
            | LoweredExprKind::Bytes(_)
            | LoweredExprKind::Prompt { .. }
            | LoweredExprKind::Enum { .. }
            | LoweredExprKind::Try(_)
            | LoweredExprKind::Unary { .. }
            | LoweredExprKind::Binary { .. }
            | LoweredExprKind::ShortClosure { .. }
            | LoweredExprKind::Spawn(_)
            | LoweredExprKind::Await(_)
            | LoweredExprKind::Compose { .. }
            | LoweredExprKind::Pipe { .. }
            | LoweredExprKind::Range { .. }
            | LoweredExprKind::Slice { .. } => CollectedEffectShape::Other,
        })
    }

    pub(super) fn body_shape(
        body: &LoweredBody,
        bundle: &ResolvedBundle,
        module: &str,
        shapes: &BTreeMap<String, CollectedEffectShape>,
        current: &[Vec<String>],
    ) -> Result<CollectedEffectShape, Diagnostic> {
        let mut scoped = shapes.clone();
        for statement in &body.statements {
            match statement {
                LoweredStatement::Let {
                    name,
                    annotation,
                    value,
                    ..
                } => {
                    let shape = if let Some(annotation) = annotation {
                        type_shape(annotation, bundle, module)
                    } else {
                        expression_shape(value, bundle, module, &scoped, current)?
                    };
                    scoped.insert(name.clone(), shape);
                }
                LoweredStatement::LocalFunction(function) => {
                    scoped.insert(
                        function.name.clone(),
                        local_function_shape(function, bundle, module),
                    );
                }
                _ => {}
            }
        }
        if let Some(tail) = &body.tail {
            return expression_shape(tail, bundle, module, &scoped, current);
        }
        match body.statements.last() {
            Some(
                LoweredStatement::Return(_, _)
                | LoweredStatement::Break(_)
                | LoweredStatement::Continue(_),
            ) => Ok(CollectedEffectShape::Never),
            Some(
                LoweredStatement::Let { value, .. }
                | LoweredStatement::Assignment { value, .. }
                | LoweredStatement::ControlFlow(value),
            ) if expression_shape(value, bundle, module, &scoped, current)?
                == CollectedEffectShape::Never =>
            {
                Ok(CollectedEffectShape::Never)
            }
            _ => Ok(CollectedEffectShape::Other),
        }
    }

    pub(super) fn closure_effects(
        expression: &LoweredExpr,
        bundle: &ResolvedBundle,
        module: &str,
        shapes: &BTreeMap<String, CollectedEffectShape>,
        current: &[Vec<String>],
        sets: &mut BTreeSet<Vec<String>>,
    ) {
        if let Ok(effects) = expression_effects(expression, bundle, module, shapes, current) {
            sets.insert(effects);
        }
        match &expression.kind {
            LoweredExprKind::Template(parts) => {
                for interpolation in template_interpolations(parts) {
                    closure_effects(interpolation, bundle, module, shapes, current, sets);
                }
            }
            LoweredExprKind::List(values)
            | LoweredExprKind::Tuple(values)
            | LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Tuple(values),
                ..
            } => {
                for value in values {
                    closure_effects(value, bundle, module, shapes, current, sets);
                }
            }
            LoweredExprKind::ListWithSpread(items) => {
                for item in items {
                    closure_effects(&item.value, bundle, module, shapes, current, sets);
                }
            }
            LoweredExprKind::Map(entries) => {
                for (key, value) in entries {
                    closure_effects(key, bundle, module, shapes, current, sets);
                    closure_effects(value, bundle, module, shapes, current, sets);
                }
            }
            LoweredExprKind::MapWithSpread(items) => {
                for item in items {
                    match item {
                        super::LoweredMapItem::Entry { key, value, .. } => {
                            closure_effects(key, bundle, module, shapes, current, sets);
                            closure_effects(value, bundle, module, shapes, current, sets);
                        }
                        super::LoweredMapItem::Spread { value, .. } => {
                            closure_effects(value, bundle, module, shapes, current, sets);
                        }
                    }
                }
            }
            LoweredExprKind::RecordUpdate { base, fields, .. } => {
                closure_effects(base, bundle, module, shapes, current, sets);
                for (_, value, _) in fields {
                    closure_effects(value, bundle, module, shapes, current, sets);
                }
            }
            LoweredExprKind::Record { fields, .. }
            | LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Record(fields),
                ..
            } => {
                for (_, value, _) in fields {
                    closure_effects(value, bundle, module, shapes, current, sets);
                }
            }
            LoweredExprKind::Prompt {
                system,
                context,
                data,
                ..
            } => {
                closure_effects(system, bundle, module, shapes, current, sets);
                if let Some(context) = context {
                    closure_effects(context, bundle, module, shapes, current, sets);
                }
                if let Some(data) = data {
                    closure_effects(data, bundle, module, shapes, current, sets);
                }
            }
            LoweredExprKind::Binary { left, right, .. }
            | LoweredExprKind::Compose { left, right, .. }
            | LoweredExprKind::Range {
                start: left,
                end: right,
                ..
            } => {
                closure_effects(left, bundle, module, shapes, current, sets);
                closure_effects(right, bundle, module, shapes, current, sets);
            }
            LoweredExprKind::Pipe { left, stage, .. } => {
                closure_effects(left, bundle, module, shapes, current, sets);
                closure_effects(stage, bundle, module, shapes, current, sets);
            }
            LoweredExprKind::Index { collection, index }
            | LoweredExprKind::Slice {
                collection,
                range: index,
                ..
            } => {
                closure_effects(collection, bundle, module, shapes, current, sets);
                closure_effects(index, bundle, module, shapes, current, sets);
            }
            LoweredExprKind::FieldGet { record, .. }
            | LoweredExprKind::OptionalFieldGet {
                receiver: record, ..
            }
            | LoweredExprKind::Try(record)
            | LoweredExprKind::Unary {
                operand: record, ..
            } => {
                closure_effects(record, bundle, module, shapes, current, sets);
            }
            LoweredExprKind::Spawn(value) | LoweredExprKind::Await(value) => {
                closure_effects(value, bundle, module, shapes, current, sets);
            }
            LoweredExprKind::AwaitBlock(body) => {
                closure_body_effects(body, bundle, module, shapes, current, sets);
            }
            LoweredExprKind::Match { source, arms } => {
                closure_effects(source, bundle, module, shapes, current, sets);
                let source_shape = expression_shape(source, bundle, module, shapes, current).ok();
                for (pattern, value, _) in arms {
                    let mut nested = shapes.clone();
                    if let Some(source_shape) = &source_shape {
                        bind_match_shape(pattern, source_shape, &mut nested);
                    }
                    closure_effects(value, bundle, module, &nested, current, sets);
                }
            }
            LoweredExprKind::If {
                condition,
                then_body,
                else_branch,
            } => {
                closure_effects(condition, bundle, module, shapes, current, sets);
                closure_body_effects(then_body, bundle, module, shapes, current, sets);
                match else_branch {
                    Some(LoweredElse::Body(body)) => {
                        closure_body_effects(body, bundle, module, shapes, current, sets);
                    }
                    Some(LoweredElse::If(expression)) => {
                        closure_effects(expression, bundle, module, shapes, current, sets);
                    }
                    None => {}
                }
            }
            LoweredExprKind::Call {
                callee, arguments, ..
            } => {
                closure_effects(callee, bundle, module, shapes, current, sets);
                for argument in arguments {
                    closure_effects(&argument.value, bundle, module, shapes, current, sets);
                }
            }
            LoweredExprKind::Closure {
                parameters,
                return_type,
                declared_effects,
                body,
            } => {
                let lowered = LoweredFunction {
                    exported: false,
                    is_async: false,
                    name: "$effect-closure".to_owned(),
                    name_span: expression.span,
                    generics: Vec::new(),
                    parameters: parameters.clone(),
                    parameter_defaults: vec![None; parameters.len()],
                    return_type: return_type.clone(),
                    declared_effects: declared_effects.clone(),
                    effects_span: None,
                    body: body.as_ref().clone(),
                };
                let closure = FunctionInfo {
                    is_const: false,
                    module: module.to_owned(),
                    symbol: 0,
                    bytecode: None,
                    lowered,
                    parameters: Vec::new(),
                    return_type: SemanticType::Value(ValueType::Unit),
                    effects: Vec::new(),
                };
                if let Ok(effects) = required_body_effects(bundle, &closure, current) {
                    sets.insert(declared_effects.clone().unwrap_or(effects));
                }
                let mut nested = shapes.clone();
                for (name, value_type, _) in parameters {
                    nested.insert(name.clone(), type_shape(value_type, bundle, module));
                }
                closure_body_effects(body, bundle, module, &nested, current, sets);
            }
            LoweredExprKind::ShortClosure { .. }
            | LoweredExprKind::Unit
            | LoweredExprKind::Int(_)
            | LoweredExprKind::Float(_)
            | LoweredExprKind::Bool(_)
            | LoweredExprKind::String(_)
            | LoweredExprKind::Bytes(_)
            | LoweredExprKind::Variable(_)
            | LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Unit,
                ..
            } => {}
        }
    }

    pub(super) fn closure_body_effects(
        body: &LoweredBody,
        bundle: &ResolvedBundle,
        module: &str,
        shapes: &BTreeMap<String, CollectedEffectShape>,
        current: &[Vec<String>],
        sets: &mut BTreeSet<Vec<String>>,
    ) {
        if let Ok(effects) = body_effects(body, bundle, module, shapes, current) {
            sets.insert(effects);
        }
        let mut scoped = shapes.clone();
        for statement in &body.statements {
            match statement {
                LoweredStatement::Let {
                    name,
                    annotation,
                    value,
                    ..
                } => {
                    closure_effects(value, bundle, module, &scoped, current, sets);
                    let shape = annotation
                        .as_ref()
                        .map(|annotation| type_shape(annotation, bundle, module))
                        .or_else(|| expression_shape(value, bundle, module, &scoped, current).ok());
                    if let Some(shape) = shape {
                        scoped.insert(name.clone(), shape);
                    }
                }
                LoweredStatement::Assignment { value, .. }
                | LoweredStatement::ControlFlow(value) => {
                    closure_effects(value, bundle, module, &scoped, current, sets);
                }
                LoweredStatement::Return(value, _) => {
                    if let Some(value) = value {
                        closure_effects(value, bundle, module, &scoped, current, sets);
                    }
                }
                LoweredStatement::While {
                    condition, body, ..
                } => {
                    closure_effects(condition, bundle, module, &scoped, current, sets);
                    if let (Ok(condition_effects), Ok(body_effects)) = (
                        expression_effects(condition, bundle, module, &scoped, current),
                        body_effects(body, bundle, module, &scoped, current),
                    ) {
                        sets.insert(
                            condition_effects
                                .into_iter()
                                .chain(body_effects)
                                .collect::<BTreeSet<_>>()
                                .into_iter()
                                .collect(),
                        );
                    }
                    closure_body_effects(body, bundle, module, &scoped, current, sets);
                }
                LoweredStatement::Loop { body, .. } => {
                    closure_body_effects(body, bundle, module, &scoped, current, sets);
                }
                LoweredStatement::For {
                    binding,
                    source,
                    body,
                    ..
                } => {
                    let (source_effects, yielded) = match source {
                        LoweredForSource::Iterable(value) => {
                            closure_effects(value, bundle, module, &scoped, current, sets);
                            (
                                expression_effects(value, bundle, module, &scoped, current).ok(),
                                expression_shape(value, bundle, module, &scoped, current)
                                    .ok()
                                    .map(iterable_shape),
                            )
                        }
                    };
                    let mut nested = scoped.clone();
                    if let Some(yielded) = yielded {
                        bind_loop_shape(binding, yielded, &mut nested);
                    }
                    if let (Some(source_effects), Ok(body_effects)) = (
                        source_effects,
                        body_effects(body, bundle, module, &nested, current),
                    ) {
                        sets.insert(
                            source_effects
                                .into_iter()
                                .chain(body_effects)
                                .collect::<BTreeSet<_>>()
                                .into_iter()
                                .collect(),
                        );
                    }
                    closure_body_effects(body, bundle, module, &nested, current, sets);
                }
                LoweredStatement::LocalFunction(function) => {
                    let declared = function.declared_effects.clone().unwrap_or_default();
                    sets.insert(declared.clone());
                    for effect in declared {
                        sets.insert(vec![effect]);
                    }
                    for (_, value_type, _) in &function.parameters {
                        type_effects(value_type, sets);
                    }
                    type_effects(&function.return_type, sets);
                    let mut local_shapes = BTreeMap::new();
                    for (parameter_index, (name, value_type, _)) in
                        function.parameters.iter().enumerate()
                    {
                        if let Some(default) = &function.parameter_defaults[parameter_index] {
                            closure_effects(
                                &default.value,
                                bundle,
                                module,
                                &local_shapes,
                                current,
                                sets,
                            );
                        }
                        local_shapes.insert(name.clone(), type_shape(value_type, bundle, module));
                    }
                    closure_body_effects(
                        &function.body,
                        bundle,
                        module,
                        &local_shapes,
                        current,
                        sets,
                    );
                    scoped.insert(
                        function.name.clone(),
                        local_function_shape(function, bundle, module),
                    );
                }
                LoweredStatement::Break(_) | LoweredStatement::Continue(_) => {}
            }
        }
        if let Some(tail) = &body.tail {
            closure_effects(tail, bundle, module, &scoped, current, sets);
        }
    }

    let mut sets = BTreeSet::from([Vec::new()]);
    for binding in bundle.tools.values() {
        sets.insert(vec![binding.effect.clone()]);
    }
    for module in bundle.modules.values() {
        for declaration in &module.types {
            match declaration {
                LoweredTypeDeclaration::Record { fields, .. } => {
                    for (_, field, _) in fields {
                        type_effects(field, &mut sets);
                    }
                }
                LoweredTypeDeclaration::Enum { variants, .. } => {
                    for variant in variants {
                        match &variant.payload {
                            LoweredEnumPayload::Unit => {}
                            LoweredEnumPayload::Tuple(values) => {
                                for value in values {
                                    type_effects(value, &mut sets);
                                }
                            }
                            LoweredEnumPayload::Record(fields) => {
                                for (_, field, _) in fields {
                                    type_effects(field, &mut sets);
                                }
                            }
                        }
                    }
                }
                LoweredTypeDeclaration::Alias { target, .. } => {
                    type_effects(target, &mut sets);
                }
                LoweredTypeDeclaration::Newtype { underlying, .. } => {
                    type_effects(underlying, &mut sets);
                }
            }
        }
    }
    let current = bundle
        .functions
        .iter()
        .map(|function| function.effects.clone())
        .collect::<Vec<_>>();
    for function in &bundle.functions {
        sets.insert(function.effects.clone());
        for effect in &function.effects {
            sets.insert(vec![effect.clone()]);
        }
        for (_, parameter, _) in &function.lowered.parameters {
            type_effects(parameter, &mut sets);
        }
        type_effects(&function.lowered.return_type, &mut sets);
        let shapes = function
            .lowered
            .parameters
            .iter()
            .map(|(name, value_type, _)| {
                (
                    name.clone(),
                    type_shape(value_type, bundle, &function.module),
                )
            })
            .collect();
        closure_body_effects(
            &function.lowered.body,
            bundle,
            &function.module,
            &shapes,
            &current,
            &mut sets,
        );
    }
    sets.into_iter().collect()
}

#[allow(clippy::too_many_lines)]
pub(super) fn resolve_deferred_effect_sets(
    bundle: &mut ResolvedBundle,
    effect_sets: &[Vec<String>],
) {
    pub(super) fn resolve_value(
        value_type: &mut ValueType,
        deferred_effect_sets: &[Vec<String>],
        effect_sets: &[Vec<String>],
    ) {
        match value_type {
            ValueType::List(value)
            | ValueType::Option(value)
            | ValueType::Future(value)
            | ValueType::Task(value)
            | ValueType::Sequence(value)
            | ValueType::Newtype {
                underlying: value, ..
            } => {
                resolve_value(value, deferred_effect_sets, effect_sets);
            }
            ValueType::Map(key, value) | ValueType::Result(key, value) => {
                resolve_value(key, deferred_effect_sets, effect_sets);
                resolve_value(value, deferred_effect_sets, effect_sets);
            }
            ValueType::Tuple(values) => {
                for value in values {
                    resolve_value(value, deferred_effect_sets, effect_sets);
                }
            }
            ValueType::Record(fields) => {
                for field in fields {
                    resolve_value(&mut field.value_type, deferred_effect_sets, effect_sets);
                }
            }
            ValueType::Function {
                parameters,
                return_type,
                effects,
            } => {
                for parameter in parameters {
                    resolve_value(parameter, deferred_effect_sets, effect_sets);
                }
                resolve_value(return_type, deferred_effect_sets, effect_sets);
                if let Some(index) = effects.checked_sub(DEFERRED_EFFECT_SET_BASE) {
                    *effects = effect_id(effect_sets, &deferred_effect_sets[index as usize]);
                }
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
            | ValueType::ExternalFsAccess
            | ValueType::Workspace
            | ValueType::SubAgent
            | ValueType::Unknown => {}
        }
    }

    pub(super) fn resolve_semantic(
        value_type: &mut SemanticType,
        deferred_effect_sets: &[Vec<String>],
        effect_sets: &[Vec<String>],
    ) {
        match value_type {
            SemanticType::Value(value) => {
                resolve_value(value, deferred_effect_sets, effect_sets);
            }
            SemanticType::Function {
                parameters,
                return_type,
                ..
            } => {
                for parameter in parameters {
                    resolve_semantic(parameter, deferred_effect_sets, effect_sets);
                }
                resolve_semantic(return_type, deferred_effect_sets, effect_sets);
            }
            SemanticType::Generic(_) => {}
        }
    }

    let deferred_effect_sets = bundle.deferred_effect_sets.clone();
    for value_type in bundle.types.values_mut() {
        resolve_value(value_type, &deferred_effect_sets, effect_sets);
    }
    for enum_type in &mut bundle.enum_types {
        for variant in &mut enum_type.variants {
            match &mut variant.payload {
                EnumPayloadType::Unit => {}
                EnumPayloadType::Tuple(values) => {
                    for value in values {
                        resolve_value(value, &deferred_effect_sets, effect_sets);
                    }
                }
                EnumPayloadType::Record(fields) => {
                    for field in fields {
                        resolve_value(&mut field.value_type, &deferred_effect_sets, effect_sets);
                    }
                }
            }
        }
    }
    for function in &mut bundle.functions {
        for parameter in &mut function.parameters {
            resolve_semantic(parameter, &deferred_effect_sets, effect_sets);
        }
        resolve_semantic(
            &mut function.return_type,
            &deferred_effect_sets,
            effect_sets,
        );
    }
}
