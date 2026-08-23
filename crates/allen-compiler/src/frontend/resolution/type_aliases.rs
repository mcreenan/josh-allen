//! Transparent type-alias lookup and bounded dependency-cycle validation.

use super::{
    BTreeMap, BTreeSet, Diagnostic, LoweredModule, LoweredType, LoweredTypeDeclaration,
    MAX_VALUE_NESTING, RecordField, SemanticType, Span, ValueType, agent_error_type,
    contains_affine, contains_stored_sub_agent, contains_workspace,
    external_directory_request_type, external_file_request_type, file_error_type,
    http_response_type, model_error_type, network_error_type, permission_error_type,
    record_field_value_type, resolve_import_path, sub_agent_error_type, user_error_type,
};

pub(super) fn resolve_record_layout(
    fields: &[(String, LoweredType, Span)],
    module: &str,
    modules: &BTreeMap<String, LoweredModule>,
    types: &BTreeMap<(String, String), ValueType>,
    deferred_effect_sets: &[Vec<String>],
) -> Result<Vec<RecordField>, Diagnostic> {
    let mut field_names = BTreeSet::new();
    let mut layout = Vec::new();
    for (field, field_type, span) in fields {
        if !field_names.insert(field.clone()) {
            return Err(Diagnostic::new(
                "E3005",
                format!("duplicate record field '{field}'"),
                *span,
            )
            .with_source(module));
        }
        let value_type =
            record_field_value_type(field_type, module, modules, types, deferred_effect_sets)
                .map_err(|diagnostic| diagnostic.with_source(module))?;
        if contains_affine(&value_type) || contains_stored_sub_agent(&value_type) {
            return Err(Diagnostic::new(
                "E3011",
                "future, task, or SubAgent values cannot be stored in a record",
                *span,
            )
            .with_source(module));
        }
        if contains_workspace(&value_type) {
            return Err(
                Diagnostic::new("E3011", "Workspace cannot be stored in a record", *span)
                    .with_source(module),
            );
        }
        layout.push(RecordField {
            name: field.clone(),
            value_type,
        });
    }
    layout.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(layout)
}

pub(super) fn builtin_semantic_type(name: &str) -> Option<SemanticType> {
    Some(SemanticType::Value(match name {
        "Int" => ValueType::Int,
        "Bool" => ValueType::Bool,
        "Float" => ValueType::Float,
        "String" => ValueType::String,
        "Bytes" => ValueType::Bytes,
        "Void" => ValueType::Unit,
        "Never" => ValueType::Never,
        "unknown" => ValueType::Unknown,
        "Workspace" => ValueType::Workspace,
        "ExternalFsAccess" => ValueType::ExternalFsAccess,
        "SubAgent" => ValueType::SubAgent,
        "ExternalFileRequest" => external_file_request_type(),
        "ExternalDirectoryRequest" => external_directory_request_type(),
        "HttpResponse" => http_response_type(),
        "FileError" => file_error_type(),
        "NetworkError" => network_error_type(),
        "AgentError" => agent_error_type(),
        "UserError" => user_error_type(),
        "SubAgentError" => sub_agent_error_type(),
        "ModelError" => model_error_type(),
        "PermissionError" => permission_error_type(),
        _ => return None,
    }))
}

pub(super) fn resolve_alias_target<'a>(
    modules: &'a BTreeMap<String, LoweredModule>,
    module: &str,
    name: &str,
    span: Span,
) -> Result<Option<(String, &'a LoweredType)>, Diagnostic> {
    if builtin_semantic_type(name).is_some() {
        return Ok(None);
    }
    if let Some(declaration) = modules[module]
        .types
        .iter()
        .find(|declaration| declaration.name() == name)
    {
        return Ok(match declaration {
            LoweredTypeDeclaration::Alias { target, .. } => Some((module.to_owned(), target)),
            LoweredTypeDeclaration::Record { .. } | LoweredTypeDeclaration::Enum { .. } => None,
        });
    }
    for import in &modules[module].imports {
        let Some((imported, _, _)) = import.names.iter().find(|(_, local, _)| local == name) else {
            continue;
        };
        let target_module = resolve_import_path(module, import)?;
        let declaration = modules[&target_module]
            .types
            .iter()
            .find(|declaration| declaration.name() == imported);
        if !declaration.is_some_and(LoweredTypeDeclaration::exported) {
            return Err(Diagnostic::new(
                "E3003",
                format!("type '{imported}' is private to module '{target_module}'"),
                span,
            ));
        }
        return Ok(match declaration {
            Some(LoweredTypeDeclaration::Alias { target, .. }) => Some((target_module, target)),
            Some(LoweredTypeDeclaration::Record { .. } | LoweredTypeDeclaration::Enum { .. })
            | None => None,
        });
    }
    Ok(None)
}

