use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sha2::{Digest, Sha256};

use crate::{Descriptor, ExactVersion, ToolDefinition};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ToolName(String);

impl ToolName {
    /// Validate canonical tool-name bytes, including the UTF-8 boundary.
    ///
    /// # Errors
    ///
    /// Rejects invalid UTF-8 and every noncanonical tool name.
    pub fn parse_bytes(value: &[u8]) -> Result<Self, NameError> {
        let value = std::str::from_utf8(value).map_err(|_| NameError::InvalidToolName)?;
        Self::parse(value)
    }

    /// Validate one exact canonical UTF-8 tool name.
    ///
    /// # Errors
    ///
    /// Rejects empty and over-limit segments, controls, whitespace, and names
    /// outside the 1 through 255-byte profile.
    pub fn parse(value: impl Into<String>) -> Result<Self, NameError> {
        let value = value.into();
        if value.is_empty() || value.len() > 255 {
            return Err(NameError::InvalidToolName);
        }
        for segment in value.split('.') {
            if segment.is_empty()
                || segment.len() > 63
                || segment
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace())
            {
                return Err(NameError::InvalidToolName);
            }
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaRole {
    Input,
    Output,
    Error,
}

impl SchemaRole {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Input => "Input_union_",
            Self::Output => "Output_union_",
            Self::Error => "Error_union_",
        }
    }
    const fn root(self) -> &'static str {
        match self {
            Self::Input => "/input",
            Self::Output => "/output",
            Self::Error => "/error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedName {
    pub source_name: String,
    pub pointer: String,
    pub variants: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameError {
    InvalidToolName,
    InvalidVersionMajor,
    Collision,
}

impl fmt::Display for NameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidToolName => "tool name is not canonical",
            Self::InvalidVersionMajor => "tool version major must be positive",
            Self::Collision => "generated tool name collision",
        })
    }
}
impl std::error::Error for NameError {}

/// Mangle one canonical tool-name segment into an ALLEN source identifier.
#[must_use]
pub fn mangle_source_segment(segment: &str) -> String {
    let mut output = String::new();
    let mut first_preserved = None;
    for byte in segment.as_bytes() {
        if byte.is_ascii_alphanumeric() || *byte == b'_' {
            first_preserved.get_or_insert(*byte);
            output.push(char::from(*byte));
        } else {
            use std::fmt::Write;
            write!(output, "_x{byte:02X}_").expect("writing to String cannot fail");
        }
    }
    if first_preserved.is_some_and(|byte| byte.is_ascii_digit()) {
        output.insert_str(0, "_n_");
    }
    if is_reserved_word(&output) {
        output.insert_str(0, "_kw_");
    }
    output
}

/// Mangle one tool segment for a canonical generated effect ID.
#[must_use]
pub fn mangle_effect_segment(segment: &str) -> String {
    let mut output = String::new();
    let mut first_preserved = None;
    for byte in segment.as_bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' {
            first_preserved.get_or_insert(*byte);
            output.push(char::from(*byte));
        } else {
            use std::fmt::Write;
            write!(output, "_x{byte:02x}_").expect("writing to String cannot fail");
        }
    }
    if first_preserved.is_some_and(|byte| byte.is_ascii_digit()) {
        output.insert_str(0, "_n_");
    }
    output
}

/// Generate the one version-major tool effect for a selected definition.
///
/// # Errors
///
/// Version major zero cannot form a canonical versioned effect ID.
pub fn generated_tool_effect(name: &ToolName, version: ExactVersion) -> Result<String, NameError> {
    if version.major == 0 {
        return Err(NameError::InvalidVersionMajor);
    }
    Ok(format!(
        "tool.{}@{}",
        name.segments()
            .map(mangle_effect_segment)
            .collect::<Vec<_>>()
            .join("."),
        version.major
    ))
}

/// Enumerate generated tagged-union names and their exact expanded pointers.
#[must_use]
pub fn union_declarations(descriptor: &Descriptor, role: SchemaRole) -> Vec<GeneratedName> {
    let mut output = Vec::new();
    collect_unions(descriptor, role, role.root(), &mut output);
    output
}

