use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::SCHEMA_DIALECT;
use crate::canonical::{canonical_json, digest_bytes};
use crate::json::{StrictJsonError, StrictValue, parse_strict_value};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaLimits {
    pub schema_bytes: usize,
    pub nodes: usize,
    pub depth: usize,
    pub properties: usize,
    pub definitions: usize,
    pub enum_strings: usize,
    pub union_branches: usize,
    pub name_bytes: usize,
    pub bound: usize,
    pub validation_issues: usize,
    pub issue_pointer_bytes: usize,
}

impl Default for SchemaLimits {
    fn default() -> Self {
        Self {
            schema_bytes: 262_144,
            nodes: 4_096,
            depth: 32,
            properties: 256,
            definitions: 256,
            enum_strings: 256,
            union_branches: 64,
            name_bytes: 255,
            bound: 1_048_576,
            validation_issues: 16,
            issue_pointer_bytes: 1_024,
        }
    }
}

impl SchemaLimits {
    #[must_use]
    pub fn bounded_by(self, host: Self) -> Self {
        Self {
            schema_bytes: self.schema_bytes.min(host.schema_bytes),
            nodes: self.nodes.min(host.nodes),
            depth: self.depth.min(host.depth),
            properties: self.properties.min(host.properties),
            definitions: self.definitions.min(host.definitions),
            enum_strings: self.enum_strings.min(host.enum_strings),
            union_branches: self.union_branches.min(host.union_branches),
            name_bytes: self.name_bytes.min(host.name_bytes),
            bound: self.bound.min(host.bound),
            validation_issues: self.validation_issues.min(host.validation_issues),
            issue_pointer_bytes: self.issue_pointer_bytes.min(host.issue_pointer_bytes),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Descriptor {
    Unit,
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: Option<f64>,
        max: Option<f64>,
    },
    String {
        min: Option<usize>,
        max: Option<usize>,
        enumeration: Vec<String>,
    },
    List {
        min: usize,
        max: usize,
        items: Box<Self>,
    },
    Tuple {
        items: Vec<Self>,
    },
    Record {
        fields: Vec<Field>,
    },
    StringMap {
        values: Box<Self>,
    },
    TaggedUnion {
        variants: Vec<Variant>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub schema: Descriptor,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub tag: String,
    pub fields: Vec<Field>,
}

impl Descriptor {
    #[must_use]
    pub fn canonical_value(&self) -> serde_json::Value {
        match self {
            Self::Unit => serde_json::json!({"kind": "void"}),
            Self::Bool => serde_json::json!({"kind": "bool"}),
            Self::Int { min, max } => serde_json::json!({"kind": "int", "max": max, "min": min}),
            Self::Float { min, max } => {
                serde_json::json!({"kind": "float", "max": max, "min": min})
            }
            Self::String {
                min,
                max,
                enumeration,
            } => {
                serde_json::json!({"enum": enumeration, "kind": "string", "max": max, "min": min})
            }
            Self::List { min, max, items } => serde_json::json!({
                "items": items.canonical_value(), "kind": "list", "max": max, "min": min
            }),
            Self::Tuple { items } => serde_json::json!({
                "items": items.iter().map(Self::canonical_value).collect::<Vec<_>>(),
                "kind": "tuple"
            }),
            Self::Record { fields } => serde_json::json!({
                "fields": fields.iter().map(Field::canonical_value).collect::<Vec<_>>(),
                "kind": "record"
            }),
            Self::StringMap { values } => serde_json::json!({
                "kind": "string_map", "values": values.canonical_value()
            }),
            Self::TaggedUnion { variants } => serde_json::json!({
                "kind": "tagged_union",
                "variants": variants.iter().map(Variant::canonical_value).collect::<Vec<_>>()
            }),
        }
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_json(&self.canonical_value())
    }

    #[must_use]
    pub fn digest(&self) -> String {
        digest_bytes(&self.canonical_bytes())
    }

    /// Validate a JSON value without coercion and return bounded safe issues.
    ///
    /// # Errors
    ///
    /// Returns JSON Pointers and stable codes only. Issues never contain values.
    pub fn validate(
        &self,
        value: &serde_json::Value,
        limits: &SchemaLimits,
    ) -> Result<(), Vec<ValidationIssue>> {
        let mut issues = Vec::new();
        validate_at(self, value, "", limits, &mut issues);
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }
}

impl Field {
    fn canonical_value(&self) -> serde_json::Value {
        serde_json::json!({"name": self.name, "schema": self.schema.canonical_value()})
    }
}

impl Variant {
    fn canonical_value(&self) -> serde_json::Value {
        serde_json::json!({
            "fields": self.fields.iter().map(Field::canonical_value).collect::<Vec<_>>(),
            "tag": self.tag
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolSchema {
    descriptor: Descriptor,
    canonical: Vec<u8>,
    digest: String,
    source_bytes: usize,
    definition_count: usize,
    definition_name_bytes: usize,
}

impl ToolSchema {
    /// Parse and lower one strict schema document.
    ///
    /// # Errors
    ///
    /// Returns a stable schema error before any partially lowered schema escapes.
    pub fn parse(input: &str, limits: &SchemaLimits) -> Result<Self, SchemaError> {
        if input.len() > limits.schema_bytes {
            return Err(SchemaError::new(SchemaErrorCode::Limit, ""));
        }
        let raw = parse_strict_value(input).map_err(|error| {
            SchemaError::new(
                match error {
                    StrictJsonError::DuplicateKey => SchemaErrorCode::DuplicateKey,
                    StrictJsonError::Invalid => SchemaErrorCode::InvalidJson,
                },
                "",
            )
        })?;
        Self::lower(raw, input.len(), limits)
    }

    /// Lower a schema value that already came through a strict enclosing parser.
    ///
    /// # Errors
    ///
    /// Returns a stable schema error for an unsupported or over-limit value.
    pub fn from_value(
        value: &serde_json::Value,
        limits: &SchemaLimits,
    ) -> Result<Self, SchemaError> {
        let bytes = canonical_json(value);
        if bytes.len() > limits.schema_bytes {
            return Err(SchemaError::new(SchemaErrorCode::Limit, ""));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| SchemaError::new(SchemaErrorCode::InvalidJson, ""))?;
        let raw = parse_strict_value(text)
            .map_err(|_| SchemaError::new(SchemaErrorCode::InvalidJson, ""))?;
        Self::lower(raw, bytes.len(), limits)
    }

    fn lower(
        raw: StrictValue,
        source_bytes: usize,
        limits: &SchemaLimits,
    ) -> Result<Self, SchemaError> {
        let StrictValue::Object(root) = raw else {
            return Err(SchemaError::new(SchemaErrorCode::InvalidForm, ""));
        };
        let mut context = Lowering::new(root, limits)?;
        let definition_count = context.definitions.len();
        let definition_name_bytes = context
            .definitions
            .keys()
            .map(String::len)
            .max()
            .unwrap_or(0);
        let root = context.root.clone();
        let descriptor = context.lower_object(&root, 1, "")?;
        if context.used_definitions.len() != context.definitions.len() {
            return Err(SchemaError::new(
                SchemaErrorCode::UnusedDefinition,
                "/$defs",
            ));
        }
        let canonical = descriptor.canonical_bytes();
        let digest = digest_bytes(&canonical);
        Ok(Self {
            descriptor,
            canonical,
            digest,
            source_bytes,
            definition_count,
            definition_name_bytes,
        })
    }

    #[must_use]
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
    #[must_use]
    pub fn source_bytes(&self) -> usize {
        self.source_bytes
    }

    pub(crate) fn fits_limits(&self, limits: &SchemaLimits) -> bool {
        if self.source_bytes > limits.schema_bytes
            || self.definition_count > limits.definitions
            || self.definition_name_bytes > limits.name_bytes
        {
            return false;
        }
        let mut usage = DescriptorUsage::default();
        collect_usage(&self.descriptor, 1, &mut usage);
        usage.nodes <= limits.nodes
            && usage.depth <= limits.depth
            && usage.properties <= limits.properties
            && usage.enum_strings <= limits.enum_strings
            && usage.max_union_branches <= limits.union_branches.min(64)
            && usage.max_name_bytes <= limits.name_bytes
            && usage.max_bound <= limits.bound
    }

    /// Validate a value with the limits used by the caller.
    ///
    /// # Errors
    ///
    /// Returns bounded stable issues only.
    pub fn validate(
        &self,
        value: &serde_json::Value,
        limits: &SchemaLimits,
    ) -> Result<(), Vec<ValidationIssue>> {
        self.descriptor.validate(value, limits)
    }
}

#[derive(Default)]
struct DescriptorUsage {
    nodes: usize,
    depth: usize,
    properties: usize,
    enum_strings: usize,
    max_union_branches: usize,
    max_name_bytes: usize,
    max_bound: usize,
}

fn collect_usage(descriptor: &Descriptor, depth: usize, usage: &mut DescriptorUsage) {
    usage.nodes += 1;
    usage.depth = usage.depth.max(depth);
    match descriptor {
        Descriptor::String {
            min,
            max,
            enumeration,
        } => {
            usage.enum_strings += enumeration.len();
            usage.max_bound = usage.max_bound.max(min.unwrap_or(0)).max(max.unwrap_or(0));
        }
        Descriptor::List { min, max, items } => {
            usage.max_bound = usage.max_bound.max(*min).max(*max);
            collect_usage(items, depth + 1, usage);
        }
        Descriptor::Tuple { items } => {
            usage.max_bound = usage.max_bound.max(items.len());
            for item in items {
                collect_usage(item, depth + 1, usage);
            }
        }
        Descriptor::Record { fields } => {
            usage.properties += fields.len();
            for field in fields {
                usage.max_name_bytes = usage.max_name_bytes.max(field.name.len());
                collect_usage(&field.schema, depth + 1, usage);
            }
        }
        Descriptor::StringMap { values } => collect_usage(values, depth + 1, usage),
        Descriptor::TaggedUnion { variants } => {
            usage.max_union_branches = usage.max_union_branches.max(variants.len());
            usage.nodes += variants.len() * 2;
            usage.properties += variants
                .iter()
                .map(|variant| variant.fields.len() + 1)
                .sum::<usize>();
            usage.enum_strings += variants.len();
            usage.depth = usage.depth.max(depth + 2);
            for variant in variants {
                usage.max_name_bytes = usage.max_name_bytes.max(variant.tag.len());
                for field in &variant.fields {
                    usage.max_name_bytes = usage.max_name_bytes.max(field.name.len());
                    collect_usage(&field.schema, depth + 2, usage);
                }
            }
        }
        Descriptor::Unit | Descriptor::Bool | Descriptor::Int { .. } | Descriptor::Float { .. } => {
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaErrorCode {
    InvalidJson,
    DuplicateKey,
    InvalidForm,
    UnsupportedKeyword,
    InvalidBound,
    UnsortedSet,
    InvalidReference,
    CyclicReference,
    UnusedDefinition,
    Limit,
}

impl SchemaErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "schema.invalid_json",
            Self::DuplicateKey => "schema.duplicate_key",
            Self::InvalidForm => "schema.invalid_form",
            Self::UnsupportedKeyword => "schema.unsupported_keyword",
            Self::InvalidBound => "schema.invalid_bound",
            Self::UnsortedSet => "schema.unsorted_set",
            Self::InvalidReference => "schema.invalid_reference",
            Self::CyclicReference => "schema.cyclic_reference",
            Self::UnusedDefinition => "schema.unused_definition",
            Self::Limit => "schema.limit",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaError {
    pub code: SchemaErrorCode,
    pub pointer: String,
}

impl SchemaError {
    fn new(code: SchemaErrorCode, pointer: impl Into<String>) -> Self {
        Self {
            code,
            pointer: pointer.into(),
        }
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {}",
            self.code.as_str(),
            if self.pointer.is_empty() {
                "/"
            } else {
                &self.pointer
            }
        )
    }
}

impl std::error::Error for SchemaError {}

struct Lowering<'a> {
    root: Vec<(String, StrictValue)>,
    definitions: BTreeMap<String, StrictValue>,
    used_definitions: BTreeSet<String>,
    reference_stack: Vec<String>,
    limits: &'a SchemaLimits,
    nodes: usize,
    properties: usize,
    enum_strings: usize,
}

impl<'a> Lowering<'a> {
    fn new(
        mut root: Vec<(String, StrictValue)>,
        limits: &'a SchemaLimits,
    ) -> Result<Self, SchemaError> {
        let mut definitions = BTreeMap::new();
        if let Some(index) = root.iter().position(|(key, _)| key == "$defs") {
            let (_, raw_defs) = root.remove(index);
            let StrictValue::Object(entries) = raw_defs else {
                return Err(SchemaError::new(SchemaErrorCode::InvalidForm, "/$defs"));
            };
            if entries.len() > limits.definitions {
                return Err(SchemaError::new(SchemaErrorCode::Limit, "/$defs"));
            }
            ensure_sorted_names(&entries)
                .map_err(|()| SchemaError::new(SchemaErrorCode::UnsortedSet, "/$defs"))?;
            for (name, value) in entries {
                check_name(&name, limits, "/$defs")?;
                definitions.insert(name, value);
            }
        }
        if let Some(index) = root.iter().position(|(key, _)| key == "$schema") {
            let (_, dialect) = root.remove(index);
            if dialect != StrictValue::String(SCHEMA_DIALECT.to_owned()) {
                return Err(SchemaError::new(SchemaErrorCode::InvalidForm, "/$schema"));
            }
        }
        Ok(Self {
            root,
            definitions,
            used_definitions: BTreeSet::new(),
            reference_stack: Vec::new(),
            limits,
            nodes: 0,
            properties: 0,
            enum_strings: 0,
        })
    }

    fn lower(
        &mut self,
        value: &StrictValue,
        depth: usize,
        pointer: &str,
    ) -> Result<Descriptor, SchemaError> {
        let StrictValue::Object(object) = value else {
            return Err(SchemaError::new(SchemaErrorCode::InvalidForm, pointer));
        };
        self.lower_object(object, depth, pointer)
    }

    fn lower_object(
        &mut self,
        object: &[(String, StrictValue)],
        depth: usize,
        pointer: &str,
    ) -> Result<Descriptor, SchemaError> {
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or_else(|| SchemaError::new(SchemaErrorCode::Limit, pointer))?;
        if self.nodes > self.limits.nodes || depth > self.limits.depth {
            return Err(SchemaError::new(SchemaErrorCode::Limit, pointer));
        }
        let filtered: Vec<_> = object
            .iter()
            .filter(|(key, _)| key != "title" && key != "description")
            .collect();
        for (key, value) in object
            .iter()
            .filter(|(key, _)| key == "title" || key == "description")
        {
            if !matches!(value, StrictValue::String(_)) {
                return Err(SchemaError::new(
                    SchemaErrorCode::InvalidForm,
                    join(pointer, key),
                ));
            }
        }
        if object.iter().any(|(key, _)| key == "$ref") && object.len() != 1 {
            return Err(SchemaError::new(
                SchemaErrorCode::UnsupportedKeyword,
                pointer,
            ));
        }
        if filtered.len() == 1 && filtered[0].0 == "$ref" {
            return self.lower_ref(&filtered[0].1, depth, pointer);
        }
        if filtered.iter().any(|(key, _)| key == "oneOf") {
            return self.lower_union(object, depth, pointer);
        }
        let schema_type = string_member(object, "type", pointer)?;
        match schema_type {
            "null" => {
                exact_keys(object, &["description", "title", "type"], pointer)?;
                Ok(Descriptor::Unit)
            }
            "boolean" => {
                exact_keys(object, &["description", "title", "type"], pointer)?;
                Ok(Descriptor::Bool)
            }
            "integer" => Self::lower_integer(object, pointer),
            "number" => Self::lower_float(object, pointer),
            "string" => self.lower_string(object, pointer),
            "array" => self.lower_array(object, depth, pointer),
            "object" => self.lower_record_or_map(object, depth, pointer),
            _ => Err(SchemaError::new(
                SchemaErrorCode::InvalidForm,
                join(pointer, "type"),
            )),
        }
    }

    fn lower_ref(
        &mut self,
        raw: &StrictValue,
        depth: usize,
        pointer: &str,
    ) -> Result<Descriptor, SchemaError> {
        let StrictValue::String(reference) = raw else {
            return Err(SchemaError::new(SchemaErrorCode::InvalidReference, pointer));
        };
        let token = reference
            .strip_prefix("#/$defs/")
            .ok_or_else(|| SchemaError::new(SchemaErrorCode::InvalidReference, pointer))?;
        if token.is_empty() || token.contains('/') {
            return Err(SchemaError::new(SchemaErrorCode::InvalidReference, pointer));
        }
        let name = unescape_pointer_token(token)
            .ok_or_else(|| SchemaError::new(SchemaErrorCode::InvalidReference, pointer))?;
        if escape_pointer_token(&name) != token {
            return Err(SchemaError::new(SchemaErrorCode::InvalidReference, pointer));
        }
        let definition = self
            .definitions
            .get(&name)
            .cloned()
            .ok_or_else(|| SchemaError::new(SchemaErrorCode::InvalidReference, pointer))?;
        if self.reference_stack.contains(&name) {
            return Err(SchemaError::new(SchemaErrorCode::CyclicReference, pointer));
        }
        self.used_definitions.insert(name.clone());
        self.reference_stack.push(name);
        let result = self.lower(&definition, depth + 1, pointer);
        self.reference_stack.pop();
        result
    }

    fn lower_integer(
        object: &[(String, StrictValue)],
        pointer: &str,
    ) -> Result<Descriptor, SchemaError> {
        exact_keys(
            object,
            &["description", "maximum", "minimum", "title", "type"],
            pointer,
        )?;
        let min = integer_member(object, "minimum", pointer)?;
        let max = integer_member(object, "maximum", pointer)?;
        if min > max {
            return Err(SchemaError::new(SchemaErrorCode::InvalidBound, pointer));
        }
        Ok(Descriptor::Int { min, max })
    }

    fn lower_float(
        object: &[(String, StrictValue)],
        pointer: &str,
    ) -> Result<Descriptor, SchemaError> {
        exact_keys(
            object,
            &["description", "maximum", "minimum", "title", "type"],
            pointer,
        )?;
        let min = optional_float_member(object, "minimum", pointer)?;
        let max = optional_float_member(object, "maximum", pointer)?;
        if min.zip(max).is_some_and(|(min, max)| min > max) {
            return Err(SchemaError::new(SchemaErrorCode::InvalidBound, pointer));
        }
        Ok(Descriptor::Float { min, max })
    }

    fn lower_string(
        &mut self,
        object: &[(String, StrictValue)],
        pointer: &str,
    ) -> Result<Descriptor, SchemaError> {
        exact_keys(
            object,
            &[
                "description",
                "enum",
                "maxLength",
                "minLength",
                "title",
                "type",
            ],
            pointer,
        )?;
        let min = optional_bound_member(object, "minLength", self.limits, pointer)?;
        let max = optional_bound_member(object, "maxLength", self.limits, pointer)?;
        if min.zip(max).is_some_and(|(min, max)| min > max) {
            return Err(SchemaError::new(SchemaErrorCode::InvalidBound, pointer));
        }
        let enumeration = if let Some(raw) = member(object, "enum") {
            let StrictValue::Array(values) = raw else {
                return Err(SchemaError::new(
                    SchemaErrorCode::InvalidForm,
                    join(pointer, "enum"),
                ));
            };
            if values.is_empty() {
                return Err(SchemaError::new(
                    SchemaErrorCode::InvalidForm,
                    join(pointer, "enum"),
                ));
            }
            let mut strings = Vec::with_capacity(values.len());
            for value in values {
                let StrictValue::String(value) = value else {
                    return Err(SchemaError::new(
                        SchemaErrorCode::InvalidForm,
                        join(pointer, "enum"),
                    ));
                };
                strings.push(value.clone());
            }
            ensure_sorted_strings(&strings).map_err(|()| {
                SchemaError::new(SchemaErrorCode::UnsortedSet, join(pointer, "enum"))
            })?;
            self.enum_strings += strings.len();
            if self.enum_strings > self.limits.enum_strings {
                return Err(SchemaError::new(
                    SchemaErrorCode::Limit,
                    join(pointer, "enum"),
                ));
            }
            strings
        } else {
            Vec::new()
        };
        Ok(Descriptor::String {
            min,
            max,
            enumeration,
        })
    }

    fn lower_array(
        &mut self,
        object: &[(String, StrictValue)],
        depth: usize,
        pointer: &str,
    ) -> Result<Descriptor, SchemaError> {
        if member(object, "prefixItems").is_some() {
            exact_keys(
                object,
                &[
                    "description",
                    "items",
                    "maxItems",
                    "minItems",
                    "prefixItems",
                    "title",
                    "type",
                ],
                pointer,
            )?;
            if member(object, "items") != Some(&StrictValue::Bool(false)) {
                return Err(SchemaError::new(
                    SchemaErrorCode::InvalidForm,
                    join(pointer, "items"),
                ));
            }
            let StrictValue::Array(raw_items) = required_member(object, "prefixItems", pointer)?
            else {
                return Err(SchemaError::new(
                    SchemaErrorCode::InvalidForm,
                    join(pointer, "prefixItems"),
                ));
            };
            if raw_items.is_empty() || raw_items.len() > self.limits.bound {
                return Err(SchemaError::new(
                    SchemaErrorCode::Limit,
                    join(pointer, "prefixItems"),
                ));
            }
            let min = bound_member(object, "minItems", self.limits, pointer)?;
            let max = bound_member(object, "maxItems", self.limits, pointer)?;
            if min != raw_items.len() || max != raw_items.len() {
                return Err(SchemaError::new(SchemaErrorCode::InvalidBound, pointer));
            }
            let mut items = Vec::with_capacity(raw_items.len());
            for (index, item) in raw_items.iter().enumerate() {
                items.push(self.lower(
                    item,
                    depth + 1,
                    &join(&join(pointer, "items"), &index.to_string()),
                )?);
            }
            Ok(Descriptor::Tuple { items })
        } else {
            exact_keys(
                object,
                &[
                    "description",
                    "items",
                    "maxItems",
                    "minItems",
                    "title",
                    "type",
                ],
                pointer,
            )?;
            let min = bound_member(object, "minItems", self.limits, pointer)?;
            let max = bound_member(object, "maxItems", self.limits, pointer)?;
            if min > max {
                return Err(SchemaError::new(SchemaErrorCode::InvalidBound, pointer));
            }
            let items = self.lower(
                required_member(object, "items", pointer)?,
                depth + 1,
                &join(pointer, "items"),
            )?;
            Ok(Descriptor::List {
                min,
                max,
                items: Box::new(items),
            })
        }
    }

    fn lower_record_or_map(
        &mut self,
        object: &[(String, StrictValue)],
        depth: usize,
        pointer: &str,
    ) -> Result<Descriptor, SchemaError> {
        exact_keys(
            object,
            &[
                "additionalProperties",
                "description",
                "properties",
                "required",
                "title",
                "type",
            ],
            pointer,
        )?;
        let StrictValue::Object(properties) = required_member(object, "properties", pointer)?
        else {
            return Err(SchemaError::new(
                SchemaErrorCode::InvalidForm,
                join(pointer, "properties"),
            ));
        };
        self.properties += properties.len();
        if self.properties > self.limits.properties {
            return Err(SchemaError::new(
                SchemaErrorCode::Limit,
                join(pointer, "properties"),
            ));
        }
        let required = string_array(
            required_member(object, "required", pointer)?,
            &join(pointer, "required"),
        )?;
        ensure_sorted_strings(&required).map_err(|()| {
            SchemaError::new(SchemaErrorCode::UnsortedSet, join(pointer, "required"))
        })?;
        match required_member(object, "additionalProperties", pointer)? {
            StrictValue::Bool(false) => {
                let mut names: Vec<_> = properties.iter().map(|(name, _)| name.clone()).collect();
                names.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
                if required != names {
                    return Err(SchemaError::new(
                        SchemaErrorCode::InvalidForm,
                        join(pointer, "required"),
                    ));
                }
                let mut by_name: Vec<_> = properties.iter().collect();
                by_name.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                let mut fields = Vec::with_capacity(by_name.len());
                for (name, raw) in by_name {
                    check_name(name, self.limits, &join(pointer, "properties"))?;
                    fields.push(Field {
                        name: name.clone(),
                        schema: self.lower(
                            raw,
                            depth + 1,
                            &join(&join(pointer, "fields"), name),
                        )?,
                    });
                }
                Ok(Descriptor::Record { fields })
            }
            raw_schema if properties.is_empty() && required.is_empty() => {
                let values = self.lower(raw_schema, depth + 1, &join(pointer, "values"))?;
                Ok(Descriptor::StringMap {
                    values: Box::new(values),
                })
            }
            _ => Err(SchemaError::new(
                SchemaErrorCode::InvalidForm,
                join(pointer, "additionalProperties"),
            )),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn lower_union(
        &mut self,
        object: &[(String, StrictValue)],
        depth: usize,
        pointer: &str,
    ) -> Result<Descriptor, SchemaError> {
        exact_keys(object, &["description", "oneOf", "title"], pointer)?;
        let StrictValue::Array(branches) = required_member(object, "oneOf", pointer)? else {
            return Err(SchemaError::new(
                SchemaErrorCode::InvalidForm,
                join(pointer, "oneOf"),
            ));
        };
        if !(2..=self.limits.union_branches.min(64)).contains(&branches.len()) {
            return Err(SchemaError::new(
                SchemaErrorCode::Limit,
                join(pointer, "oneOf"),
            ));
        }
        let mut variants = Vec::with_capacity(branches.len());
        for (index, branch) in branches.iter().enumerate() {
            let branch_pointer = join(&join(pointer, "variants"), &index.to_string());
            self.nodes = self
                .nodes
                .checked_add(2)
                .ok_or_else(|| SchemaError::new(SchemaErrorCode::Limit, &branch_pointer))?;
            if self.nodes > self.limits.nodes || depth + 2 > self.limits.depth {
                return Err(SchemaError::new(SchemaErrorCode::Limit, &branch_pointer));
            }
            let StrictValue::Object(branch) = branch else {
                return Err(SchemaError::new(
                    SchemaErrorCode::InvalidForm,
                    &branch_pointer,
                ));
            };
            exact_keys(
                branch,
                &[
                    "additionalProperties",
                    "description",
                    "properties",
                    "required",
                    "title",
                    "type",
                ],
                &branch_pointer,
            )?;
            if string_member(branch, "type", &branch_pointer)? != "object"
                || member(branch, "additionalProperties") != Some(&StrictValue::Bool(false))
            {
                return Err(SchemaError::new(
                    SchemaErrorCode::InvalidForm,
                    &branch_pointer,
                ));
            }
            let StrictValue::Object(properties) =
                required_member(branch, "properties", &branch_pointer)?
            else {
                return Err(SchemaError::new(
                    SchemaErrorCode::InvalidForm,
                    &branch_pointer,
                ));
            };
            self.properties += properties.len();
            if self.properties > self.limits.properties {
                return Err(SchemaError::new(SchemaErrorCode::Limit, &branch_pointer));
            }
            let required = string_array(
                required_member(branch, "required", &branch_pointer)?,
                &join(&branch_pointer, "required"),
            )?;
            ensure_sorted_strings(&required).map_err(|()| {
                SchemaError::new(
                    SchemaErrorCode::UnsortedSet,
                    join(&branch_pointer, "required"),
                )
            })?;
            let mut names: Vec<_> = properties.iter().map(|(name, _)| name.clone()).collect();
            names.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
            if required != names || !names.iter().any(|name| name == "tag") {
                return Err(SchemaError::new(
                    SchemaErrorCode::InvalidForm,
                    &branch_pointer,
                ));
            }
            let tag_schema = properties
                .iter()
                .find(|(name, _)| name == "tag")
                .map(|(_, value)| value)
                .ok_or_else(|| SchemaError::new(SchemaErrorCode::InvalidForm, &branch_pointer))?;
            let tag = single_tag(tag_schema, self.limits, &join(&branch_pointer, "tag"))?;
            self.enum_strings += 1;
            if self.enum_strings > self.limits.enum_strings {
                return Err(SchemaError::new(SchemaErrorCode::Limit, &branch_pointer));
            }
            let mut fields = Vec::new();
            let mut sorted: Vec<_> = properties
                .iter()
                .filter(|(name, _)| name != "tag")
                .collect();
            sorted.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
            for (name, raw) in sorted {
                check_name(name, self.limits, &branch_pointer)?;
                fields.push(Field {
                    name: name.clone(),
                    schema: self.lower(
                        raw,
                        depth + 2,
                        &join(&join(&branch_pointer, "fields"), name),
                    )?,
                });
            }
            variants.push(Variant { tag, fields });
        }
        variants.sort_unstable_by(|left, right| left.tag.as_bytes().cmp(right.tag.as_bytes()));
        if variants.windows(2).any(|pair| pair[0].tag == pair[1].tag) {
            return Err(SchemaError::new(
                SchemaErrorCode::InvalidForm,
                join(pointer, "oneOf"),
            ));
        }
        Ok(Descriptor::TaggedUnion { variants })
    }
}

fn exact_keys(
    object: &[(String, StrictValue)],
    allowed: &[&str],
    pointer: &str,
) -> Result<(), SchemaError> {
    for (key, _) in object {
        if !allowed.contains(&key.as_str()) {
            return Err(SchemaError::new(
                SchemaErrorCode::UnsupportedKeyword,
                join(pointer, key),
            ));
        }
    }
    Ok(())
}

fn member<'a>(object: &'a [(String, StrictValue)], key: &str) -> Option<&'a StrictValue> {
    object
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}
fn required_member<'a>(
    object: &'a [(String, StrictValue)],
    key: &str,
    pointer: &str,
) -> Result<&'a StrictValue, SchemaError> {
    member(object, key)
        .ok_or_else(|| SchemaError::new(SchemaErrorCode::InvalidForm, join(pointer, key)))
}
fn string_member<'a>(
    object: &'a [(String, StrictValue)],
    key: &str,
    pointer: &str,
) -> Result<&'a str, SchemaError> {
    match required_member(object, key, pointer)? {
        StrictValue::String(value) => Ok(value),
        _ => Err(SchemaError::new(
            SchemaErrorCode::InvalidForm,
            join(pointer, key),
        )),
    }
}
fn integer_member(
    object: &[(String, StrictValue)],
    key: &str,
    pointer: &str,
) -> Result<i64, SchemaError> {
    match required_member(object, key, pointer)? {
        StrictValue::Number(value) => value
            .as_i64()
            .ok_or_else(|| SchemaError::new(SchemaErrorCode::InvalidBound, join(pointer, key))),
        _ => Err(SchemaError::new(
            SchemaErrorCode::InvalidBound,
            join(pointer, key),
        )),
    }
}
fn float_value(value: &StrictValue) -> Option<f64> {
    match value {
        StrictValue::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        _ => None,
    }
}
fn optional_float_member(
    object: &[(String, StrictValue)],
    key: &str,
    pointer: &str,
) -> Result<Option<f64>, SchemaError> {
    member(object, key)
        .map(|value| {
            float_value(value)
                .ok_or_else(|| SchemaError::new(SchemaErrorCode::InvalidBound, join(pointer, key)))
        })
        .transpose()
}
fn bound_value(
    value: &StrictValue,
    limits: &SchemaLimits,
    pointer: &str,
) -> Result<usize, SchemaError> {
    let StrictValue::Number(number) = value else {
        return Err(SchemaError::new(SchemaErrorCode::InvalidBound, pointer));
    };
    let bound = number
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| SchemaError::new(SchemaErrorCode::InvalidBound, pointer))?;
    if bound > limits.bound {
        Err(SchemaError::new(SchemaErrorCode::Limit, pointer))
    } else {
        Ok(bound)
    }
}
fn bound_member(
    object: &[(String, StrictValue)],
    key: &str,
    limits: &SchemaLimits,
    pointer: &str,
) -> Result<usize, SchemaError> {
    bound_value(
        required_member(object, key, pointer)?,
        limits,
        &join(pointer, key),
    )
}
fn optional_bound_member(
    object: &[(String, StrictValue)],
    key: &str,
    limits: &SchemaLimits,
    pointer: &str,
) -> Result<Option<usize>, SchemaError> {
    member(object, key)
        .map(|value| bound_value(value, limits, &join(pointer, key)))
        .transpose()
}
fn string_array(value: &StrictValue, pointer: &str) -> Result<Vec<String>, SchemaError> {
    let StrictValue::Array(values) = value else {
        return Err(SchemaError::new(SchemaErrorCode::InvalidForm, pointer));
    };
    values
        .iter()
        .map(|value| match value {
            StrictValue::String(value) => Ok(value.clone()),
            _ => Err(SchemaError::new(SchemaErrorCode::InvalidForm, pointer)),
        })
        .collect()
}
fn ensure_sorted_strings(values: &[String]) -> Result<(), ()> {
    if values
        .windows(2)
        .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
    {
        Ok(())
    } else {
        Err(())
    }
}
fn ensure_sorted_names(values: &[(String, StrictValue)]) -> Result<(), ()> {
    if values
        .windows(2)
        .all(|pair| pair[0].0.as_bytes() < pair[1].0.as_bytes())
    {
        Ok(())
    } else {
        Err(())
    }
}
fn check_name(name: &str, limits: &SchemaLimits, pointer: &str) -> Result<(), SchemaError> {
    if name.len() > limits.name_bytes {
        Err(SchemaError::new(SchemaErrorCode::Limit, pointer))
    } else {
        Ok(())
    }
}
fn single_tag(
    value: &StrictValue,
    limits: &SchemaLimits,
    pointer: &str,
) -> Result<String, SchemaError> {
    let StrictValue::Object(object) = value else {
        return Err(SchemaError::new(SchemaErrorCode::InvalidForm, pointer));
    };
    exact_keys(
        object,
        &[
            "description",
            "enum",
            "maxLength",
            "minLength",
            "title",
            "type",
        ],
        pointer,
    )?;
    if string_member(object, "type", pointer)? != "string"
        || member(object, "minLength").is_some()
        || member(object, "maxLength").is_some()
    {
        return Err(SchemaError::new(SchemaErrorCode::InvalidForm, pointer));
    }
    let StrictValue::Array(values) = required_member(object, "enum", pointer)? else {
        return Err(SchemaError::new(SchemaErrorCode::InvalidForm, pointer));
    };
    if values.len() != 1 {
        return Err(SchemaError::new(SchemaErrorCode::InvalidForm, pointer));
    }
    let StrictValue::String(tag) = &values[0] else {
        return Err(SchemaError::new(SchemaErrorCode::InvalidForm, pointer));
    };
    check_name(tag, limits, pointer)?;
    Ok(tag.clone())
}
fn join(base: &str, token: &str) -> String {
    format!("{base}/{}", escape_pointer_token(token))
}
fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}
fn unescape_pointer_token(token: &str) -> Option<String> {
    let mut output = String::new();
    let mut chars = token.chars();
    while let Some(character) = chars.next() {
        if character == '~' {
            match chars.next()? {
                '0' => output.push('~'),
                '1' => output.push('/'),
                _ => return None,
            }
        } else {
            output.push(character);
        }
    }
    Some(output)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationCode {
    Type,
    MissingField,
    ExtraField,
    Minimum,
    Maximum,
    Length,
    Enum,
    Tag,
}
impl ValidationCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::MissingField => "missing_field",
            Self::ExtraField => "extra_field",
            Self::Minimum => "minimum",
            Self::Maximum => "maximum",
            Self::Length => "length",
            Self::Enum => "enum",
            Self::Tag => "tag",
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationIssue {
    pub pointer: String,
    pub code: ValidationCode,
}

#[allow(clippy::too_many_lines)]
fn validate_at(
    schema: &Descriptor,
    value: &serde_json::Value,
    pointer: &str,
    limits: &SchemaLimits,
    issues: &mut Vec<ValidationIssue>,
) {
    if issues.len() >= limits.validation_issues.max(1) {
        return;
    }
    match schema {
        Descriptor::Unit => {
            if !value.is_null() {
                issue(issues, limits, pointer, ValidationCode::Type);
            }
        }
        Descriptor::Bool => {
            if !value.is_boolean() {
                issue(issues, limits, pointer, ValidationCode::Type);
            }
        }
        Descriptor::Int { min, max } => match value.as_i64() {
            Some(number) => {
                if number < *min {
                    issue(issues, limits, pointer, ValidationCode::Minimum);
                }
                if number > *max {
                    issue(issues, limits, pointer, ValidationCode::Maximum);
                }
            }
            None => issue(issues, limits, pointer, ValidationCode::Type),
        },
        Descriptor::Float { min, max } => match value.as_f64().filter(|value| value.is_finite()) {
            Some(number) => {
                if min.is_some_and(|min| number < min) {
                    issue(issues, limits, pointer, ValidationCode::Minimum);
                }
                if max.is_some_and(|max| number > max) {
                    issue(issues, limits, pointer, ValidationCode::Maximum);
                }
            }
            None => issue(issues, limits, pointer, ValidationCode::Type),
        },
        Descriptor::String {
            min,
            max,
            enumeration,
        } => match value.as_str() {
            Some(string) => {
                let length = string.chars().count();
                if min.is_some_and(|min| length < min) || max.is_some_and(|max| length > max) {
                    issue(issues, limits, pointer, ValidationCode::Length);
                }
                if !enumeration.is_empty()
                    && enumeration
                        .binary_search_by(|entry| entry.as_bytes().cmp(string.as_bytes()))
                        .is_err()
                {
                    issue(issues, limits, pointer, ValidationCode::Enum);
                }
            }
            None => issue(issues, limits, pointer, ValidationCode::Type),
        },
        Descriptor::List { min, max, items } => match value.as_array() {
            Some(values) => {
                if values.len() < *min || values.len() > *max {
                    issue(issues, limits, pointer, ValidationCode::Length);
                }
                for (index, value) in values.iter().enumerate() {
                    validate_at(
                        items,
                        value,
                        &join(pointer, &index.to_string()),
                        limits,
                        issues,
                    );
                }
            }
            None => issue(issues, limits, pointer, ValidationCode::Type),
        },
        Descriptor::Tuple { items } => match value.as_array() {
            Some(values) if values.len() == items.len() => {
                for (index, (schema, value)) in items.iter().zip(values).enumerate() {
                    validate_at(
                        schema,
                        value,
                        &join(pointer, &index.to_string()),
                        limits,
                        issues,
                    );
                }
            }
            Some(_) => issue(issues, limits, pointer, ValidationCode::Length),
            None => issue(issues, limits, pointer, ValidationCode::Type),
        },
        Descriptor::Record { fields } => match value.as_object() {
            Some(object) => {
                for field in fields {
                    match object.get(&field.name) {
                        Some(value) => validate_at(
                            &field.schema,
                            value,
                            &join(pointer, &field.name),
                            limits,
                            issues,
                        ),
                        None => issue(
                            issues,
                            limits,
                            &join(pointer, &field.name),
                            ValidationCode::MissingField,
                        ),
                    }
                }
                for key in object.keys() {
                    if fields
                        .binary_search_by(|field| field.name.as_bytes().cmp(key.as_bytes()))
                        .is_err()
                    {
                        issue(
                            issues,
                            limits,
                            &join(pointer, key),
                            ValidationCode::ExtraField,
                        );
                    }
                }
            }
            None => issue(issues, limits, pointer, ValidationCode::Type),
        },
        Descriptor::StringMap { values } => match value.as_object() {
            Some(object) => {
                for (key, value) in object {
                    validate_at(values, value, &join(pointer, key), limits, issues);
                }
            }
            None => issue(issues, limits, pointer, ValidationCode::Type),
        },
        Descriptor::TaggedUnion { variants } => match value.as_object() {
            Some(object) => match object
                .get("tag")
                .and_then(serde_json::Value::as_str)
                .and_then(|tag| variants.iter().find(|variant| variant.tag == tag))
            {
                Some(variant) => {
                    for field in &variant.fields {
                        match object.get(&field.name) {
                            Some(value) => validate_at(
                                &field.schema,
                                value,
                                &join(pointer, &field.name),
                                limits,
                                issues,
                            ),
                            None => issue(
                                issues,
                                limits,
                                &join(pointer, &field.name),
                                ValidationCode::MissingField,
                            ),
                        }
                    }
                    for key in object.keys() {
                        if key != "tag" && !variant.fields.iter().any(|field| field.name == *key) {
                            issue(
                                issues,
                                limits,
                                &join(pointer, key),
                                ValidationCode::ExtraField,
                            );
                        }
                    }
                }
                None => issue(issues, limits, &join(pointer, "tag"), ValidationCode::Tag),
            },
            None => issue(issues, limits, pointer, ValidationCode::Type),
        },
    }
}
fn issue(
    issues: &mut Vec<ValidationIssue>,
    limits: &SchemaLimits,
    pointer: &str,
    code: ValidationCode,
) {
    if issues.len() < limits.validation_issues.max(1) {
        let mut pointer = pointer.to_owned();
        if pointer.len() > limits.issue_pointer_bytes {
            pointer.truncate(floor_char_boundary(&pointer, limits.issue_pointer_bytes));
        }
        issues.push(ValidationIssue { pointer, code });
    }
}
fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> ToolSchema {
        ToolSchema::parse(input, &SchemaLimits::default()).unwrap()
    }

    #[test]
    fn annotations_and_references_lower_to_one_golden_descriptor() {
        let direct = parse(
            r#"{"type":"object","properties":{"a":{"type":"string"}},"required":["a"],"additionalProperties":false}"#,
        );
        let referenced = parse(
            r##"{"$schema":"https://json-schema.org/draft/2020-12/schema","$defs":{"text":{"description":"ignored","type":"string"}},"title":"ignored","type":"object","properties":{"a":{"$ref":"#/$defs/text"}},"required":["a"],"additionalProperties":false}"##,
        );
        assert_eq!(direct.canonical_bytes(), referenced.canonical_bytes());
        assert_eq!(direct.digest(), referenced.digest());
        assert_eq!(
            std::str::from_utf8(direct.canonical_bytes()).unwrap(),
            r#"{"fields":[{"name":"a","schema":{"enum":[],"kind":"string","max":null,"min":null}}],"kind":"record"}"#
        );
    }

    #[test]
    fn all_supported_aggregate_forms_lower_and_validate_exactly() {
        let schema = parse(
            r#"{"oneOf":[{"type":"object","properties":{"tag":{"type":"string","enum":["none"]}},"required":["tag"],"additionalProperties":false},{"type":"object","properties":{"tag":{"type":"string","enum":["some"]},"value":{"type":"array","prefixItems":[{"type":"integer","minimum":0,"maximum":9},{"type":"boolean"}],"items":false,"minItems":2,"maxItems":2}},"required":["tag","value"],"additionalProperties":false}]}"#,
        );
        assert!(
            schema
                .validate(
                    &serde_json::json!({"tag":"some","value":[3,true]}),
                    &SchemaLimits::default()
                )
                .is_ok()
        );
        let issues = schema
            .validate(
                &serde_json::json!({"tag":"some","value":[10,true],"extra":1}),
                &SchemaLimits::default(),
            )
            .unwrap_err();
        assert_eq!(
            issues.iter().map(|issue| issue.code).collect::<Vec<_>>(),
            vec![ValidationCode::Maximum, ValidationCode::ExtraField]
        );
    }

    #[test]
    fn unsupported_optional_open_unsorted_cyclic_and_unused_forms_fail() {
        let invalid = [
            r#"{"type":"string","format":"uri"}"#,
            r#"{"type":"integer","minimum":0}"#,
            r#"{"type":"object","properties":{"a":{"type":"boolean"}},"required":[],"additionalProperties":false}"#,
            r#"{"type":"string","enum":["b","a"]}"#,
            r##"{"$defs":{"a":{"type":"boolean"}},"$ref":"#/$defs/a","description":"sibling"}"##,
            r##"{"$defs":{"a":{"$ref":"#/$defs/a"}},"$ref":"#/$defs/a"}"##,
            r#"{"$defs":{"a":{"type":"boolean"}},"type":"boolean"}"#,
        ];
        for value in invalid {
            assert!(
                ToolSchema::parse(value, &SchemaLimits::default()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn duplicate_keys_and_lowered_limits_fail() {
        assert_eq!(
            ToolSchema::parse(
                r#"{"type":"boolean","type":"null"}"#,
                &SchemaLimits::default()
            )
            .unwrap_err()
            .code,
            SchemaErrorCode::DuplicateKey
        );
        let limits = SchemaLimits {
            depth: 1,
            ..SchemaLimits::default()
        };
        assert_eq!(
            ToolSchema::parse(
                r#"{"type":"array","items":{"type":"boolean"},"minItems":0,"maxItems":1}"#,
                &limits
            )
            .unwrap_err()
            .code,
            SchemaErrorCode::Limit
        );
    }

    #[test]
    fn every_frozen_schema_limit_has_a_direct_rejection_vector() {
        let boolean = r#"{"type":"boolean"}"#;
        let cases = vec![
            (
                "document bytes",
                boolean,
                SchemaLimits {
                    schema_bytes: boolean.len() - 1,
                    ..SchemaLimits::default()
                },
            ),
            (
                "nodes",
                boolean,
                SchemaLimits {
                    nodes: 0,
                    ..SchemaLimits::default()
                },
            ),
            (
                "depth",
                r#"{"type":"array","items":{"type":"boolean"},"minItems":0,"maxItems":1}"#,
                SchemaLimits {
                    depth: 1,
                    ..SchemaLimits::default()
                },
            ),
            (
                "properties",
                r#"{"type":"object","properties":{"a":{"type":"boolean"}},"required":["a"],"additionalProperties":false}"#,
                SchemaLimits {
                    properties: 0,
                    ..SchemaLimits::default()
                },
            ),
            (
                "definitions",
                r##"{"$defs":{"a":{"type":"boolean"}},"$ref":"#/$defs/a"}"##,
                SchemaLimits {
                    definitions: 0,
                    ..SchemaLimits::default()
                },
            ),
            (
                "enum strings",
                r#"{"type":"string","enum":["a"]}"#,
                SchemaLimits {
                    enum_strings: 0,
                    ..SchemaLimits::default()
                },
            ),
            (
                "union branches",
                r#"{"oneOf":[{"type":"object","properties":{"tag":{"type":"string","enum":["a"]}},"required":["tag"],"additionalProperties":false},{"type":"object","properties":{"tag":{"type":"string","enum":["b"]}},"required":["tag"],"additionalProperties":false}]}"#,
                SchemaLimits {
                    union_branches: 1,
                    ..SchemaLimits::default()
                },
            ),
            (
                "name bytes",
                r#"{"type":"object","properties":{"aa":{"type":"boolean"}},"required":["aa"],"additionalProperties":false}"#,
                SchemaLimits {
                    name_bytes: 1,
                    ..SchemaLimits::default()
                },
            ),
            (
                "collection bound",
                r#"{"type":"array","items":{"type":"boolean"},"minItems":0,"maxItems":2}"#,
                SchemaLimits {
                    bound: 1,
                    ..SchemaLimits::default()
                },
            ),
            (
                "string bound",
                r#"{"type":"string","minLength":0,"maxLength":2}"#,
                SchemaLimits {
                    bound: 1,
                    ..SchemaLimits::default()
                },
            ),
        ];
        for (name, source, limits) in cases {
            assert_eq!(
                ToolSchema::parse(source, &limits).unwrap_err().code,
                SchemaErrorCode::Limit,
                "{name}",
            );
        }
    }

    #[test]
    fn validation_limits_and_exact_boundary_failures_are_bounded() {
        let bounded = Descriptor::Record {
            fields: vec![
                Field {
                    name: "aaa".to_owned(),
                    schema: Descriptor::Bool,
                },
                Field {
                    name: "bbb".to_owned(),
                    schema: Descriptor::Bool,
                },
                Field {
                    name: "éé".to_owned(),
                    schema: Descriptor::Bool,
                },
            ],
        };
        let issues = bounded
            .validate(
                &serde_json::json!({}),
                &SchemaLimits {
                    validation_issues: 2,
                    issue_pointer_bytes: 3,
                    ..SchemaLimits::default()
                },
            )
            .unwrap_err();
        assert_eq!(issues.len(), 2);
        assert!(issues.iter().all(|issue| {
            issue.pointer.len() <= 3 && issue.pointer.is_char_boundary(issue.pointer.len())
        }));

        let integer = Descriptor::Int { min: 0, max: 9 };
        let record = Descriptor::Record {
            fields: vec![Field {
                name: "count".to_owned(),
                schema: integer,
            }],
        };
        let union = Descriptor::TaggedUnion {
            variants: vec![
                Variant {
                    tag: "a".to_owned(),
                    fields: vec![],
                },
                Variant {
                    tag: "b".to_owned(),
                    fields: vec![],
                },
            ],
        };
        let cases = vec![
            (
                "wrong tag",
                union,
                serde_json::json!({"tag":"c"}),
                ValidationCode::Tag,
            ),
            (
                "extra field",
                record.clone(),
                serde_json::json!({"count":1,"extra":true}),
                ValidationCode::ExtraField,
            ),
            (
                "missing field",
                record.clone(),
                serde_json::json!({}),
                ValidationCode::MissingField,
            ),
            (
                "coercion",
                record.clone(),
                serde_json::json!({"count":"1"}),
                ValidationCode::Type,
            ),
            (
                "out of range",
                record,
                serde_json::json!({"count":10}),
                ValidationCode::Maximum,
            ),
        ];
        for (name, schema, value, expected) in cases {
            let issues = schema
                .validate(&value, &SchemaLimits::default())
                .unwrap_err();
            assert_eq!(issues[0].code, expected, "{name}");
        }
    }

    #[test]
    fn every_primitive_and_map_has_the_exact_shape() {
        let cases = [
            (r#"{"type":"null"}"#, r#"{"kind":"void"}"#),
            (r#"{"type":"boolean"}"#, r#"{"kind":"bool"}"#),
            (
                r#"{"type":"integer","minimum":-2,"maximum":3}"#,
                r#"{"kind":"int","max":3,"min":-2}"#,
            ),
            (
                r#"{"type":"number"}"#,
                r#"{"kind":"float","max":null,"min":null}"#,
            ),
            (
                r#"{"type":"array","items":{"type":"boolean"},"minItems":0,"maxItems":2}"#,
                r#"{"items":{"kind":"bool"},"kind":"list","max":2,"min":0}"#,
            ),
            (
                r#"{"type":"object","properties":{},"required":[],"additionalProperties":{"type":"boolean"}}"#,
                r#"{"kind":"string_map","values":{"kind":"bool"}}"#,
            ),
        ];
        for (source, expected) in cases {
            assert_eq!(
                std::str::from_utf8(parse(source).canonical_bytes()).unwrap(),
                expected
            );
        }
        assert_eq!(crate::SCHEMA_PROFILE, "allen.tool-schema/0.1");
    }
}
