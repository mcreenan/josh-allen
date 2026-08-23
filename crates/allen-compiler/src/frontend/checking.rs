//! Standalone type-shape, ownership-shape, and entry-boundary semantic checks.

use super::{
    BTreeMap, Diagnostic, EffectSetId, EnumPayloadType, EnumType, LoweredExpr, LoweredExprKind,
    MAX_VALUE_NESTING, Register, Span, SymbolId, ValueType,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SemanticType {
    Value(ValueType),
    Generic(String),
    Function {
        parameters: Vec<Self>,
        return_type: Box<Self>,
        effects: Vec<String>,
    },
}

pub(super) fn effect_id(effect_sets: &[Vec<String>], effects: &[String]) -> EffectSetId {
    u32::try_from(
        effect_sets
            .binary_search_by(|candidate| candidate.as_slice().cmp(effects))
            .expect("all used effect sets are interned"),
    )
    .expect("effect set index fits")
}

pub(super) fn concrete_type(
    value_type: &SemanticType,
    substitutions: &BTreeMap<String, ValueType>,
    effect_sets: &[Vec<String>],
) -> Result<ValueType, Diagnostic> {
    match value_type {
        SemanticType::Value(value) => Ok(value.clone()),
        SemanticType::Generic(name) => substitutions.get(name).cloned().ok_or_else(|| {
            Diagnostic::new(
                "E3007",
                format!("cannot infer generic type '{name}'"),
                Span { start: 0, end: 0 },
            )
        }),
        SemanticType::Function {
            parameters,
            return_type,
            effects,
        } => Ok(ValueType::Function {
            parameters: parameters
                .iter()
                .map(|parameter| concrete_type(parameter, substitutions, effect_sets))
                .collect::<Result<_, _>>()?,
            return_type: Box::new(concrete_type(return_type, substitutions, effect_sets)?),
            effects: effect_id(effect_sets, effects),
        }),
    }
}

#[derive(Clone)]
pub(super) struct LocalBinding {
    pub(super) register: Register,
    pub(super) symbol: SymbolId,
    pub(super) value_type: ValueType,
    pub(super) scope: u32,
    pub(super) value_scope: u32,
    pub(super) mutable: bool,
    pub(super) moved: bool,
}

pub(super) fn is_affine(value_type: &ValueType) -> bool {
    matches!(value_type, ValueType::Future(_) | ValueType::Task(_))
}

pub(super) fn expected_type_diagnostic_code(label: &str) -> &'static str {
    if matches!(label, "return" | "function result") {
        "E3007"
    } else {
        "E3010"
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum EnumDepthError {
    Cycle(u32),
    Missing(u32),
}

pub(super) fn validate_declared_type_shapes(
    enum_types: &[EnumType],
    enum_spans: &[(String, Span)],
    record_shapes: &[(ValueType, String, Span)],
    alias_shapes: &[(ValueType, String, Span)],
) -> Result<(), Diagnostic> {
    let enum_depths = calculate_enum_depths(enum_types).map_err(|error| {
        let id = match error {
            EnumDepthError::Cycle(id) | EnumDepthError::Missing(id) => id,
        };
        let (source, span) = usize::try_from(id)
            .ok()
            .and_then(|index| enum_spans.get(index))
            .cloned()
            .unwrap_or_else(|| ("<unknown>".to_owned(), Span { start: 0, end: 0 }));
        Diagnostic::new("E2012", "recursive or invalid enum payload type", span).with_source(source)
    })?;
    for (index, depth) in enum_depths.iter().enumerate() {
        if *depth > MAX_VALUE_NESTING {
            return Err(Diagnostic::new(
                "E2012",
                "enum payload nesting exceeds the language limit",
                enum_spans[index].1,
            )
            .with_source(&enum_spans[index].0));
        }
    }
    for (value_type, source, span) in record_shapes {
        if max_expanded_type_depth(value_type, &enum_depths)
            .is_none_or(|depth| depth > MAX_VALUE_NESTING)
        {
            return Err(Diagnostic::new(
                "E2012",
                "record field nesting exceeds the language limit",
                *span,
            )
            .with_source(source));
        }
    }
    for (value_type, source, span) in alias_shapes {
        if max_expanded_type_depth(value_type, &enum_depths)
            .is_none_or(|depth| depth > MAX_VALUE_NESTING)
        {
            return Err(Diagnostic::new(
                "E2012",
                "type alias nesting exceeds the language limit",
                *span,
            )
            .with_source(source));
        }
    }
    Ok(())
}

pub(super) fn calculate_enum_depths(enum_types: &[EnumType]) -> Result<Vec<usize>, EnumDepthError> {
    let mut depths = vec![None; enum_types.len()];
    let mut marks = vec![0_u8; enum_types.len()];
    for index in 0..enum_types.len() {
        enum_depth(
            u32::try_from(index).expect("enum index fits"),
            enum_types,
            &mut depths,
            &mut marks,
        )?;
    }
    Ok(depths.into_iter().map(Option::unwrap).collect())
}

pub(super) fn enum_depth(
    id: u32,
    enum_types: &[EnumType],
    depths: &mut [Option<usize>],
    marks: &mut [u8],
) -> Result<usize, EnumDepthError> {
    let index = usize::try_from(id).map_err(|_| EnumDepthError::Missing(id))?;
    let Some(mark) = marks.get(index) else {
        return Err(EnumDepthError::Missing(id));
    };
    if *mark == 1 {
        return Err(EnumDepthError::Cycle(id));
    }
    if *mark == 2 {
        return Ok(depths[index].expect("completed enum depth is cached"));
    }
    let enum_type = enum_types.get(index).ok_or(EnumDepthError::Missing(id))?;
    marks[index] = 1;
    let mut depth = 0;
    for variant in &enum_type.variants {
        let payload_depth = match &variant.payload {
            EnumPayloadType::Unit => 0,
            EnumPayloadType::Tuple(elements) => elements
                .iter()
                .map(|element| enum_expanded_type_depth(element, enum_types, depths, marks))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .max()
                .map_or(0, add_depth),
            EnumPayloadType::Record(fields) => fields
                .iter()
                .map(|field| enum_expanded_type_depth(&field.value_type, enum_types, depths, marks))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .max()
                .map_or(0, add_depth),
        };
        depth = depth.max(payload_depth);
    }
    marks[index] = 2;
    depths[index] = Some(depth);
    Ok(depth)
}

pub(super) fn enum_expanded_type_depth(
    value_type: &ValueType,
    enum_types: &[EnumType],
    depths: &mut [Option<usize>],
    marks: &mut [u8],
) -> Result<usize, EnumDepthError> {
    match value_type {
        ValueType::Enum(id) => enum_depth(*id, enum_types, depths, marks),
        ValueType::List(element)
        | ValueType::Option(element)
        | ValueType::Future(element)
        | ValueType::Task(element) => Ok(add_depth(enum_expanded_type_depth(
            element, enum_types, depths, marks,
        )?)),
        ValueType::Map(key, value) | ValueType::Result(key, value) => Ok(add_depth(
            enum_expanded_type_depth(key, enum_types, depths, marks)?
                .max(enum_expanded_type_depth(value, enum_types, depths, marks)?),
        )),
        ValueType::Tuple(elements) => elements
            .iter()
            .map(|element| enum_expanded_type_depth(element, enum_types, depths, marks))
            .collect::<Result<Vec<_>, _>>()
            .map(|depths| depths.into_iter().max().map_or(0, add_depth)),
        ValueType::Record(fields) => fields
            .iter()
            .map(|field| enum_expanded_type_depth(&field.value_type, enum_types, depths, marks))
            .collect::<Result<Vec<_>, _>>()
            .map(|depths| depths.into_iter().max().map_or(0, add_depth)),
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => parameters
            .iter()
            .chain(std::iter::once(return_type.as_ref()))
            .map(|value_type| enum_expanded_type_depth(value_type, enum_types, depths, marks))
            .collect::<Result<Vec<_>, _>>()
            .map(|depths| depths.into_iter().max().map_or(0, add_depth)),
        ValueType::Int
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::ExternalFsAccess
        | ValueType::SubAgent
        | ValueType::Unit
        | ValueType::Never
        | ValueType::Workspace
        | ValueType::Unknown => Ok(0),
    }
}

pub(super) fn max_expanded_type_depth(
    value_type: &ValueType,
    enum_depths: &[usize],
) -> Option<usize> {
    let mut maximum = 0;
    let mut pending = vec![(value_type, 0_usize)];
    while let Some((value_type, depth)) = pending.pop() {
        match value_type {
            ValueType::Enum(id) => {
                let enum_depth = enum_depths.get(usize::try_from(*id).ok()?)?;
                maximum = maximum.max(depth.saturating_add(*enum_depth));
            }
            ValueType::List(element)
            | ValueType::Option(element)
            | ValueType::Future(element)
            | ValueType::Task(element) => {
                let depth = add_depth(depth);
                maximum = maximum.max(depth);
                pending.push((element, depth));
            }
            ValueType::Map(key, value) | ValueType::Result(key, value) => {
                let depth = add_depth(depth);
                maximum = maximum.max(depth);
                pending.push((key, depth));
                pending.push((value, depth));
            }
            ValueType::Tuple(elements) => {
                let depth = add_depth(depth);
                maximum = maximum.max(depth);
                pending.extend(elements.iter().map(|element| (element, depth)));
            }
            ValueType::Record(fields) => {
                let depth = add_depth(depth);
                maximum = maximum.max(depth);
                pending.extend(fields.iter().map(|field| (&field.value_type, depth)));
            }
            ValueType::Function {
                parameters,
                return_type,
                ..
            } => {
                let depth = add_depth(depth);
                maximum = maximum.max(depth);
                pending.extend(parameters.iter().map(|parameter| (parameter, depth)));
                pending.push((return_type, depth));
            }
            ValueType::Int
            | ValueType::Bool
            | ValueType::Float
            | ValueType::String
            | ValueType::Bytes
            | ValueType::ExternalFsAccess
            | ValueType::Unit
            | ValueType::Never
            | ValueType::Workspace
            | ValueType::SubAgent
            | ValueType::Unknown => maximum = maximum.max(depth),
        }
    }
    Some(maximum)
}

const fn add_depth(depth: usize) -> usize {
    depth.saturating_add(1)
}

pub(super) fn contains_affine(value_type: &ValueType) -> bool {
    match value_type {
        ValueType::Future(_) | ValueType::Task(_) => true,
        ValueType::List(value) | ValueType::Option(value) => contains_affine(value),
        ValueType::Map(key, value) | ValueType::Result(key, value) => {
            contains_affine(key) || contains_affine(value)
        }
        ValueType::Tuple(values) => values.iter().any(contains_affine),
        ValueType::Record(fields) => fields
            .iter()
            .any(|field| contains_affine(&field.value_type)),
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => parameters.iter().any(contains_affine) || contains_affine(return_type),
        ValueType::Int
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::Unit
        | ValueType::Never
        | ValueType::Enum(_)
        | ValueType::ExternalFsAccess
        | ValueType::SubAgent
        | ValueType::Workspace
        | ValueType::Unknown => false,
    }
}

pub(super) fn contains_workspace(value_type: &ValueType) -> bool {
    match value_type {
        ValueType::Workspace => true,
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value) => contains_workspace(value),
        ValueType::Map(key, value) | ValueType::Result(key, value) => {
            contains_workspace(key) || contains_workspace(value)
        }
        ValueType::Tuple(values) => values.iter().any(contains_workspace),
        ValueType::Record(fields) => fields
            .iter()
            .any(|field| contains_workspace(&field.value_type)),
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => parameters.iter().any(contains_workspace) || contains_workspace(return_type),
        ValueType::Int
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::Unit
        | ValueType::Never
        | ValueType::Enum(_)
        | ValueType::ExternalFsAccess
        | ValueType::SubAgent
        | ValueType::Unknown => false,
    }
}

pub(super) fn literal_map_key(expression: &LoweredExpr) -> Option<String> {
    match &expression.kind {
        LoweredExprKind::Int(value) => Some(format!("int:{value}")),
        LoweredExprKind::Bool(value) => Some(format!("bool:{value}")),
        LoweredExprKind::String(value) => Some(format!("string:{value}")),
        LoweredExprKind::Bytes(value) => Some(format!("bytes:{value:02x?}")),
        _ => None,
    }
}

pub(super) fn contains_sub_agent(value_type: &ValueType) -> bool {
    match value_type {
        ValueType::SubAgent => true,
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value) => contains_sub_agent(value),
        ValueType::Map(key, value) | ValueType::Result(key, value) => {
            contains_sub_agent(key) || contains_sub_agent(value)
        }
        ValueType::Tuple(values) => values.iter().any(contains_sub_agent),
        ValueType::Record(fields) => fields
            .iter()
            .any(|field| contains_sub_agent(&field.value_type)),
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => parameters.iter().any(contains_sub_agent) || contains_sub_agent(return_type),
        _ => false,
    }
}

pub(super) fn contains_stored_sub_agent(value_type: &ValueType) -> bool {
    match value_type {
        ValueType::SubAgent => true,
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value) => contains_stored_sub_agent(value),
        ValueType::Map(key, value) | ValueType::Result(key, value) => {
            contains_stored_sub_agent(key) || contains_stored_sub_agent(value)
        }
        ValueType::Tuple(values) => values.iter().any(contains_stored_sub_agent),
        ValueType::Record(fields) => fields
            .iter()
            .any(|field| contains_stored_sub_agent(&field.value_type)),
        ValueType::Function { .. }
        | ValueType::Int
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::Unit
        | ValueType::Never
        | ValueType::Enum(_)
        | ValueType::ExternalFsAccess
        | ValueType::Workspace
        | ValueType::Unknown => false,
    }
}