fn collect_unions(
    descriptor: &Descriptor,
    role: SchemaRole,
    pointer: &str,
    output: &mut Vec<GeneratedName>,
) {
    match descriptor {
        Descriptor::List { items, .. } => {
            collect_unions(items, role, &join(pointer, "items"), output);
        }
        Descriptor::Tuple { items } => {
            for (index, item) in items.iter().enumerate() {
                collect_unions(
                    item,
                    role,
                    &join(&join(pointer, "items"), &index.to_string()),
                    output,
                );
            }
        }
        Descriptor::Record { fields } => {
            for field in fields {
                collect_unions(
                    &field.schema,
                    role,
                    &join(&join(pointer, "fields"), &field.name),
                    output,
                );
            }
        }
        Descriptor::StringMap { values } => {
            collect_unions(values, role, &join(pointer, "values"), output);
        }
        Descriptor::TaggedUnion { variants } => {
            let digest = Sha256::digest(pointer.as_bytes());
            let suffix = format!("{digest:x}");
            output.push(GeneratedName {
                source_name: format!("{}{}", role.prefix(), &suffix[..16]),
                pointer: pointer.to_owned(),
                variants: variants
                    .iter()
                    .map(|variant| format!("_tag_{}", mangle_source_segment(&variant.tag)))
                    .collect(),
            });
            for (index, variant) in variants.iter().enumerate() {
                for field in &variant.fields {
                    collect_unions(
                        &field.schema,
                        role,
                        &join(
                            &join(
                                &join(&join(pointer, "variants"), &index.to_string()),
                                "fields",
                            ),
                            &field.name,
                        ),
                        output,
                    );
                }
            }
        }
        Descriptor::Unit
        | Descriptor::Bool
        | Descriptor::Int { .. }
        | Descriptor::Float { .. }
        | Descriptor::String { .. } => {}
    }
}

#[derive(Default)]
struct Namespace<'a> {
    source_segments: BTreeMap<String, &'a str>,
    children: BTreeMap<String, Self>,
    leaf: Option<&'a ToolDefinition>,
}

/// Prove source, effect, fixed-member, enum, and variant names are collision-free.
///
/// # Errors
///
/// Returns one safe collision error without exposing schema or tool values.
pub fn validate_generated_names(tools: &[ToolDefinition]) -> Result<(), NameError> {
    let mut root = Namespace::default();
    let mut effects = BTreeMap::new();
    for tool in tools {
        let mut namespace = &mut root;
        for segment in tool.name.segments() {
            let mangled = mangle_source_segment(segment);
            if namespace
                .source_segments
                .get(&mangled)
                .is_some_and(|existing| *existing != segment)
            {
                return Err(NameError::Collision);
            }
            namespace.source_segments.insert(mangled.clone(), segment);
            namespace = namespace.children.entry(mangled).or_default();
        }
        if namespace.leaf.replace(tool).is_some() {
            return Err(NameError::Collision);
        }
        let effect = generated_tool_effect(&tool.name, tool.version)?;
        if effects.insert(effect, tool.name.as_str()).is_some() {
            return Err(NameError::Collision);
        }
    }
    validate_namespace(&root)
}

fn validate_namespace(namespace: &Namespace<'_>) -> Result<(), NameError> {
    if let Some(tool) = namespace.leaf {
        let mut members: BTreeMap<String, String> = ["Input", "Output", "Error", "call"]
            .into_iter()
            .map(|name| (name.to_owned(), format!("fixed:{name}")))
            .collect();
        for child in namespace.children.keys() {
            if members
                .insert(child.clone(), format!("child:{child}"))
                .is_some()
            {
                return Err(NameError::Collision);
            }
        }
        for (role, schema) in [
            (SchemaRole::Input, &tool.input_schema),
            (SchemaRole::Output, &tool.output_schema),
            (SchemaRole::Error, &tool.error_schema),
        ] {
            for declaration in union_declarations(schema.descriptor(), role) {
                if members
                    .insert(declaration.source_name.clone(), declaration.pointer.clone())
                    .is_some()
                {
                    return Err(NameError::Collision);
                }
                let mut variants = BTreeSet::new();
                for variant in declaration.variants {
                    if !variants.insert(variant) {
                        return Err(NameError::Collision);
                    }
                }
            }
        }
    }
    for child in namespace.children.values() {
        validate_namespace(child)?;
    }
    Ok(())
}