pub(super) fn resolve_named_semantic_type(
    modules: &BTreeMap<String, LoweredModule>,
    types: &BTreeMap<(String, String), ValueType>,
    module: &str,
    name: &str,
    span: Span,
) -> Result<SemanticType, Diagnostic> {
    let mut current_module = module.to_owned();
    let mut current_name = name.to_owned();
    let mut current_span = span;
    loop {
        if let Some(builtin) = builtin_semantic_type(&current_name) {
            return Ok(builtin);
        }
        if let Some(value_type) = types.get(&(current_module.clone(), current_name.clone())) {
            return Ok(SemanticType::Value(value_type.clone()));
        }
        let definition = &modules[&current_module];
        if let Some(LoweredTypeDeclaration::Alias { target, .. }) = definition
            .types
            .iter()
            .find(|declaration| declaration.name() == current_name)
        {
            if let LoweredType::Named(next, next_span) = target {
                current_name.clone_from(next);
                current_span = *next_span;
                continue;
            }
            return super::semantic_type(target, &BTreeSet::new(), &current_module, modules, types);
        }
        let mut imported_alias = None;
        for import in &definition.imports {
            let Some((imported, _, _)) = import
                .names
                .iter()
                .find(|(_, local, _)| local == &current_name)
            else {
                continue;
            };
            let target_module = resolve_import_path(&current_module, import)?;
            let declaration = modules[&target_module]
                .types
                .iter()
                .find(|declaration| declaration.name() == imported);
            if !declaration.is_some_and(LoweredTypeDeclaration::exported) {
                return Err(Diagnostic::new(
                    "E3003",
                    format!("type '{imported}' is private to module '{target_module}'"),
                    current_span,
                ));
            }
            if matches!(declaration, Some(LoweredTypeDeclaration::Alias { .. })) {
                imported_alias = Some((target_module, imported.clone()));
                break;
            }
            return types
                .get(&(target_module.clone(), imported.clone()))
                .cloned()
                .map(SemanticType::Value)
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E3003",
                        format!("module '{target_module}' does not export type '{imported}'"),
                        current_span,
                    )
                });
        }
        if let Some((target_module, imported)) = imported_alias {
            current_module = target_module;
            current_name = imported;
            continue;
        }
        return Err(Diagnostic::new(
            "E3005",
            format!("unknown type '{current_name}'"),
            current_span,
        ));
    }
}

pub(crate) fn resolve_named_type(
    modules: &BTreeMap<String, LoweredModule>,
    types: &BTreeMap<(String, String), ValueType>,
    module: &str,
    name: &str,
    span: Span,
) -> Result<ValueType, Diagnostic> {
    let SemanticType::Value(value_type) =
        resolve_named_semantic_type(modules, types, module, name, span)?
    else {
        return Err(Diagnostic::new(
            "E3007",
            format!("type '{name}' is not a concrete value type"),
            span,
        ));
    };
    Ok(value_type)
}

pub(super) type AliasKey = (String, String);

fn expanded_alias_depth(
    value_type: &LoweredType,
    modules: &BTreeMap<String, LoweredModule>,
    module: &str,
    depths: &BTreeMap<AliasKey, usize>,
) -> Result<usize, Diagnostic> {
    let child_depth = |value| expanded_alias_depth(value, modules, module, depths);
    let depth = match value_type {
        LoweredType::Named(name, _) => referenced_alias(modules, module, name)?
            .and_then(|key| depths.get(&key).copied())
            .unwrap_or(0),
        LoweredType::Tuple(values, _) => values
            .iter()
            .map(child_depth)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0)
            .saturating_add(1),
        LoweredType::Record(fields, _) => fields
            .iter()
            .map(|(_, value, _)| child_depth(value))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0)
            .saturating_add(1),
        LoweredType::List(value, _)
        | LoweredType::Option(value, _)
        | LoweredType::Future(value, _)
        | LoweredType::Task(value, _)
        | LoweredType::Prompt(value, _) => child_depth(value)?.saturating_add(1),
        LoweredType::Map(key, value, _) | LoweredType::Result(key, value, _) => {
            child_depth(key)?.max(child_depth(value)?).saturating_add(1)
        }
        LoweredType::Function {
            parameters,
            return_type,
            ..
        } => parameters
            .iter()
            .map(child_depth)
            .chain(std::iter::once(child_depth(return_type)))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0)
            .saturating_add(1),
    };
    Ok(depth)
}

fn collect_named_type_references<'a>(value_type: &'a LoweredType, names: &mut Vec<&'a str>) {
    match value_type {
        LoweredType::Named(name, _) => names.push(name),
        LoweredType::Tuple(values, _) => {
            for value in values {
                collect_named_type_references(value, names);
            }
        }
        LoweredType::Record(fields, _) => {
            for (_, value, _) in fields {
                collect_named_type_references(value, names);
            }
        }
        LoweredType::List(value, _)
        | LoweredType::Option(value, _)
        | LoweredType::Future(value, _)
        | LoweredType::Task(value, _)
        | LoweredType::Prompt(value, _) => collect_named_type_references(value, names),
        LoweredType::Map(key, value, _) | LoweredType::Result(key, value, _) => {
            collect_named_type_references(key, names);
            collect_named_type_references(value, names);
        }
        LoweredType::Function {
            parameters,
            return_type,
            ..
        } => {
            for parameter in parameters {
                collect_named_type_references(parameter, names);
            }
            collect_named_type_references(return_type, names);
        }
    }
}

