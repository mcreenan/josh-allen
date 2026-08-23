//! Frozen-catalog tool preparation, schema validation, and compiler bindings.

use allen_bytecode::{
    EnumPayloadType, EnumType, EnumVariant, RecordField, StrictSchema, ToolContract, ValueType,
};
use allen_schema::{
    Descriptor, FrozenCatalog, SchemaRole, ToolRequirement, mangle_source_segment,
    union_declarations,
};
use std::collections::BTreeMap;

/// One frozen tool binding supplied by package/catalog resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompilerToolBinding {
    pub source_path: Vec<String>,
    pub contract: u32,
    pub input: ValueType,
    pub output: ValueType,
    pub declared_error: ValueType,
    /// Current closed operational error wrapper.
    pub error: ValueType,
    pub effect: String,
    /// Generated nominal enums. Their IDs are local to this binding until
    /// source enum IDs are known.
    pub enum_types: Vec<EnumType>,
}

/// Compiler and artifact inputs produced from one manifest-selected catalog snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PreparedTools {
    pub bindings: Vec<CompilerToolBinding>,
    pub schemas: Vec<StrictSchema>,
    pub contracts: Vec<ToolContract>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolPreparationError;

impl std::fmt::Display for ToolPreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("manifest tools do not match the frozen catalog")
    }
}

impl std::error::Error for ToolPreparationError {}

/// Select required tools and produce deterministic compiler and bytecode tables.
///
/// # Errors
///
/// Returns a safe error for a missing, out-of-range, malformed, or inconsistent tool.
pub fn prepare_tools(
    catalog: &FrozenCatalog,
    requirements: &[ToolRequirement],
) -> Result<PreparedTools, ToolPreparationError> {
    let selected = catalog
        .select(requirements)
        .map_err(|_| ToolPreparationError)?;
    let mut bindings = Vec::with_capacity(selected.len());
    let mut schemas = Vec::with_capacity(selected.len() * 3);
    let mut contracts = Vec::with_capacity(selected.len());
    for (contract_index, selected) in selected.iter().enumerate() {
        let definition = catalog.get(&selected.name).ok_or(ToolPreparationError)?;
        let mut enum_types = Vec::new();
        let input = descriptor_value_type(
            definition.input_schema.descriptor(),
            SchemaRole::Input,
            &selected.name,
            &mut enum_types,
        )?;
        let output = descriptor_value_type(
            definition.output_schema.descriptor(),
            SchemaRole::Output,
            &selected.name,
            &mut enum_types,
        )?;
        let declared_error = descriptor_value_type(
            definition.error_schema.descriptor(),
            SchemaRole::Error,
            &selected.name,
            &mut enum_types,
        )?;
        let input_schema = push_tool_schema(&mut schemas, input.clone())?;
        let output_schema = push_tool_schema(&mut schemas, output.clone())?;
        let error_schema = push_tool_schema(&mut schemas, declared_error.clone())?;
        let wrapper_id = u32::try_from(enum_types.len()).map_err(|_| ToolPreparationError)?;
        enum_types.push(EnumType {
            name: format!("tools.{}::Error", selected.name.as_str()),
            variants: vec![
                EnumVariant {
                    name: "Declared".to_owned(),
                    payload: EnumPayloadType::Tuple(vec![declared_error.clone()]),
                },
                EnumVariant {
                    name: "Unavailable".to_owned(),
                    payload: EnumPayloadType::Record(vec![
                        RecordField {
                            name: "code".to_owned(),
                            value_type: ValueType::String,
                        },
                        RecordField {
                            name: "message".to_owned(),
                            value_type: ValueType::String,
                        },
                    ]),
                },
                EnumVariant {
                    name: "Schema".to_owned(),
                    payload: EnumPayloadType::Record(vec![
                        RecordField {
                            name: "code".to_owned(),
                            value_type: ValueType::String,
                        },
                        RecordField {
                            name: "message".to_owned(),
                            value_type: ValueType::String,
                        },
                    ]),
                },
            ],
        });
        let contract = u32::try_from(contract_index).map_err(|_| ToolPreparationError)?;
        bindings.push(CompilerToolBinding {
            source_path: selected
                .name
                .segments()
                .map(mangle_source_segment)
                .collect(),
            contract,
            input,
            output,
            declared_error,
            error: ValueType::Enum(wrapper_id),
            effect: selected.effect.clone(),
            enum_types,
        });
        contracts.push(ToolContract {
            name: selected.name.as_str().to_owned(),
            version: selected.version.to_string(),
            version_requirement: selected.version_requirement.to_string(),
            effect: selected.effect.clone(),
            input_schema,
            output_schema,
            error_schema,
            input_digest: digest_array(&selected.input_schema)?,
            output_digest: digest_array(&selected.output_schema)?,
            error_digest: digest_array(&selected.error_schema)?,
        });
    }
    Ok(PreparedTools {
        bindings,
        schemas,
        contracts,
    })
}