fn is_reserved_word(value: &str) -> bool {
    matches!(
        value,
        "Bool"
            | "Bytes"
            | "Float"
            | "Int"
            | "List"
            | "Map"
            | "Never"
            | "Option"
            | "Result"
            | "String"
            | "Void"
            | "any"
            | "async"
            | "await"
            | "break"
            | "catch"
            | "continue"
            | "detach"
            | "effects"
            | "else"
            | "enum"
            | "export"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "import"
            | "in"
            | "let"
            | "manifest"
            | "map"
            | "match"
            | "mut"
            | "record"
            | "return"
            | "scope"
            | "spawn"
            | "throw"
            | "true"
            | "type"
            | "try"
            | "undefined"
            | "unknown"
            | "while"
            | "null"
    )
}

fn join(base: &str, token: &str) -> String {
    format!("{base}/{}", token.replace('~', "~0").replace('/', "~1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_source_and_effect_mangling_is_golden() {
        let name = ToolName::parse("release-tools.create-issue").unwrap();
        assert_eq!(
            name.segments()
                .map(mangle_source_segment)
                .collect::<Vec<_>>(),
            ["release_x2D_tools", "create_x2D_issue"]
        );
        assert_eq!(
            generated_tool_effect(&name, ExactVersion::parse("2.1.3").unwrap()).unwrap(),
            "tool.release_x2d_tools.create_x2d_issue@2"
        );
        assert_eq!(mangle_source_segment("9é"), "_n_9_xC3__xA9_");
        assert_eq!(mangle_source_segment("match"), "_kw_match");
        assert_eq!(mangle_source_segment("type"), "_kw_type");
        assert_eq!(mangle_effect_segment("Aé"), "_x41__xc3__xa9_");
    }

    #[test]
    fn canonical_tool_names_reject_boundaries_and_whitespace() {
        for invalid in ["", ".a", "a.", "a..b", "a b", "a\n"] {
            assert!(ToolName::parse(invalid).is_err(), "{invalid:?}");
        }
        assert!(ToolName::parse("é.tool").is_ok());
        assert!(ToolName::parse("a".repeat(64)).is_err());
        assert!(ToolName::parse_bytes(&[0xff]).is_err());
    }

    #[test]
    fn escape_like_source_text_deliberately_collides() {
        assert_eq!(
            mangle_source_segment("a-b"),
            mangle_source_segment("a_x2D_b")
        );
        assert_eq!(
            mangle_effect_segment("a-b"),
            mangle_effect_segment("a_x2d_b")
        );
    }

    #[test]
    fn expanded_union_pointer_and_companion_name_are_golden() {
        let schema = crate::ToolSchema::parse(
            r#"{"type":"object","properties":{"payload":{"oneOf":[{"type":"object","properties":{"tag":{"type":"string","enum":["a-b"]}},"required":["tag"],"additionalProperties":false},{"type":"object","properties":{"tag":{"type":"string","enum":["other"]}},"required":["tag"],"additionalProperties":false}]}},"required":["payload"],"additionalProperties":false}"#,
            &crate::SchemaLimits::default(),
        )
        .unwrap();
        assert_eq!(
            union_declarations(schema.descriptor(), SchemaRole::Input),
            vec![GeneratedName {
                source_name: "Input_union_584ea60b06c5bfa5".to_owned(),
                pointer: "/input/fields/payload".to_owned(),
                variants: vec!["_tag_a_x2D_b".to_owned(), "_tag_other".to_owned()],
            }]
        );
    }
}