pub(super) fn has_pending_alias_dependency(
    modules: &BTreeMap<String, LoweredModule>,
    module: &str,
    target: &LoweredType,
    pending: &BTreeSet<AliasKey>,
) -> Result<bool, Diagnostic> {
    let mut names = Vec::new();
    collect_named_type_references(target, &mut names);
    for name in names {
        if referenced_alias(modules, module, name)?.is_some_and(|key| pending.contains(&key)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn referenced_alias(
    modules: &BTreeMap<String, LoweredModule>,
    module: &str,
    name: &str,
) -> Result<Option<AliasKey>, Diagnostic> {
    if builtin_semantic_type(name).is_some() {
        return Ok(None);
    }
    if let Some(declaration) = modules[module]
        .types
        .iter()
        .find(|declaration| declaration.name() == name)
    {
        return Ok(matches!(declaration, LoweredTypeDeclaration::Alias { .. })
            .then(|| (module.to_owned(), name.to_owned())));
    }
    for import in &modules[module].imports {
        let Some((imported, _, _)) = import.names.iter().find(|(_, local, _)| local == name) else {
            continue;
        };
        let target = resolve_import_path(module, import)?;
        let declaration = modules[&target]
            .types
            .iter()
            .find(|declaration| declaration.name() == imported);
        return Ok(declaration
            .filter(|declaration| declaration.exported())
            .filter(|declaration| matches!(declaration, LoweredTypeDeclaration::Alias { .. }))
            .map(|_| (target, imported.clone())));
    }
    Ok(None)
}

pub(super) fn validate_alias_cycles(
    modules: &BTreeMap<String, LoweredModule>,
) -> Result<Vec<AliasKey>, Diagnostic> {
    let mut aliases = BTreeMap::<AliasKey, (Span, &LoweredType)>::new();
    for (module, definition) in modules {
        for declaration in &definition.types {
            if let LoweredTypeDeclaration::Alias {
                name,
                name_span,
                target,
                ..
            } = declaration
            {
                aliases.insert((module.clone(), name.clone()), (*name_span, target));
            }
        }
    }

    let mut dependencies = BTreeMap::<AliasKey, BTreeSet<AliasKey>>::new();
    let mut dependents = BTreeMap::<AliasKey, BTreeSet<AliasKey>>::new();
    for (key, (_, target)) in &aliases {
        let mut names = Vec::new();
        collect_named_type_references(target, &mut names);
        let mut direct = BTreeSet::new();
        for name in names {
            if let Some(dependency) = referenced_alias(modules, &key.0, name)? {
                direct.insert(dependency);
            }
        }
        for dependency in &direct {
            dependents
                .entry(dependency.clone())
                .or_default()
                .insert(key.clone());
        }
        dependencies.insert(key.clone(), direct);
    }

    let mut ready = dependencies
        .iter()
        .filter(|(_, direct)| direct.is_empty())
        .map(|(key, _)| key.clone())
        .collect::<BTreeSet<_>>();
    let mut removed = BTreeSet::new();
    let mut depths = BTreeMap::new();
    let mut order = Vec::with_capacity(aliases.len());
    while let Some(key) = ready.pop_first() {
        let depth = expanded_alias_depth(aliases[&key].1, modules, &key.0, &depths)?;
        if depth > MAX_VALUE_NESTING {
            return Err(Diagnostic::new(
                "E2012",
                "type nesting exceeds the language limit",
                aliases[&key].0,
            )
            .with_source(&key.0));
        }
        depths.insert(key.clone(), depth);
        removed.insert(key.clone());
        order.push(key.clone());
        if let Some(users) = dependents.get(&key) {
            for user in users {
                let direct = dependencies
                    .get_mut(user)
                    .expect("alias dependent has a dependency set");
                direct.remove(&key);
                if direct.is_empty() {
                    ready.insert(user.clone());
                }
            }
        }
    }
    if removed.len() == aliases.len() {
        return Ok(order);
    }

    let remaining = aliases
        .keys()
        .filter(|key| !removed.contains(*key))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut current = remaining
        .first()
        .expect("an incomplete topological traversal has a remaining alias")
        .clone();
    let mut path = BTreeSet::new();
    loop {
        if !path.insert(current.clone()) {
            let span = aliases[&current].0;
            return Err(Diagnostic::new(
                "E3005",
                format!("cyclic type alias involving '{}'", current.1),
                span,
            )
            .with_source(&current.0));
        }
        current = dependencies[&current]
            .iter()
            .find(|dependency| remaining.contains(*dependency))
            .expect("every remaining alias reaches a cycle")
            .clone();
    }
}