fn push_tool_schema(
    schemas: &mut Vec<StrictSchema>,
    value_type: ValueType,
) -> Result<u32, ToolPreparationError> {
    let index = u32::try_from(schemas.len()).map_err(|_| ToolPreparationError)?;
    schemas.push(StrictSchema { value_type });
    Ok(index)
}

fn digest_array(value: &str) -> Result<[u8; 32], ToolPreparationError> {
    let hex = value.strip_prefix("sha256:").ok_or(ToolPreparationError)?;
    if hex.len() != 64 {
        return Err(ToolPreparationError);
    }
    let mut output = [0; 32];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(output)
}

fn hex_nibble(value: u8) -> Result<u8, ToolPreparationError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(ToolPreparationError),
    }
}

fn descriptor_value_type(
    descriptor: &Descriptor,
    role: SchemaRole,
    tool: &allen_schema::ToolName,
    enum_types: &mut Vec<EnumType>,
) -> Result<ValueType, ToolPreparationError> {
    let names = union_declarations(descriptor, role)
        .into_iter()
        .map(|declaration| (declaration.pointer, declaration.source_name))
        .collect::<BTreeMap<_, _>>();
    lower_descriptor(
        descriptor,
        match role {
            SchemaRole::Input => "/input",
            SchemaRole::Output => "/output",
            SchemaRole::Error => "/error",
        },
        tool,
        &names,
        enum_types,
    )
}

#[allow(clippy::too_many_lines)]
fn lower_descriptor(
    descriptor: &Descriptor,
    pointer: &str,
    tool: &allen_schema::ToolName,
    names: &BTreeMap<String, String>,
    enum_types: &mut Vec<EnumType>,
) -> Result<ValueType, ToolPreparationError> {
    Ok(match descriptor {
        Descriptor::Unit => ValueType::Unit,
        Descriptor::Bool => ValueType::Bool,
        Descriptor::Int { .. } => ValueType::Int,
        Descriptor::Float { .. } => ValueType::Float,
        Descriptor::String { .. } => ValueType::String,
        Descriptor::List { items, .. } => ValueType::List(Box::new(lower_descriptor(
            items,
            &schema_pointer(pointer, "items"),
            tool,
            names,
            enum_types,
        )?)),
        Descriptor::Tuple { items } => ValueType::Tuple(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    lower_descriptor(
                        item,
                        &schema_pointer(&schema_pointer(pointer, "items"), &index.to_string()),
                        tool,
                        names,
                        enum_types,
                    )
                })
                .collect::<Result<_, _>>()?,
        ),
        Descriptor::Record { fields } => ValueType::Record(
            fields
                .iter()
                .map(|field| {
                    Ok(RecordField {
                        name: field.name.clone(),
                        value_type: lower_descriptor(
                            &field.schema,
                            &schema_pointer(&schema_pointer(pointer, "fields"), &field.name),
                            tool,
                            names,
                            enum_types,
                        )?,
                    })
                })
                .collect::<Result<_, ToolPreparationError>>()?,
        ),
        Descriptor::StringMap { values } => ValueType::Map(
            Box::new(ValueType::String),
            Box::new(lower_descriptor(
                values,
                &schema_pointer(pointer, "values"),
                tool,
                names,
                enum_types,
            )?),
        ),
        Descriptor::TaggedUnion { variants } => {
            let id = u32::try_from(enum_types.len()).map_err(|_| ToolPreparationError)?;
            let name = names.get(pointer).ok_or(ToolPreparationError)?;
            enum_types.push(EnumType {
                name: String::new(),
                variants: Vec::new(),
            });
            let mut lowered_variants = Vec::with_capacity(variants.len());
            for (index, variant) in variants.iter().enumerate() {
                let fields = variant
                    .fields
                    .iter()
                    .map(|field| {
                        Ok(RecordField {
                            name: field.name.clone(),
                            value_type: lower_descriptor(
                                &field.schema,
                                &schema_pointer(
                                    &schema_pointer(
                                        &schema_pointer(
                                            &schema_pointer(pointer, "variants"),
                                            &index.to_string(),
                                        ),
                                        "fields",
                                    ),
                                    &field.name,
                                ),
                                tool,
                                names,
                                enum_types,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>, ToolPreparationError>>()?;
                lowered_variants.push(EnumVariant {
                    name: format!("_tag_{}", mangle_source_segment(&variant.tag)),
                    payload: if fields.is_empty() {
                        EnumPayloadType::Unit
                    } else {
                        EnumPayloadType::Record(fields)
                    },
                });
            }
            enum_types[id as usize] = EnumType {
                name: format!(
                    "tools.{}.{}",
                    tool.segments()
                        .map(mangle_source_segment)
                        .collect::<Vec<_>>()
                        .join("."),
                    name
                ),
                variants: lowered_variants,
            };
            ValueType::Enum(id)
        }
    })
}

fn schema_pointer(base: &str, token: &str) -> String {
    format!("{base}/{}", token.replace('~', "~0").replace('/', "~1"))
}
