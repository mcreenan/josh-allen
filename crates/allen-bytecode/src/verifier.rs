//! Structural, type, control-flow, and ownership verification.

use crate::model::{
    CANONICAL_NAN_BITS, CapabilityOperation, CheckedIntOperation, CompareOp, Constant, Conversion,
    EffectOperation, EffectSetId, EnumPayloadType, Function, Instruction, MAX_VALUE_NESTING,
    Module, RecordField, Register, SafeCollectionOperation, StringOperation,
    ToolVerificationContract, ValueType, agent_error_type, effect_result_type,
    external_directory_request_type, external_file_request_type, file_error_type, is_nan_bits,
    model_error_type, prompt_output_type, sub_agent_error_type, sub_agent_projection_type,
    task_snapshot_type, tool_declared_error_type, transcript_part_enum_id, transcript_query_type,
    user_error_type,
};
use std::collections::BTreeSet;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifyError {
    pub function: Option<u32>,
    pub instruction: Option<u32>,
    pub message: String,
}

impl VerifyError {
    fn module(message: impl Into<String>) -> Self {
        Self {
            function: None,
            instruction: None,
            message: message.into(),
        }
    }

    fn instruction(function: usize, instruction: usize, message: impl Into<String>) -> Self {
        Self {
            function: Some(u32::try_from(function).unwrap_or(u32::MAX)),
            instruction: Some(u32::try_from(instruction).unwrap_or(u32::MAX)),
            message: message.into(),
        }
    }
}

impl fmt::Display for VerifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.function, self.instruction) {
            (Some(function), Some(instruction)) => write!(
                formatter,
                "bytecode verification failed in function {function} at instruction {instruction}: {}",
                self.message
            ),
            _ => write!(formatter, "bytecode verification failed: {}", self.message),
        }
    }
}

impl std::error::Error for VerifyError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedModule(Module);

impl VerifiedModule {
    #[must_use]
    pub fn module(&self) -> &Module {
        &self.0
    }

    #[must_use]
    pub fn entry_function(&self) -> &Function {
        &self.0.functions[self.0.entry as usize]
    }
}

/// Verify all bytecode invariants required before a VM can execute a module.
///
/// # Errors
///
/// Returns a bounded location and deterministic message when the module is invalid.
pub fn verify(module: Module) -> Result<VerifiedModule, VerifyError> {
    verify_internal(module, None)
}

/// Verify current-version bytecode against a trusted, frozen tool catalog.
///
/// The catalog order defines the valid indexes for [`Instruction::ToolInvoke`].
/// Unlike [`verify`], this entry point can accept tool invocation because it has
/// the exact input, output, and declared-error types required for
/// independent verification. It does not validate artifact manifests or
/// digests; use `decode_and_verify` for encoded artifacts.
///
/// # Errors
///
/// Returns a bounded location and deterministic message when the module or any
/// tool invocation does not match the frozen catalog.
pub fn verify_with_frozen_tool_catalog(
    module: Module,
    tool_contracts: &[ToolVerificationContract],
) -> Result<VerifiedModule, VerifyError> {
    verify_internal(module, Some(tool_contracts))
}

pub(crate) fn verify_internal(
    module: Module,
    tool_contracts: Option<&[ToolVerificationContract]>,
) -> Result<VerifiedModule, VerifyError> {
    verify_constants(&module.constants)?;
    verify_effect_sets(&module.effect_sets)?;
    verify_enum_types(&module)?;

    if module.functions.len() > u32::MAX as usize {
        return Err(VerifyError::module("function table is too large"));
    }
    if !module
        .async_functions
        .windows(2)
        .all(|pair| pair[0] < pair[1])
        || module
            .async_functions
            .last()
            .is_some_and(|id| *id as usize >= module.functions.len())
    {
        return Err(VerifyError::module(
            "async function IDs must be unique, sorted, and in range",
        ));
    }

    let entry = usize::try_from(module.entry)
        .ok()
        .filter(|entry| *entry < module.functions.len())
        .ok_or_else(|| VerifyError::module("entry function is out of range"))?;

    if !module.functions[entry].captures.is_empty() {
        return Err(VerifyError::module("entry function cannot have captures"));
    }
    if is_affine_type(&module.functions[entry].return_type) {
        return Err(VerifyError::module(
            "entry function cannot return Future or Task",
        ));
    }

    let mut function_names = BTreeSet::new();
    for (function_index, function) in module.functions.iter().enumerate() {
        if !is_canonical_function_symbol(&function.name) {
            return Err(VerifyError::module("function symbol is not canonical"));
        }
        if !function_names.insert(function.name.as_str()) {
            return Err(VerifyError::module("function symbols must be unique"));
        }
        verify_function_layout(&module, function)?;
        verify_cfg_function(&module, function_index, function, tool_contracts)?;
    }

    Ok(VerifiedModule(module))
}

fn is_canonical_function_symbol(symbol: &str) -> bool {
    if is_source_identifier(symbol) {
        return true;
    }
    let Some((module, declaration)) = symbol.rsplit_once("::") else {
        return false;
    };
    if !is_normalized_module_path(module) {
        return false;
    }
    let base = declaration
        .split_once('<')
        .map_or(declaration, |(base, _)| base);
    let base_is_identifier = is_source_identifier(base);
    let closure_is_canonical = base.strip_prefix("$closure@").is_some_and(|offset| {
        !offset.is_empty() && offset.bytes().all(|byte| byte.is_ascii_digit())
    });
    (base_is_identifier || closure_is_canonical)
        && declaration
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        && !declaration.contains('/')
        && !declaration.contains('\\')
}

fn is_normalized_module_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path.as_bytes().ends_with(b".allen")
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn is_source_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn verify_effect_sets(effect_sets: &[Vec<String>]) -> Result<(), VerifyError> {
    if effect_sets.len() > u32::MAX as usize {
        return Err(VerifyError::module("effect set table is too large"));
    }
    let mut previous_set: Option<&[String]> = None;
    for effect_set in effect_sets {
        if previous_set.is_some_and(|previous| previous >= effect_set.as_slice()) {
            return Err(VerifyError::module(
                "effect sets must be unique and sorted lexicographically",
            ));
        }
        let mut previous_effect: Option<&[u8]> = None;
        for effect in effect_set {
            if !is_canonical_effect_id(effect) {
                return Err(VerifyError::module("effect ID is not canonical"));
            }
            if previous_effect.is_some_and(|previous| previous >= effect.as_bytes()) {
                return Err(VerifyError::module(
                    "effects must be unique and sorted by UTF-8 bytes",
                ));
            }
            previous_effect = Some(effect.as_bytes());
        }
        previous_set = Some(effect_set);
    }
    Ok(())
}

pub(crate) fn is_canonical_effect_id(effect: &str) -> bool {
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
    name.split('.').all(|segment| {
        let mut bytes = segment.bytes();
        bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    })
}

fn verify_function_layout(module: &Module, function: &Function) -> Result<(), VerifyError> {
    if function.name.is_empty() {
        return Err(VerifyError::module("function name must be nonempty"));
    }
    if function.registers.len() > Register::MAX as usize + 1 {
        return Err(VerifyError::module("function register table is too large"));
    }
    if function.effects as usize >= module.effect_sets.len() {
        return Err(VerifyError::module(
            "function effect set ID is out of range",
        ));
    }
    let mut inputs = std::collections::BTreeSet::new();
    for register in function.parameters.iter().chain(&function.captures) {
        if !inputs.insert(*register) {
            return Err(VerifyError::module(
                "function parameter and capture registers must be distinct",
            ));
        }
        if *register as usize >= function.registers.len() {
            return Err(VerifyError::module(
                "function parameter or capture register is out of range",
            ));
        }
        if function.registers[*register as usize] == ValueType::Never {
            return Err(VerifyError::module(
                "function parameter or capture cannot have type Never",
            ));
        }
        if function.captures.contains(register)
            && is_affine_type(&function.registers[*register as usize])
        {
            return Err(VerifyError::module(
                "closure capture cannot have type Future or Task",
            ));
        }
    }
    for register_type in &function.registers {
        verify_module_value_type(module, register_type, 0).map_err(VerifyError::module)?;
    }
    verify_module_value_type(module, &function.return_type, 0).map_err(VerifyError::module)?;
    Ok(())
}

fn verify_enum_types(module: &Module) -> Result<(), VerifyError> {
    if module.enum_types.len() > u32::MAX as usize {
        return Err(VerifyError::module("enum type table is too large"));
    }

    let mut names = std::collections::BTreeSet::new();
    for enum_type in &module.enum_types {
        if enum_type.name.is_empty() || !names.insert(enum_type.name.as_str()) {
            return Err(VerifyError::module(
                "enum type names must be nonempty and unique",
            ));
        }
        if enum_type.variants.is_empty() || enum_type.variants.len() > u32::MAX as usize {
            return Err(VerifyError::module(
                "enum type must have a representable nonempty variant table",
            ));
        }
        let mut variant_names = std::collections::BTreeSet::new();
        for variant in &enum_type.variants {
            if variant.name.is_empty() || !variant_names.insert(variant.name.as_str()) {
                return Err(VerifyError::module(
                    "enum variant names must be nonempty and unique",
                ));
            }
            match &variant.payload {
                EnumPayloadType::Unit => {}
                EnumPayloadType::Tuple(elements) => {
                    if elements.is_empty() {
                        return Err(VerifyError::module(
                            "empty enum tuple payload must be payloadless",
                        ));
                    }
                    for element in elements {
                        if contains_affine_type(element) {
                            return Err(VerifyError::module(
                                "Future and Task cannot be stored in enum payloads",
                            ));
                        }
                        if contains_stored_sub_agent(element) {
                            return Err(VerifyError::module(
                                "SubAgent cannot be stored in enum payloads",
                            ));
                        }
                        verify_module_value_type(module, element, 0)
                            .map_err(VerifyError::module)?;
                    }
                }
                EnumPayloadType::Record(fields) => {
                    if fields
                        .iter()
                        .any(|field| contains_affine_type(&field.value_type))
                    {
                        return Err(VerifyError::module(
                            "Future and Task cannot be stored in enum payloads",
                        ));
                    }
                    if fields
                        .iter()
                        .any(|field| contains_stored_sub_agent(&field.value_type))
                    {
                        return Err(VerifyError::module(
                            "SubAgent cannot be stored in enum payloads",
                        ));
                    }
                    verify_record_layout(module, fields, 0).map_err(VerifyError::module)?;
                }
            }
        }
    }

    let mut marks = vec![0_u8; module.enum_types.len()];
    let mut nesting = vec![None; module.enum_types.len()];
    for enum_index in 0..module.enum_types.len() {
        verify_enum_nesting(module, enum_index, &mut marks, &mut nesting, 0)?;
    }
    verify_expanded_module_types(module, &nesting)?;
    Ok(())
}

fn verify_expanded_module_types(
    module: &Module,
    enum_nesting: &[Option<usize>],
) -> Result<(), VerifyError> {
    for enum_type in &module.enum_types {
        for variant in &enum_type.variants {
            match &variant.payload {
                EnumPayloadType::Unit => {}
                EnumPayloadType::Tuple(elements) => {
                    for element in elements {
                        verify_expanded_value_type(element, enum_nesting, 1)?;
                    }
                }
                EnumPayloadType::Record(fields) => {
                    for field in fields {
                        verify_expanded_value_type(&field.value_type, enum_nesting, 1)?;
                    }
                }
            }
        }
    }
    for function in &module.functions {
        for register in &function.registers {
            verify_expanded_value_type(register, enum_nesting, 0)?;
        }
        verify_expanded_value_type(&function.return_type, enum_nesting, 0)?;
        for instruction in &function.code {
            if let Instruction::Narrow { target, .. } = instruction {
                verify_expanded_value_type(target, enum_nesting, 0)?;
            }
        }
    }
    Ok(())
}

fn verify_expanded_value_type(
    value_type: &ValueType,
    enum_nesting: &[Option<usize>],
    depth: usize,
) -> Result<(), VerifyError> {
    if depth > MAX_VALUE_NESTING {
        return Err(VerifyError::module("value type nesting exceeds limit"));
    }
    match value_type {
        ValueType::Enum(id) => {
            let enum_depth = enum_nesting
                .get(*id as usize)
                .and_then(|depth| *depth)
                .ok_or_else(|| VerifyError::module("enum type ID is out of range"))?;
            if depth + enum_depth > MAX_VALUE_NESTING {
                return Err(VerifyError::module("value type nesting exceeds limit"));
            }
        }
        ValueType::List(element)
        | ValueType::Option(element)
        | ValueType::Future(element)
        | ValueType::Task(element) => {
            verify_expanded_value_type(element, enum_nesting, depth + 1)?;
        }
        ValueType::Map(key, value) | ValueType::Result(key, value) => {
            verify_expanded_value_type(key, enum_nesting, depth + 1)?;
            verify_expanded_value_type(value, enum_nesting, depth + 1)?;
        }
        ValueType::Tuple(elements) => {
            for element in elements {
                verify_expanded_value_type(element, enum_nesting, depth + 1)?;
            }
        }
        ValueType::Record(fields) => {
            for field in fields {
                verify_expanded_value_type(&field.value_type, enum_nesting, depth + 1)?;
            }
        }
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => {
            for parameter in parameters {
                verify_expanded_value_type(parameter, enum_nesting, depth + 1)?;
            }
            verify_expanded_value_type(return_type, enum_nesting, depth + 1)?;
        }
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
        | ValueType::Unknown => {}
    }
    Ok(())
}

fn verify_enum_nesting(
    module: &Module,
    enum_index: usize,
    marks: &mut [u8],
    nesting: &mut [Option<usize>],
    depth: usize,
) -> Result<usize, VerifyError> {
    if depth > MAX_VALUE_NESTING {
        return Err(VerifyError::module("enum type nesting exceeds limit"));
    }
    if let Some(nesting) = nesting[enum_index] {
        if depth + nesting > MAX_VALUE_NESTING {
            return Err(VerifyError::module("enum type nesting exceeds limit"));
        }
        return Ok(nesting);
    }
    match marks[enum_index] {
        1 => {
            return Err(VerifyError::module(
                "recursive enum types are not supported",
            ));
        }
        2 => unreachable!("completed enum nesting has a memoized value"),
        _ => {}
    }
    marks[enum_index] = 1;
    let mut maximum = 0;
    for variant in &module.enum_types[enum_index].variants {
        maximum = maximum.max(verify_payload_nesting(
            module,
            &variant.payload,
            marks,
            nesting,
            depth,
        )?);
    }
    marks[enum_index] = 2;
    nesting[enum_index] = Some(maximum);
    Ok(maximum)
}

fn verify_payload_nesting(
    module: &Module,
    payload: &EnumPayloadType,
    marks: &mut [u8],
    nesting: &mut [Option<usize>],
    depth: usize,
) -> Result<usize, VerifyError> {
    let values: Vec<&ValueType> = match payload {
        EnumPayloadType::Unit => return Ok(0),
        EnumPayloadType::Tuple(elements) => elements.iter().collect(),
        EnumPayloadType::Record(fields) => fields.iter().map(|field| &field.value_type).collect(),
    };
    let mut maximum = 0;
    for value_type in values {
        maximum = maximum
            .max(1 + verify_expanded_type_nesting(module, value_type, marks, nesting, depth + 1)?);
    }
    Ok(maximum)
}

fn verify_expanded_type_nesting(
    module: &Module,
    value_type: &ValueType,
    marks: &mut [u8],
    nesting: &mut [Option<usize>],
    depth: usize,
) -> Result<usize, VerifyError> {
    if depth > MAX_VALUE_NESTING {
        return Err(VerifyError::module("enum type nesting exceeds limit"));
    }
    match value_type {
        ValueType::Enum(id) => verify_enum_nesting(module, *id as usize, marks, nesting, depth),
        ValueType::List(element)
        | ValueType::Option(element)
        | ValueType::Future(element)
        | ValueType::Task(element) => {
            Ok(1 + verify_expanded_type_nesting(module, element, marks, nesting, depth + 1)?)
        }
        ValueType::Map(key, value) | ValueType::Result(key, value) => Ok(1
            + verify_expanded_type_nesting(module, key, marks, nesting, depth + 1)?.max(
                verify_expanded_type_nesting(module, value, marks, nesting, depth + 1)?,
            )),
        ValueType::Tuple(elements) => {
            let mut maximum = 0;
            for element in elements {
                maximum = maximum.max(
                    1 + verify_expanded_type_nesting(module, element, marks, nesting, depth + 1)?,
                );
            }
            Ok(maximum)
        }
        ValueType::Record(fields) => {
            let mut maximum = 0;
            for field in fields {
                maximum = maximum.max(
                    1 + verify_expanded_type_nesting(
                        module,
                        &field.value_type,
                        marks,
                        nesting,
                        depth + 1,
                    )?,
                );
            }
            Ok(maximum)
        }
        ValueType::Function {
            parameters,
            return_type,
            ..
        } => {
            let mut maximum =
                verify_expanded_type_nesting(module, return_type, marks, nesting, depth + 1)?;
            for parameter in parameters {
                maximum = maximum.max(verify_expanded_type_nesting(
                    module,
                    parameter,
                    marks,
                    nesting,
                    depth + 1,
                )?);
            }
            Ok(1 + maximum)
        }
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

#[allow(clippy::too_many_lines)]
fn verify_cfg_function(
    module: &Module,
    function_index: usize,
    function: &Function,
    tool_contracts: Option<&[ToolVerificationContract]>,
) -> Result<(), VerifyError> {
    verify_instruction_types(module, function_index, function, tool_contracts)?;
    let code_len = function.code.len();
    if code_len == 0 {
        return Err(error(
            function_index,
            0,
            "function has no reachable terminal instruction",
        ));
    }

    for (instruction_index, instruction) in function.code.iter().enumerate() {
        match instruction {
            Instruction::BranchBool {
                true_target,
                false_target,
                ..
            } => {
                validate_target(function_index, instruction_index, *true_target, code_len)?;
                validate_target(function_index, instruction_index, *false_target, code_len)?;
            }
            Instruction::SwitchEnum { arms, .. } => {
                for arm in arms {
                    validate_target(function_index, instruction_index, arm.target, code_len)?;
                }
            }
            Instruction::Jump { target } => {
                validate_target(function_index, instruction_index, *target, code_len)?;
            }
            _ => {}
        }
    }

    let mut initial_state = vec![false; function.registers.len()];
    for register in function.parameters.iter().chain(&function.captures) {
        initial_state[*register as usize] = true;
    }
    let mut entry_states = vec![None; code_len];
    entry_states[0] = Some(initial_state);
    let mut worklist = std::collections::VecDeque::from([0_usize]);

    while let Some(instruction_index) = worklist.pop_front() {
        let mut state = entry_states[instruction_index]
            .clone()
            .expect("worklist contains only reachable instructions");
        let instruction = &function.code[instruction_index];

        for source in instruction_sources(instruction) {
            if !state[source as usize] {
                return Err(error(
                    function_index,
                    instruction_index,
                    "register is not initialized",
                ));
            }
        }

        if let Some(destination) = instruction_destination(instruction) {
            state[destination as usize] = true;
        }

        match instruction {
            Instruction::Return { .. } | Instruction::Stop { .. } => {}
            Instruction::Jump { target } => {
                merge_edge(&mut entry_states, &mut worklist, *target as usize, state);
            }
            Instruction::BranchBool {
                true_target,
                false_target,
                ..
            } => {
                merge_edge(
                    &mut entry_states,
                    &mut worklist,
                    *true_target as usize,
                    state.clone(),
                );
                merge_edge(
                    &mut entry_states,
                    &mut worklist,
                    *false_target as usize,
                    state,
                );
            }
            Instruction::SwitchEnum { arms, .. } => {
                for arm in arms {
                    let mut edge_state = state.clone();
                    for binding in &arm.bindings {
                        edge_state[*binding as usize] = true;
                    }
                    merge_edge(
                        &mut entry_states,
                        &mut worklist,
                        arm.target as usize,
                        edge_state,
                    );
                }
            }
            _ => {
                let next = instruction_index + 1;
                if next == code_len {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "reachable path falls through the function",
                    ));
                }
                merge_edge(&mut entry_states, &mut worklist, next, state);
            }
        }
    }

    if let Some(unreachable) = entry_states.iter().position(Option::is_none) {
        return Err(error(
            function_index,
            unreachable,
            "instruction is unreachable",
        ));
    }
    verify_sub_agent_cfg(function_index, function, &entry_states)?;
    verify_structured_back_edges(function_index, function)?;
    verify_affine_cfg(module, function_index, function, &entry_states)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubAgentAvailability {
    Unavailable,
    Available { scope: u32, provenance: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SubAgentState {
    registers: Vec<SubAgentAvailability>,
    scopes: Vec<u32>,
}

#[allow(clippy::too_many_lines)]
fn verify_sub_agent_cfg(
    function_index: usize,
    function: &Function,
    definite_initialization: &[Option<Vec<bool>>],
) -> Result<(), VerifyError> {
    if !function.registers.iter().any(contains_stored_sub_agent) {
        return Ok(());
    }
    let mut registers = vec![SubAgentAvailability::Unavailable; function.registers.len()];
    for register in function.parameters.iter().chain(&function.captures) {
        if contains_stored_sub_agent(&function.registers[*register as usize]) {
            registers[*register as usize] = SubAgentAvailability::Available {
                scope: 0,
                provenance: u64::from(*register),
            };
        }
    }
    let mut entry_states = vec![None; function.code.len()];
    entry_states[0] = Some(SubAgentState {
        registers,
        scopes: Vec::new(),
    });
    let mut worklist = std::collections::VecDeque::from([0_usize]);

    while let Some(instruction_index) = worklist.pop_front() {
        let mut state = entry_states[instruction_index]
            .clone()
            .expect("SubAgent worklist contains reachable instructions");
        let instruction = &function.code[instruction_index];

        for source in instruction_sources(instruction) {
            if contains_stored_sub_agent(&function.registers[source as usize])
                && state.registers[source as usize] == SubAgentAvailability::Unavailable
            {
                return Err(error(
                    function_index,
                    instruction_index,
                    "SubAgent register is not available in this lexical scope",
                ));
            }
        }

        match instruction {
            Instruction::TaskScopeEnter { scope } => state.scopes.push(*scope),
            Instruction::TaskScopeExit { scope } => {
                if state.scopes.pop() != Some(*scope) {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "task scope exit does not match the current scope",
                    ));
                }
                for availability in &mut state.registers {
                    if matches!(availability, SubAgentAvailability::Available { scope: origin, .. } if origin == scope)
                    {
                        *availability = SubAgentAvailability::Unavailable;
                    }
                }
            }
            _ => {}
        }
        if let Some(destination) = instruction_destination(instruction) {
            if contains_stored_sub_agent(&function.registers[destination as usize]) {
                state.registers[destination as usize] = sub_agent_result_availability(
                    function_index,
                    instruction_index,
                    function,
                    instruction,
                    &state,
                )?;
            }
        }

        match instruction {
            Instruction::Return { .. } | Instruction::Stop { .. } => {}
            Instruction::Jump { target } => merge_sub_agent_edge(
                function_index,
                instruction_index,
                *target as usize,
                state,
                &mut entry_states,
                &mut worklist,
                (
                    &function.registers,
                    definite_initialization[*target as usize]
                        .as_deref()
                        .expect("SubAgent successor is reachable"),
                ),
            )?,
            Instruction::BranchBool {
                true_target,
                false_target,
                ..
            } => {
                merge_sub_agent_edge(
                    function_index,
                    instruction_index,
                    *true_target as usize,
                    state.clone(),
                    &mut entry_states,
                    &mut worklist,
                    (
                        &function.registers,
                        definite_initialization[*true_target as usize]
                            .as_deref()
                            .expect("SubAgent successor is reachable"),
                    ),
                )?;
                merge_sub_agent_edge(
                    function_index,
                    instruction_index,
                    *false_target as usize,
                    state,
                    &mut entry_states,
                    &mut worklist,
                    (
                        &function.registers,
                        definite_initialization[*false_target as usize]
                            .as_deref()
                            .expect("SubAgent successor is reachable"),
                    ),
                )?;
            }
            Instruction::SwitchEnum { source, arms } => {
                for arm in arms {
                    let mut edge_state = state.clone();
                    for binding in &arm.bindings {
                        if contains_stored_sub_agent(&function.registers[*binding as usize]) {
                            edge_state.registers[*binding as usize] =
                                edge_state.registers[*source as usize];
                        }
                    }
                    merge_sub_agent_edge(
                        function_index,
                        instruction_index,
                        arm.target as usize,
                        edge_state,
                        &mut entry_states,
                        &mut worklist,
                        (
                            &function.registers,
                            definite_initialization[arm.target as usize]
                                .as_deref()
                                .expect("SubAgent successor is reachable"),
                        ),
                    )?;
                }
            }
            _ => merge_sub_agent_edge(
                function_index,
                instruction_index,
                instruction_index + 1,
                state,
                &mut entry_states,
                &mut worklist,
                (
                    &function.registers,
                    definite_initialization[instruction_index + 1]
                        .as_deref()
                        .expect("SubAgent successor is reachable"),
                ),
            )?,
        }
    }
    Ok(())
}

fn sub_agent_result_availability(
    function_index: usize,
    instruction_index: usize,
    function: &Function,
    instruction: &Instruction,
    state: &SubAgentState,
) -> Result<SubAgentAvailability, VerifyError> {
    let current_scope = SubAgentAvailability::Available {
        scope: state.scopes.last().copied().unwrap_or(0),
        provenance: (1_u64 << 63) | instruction_index as u64,
    };
    match instruction {
        Instruction::Move { source, .. }
        | Instruction::Await { source, .. }
        | Instruction::TryResult { source, .. } => Ok(state.registers[*source as usize]),
        Instruction::Spawn { future, .. } => Ok(state.registers[*future as usize]),
        Instruction::DirectCall { arguments, .. } | Instruction::ClosureCall { arguments, .. } => {
            let mut sources = arguments
                .iter()
                .filter(|argument| {
                    contains_stored_sub_agent(&function.registers[**argument as usize])
                })
                .map(|argument| state.registers[*argument as usize]);
            let Some(first) = sources.next() else {
                return Err(error(
                    function_index,
                    instruction_index,
                    match instruction {
                        Instruction::DirectCall { .. } => {
                            "direct call returning SubAgent requires a SubAgent-containing argument"
                        }
                        Instruction::ClosureCall { .. } => {
                            "closure call returning SubAgent requires a SubAgent-containing argument"
                        }
                        _ => unreachable!("matched call instruction"),
                    },
                ));
            };
            Ok(sources.fold(first, |combined, source| {
                combine_sub_agent_sources(combined, source, instruction_index, &state.scopes)
            }))
        }
        // Async execution and intrinsic creation are anchored to the lexical
        // scope in which the producer is started. Await and Spawn then retain
        // that provenance instead of relabeling the result at consumption.
        Instruction::AsyncCall { .. }
        | Instruction::EffectCall {
            operation: EffectOperation::SubAgentCreate,
            ..
        } => Ok(current_scope),
        _ => Err(error(
            function_index,
            instruction_index,
            "instruction cannot produce a SubAgent-containing value",
        )),
    }
}

fn combine_sub_agent_sources(
    left: SubAgentAvailability,
    right: SubAgentAvailability,
    instruction_index: usize,
    active_scopes: &[u32],
) -> SubAgentAvailability {
    let SubAgentAvailability::Available {
        scope: left_scope,
        provenance: left_provenance,
    } = left
    else {
        unreachable!("SubAgent-containing instruction source was checked for availability")
    };
    let SubAgentAvailability::Available {
        scope: right_scope,
        provenance: right_provenance,
    } = right
    else {
        unreachable!("SubAgent-containing instruction source was checked for availability")
    };
    let scope = if sub_agent_scope_depth(left, active_scopes)
        >= sub_agent_scope_depth(right, active_scopes)
    {
        left_scope
    } else {
        right_scope
    };
    SubAgentAvailability::Available {
        scope,
        provenance: if left_provenance == right_provenance {
            left_provenance
        } else {
            (1_u64 << 63) | instruction_index as u64
        },
    }
}

fn sub_agent_scope_depth(availability: SubAgentAvailability, active_scopes: &[u32]) -> usize {
    let SubAgentAvailability::Available { scope, .. } = availability else {
        unreachable!("SubAgent-containing instruction source was checked for availability")
    };
    if scope == 0 {
        0
    } else {
        active_scopes
            .iter()
            .position(|active| *active == scope)
            .map_or_else(
                || unreachable!("available SubAgent scope must still be active"),
                |depth| depth + 1,
            )
    }
}

fn normalize_uninitialized_sub_agents(
    state: &mut SubAgentState,
    register_types: &[ValueType],
    initialized: &[bool],
) {
    for ((availability, register_type), initialized) in state
        .registers
        .iter_mut()
        .zip(register_types)
        .zip(initialized)
    {
        if contains_stored_sub_agent(register_type) && !initialized {
            *availability = SubAgentAvailability::Unavailable;
        }
    }
}

fn merge_sub_agent_edge(
    function_index: usize,
    instruction_index: usize,
    target: usize,
    mut incoming: SubAgentState,
    entry_states: &mut [Option<SubAgentState>],
    worklist: &mut std::collections::VecDeque<usize>,
    target_layout: (&[ValueType], &[bool]),
) -> Result<(), VerifyError> {
    if target <= instruction_index
        && incoming
            .registers
            .iter()
            .any(|availability| matches!(availability, SubAgentAvailability::Available { .. }))
    {
        return Err(error(
            function_index,
            instruction_index,
            "backward control-flow edge cannot carry an available SubAgent",
        ));
    }
    normalize_uninitialized_sub_agents(&mut incoming, target_layout.0, target_layout.1);
    match &entry_states[target] {
        None => {
            entry_states[target] = Some(incoming);
            worklist.push_back(target);
        }
        Some(existing) if existing == &incoming => {}
        Some(_) => {
            return Err(error(
                function_index,
                target,
                "control-flow join has inconsistent SubAgent availability or lexical scopes",
            ));
        }
    }
    Ok(())
}

/// Version 10 admits compiler-lowered loops while retaining a structured CFG.
/// Every edge to an earlier instruction must be a natural-loop edge whose target
/// dominates its source. The iterative dominator construction uses linear state.
#[allow(clippy::too_many_lines)]
fn verify_structured_back_edges(
    function_index: usize,
    function: &Function,
) -> Result<(), VerifyError> {
    let code_len = function.code.len();
    let mut successors = vec![Vec::new(); code_len];
    let mut predecessors = vec![Vec::new(); code_len];

    for (source, instruction) in function.code.iter().enumerate() {
        let targets = match instruction {
            Instruction::Return { .. } | Instruction::Stop { .. } => Vec::new(),
            Instruction::Jump { target } => vec![*target as usize],
            Instruction::BranchBool {
                true_target,
                false_target,
                ..
            } => vec![*true_target as usize, *false_target as usize],
            Instruction::SwitchEnum { arms, .. } => {
                arms.iter().map(|arm| arm.target as usize).collect()
            }
            _ => vec![source + 1],
        };
        for target in targets {
            successors[source].push(target);
            predecessors[target].push(source);
        }
    }

    let mut visited = vec![false; code_len];
    let mut postorder = Vec::with_capacity(code_len);
    visited[0] = true;
    let mut stack = vec![(0_usize, 0_usize)];
    while let Some((node, next_successor)) = stack.last_mut() {
        if *next_successor == successors[*node].len() {
            postorder.push(*node);
            stack.pop();
            continue;
        }
        let successor = successors[*node][*next_successor];
        *next_successor += 1;
        if !visited[successor] {
            visited[successor] = true;
            stack.push((successor, 0));
        }
    }
    postorder.reverse();
    let mut reverse_postorder = vec![0_usize; code_len];
    for (position, node) in postorder.iter().copied().enumerate() {
        reverse_postorder[node] = position;
    }

    let mut immediate_dominator = vec![None; code_len];
    immediate_dominator[0] = Some(0);
    loop {
        let mut changed = false;
        for node in postorder.iter().copied().skip(1) {
            let mut defined_predecessors = predecessors[node]
                .iter()
                .copied()
                .filter(|predecessor| immediate_dominator[*predecessor].is_some());
            let Some(mut dominator) = defined_predecessors.next() else {
                continue;
            };
            for predecessor in defined_predecessors {
                dominator = intersect_dominators(
                    predecessor,
                    dominator,
                    &immediate_dominator,
                    &reverse_postorder,
                );
            }
            if immediate_dominator[node] != Some(dominator) {
                immediate_dominator[node] = Some(dominator);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut dominator_children = vec![Vec::new(); code_len];
    for (node, parent) in immediate_dominator.iter().copied().enumerate().skip(1) {
        let parent =
            parent.expect("all instructions were proven reachable before dominator verification");
        dominator_children[parent].push(node);
    }
    let mut entry_time = vec![0_usize; code_len];
    let mut exit_time = vec![0_usize; code_len];
    let mut time = 0_usize;
    let mut traversal = vec![(0_usize, false)];
    while let Some((node, exiting)) = traversal.pop() {
        if exiting {
            exit_time[node] = time;
            continue;
        }
        entry_time[node] = time;
        time += 1;
        traversal.push((node, true));
        traversal.extend(
            dominator_children[node]
                .iter()
                .rev()
                .map(|child| (*child, false)),
        );
    }

    for (source, targets) in successors.iter().enumerate() {
        for target in targets.iter().copied().filter(|target| *target <= source) {
            let dominates =
                entry_time[target] <= entry_time[source] && entry_time[source] < exit_time[target];
            if !dominates {
                return Err(error(
                    function_index,
                    source,
                    "backward control-flow target must dominate its source",
                ));
            }
        }
    }
    Ok(())
}

fn intersect_dominators(
    mut left: usize,
    mut right: usize,
    immediate_dominator: &[Option<usize>],
    reverse_postorder: &[usize],
) -> usize {
    while left != right {
        while reverse_postorder[left] > reverse_postorder[right] {
            left = immediate_dominator[left].expect("processed node has a dominator");
        }
        while reverse_postorder[right] > reverse_postorder[left] {
            right = immediate_dominator[right].expect("processed node has a dominator");
        }
    }
    left
}

fn validate_target(
    function_index: usize,
    instruction_index: usize,
    target: u32,
    code_len: usize,
) -> Result<(), VerifyError> {
    if usize::try_from(target).is_ok_and(|target| target < code_len) {
        Ok(())
    } else {
        Err(error(
            function_index,
            instruction_index,
            "control-flow target is out of range",
        ))
    }
}

fn merge_edge(
    entry_states: &mut [Option<Vec<bool>>],
    worklist: &mut std::collections::VecDeque<usize>,
    target: usize,
    incoming: Vec<bool>,
) {
    match &mut entry_states[target] {
        None => {
            entry_states[target] = Some(incoming);
            worklist.push_back(target);
        }
        Some(current) => {
            let mut changed = false;
            for (initialized, incoming) in current.iter_mut().zip(incoming) {
                if *initialized && !incoming {
                    *initialized = false;
                    changed = true;
                }
            }
            if changed {
                worklist.push_back(target);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AffineState {
    Uninitialized,
    Live { origin: u32, must_consume: bool },
    Consumed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OwnershipState {
    registers: Vec<AffineState>,
    scopes: Vec<u32>,
}

#[allow(clippy::too_many_lines)]
fn verify_affine_cfg(
    module: &Module,
    function_index: usize,
    function: &Function,
    definite_initialization: &[Option<Vec<bool>>],
) -> Result<(), VerifyError> {
    let mut entered_scopes = BTreeSet::new();
    for (instruction_index, instruction) in function.code.iter().enumerate() {
        if let Instruction::TaskScopeEnter { scope } = instruction {
            if *scope == 0 || !entered_scopes.insert(*scope) {
                return Err(error(
                    function_index,
                    instruction_index,
                    "task scope IDs must be nonzero and unique within a function",
                ));
            }
        }
    }

    let mut registers = vec![AffineState::Uninitialized; function.registers.len()];
    for register in &function.parameters {
        if is_affine_type(&function.registers[*register as usize]) {
            registers[*register as usize] = AffineState::Live {
                origin: 0,
                must_consume: true,
            };
        }
    }
    let mut initial_state = OwnershipState {
        registers,
        scopes: Vec::new(),
    };
    normalize_dead_affine_temporaries(
        &mut initial_state,
        &function.registers,
        definite_initialization[0]
            .as_deref()
            .expect("function entry is reachable"),
    );
    let mut entry_states = vec![None; function.code.len()];
    entry_states[0] = Some(initial_state);
    let mut worklist = std::collections::VecDeque::from([0_usize]);

    while let Some(instruction_index) = worklist.pop_front() {
        let mut state = entry_states[instruction_index]
            .clone()
            .expect("ownership worklist contains reachable instructions");
        let instruction = &function.code[instruction_index];

        for source in instruction_sources(instruction) {
            if is_affine_type(&function.registers[source as usize])
                && !matches!(state.registers[source as usize], AffineState::Live { .. })
            {
                return Err(error(
                    function_index,
                    instruction_index,
                    "Future or Task register is not live",
                ));
            }
        }

        let destination = instruction_destination(instruction);
        if let Some(destination) = destination {
            if matches!(
                state.registers[destination as usize],
                AffineState::Live {
                    must_consume: true,
                    ..
                }
            ) && !instruction_consumes_register(instruction, destination)
            {
                return Err(error(
                    function_index,
                    instruction_index,
                    "live affine obligation cannot be overwritten",
                ));
            }
        }

        let mut result_origin = 0;
        let mut result_must_consume = false;
        match instruction {
            Instruction::Move {
                destination,
                source,
            } if is_affine_type(&function.registers[*source as usize]) => {
                if destination == source {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "affine move source and destination must differ",
                    ));
                }
                let AffineState::Live {
                    origin,
                    must_consume,
                } = state.registers[*source as usize]
                else {
                    unreachable!("live source checked above")
                };
                state.registers[*source as usize] = AffineState::Consumed;
                result_origin = origin;
                result_must_consume = must_consume;
            }
            Instruction::DirectCall { arguments, .. }
            | Instruction::ClosureCall { arguments, .. } => {
                let mut consumed = BTreeSet::new();
                let mut captured_origin = 0;
                for argument in arguments {
                    if is_affine_type(&function.registers[*argument as usize]) {
                        if !consumed.insert(*argument) {
                            return Err(error(
                                function_index,
                                instruction_index,
                                "call cannot consume one Future or Task argument more than once",
                            ));
                        }
                        let AffineState::Live {
                            origin,
                            must_consume,
                        } = state.registers[*argument as usize]
                        else {
                            unreachable!("live source checked above")
                        };
                        if captured_origin == 0 {
                            captured_origin = origin;
                        } else if origin != 0 && captured_origin != origin {
                            return Err(error(
                                function_index,
                                instruction_index,
                                "call cannot combine affine values from different task scopes",
                            ));
                        }
                        result_must_consume |= must_consume;
                        state.registers[*argument as usize] = AffineState::Consumed;
                    }
                }
                let destination =
                    instruction_destination(instruction).expect("call has destination") as usize;
                if matches!(function.registers[destination], ValueType::Task(_)) {
                    result_origin = state.scopes.last().copied().unwrap_or(captured_origin);
                    result_must_consume = true;
                } else {
                    result_origin = captured_origin;
                }
            }
            Instruction::AsyncCall {
                destination,
                function: target,
                arguments,
            } => {
                let target = &module.functions[*target as usize];
                let mut consumed = BTreeSet::new();
                let mut captured_origin = 0;
                for (argument, parameter) in arguments.iter().zip(&target.parameters) {
                    if is_affine_type(&function.registers[*argument as usize]) {
                        if !consumed.insert(*argument) {
                            return Err(error(
                                function_index,
                                instruction_index,
                                "call cannot consume one Future or Task argument more than once",
                            ));
                        }
                        let AffineState::Live {
                            origin,
                            must_consume,
                        } = state.registers[*argument as usize]
                        else {
                            unreachable!("live source checked above")
                        };
                        if captured_origin == 0 {
                            captured_origin = origin;
                        } else if origin != 0 && captured_origin != origin {
                            return Err(error(
                                function_index,
                                instruction_index,
                                "call cannot combine affine values from different task scopes",
                            ));
                        }
                        result_must_consume |=
                            matches!(target.registers[*parameter as usize], ValueType::Task(_))
                                || must_consume;
                        state.registers[*argument as usize] = AffineState::Consumed;
                    }
                }
                result_origin = captured_origin;
                let _ = destination;
            }
            Instruction::Spawn { future, scope, .. } => {
                let expected_scope = state.scopes.last().copied().unwrap_or(0);
                if *scope != expected_scope {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "spawn scope does not match the current lexical task scope",
                    ));
                }
                state.registers[*future as usize] = AffineState::Consumed;
                result_origin = *scope;
                result_must_consume = true;
            }
            Instruction::Await { source, .. } => {
                let AffineState::Live { origin, .. } = state.registers[*source as usize] else {
                    unreachable!("live source checked above")
                };
                state.registers[*source as usize] = AffineState::Consumed;
                result_origin = if matches!(
                    function.registers[instruction_destination(instruction)
                        .expect("await has destination")
                        as usize],
                    ValueType::Task(_)
                ) {
                    state.scopes.last().copied().unwrap_or(origin)
                } else {
                    origin
                };
                result_must_consume = is_affine_type(
                    &function.registers[instruction_destination(instruction)
                        .expect("await has destination")
                        as usize],
                );
            }
            Instruction::EffectCall { .. } | Instruction::ToolInvoke { .. } => {
                result_must_consume = true;
            }
            Instruction::TaskScopeEnter { scope } => state.scopes.push(*scope),
            Instruction::TaskScopeExit { scope } => {
                if state.scopes.pop() != Some(*scope) {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "task scope exit does not match the current scope",
                    ));
                }
                for (register, ownership) in state.registers.iter_mut().enumerate() {
                    if matches!(
                        ownership,
                        AffineState::Live { origin, .. } if *origin == *scope
                    ) {
                        let nested_task_result = matches!(
                            &function.registers[register],
                            ValueType::Task(value) if is_affine_type(value)
                        );
                        let hidden_future_obligation = matches!(
                            (&function.registers[register], &*ownership),
                            (
                                ValueType::Future(_),
                                AffineState::Live {
                                    must_consume: true,
                                    ..
                                }
                            )
                        );
                        if nested_task_result || hidden_future_obligation {
                            return Err(error(
                                function_index,
                                instruction_index,
                                "nested affine result must be awaited before scope exit",
                            ));
                        }
                        *ownership = AffineState::Consumed;
                    }
                }
            }
            Instruction::TryResult { .. } => {
                for (register, ownership) in state.registers.iter().enumerate() {
                    if matches!(
                        ownership,
                        AffineState::Live {
                            must_consume: true,
                            ..
                        }
                    ) && (matches!(function.registers[register], ValueType::Future(_))
                        || matches!(ownership, AffineState::Live { origin: 0, .. }))
                    {
                        let _ = register;
                        return Err(error(
                            function_index,
                            instruction_index,
                            "try error path would discard a live affine obligation",
                        ));
                    }
                }
            }
            Instruction::Return { source } => {
                if !state.scopes.is_empty() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "return cannot leave an explicit task scope",
                    ));
                }
                for (register, ownership) in state.registers.iter().enumerate() {
                    if matches!(
                        ownership,
                        AffineState::Live {
                            must_consume: true,
                            ..
                        }
                    ) && register != *source as usize
                    {
                        return Err(error(
                            function_index,
                            instruction_index,
                            "return would discard a live affine obligation",
                        ));
                    }
                }
                if is_affine_type(&function.registers[*source as usize]) {
                    let AffineState::Live { origin, .. } = state.registers[*source as usize] else {
                        unreachable!("live source checked above")
                    };
                    if origin != 0 {
                        return Err(error(
                            function_index,
                            instruction_index,
                            "scope-owned Task cannot escape its task scope",
                        ));
                    }
                }
            }
            _ => {}
        }

        if let Some(destination) = destination {
            if is_affine_type(&function.registers[destination as usize]) {
                state.registers[destination as usize] = AffineState::Live {
                    origin: result_origin,
                    must_consume: result_must_consume,
                };
            }
        }

        let successors: Vec<usize> = match instruction {
            Instruction::Return { .. } | Instruction::Stop { .. } => Vec::new(),
            Instruction::Jump { target } => vec![*target as usize],
            Instruction::BranchBool {
                true_target,
                false_target,
                ..
            } => vec![*true_target as usize, *false_target as usize],
            Instruction::SwitchEnum { arms, .. } => {
                arms.iter().map(|arm| arm.target as usize).collect()
            }
            _ => vec![instruction_index + 1],
        };
        for successor in successors {
            let mut edge_state = state.clone();
            if successor <= instruction_index
                && edge_state
                    .registers
                    .iter()
                    .any(|ownership| matches!(ownership, AffineState::Live { .. }))
            {
                return Err(error(
                    function_index,
                    instruction_index,
                    "backward control-flow edge cannot carry a live Future or Task",
                ));
            }
            normalize_dead_affine_temporaries(
                &mut edge_state,
                &function.registers,
                definite_initialization[successor]
                    .as_deref()
                    .expect("affine successor is reachable"),
            );
            match &entry_states[successor] {
                None => {
                    entry_states[successor] = Some(edge_state);
                    worklist.push_back(successor);
                }
                Some(existing) if existing == &edge_state => {}
                Some(_) => {
                    return Err(error(
                        function_index,
                        successor,
                        "control-flow join has inconsistent affine ownership or task scopes",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn normalize_dead_affine_temporaries(
    state: &mut OwnershipState,
    register_types: &[ValueType],
    initialized: &[bool],
) {
    for ((ownership, register_type), initialized) in state
        .registers
        .iter_mut()
        .zip(register_types)
        .zip(initialized)
    {
        if is_affine_type(register_type)
            && !initialized
            && matches!(
                ownership,
                AffineState::Uninitialized
                    | AffineState::Consumed
                    | AffineState::Live {
                        must_consume: false,
                        ..
                    }
            )
        {
            *ownership = AffineState::Consumed;
        }
    }
}

fn instruction_consumes_register(instruction: &Instruction, register: Register) -> bool {
    match instruction {
        Instruction::Move { source, .. }
        | Instruction::Await { source, .. }
        | Instruction::Spawn { future: source, .. } => *source == register,
        Instruction::DirectCall { arguments, .. }
        | Instruction::ClosureCall { arguments, .. }
        | Instruction::AsyncCall { arguments, .. } => arguments.contains(&register),
        _ => false,
    }
}

fn instruction_destination(instruction: &Instruction) -> Option<Register> {
    match instruction {
        Instruction::Const { destination, .. }
        | Instruction::Move { destination, .. }
        | Instruction::IntBinary { destination, .. }
        | Instruction::IntRemainder { destination, .. }
        | Instruction::FloatBinary { destination, .. }
        | Instruction::IntNegate { destination, .. }
        | Instruction::FloatNegate { destination, .. }
        | Instruction::Compare { destination, .. }
        | Instruction::BoolNot { destination, .. }
        | Instruction::BoolBinary { destination, .. }
        | Instruction::ListNew { destination, .. }
        | Instruction::Length { destination, .. }
        | Instruction::ListAppend { destination, .. }
        | Instruction::ListSet { destination, .. }
        | Instruction::MapNew { destination, .. }
        | Instruction::TupleNew { destination, .. }
        | Instruction::IndexGet { destination, .. }
        | Instruction::MapEntryAt { destination, .. }
        | Instruction::TupleGet { destination, .. }
        | Instruction::Convert { destination, .. }
        | Instruction::RecordNew { destination, .. }
        | Instruction::FieldGet { destination, .. }
        | Instruction::EnumNew { destination, .. }
        | Instruction::TryResult { destination, .. }
        | Instruction::ToUnknown { destination, .. }
        | Instruction::Narrow { destination, .. }
        | Instruction::DirectCall { destination, .. }
        | Instruction::ClosureNew { destination, .. }
        | Instruction::ClosureCall { destination, .. }
        | Instruction::AsyncCall { destination, .. }
        | Instruction::Spawn { destination, .. }
        | Instruction::Await { destination, .. }
        | Instruction::TaskSnapshot { destination, .. }
        | Instruction::WorkspaceGet { destination }
        | Instruction::EffectCall { destination, .. }
        | Instruction::StringCall { destination, .. }
        | Instruction::CapabilityInspect { destination, .. }
        | Instruction::SafeCollectionCall { destination, .. }
        | Instruction::CheckedIntCall { destination, .. }
        | Instruction::ToolInvoke { destination, .. } => Some(*destination),
        Instruction::BranchBool { .. }
        | Instruction::SwitchEnum { .. }
        | Instruction::Jump { .. }
        | Instruction::TaskScopeEnter { .. }
        | Instruction::TaskScopeExit { .. }
        | Instruction::Stop { .. }
        | Instruction::Return { .. } => None,
    }
}

fn instruction_sources(instruction: &Instruction) -> Vec<Register> {
    match instruction {
        Instruction::Const { .. }
        | Instruction::WorkspaceGet { .. }
        | Instruction::Jump { .. }
        | Instruction::TaskScopeEnter { .. }
        | Instruction::TaskScopeExit { .. } => Vec::new(),
        Instruction::Move { source, .. }
        | Instruction::IntNegate { source, .. }
        | Instruction::FloatNegate { source, .. }
        | Instruction::BoolNot { source, .. }
        | Instruction::Convert { source, .. }
        | Instruction::TryResult { source, .. }
        | Instruction::ToUnknown { source, .. }
        | Instruction::Narrow { source, .. }
        | Instruction::Await { source, .. }
        | Instruction::TaskSnapshot { source, .. }
        | Instruction::ToolInvoke { input: source, .. }
        | Instruction::SwitchEnum { source, .. }
        | Instruction::Return { source } => vec![*source],
        Instruction::Spawn { future, .. } => vec![*future],
        Instruction::Stop { reason } => vec![*reason],
        Instruction::IntBinary { left, right, .. }
        | Instruction::IntRemainder { left, right, .. }
        | Instruction::FloatBinary { left, right, .. }
        | Instruction::Compare { left, right, .. }
        | Instruction::BoolBinary { left, right, .. } => vec![*left, *right],
        Instruction::ListNew { elements, .. } | Instruction::TupleNew { elements, .. } => {
            elements.clone()
        }
        Instruction::Length { collection, .. } => vec![*collection],
        Instruction::ListAppend { values, value, .. } => vec![*values, *value],
        Instruction::ListSet {
            values,
            index,
            value,
            ..
        } => vec![*values, *index, *value],
        Instruction::MapNew { entries, .. } => entries
            .iter()
            .flat_map(|(key, value)| [*key, *value])
            .collect(),
        Instruction::IndexGet {
            collection, index, ..
        } => vec![*collection, *index],
        Instruction::MapEntryAt { map, index, .. } => vec![*map, *index],
        Instruction::TupleGet { tuple, .. } => vec![*tuple],
        Instruction::RecordNew { fields, .. } => fields.iter().map(|(_, source)| *source).collect(),
        Instruction::FieldGet { record, .. } => vec![*record],
        Instruction::EnumNew { payload, .. } => payload.clone(),
        Instruction::BranchBool { condition, .. } => vec![*condition],
        Instruction::DirectCall { arguments, .. } | Instruction::AsyncCall { arguments, .. } => {
            arguments.clone()
        }
        Instruction::EffectCall { arguments, .. }
        | Instruction::StringCall { arguments, .. }
        | Instruction::CapabilityInspect { arguments, .. }
        | Instruction::SafeCollectionCall { arguments, .. }
        | Instruction::CheckedIntCall { arguments, .. } => arguments.clone(),
        Instruction::ClosureNew { captures, .. } => captures.clone(),
        Instruction::ClosureCall {
            closure, arguments, ..
        } => std::iter::once(*closure)
            .chain(arguments.iter().copied())
            .collect(),
    }
}

fn verify_constants(constants: &[Constant]) -> Result<(), VerifyError> {
    for constant in constants {
        if let Constant::Float(bits) = constant {
            if is_nan_bits(*bits) && *bits != CANONICAL_NAN_BITS {
                return Err(VerifyError::module(
                    "Float constant has noncanonical NaN bits",
                ));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn verify_instruction_types(
    module: &Module,
    function_index: usize,
    function: &Function,
    tool_contracts: Option<&[ToolVerificationContract]>,
) -> Result<(), VerifyError> {
    let function_id = u32::try_from(function_index).expect("verified function table size fits u32");
    for register_type in &function.registers {
        verify_module_value_type(module, register_type, 0).map_err(VerifyError::module)?;
    }
    verify_module_value_type(module, &function.return_type, 0).map_err(VerifyError::module)?;

    let mut initialized = vec![true; function.registers.len()];
    let mut concrete_enums = vec![None; module.enum_types.len()];
    let mut equatable_enums = vec![None; module.enum_types.len()];

    for (instruction_index, instruction) in function.code.iter().enumerate() {
        match instruction {
            Instruction::Const {
                destination,
                constant,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let constant = module.constants.get(*constant as usize).ok_or_else(|| {
                    error(
                        function_index,
                        instruction_index,
                        "constant is out of range",
                    )
                })?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    &constant.value_type(),
                    "constant destination",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::Move {
                destination,
                source,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let source_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *source,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    source_type,
                    "move",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::IntBinary {
                destination,
                left,
                right,
                operation: _,
            } => {
                require_binary_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *destination,
                    *left,
                    *right,
                    &ValueType::Int,
                    "integer binary operation",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::IntRemainder {
                destination,
                left,
                right,
            } => {
                require_binary_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *destination,
                    *left,
                    *right,
                    &ValueType::Int,
                    "integer remainder",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::FloatBinary {
                destination,
                left,
                right,
                operation: _,
            } => {
                require_binary_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *destination,
                    *left,
                    *right,
                    &ValueType::Float,
                    "float binary operation",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::IntNegate {
                destination,
                source,
            } => {
                require_unary_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *destination,
                    *source,
                    &ValueType::Int,
                    "integer negation",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::FloatNegate {
                destination,
                source,
            } => {
                require_unary_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *destination,
                    *source,
                    &ValueType::Float,
                    "float negation",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::Compare {
                destination,
                left,
                right,
                operation,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    &ValueType::Bool,
                    "comparison destination",
                )?;
                let left_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *left,
                )?;
                let right_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *right,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    left_type,
                    right_type,
                    "comparison operands",
                )?;
                let supported = match operation {
                    CompareOp::Equal | CompareOp::NotEqual => {
                        is_equatable_in_module(module, left_type, &mut equatable_enums)
                    }
                    CompareOp::Less
                    | CompareOp::LessEqual
                    | CompareOp::Greater
                    | CompareOp::GreaterEqual => left_type.is_ordered(),
                };
                if !supported {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "comparison is not supported for operand type",
                    ));
                }
                initialized[*destination as usize] = true;
            }
            Instruction::BoolNot {
                destination,
                source,
            } => {
                require_unary_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *destination,
                    *source,
                    &ValueType::Bool,
                    "Boolean negation",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::BoolBinary {
                destination,
                left,
                right,
                operation: _,
            } => {
                require_binary_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *destination,
                    *left,
                    *right,
                    &ValueType::Bool,
                    "Boolean binary operation",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::ListNew {
                destination,
                elements,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let ValueType::List(element_type) = destination_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "list construction destination must be List",
                    ));
                };
                for element in elements {
                    let actual = initialized_type(
                        function,
                        &initialized,
                        function_index,
                        instruction_index,
                        *element,
                    )?;
                    require_type(
                        function_index,
                        instruction_index,
                        actual,
                        element_type,
                        "list element",
                    )?;
                }
                initialized[*destination as usize] = true;
            }
            Instruction::Length {
                destination,
                collection,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    &ValueType::Int,
                    "length destination",
                )?;
                let collection_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *collection,
                )?;
                let supported = matches!(
                    collection_type,
                    ValueType::Bytes
                        | ValueType::String
                        | ValueType::List(_)
                        | ValueType::Map(_, _)
                );
                if !supported {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "length collection must be Bytes, String, List, or Map",
                    ));
                }
                initialized[*destination as usize] = true;
            }
            Instruction::StringCall {
                destination,
                operation,
                arguments,
            } => {
                let signature = string_operation_signature(
                    function_index,
                    instruction_index,
                    *operation,
                    arguments.len(),
                )?;
                verify_call_arguments(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    arguments,
                    signature.parameters.iter(),
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type(function, function_index, instruction_index, *destination)?,
                    &signature.result,
                    "String operation result",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::ListAppend {
                destination,
                values,
                value,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let ValueType::List(destination_element) = destination_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "list append destination must be List",
                    ));
                };
                let values_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *values,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    values_type,
                    destination_type,
                    "list append values",
                )?;
                let appended_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *value,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    appended_type,
                    destination_element,
                    "list append value",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::ListSet {
                destination,
                values,
                index,
                value,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let ValueType::List(destination_element) = destination_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "list set destination must be List",
                    ));
                };
                let values_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *values,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    values_type,
                    destination_type,
                    "list set values",
                )?;
                let index_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *index,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    index_type,
                    &ValueType::Int,
                    "list set index",
                )?;
                let replacement_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *value,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    replacement_type,
                    destination_element,
                    "list set value",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::MapNew {
                destination,
                entries,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let ValueType::Map(key_type, value_type) = destination_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "map construction destination must be Map",
                    ));
                };
                if !key_type.is_map_key() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "map key type is not allowed",
                    ));
                }
                for (key, value) in entries {
                    let actual_key = initialized_type(
                        function,
                        &initialized,
                        function_index,
                        instruction_index,
                        *key,
                    )?;
                    require_type(
                        function_index,
                        instruction_index,
                        actual_key,
                        key_type,
                        "map key",
                    )?;
                    let actual_value = initialized_type(
                        function,
                        &initialized,
                        function_index,
                        instruction_index,
                        *value,
                    )?;
                    require_type(
                        function_index,
                        instruction_index,
                        actual_value,
                        value_type,
                        "map value",
                    )?;
                }
                initialized[*destination as usize] = true;
            }
            Instruction::TupleNew {
                destination,
                elements,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let ValueType::Tuple(element_types) = destination_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "tuple construction destination must be Tuple",
                    ));
                };
                if element_types.is_empty() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "empty tuple must use Void",
                    ));
                }
                if elements.len() != element_types.len() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "tuple construction has wrong element count",
                    ));
                }
                for (element, expected) in elements.iter().zip(element_types) {
                    let actual = initialized_type(
                        function,
                        &initialized,
                        function_index,
                        instruction_index,
                        *element,
                    )?;
                    require_type(
                        function_index,
                        instruction_index,
                        actual,
                        expected,
                        "tuple element",
                    )?;
                }
                initialized[*destination as usize] = true;
            }
            Instruction::IndexGet {
                destination,
                collection,
                index,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let collection_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *collection,
                )?;
                let index_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *index,
                )?;
                match collection_type {
                    ValueType::List(element_type) => {
                        require_type(
                            function_index,
                            instruction_index,
                            index_type,
                            &ValueType::Int,
                            "list index",
                        )?;
                        require_type(
                            function_index,
                            instruction_index,
                            destination_type,
                            element_type,
                            "list index result",
                        )?;
                    }
                    ValueType::Bytes => {
                        require_type(
                            function_index,
                            instruction_index,
                            index_type,
                            &ValueType::Int,
                            "bytes index",
                        )?;
                        require_type(
                            function_index,
                            instruction_index,
                            destination_type,
                            &ValueType::Int,
                            "bytes index result",
                        )?;
                    }
                    ValueType::Map(key_type, value_type) => {
                        require_type(
                            function_index,
                            instruction_index,
                            index_type,
                            key_type,
                            "map index",
                        )?;
                        require_type(
                            function_index,
                            instruction_index,
                            destination_type,
                            value_type,
                            "map index result",
                        )?;
                    }
                    _ => {
                        return Err(error(
                            function_index,
                            instruction_index,
                            "index collection must be List, Map, or Bytes",
                        ));
                    }
                }
                initialized[*destination as usize] = true;
            }
            Instruction::MapEntryAt {
                destination,
                map,
                index,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let map_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *map,
                )?;
                let ValueType::Map(key_type, value_type) = map_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "map entry access source must be Map",
                    ));
                };
                let index_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *index,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    index_type,
                    &ValueType::Int,
                    "map entry index",
                )?;
                let ValueType::Tuple(elements) = destination_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "map entry access destination must be Tuple",
                    ));
                };
                if elements.len() != 2 || elements[0] != **key_type || elements[1] != **value_type {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "map entry access destination must be exact key-value Tuple",
                    ));
                }
                initialized[*destination as usize] = true;
            }
            Instruction::TupleGet {
                destination,
                tuple,
                index,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let tuple_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *tuple,
                )?;
                let ValueType::Tuple(elements) = tuple_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "tuple access source must be Tuple",
                    ));
                };
                let element_type = elements.get(*index as usize).ok_or_else(|| {
                    error(
                        function_index,
                        instruction_index,
                        "tuple index is out of range",
                    )
                })?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    element_type,
                    "tuple index result",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::Convert {
                destination,
                source,
                conversion,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let source_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *source,
                )?;
                match conversion {
                    Conversion::IntToFloat => {
                        require_type(
                            function_index,
                            instruction_index,
                            source_type,
                            &ValueType::Int,
                            "IntToFloat source",
                        )?;
                        require_type(
                            function_index,
                            instruction_index,
                            destination_type,
                            &ValueType::Float,
                            "IntToFloat destination",
                        )?;
                    }
                    Conversion::ToString => {
                        if !matches!(
                            source_type,
                            ValueType::Bool | ValueType::Int | ValueType::Float | ValueType::String
                        ) {
                            return Err(error(
                                function_index,
                                instruction_index,
                                "ToString source must be Bool, Int, Float, or String",
                            ));
                        }
                        require_type(
                            function_index,
                            instruction_index,
                            destination_type,
                            &ValueType::String,
                            "ToString destination",
                        )?;
                    }
                    Conversion::StringToBytes => {
                        require_type(
                            function_index,
                            instruction_index,
                            source_type,
                            &ValueType::String,
                            "StringToBytes source",
                        )?;
                        require_type(
                            function_index,
                            instruction_index,
                            destination_type,
                            &ValueType::Bytes,
                            "StringToBytes destination",
                        )?;
                    }
                }
                initialized[*destination as usize] = true;
            }
            Instruction::RecordNew {
                destination,
                fields,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let ValueType::Record(layout) = destination_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "record construction destination must be Record",
                    ));
                };
                if fields.len() != layout.len() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "record construction has wrong field count",
                    ));
                }
                for (expected_index, ((field, source), expected)) in
                    fields.iter().zip(layout).enumerate()
                {
                    if usize::try_from(*field).ok() != Some(expected_index) {
                        return Err(error(
                            function_index,
                            instruction_index,
                            "record construction fields are not canonical",
                        ));
                    }
                    let actual = initialized_type(
                        function,
                        &initialized,
                        function_index,
                        instruction_index,
                        *source,
                    )?;
                    require_type(
                        function_index,
                        instruction_index,
                        actual,
                        &expected.value_type,
                        "record field",
                    )?;
                }
                initialized[*destination as usize] = true;
            }
            Instruction::FieldGet {
                destination,
                record,
                field,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let record_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *record,
                )?;
                let ValueType::Record(layout) = record_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "field access source must be Record",
                    ));
                };
                let field_type = layout
                    .get(*field as usize)
                    .map(|field| &field.value_type)
                    .ok_or_else(|| {
                        error(
                            function_index,
                            instruction_index,
                            "record field is out of range",
                        )
                    })?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    field_type,
                    "field access result",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::EnumNew {
                destination,
                variant,
                payload,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let payload_types = enum_payload_types(module, destination_type, *variant)
                    .ok_or_else(|| {
                        error(
                            function_index,
                            instruction_index,
                            "enum variant is out of range",
                        )
                    })?;
                if payload.len() != payload_types.len() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "enum construction has wrong payload count",
                    ));
                }
                for (source, expected) in payload.iter().zip(&payload_types) {
                    let actual = initialized_type(
                        function,
                        &initialized,
                        function_index,
                        instruction_index,
                        *source,
                    )?;
                    require_type(
                        function_index,
                        instruction_index,
                        actual,
                        expected,
                        "enum payload",
                    )?;
                }
                initialized[*destination as usize] = true;
            }
            Instruction::BranchBool { condition, .. } => {
                let condition_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *condition,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    condition_type,
                    &ValueType::Bool,
                    "Boolean branch condition",
                )?;
            }
            Instruction::SwitchEnum { source, arms } => {
                let source_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *source,
                )?;
                let variant_count = enum_variant_count(module, source_type).ok_or_else(|| {
                    error(
                        function_index,
                        instruction_index,
                        "enum switch source must be Enum, Option, or Result",
                    )
                })?;
                if arms.len() != variant_count {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "enum switch is not exhaustive",
                    ));
                }
                for (expected_variant, arm) in arms.iter().enumerate() {
                    if usize::try_from(arm.variant).ok() != Some(expected_variant) {
                        return Err(error(
                            function_index,
                            instruction_index,
                            "enum switch arms are not canonical",
                        ));
                    }
                    let payload_types = enum_payload_types(module, source_type, arm.variant)
                        .expect("canonical exhaustive variant was checked");
                    if arm.bindings.len() != payload_types.len() {
                        return Err(error(
                            function_index,
                            instruction_index,
                            "enum switch arm has wrong binding count",
                        ));
                    }
                    let mut bindings = std::collections::BTreeSet::new();
                    for (binding, expected) in arm.bindings.iter().zip(payload_types) {
                        if !bindings.insert(*binding) {
                            return Err(error(
                                function_index,
                                instruction_index,
                                "enum switch arm has duplicate binding registers",
                            ));
                        }
                        let actual = destination_type(
                            function,
                            function_index,
                            instruction_index,
                            *binding,
                        )?;
                        require_type(
                            function_index,
                            instruction_index,
                            actual,
                            &expected,
                            "enum switch binding",
                        )?;
                    }
                }
            }
            Instruction::Jump { .. } => {}
            Instruction::TryResult {
                destination,
                source,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let source_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *source,
                )?;
                let ValueType::Result(ok_type, error_type) = source_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "try source must be Result",
                    ));
                };
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    ok_type,
                    "try destination",
                )?;
                let ValueType::Result(_, return_error_type) = &function.return_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "try requires a Result function return type",
                    ));
                };
                require_type(
                    function_index,
                    instruction_index,
                    error_type,
                    return_error_type,
                    "try error type",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::ToUnknown {
                destination,
                source,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let source_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *source,
                )?;
                if matches!(
                    source_type,
                    ValueType::Function { .. }
                        | ValueType::Future(_)
                        | ValueType::Task(_)
                        | ValueType::Workspace
                        | ValueType::SubAgent
                        | ValueType::Never
                ) || contains_workspace(source_type)
                    || contains_sub_agent(source_type)
                {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "to_unknown source cannot be Function, Future, Task, or Never",
                    ));
                }
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    &ValueType::Unknown,
                    "to_unknown destination",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::Narrow {
                destination,
                source,
                target,
            } => {
                verify_module_value_type(module, target, 0).map_err(|message| {
                    VerifyError::instruction(function_index, instruction_index, message)
                })?;
                if !is_concrete_type(module, target, &mut concrete_enums) {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "narrow target must be concrete",
                    ));
                }
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let source_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *source,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    source_type,
                    &ValueType::Unknown,
                    "narrow source",
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    &ValueType::Option(Box::new(target.clone())),
                    "narrow destination",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::DirectCall {
                destination,
                function: callee_id,
                arguments,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let callee = module.functions.get(*callee_id as usize).ok_or_else(|| {
                    error(
                        function_index,
                        instruction_index,
                        "direct call function ID is out of range",
                    )
                })?;
                if module.async_functions.binary_search(callee_id).is_ok() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "direct call target cannot be async",
                    ));
                }
                if !callee.captures.is_empty() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "direct call target requires closure captures",
                    ));
                }
                verify_call_arguments(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    arguments,
                    callee
                        .parameters
                        .iter()
                        .map(|register| &callee.registers[*register as usize]),
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    &callee.return_type,
                    "direct call result",
                )?;
                require_effect_subset(
                    module,
                    function_index,
                    instruction_index,
                    callee.effects,
                    function.effects,
                    "direct call effect set exceeds caller effect set",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::ClosureNew {
                destination,
                function: closure_id,
                captures,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let closure_function =
                    module.functions.get(*closure_id as usize).ok_or_else(|| {
                        error(
                            function_index,
                            instruction_index,
                            "closure function ID is out of range",
                        )
                    })?;
                if module.async_functions.binary_search(closure_id).is_ok() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "async function cannot be used as a closure",
                    ));
                }
                verify_call_arguments(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    captures,
                    closure_function
                        .captures
                        .iter()
                        .map(|register| &closure_function.registers[*register as usize]),
                )?;
                if captures
                    .iter()
                    .any(|capture| contains_sub_agent(&function.registers[*capture as usize]))
                {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "closure capture cannot contain SubAgent",
                    ));
                }
                let expected_type = function_value_type(closure_function);
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    &expected_type,
                    "closure construction result",
                )?;
                require_effect_subset(
                    module,
                    function_index,
                    instruction_index,
                    closure_function.effects,
                    function.effects,
                    "closure effect set exceeds caller effect set",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::ClosureCall {
                destination,
                closure,
                arguments,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let closure_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *closure,
                )?;
                let ValueType::Function {
                    parameters,
                    return_type,
                    effects,
                } = closure_type
                else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "closure call source must be a function",
                    ));
                };
                verify_call_arguments(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    arguments,
                    parameters.iter(),
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    return_type,
                    "closure call result",
                )?;
                require_effect_subset(
                    module,
                    function_index,
                    instruction_index,
                    *effects,
                    function.effects,
                    "closure call effect set exceeds caller effect set",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::AsyncCall {
                destination,
                function: callee_id,
                arguments,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let callee = module.functions.get(*callee_id as usize).ok_or_else(|| {
                    error(
                        function_index,
                        instruction_index,
                        "async call function ID is out of range",
                    )
                })?;
                if module.async_functions.binary_search(callee_id).is_err() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "async call target is not declared async",
                    ));
                }
                if !callee.captures.is_empty() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "async call target requires closure captures",
                    ));
                }
                verify_call_arguments(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    arguments,
                    callee
                        .parameters
                        .iter()
                        .map(|register| &callee.registers[*register as usize]),
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    &ValueType::Future(Box::new(callee.return_type.clone())),
                    "async call result",
                )?;
                require_effect_subset(
                    module,
                    function_index,
                    instruction_index,
                    callee.effects,
                    function.effects,
                    "async call effect set exceeds caller effect set",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::Spawn {
                destination,
                future,
                scope: _,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let future_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *future,
                )?;
                let ValueType::Future(result) = future_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "spawn source must be Future",
                    ));
                };
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    &ValueType::Task(result.clone()),
                    "spawn result",
                )?;
                if module.effect_sets[function.effects as usize]
                    .binary_search_by(|effect| effect.as_str().cmp("task.spawn"))
                    .is_err()
                {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "spawn requires the task.spawn effect",
                    ));
                }
                initialized[*destination as usize] = true;
            }
            Instruction::Await {
                destination,
                source,
            } => {
                if module.async_functions.binary_search(&function_id).is_err() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "await requires an async function",
                    ));
                }
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let source_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *source,
                )?;
                let (ValueType::Future(result) | ValueType::Task(result)) = source_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "await source must be Future or Task",
                    ));
                };
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    result,
                    "await result",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::TaskSnapshot {
                destination,
                source,
            } => {
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let source_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *source,
                )?;
                if !matches!(source_type, ValueType::Task(_)) {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "task snapshot source must be Task",
                    ));
                }
                require_type(
                    function_index,
                    instruction_index,
                    destination_type,
                    &task_snapshot_type(),
                    "task snapshot destination",
                )?;
                if module.effect_sets[function.effects as usize]
                    .binary_search_by(|effect| effect.as_str().cmp("debug.inspect"))
                    .is_err()
                {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "task snapshot requires the debug.inspect effect",
                    ));
                }
                initialized[*destination as usize] = true;
            }
            Instruction::CapabilityInspect {
                destination,
                operation,
                arguments,
            } => {
                let (parameters, result) = match operation {
                    CapabilityOperation::IsGranted => (vec![ValueType::String], ValueType::Bool),
                    CapabilityOperation::Granted => {
                        (Vec::new(), ValueType::List(Box::new(ValueType::String)))
                    }
                };
                verify_call_arguments(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    arguments,
                    parameters.iter(),
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type(function, function_index, instruction_index, *destination)?,
                    &result,
                    "capability inspection result",
                )?;
                if module.effect_sets[function.effects as usize]
                    .binary_search_by(|effect| effect.as_str().cmp("capability.inspect"))
                    .is_err()
                {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "capability inspection requires the capability.inspect effect",
                    ));
                }
                initialized[*destination as usize] = true;
            }
            Instruction::SafeCollectionCall {
                destination,
                operation,
                arguments,
            } => {
                let argument_types = arguments
                    .iter()
                    .map(|register| {
                        initialized_type(
                            function,
                            &initialized,
                            function_index,
                            instruction_index,
                            *register,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let result = match (operation, argument_types.as_slice()) {
                    (SafeCollectionOperation::ListGet, [ValueType::List(item), ValueType::Int]) => {
                        ValueType::Option(item.clone())
                    }
                    (
                        SafeCollectionOperation::ListTrySet,
                        [ValueType::List(item), ValueType::Int, value],
                    ) if item.as_ref() == *value => {
                        ValueType::Option(Box::new(ValueType::List(item.clone())))
                    }
                    (SafeCollectionOperation::BytesGet, [ValueType::Bytes, ValueType::Int]) => {
                        ValueType::Option(Box::new(ValueType::Int))
                    }
                    (SafeCollectionOperation::MapGet, [ValueType::Map(key, value), lookup])
                        if key.as_ref() == *lookup =>
                    {
                        ValueType::Option(value.clone())
                    }
                    _ => {
                        return Err(error(
                            function_index,
                            instruction_index,
                            "safe collection operation signature is invalid",
                        ));
                    }
                };
                require_type(
                    function_index,
                    instruction_index,
                    destination_type(function, function_index, instruction_index, *destination)?,
                    &result,
                    "safe collection operation result",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::CheckedIntCall {
                destination,
                operation,
                arguments,
            } => {
                let arity = if *operation == CheckedIntOperation::Negate {
                    1
                } else {
                    2
                };
                if arguments.len() != arity {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "checked integer operation arity is invalid",
                    ));
                }
                verify_call_arguments(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    arguments,
                    std::iter::repeat_n(&ValueType::Int, arity),
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type(function, function_index, instruction_index, *destination)?,
                    &ValueType::Option(Box::new(ValueType::Int)),
                    "checked integer operation result",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::WorkspaceGet { destination } => {
                require_type(
                    function_index,
                    instruction_index,
                    destination_type(function, function_index, instruction_index, *destination)?,
                    &ValueType::Workspace,
                    "workspace destination",
                )?;
                initialized[*destination as usize] = true;
            }
            Instruction::EffectCall {
                destination,
                operation,
                arguments,
            } => {
                let expected = effect_operation_signature(
                    module,
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *operation,
                    arguments,
                )?;
                verify_call_arguments(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    arguments,
                    expected.parameters.iter(),
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    destination_type(function, function_index, instruction_index, *destination)?,
                    &ValueType::Future(Box::new(expected.result)),
                    "effect operation result",
                )?;
                if module.effect_sets[function.effects as usize]
                    .binary_search_by(|effect| effect.as_str().cmp(operation.required_effect()))
                    .is_err()
                {
                    let message = if matches!(
                        operation,
                        EffectOperation::ReadText
                            | EffectOperation::ReadBytes
                            | EffectOperation::WriteText
                            | EffectOperation::WriteBytes
                            | EffectOperation::List
                            | EffectOperation::Search
                    ) {
                        "filesystem operation requires its matching effect"
                    } else {
                        "effect operation requires its matching effect"
                    };
                    return Err(error(function_index, instruction_index, message));
                }
                initialized[*destination as usize] = true;
            }
            Instruction::ToolInvoke {
                destination,
                tool,
                input,
            } => {
                let contract = match tool_contracts {
                    Some(tool_contracts) => {
                        Some(tool_contracts.get(*tool as usize).ok_or_else(|| {
                            error(
                                function_index,
                                instruction_index,
                                "tool invocation contract is out of range",
                            )
                        })?)
                    }
                    None => {
                        return Err(error(
                            function_index,
                            instruction_index,
                            "tool invocation requires a frozen tool catalog",
                        ));
                    }
                };
                let input_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *input,
                )?;
                let destination_type =
                    destination_type(function, function_index, instruction_index, *destination)?;
                let ValueType::Future(result) = destination_type else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "tool invocation destination must be Future<Result<Output, Error>>",
                    ));
                };
                let ValueType::Result(output, error_type) = result.as_ref() else {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "tool invocation destination must be Future<Result<Output, Error>>",
                    ));
                };
                let tool_name = contract.map_or("", |contract| contract.tool_name.as_str());
                let declared_error = tool_declared_error_type(module, error_type, tool_name)
                    .ok_or_else(|| {
                        error(
                            function_index,
                            instruction_index,
                            "tool invocation error wrapper is invalid",
                        )
                    })?;
                if let Some(contract) = contract {
                    require_type(
                        function_index,
                        instruction_index,
                        input_type,
                        &contract.input,
                        "tool invocation input",
                    )?;
                    require_type(
                        function_index,
                        instruction_index,
                        output,
                        &contract.output,
                        "tool invocation output",
                    )?;
                    require_type(
                        function_index,
                        instruction_index,
                        declared_error,
                        &contract.declared_error,
                        "tool invocation declared error",
                    )?;
                }
                initialized[*destination as usize] = true;
            }
            Instruction::TaskScopeEnter { scope } | Instruction::TaskScopeExit { scope } => {
                if *scope == 0 {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "explicit task scope ID must be nonzero",
                    ));
                }
                if module.async_functions.binary_search(&function_id).is_err() {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "task scope requires an async function",
                    ));
                }
            }
            Instruction::Stop { reason } => {
                let reason_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *reason,
                )?;
                require_type(
                    function_index,
                    instruction_index,
                    reason_type,
                    &ValueType::String,
                    "stop reason",
                )?;
            }
            Instruction::Return { source } => {
                let source_type = initialized_type(
                    function,
                    &initialized,
                    function_index,
                    instruction_index,
                    *source,
                )?;
                if source_type == &ValueType::Never || function.return_type == ValueType::Never {
                    return Err(error(
                        function_index,
                        instruction_index,
                        "ordinary return cannot return Never",
                    ));
                }
                require_type(
                    function_index,
                    instruction_index,
                    source_type,
                    &function.return_type,
                    "return",
                )?;
            }
        }
    }

    Ok(())
}

fn function_value_type(function: &Function) -> ValueType {
    ValueType::Function {
        parameters: function
            .parameters
            .iter()
            .map(|register| function.registers[*register as usize].clone())
            .collect(),
        return_type: Box::new(function.return_type.clone()),
        effects: function.effects,
    }
}

struct EffectOperationSignature {
    parameters: Vec<ValueType>,
    result: ValueType,
}

fn string_operation_signature(
    function_index: usize,
    instruction_index: usize,
    operation: StringOperation,
    argument_count: usize,
) -> Result<EffectOperationSignature, VerifyError> {
    let string = ValueType::String;
    let option_string = ValueType::Option(Box::new(ValueType::String));
    let list_string = ValueType::List(Box::new(ValueType::String));
    let (parameters, result) = match operation {
        StringOperation::ByteLength => (vec![string], ValueType::Int),
        StringOperation::Concat => (vec![string.clone(), string], ValueType::String),
        StringOperation::Get => (vec![string, ValueType::Int], option_string),
        StringOperation::Slice => (vec![string, ValueType::Int, ValueType::Int], option_string),
        StringOperation::Find => (
            vec![string.clone(), string],
            ValueType::Option(Box::new(ValueType::Int)),
        ),
        StringOperation::Contains | StringOperation::StartsWith | StringOperation::EndsWith => {
            (vec![string.clone(), string], ValueType::Bool)
        }
        StringOperation::Split => (
            vec![string.clone(), string],
            ValueType::Option(Box::new(list_string.clone())),
        ),
        StringOperation::Join => (vec![list_string, string], ValueType::String),
        StringOperation::TrimAscii => (vec![string], ValueType::String),
        StringOperation::FromUtf8 => (vec![ValueType::Bytes], option_string),
        StringOperation::TemplateConcat => {
            if argument_count == 0 {
                return Err(error(
                    function_index,
                    instruction_index,
                    "template concatenation requires at least one String segment",
                ));
            }
            (vec![ValueType::String; argument_count], ValueType::String)
        }
    };
    Ok(EffectOperationSignature { parameters, result })
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn effect_operation_signature(
    module: &Module,
    function: &Function,
    initialized: &[bool],
    function_index: usize,
    instruction_index: usize,
    operation: EffectOperation,
    arguments: &[Register],
) -> Result<EffectOperationSignature, VerifyError> {
    let workspace = ValueType::Workspace;
    let string = ValueType::String;
    let parameters = match operation {
        EffectOperation::ReadText | EffectOperation::ReadBytes | EffectOperation::List => {
            vec![workspace, string]
        }
        EffectOperation::WriteText | EffectOperation::Search => {
            vec![workspace, string.clone(), string]
        }
        EffectOperation::WriteBytes => vec![workspace, string, ValueType::Bytes],
        EffectOperation::HttpGet | EffectOperation::AgentMessage => {
            vec![ValueType::String]
        }
        EffectOperation::AgentAsk => {
            let [argument] = arguments else {
                return Err(error(
                    function_index,
                    instruction_index,
                    "agent.ask requires one argument",
                ));
            };
            let argument_type = initialized_type(
                function,
                initialized,
                function_index,
                instruction_index,
                *argument,
            )?;
            if prompt_output_type(argument_type).is_none() {
                return Err(error(
                    function_index,
                    instruction_index,
                    "agent.ask argument must be Prompt<T>",
                ));
            }
            vec![argument_type.clone()]
        }
        EffectOperation::PermissionRequestFile => vec![external_file_request_type()],
        EffectOperation::PermissionRequestDirectory => vec![external_directory_request_type()],
        EffectOperation::AgentTranscript => vec![transcript_query_type()],
        EffectOperation::ModelRequest | EffectOperation::UserAsk => {
            let [argument] = arguments else {
                return Err(error(
                    function_index,
                    instruction_index,
                    "typed request requires one prompt",
                ));
            };
            let prompt = initialized_type(
                function,
                initialized,
                function_index,
                instruction_index,
                *argument,
            )?;
            if prompt_output_type(prompt).is_none() {
                return Err(error(
                    function_index,
                    instruction_index,
                    "typed request argument must be Prompt<T>",
                ));
            }
            vec![prompt.clone()]
        }
        EffectOperation::SubAgentCreate => {
            let [prompt, projection] = arguments else {
                return Err(error(
                    function_index,
                    instruction_index,
                    "sub_agent.create requires prompt and projection",
                ));
            };
            let prompt = initialized_type(
                function,
                initialized,
                function_index,
                instruction_index,
                *prompt,
            )?;
            let projection = initialized_type(
                function,
                initialized,
                function_index,
                instruction_index,
                *projection,
            )?;
            if prompt_output_type(prompt) != Some(&ValueType::Unit)
                || projection != &sub_agent_projection_type()
            {
                return Err(error(
                    function_index,
                    instruction_index,
                    "sub_agent.create signature is invalid",
                ));
            }
            vec![prompt.clone(), projection.clone()]
        }
        EffectOperation::SubAgentRun => {
            let [prompt, projection] = arguments else {
                return Err(error(
                    function_index,
                    instruction_index,
                    "sub_agent.run requires prompt and projection",
                ));
            };
            let prompt = initialized_type(
                function,
                initialized,
                function_index,
                instruction_index,
                *prompt,
            )?;
            let projection = initialized_type(
                function,
                initialized,
                function_index,
                instruction_index,
                *projection,
            )?;
            if prompt_output_type(prompt).is_none() || projection != &sub_agent_projection_type() {
                return Err(error(
                    function_index,
                    instruction_index,
                    "sub_agent.run signature is invalid",
                ));
            }
            vec![prompt.clone(), projection.clone()]
        }
        EffectOperation::SubAgentMessage => vec![ValueType::SubAgent, ValueType::String],
        EffectOperation::SubAgentAsk => {
            let [target, prompt] = arguments else {
                return Err(error(
                    function_index,
                    instruction_index,
                    "sub_agent.ask requires target and prompt",
                ));
            };
            let target = initialized_type(
                function,
                initialized,
                function_index,
                instruction_index,
                *target,
            )?;
            let prompt = initialized_type(
                function,
                initialized,
                function_index,
                instruction_index,
                *prompt,
            )?;
            if target != &ValueType::SubAgent || prompt_output_type(prompt).is_none() {
                return Err(error(
                    function_index,
                    instruction_index,
                    "sub_agent.ask signature is invalid",
                ));
            }
            vec![target.clone(), prompt.clone()]
        }
    };
    let result = match operation {
        EffectOperation::AgentAsk if prompt_output_type(&parameters[0]).is_some() => {
            prompt_output_type(&parameters[0])
                .expect("checked prompt")
                .clone()
        }
        EffectOperation::ModelRequest | EffectOperation::UserAsk => {
            prompt_output_type(&parameters[0])
                .expect("checked prompt")
                .clone()
        }
        EffectOperation::SubAgentCreate => ValueType::SubAgent,
        EffectOperation::SubAgentRun => prompt_output_type(&parameters[0])
            .expect("checked prompt")
            .clone(),
        EffectOperation::SubAgentMessage => ValueType::Unit,
        EffectOperation::SubAgentAsk => prompt_output_type(&parameters[1])
            .expect("checked prompt")
            .clone(),
        _ => effect_result_type(operation, transcript_part_enum_id(module)).ok_or_else(|| {
            error(
                function_index,
                instruction_index,
                "effect result type is unavailable",
            )
        })?,
    };
    let needs_current_envelope = match operation {
        EffectOperation::AgentAsk => prompt_output_type(&parameters[0]).is_some(),
        EffectOperation::ModelRequest
        | EffectOperation::UserAsk
        | EffectOperation::SubAgentCreate
        | EffectOperation::SubAgentRun
        | EffectOperation::SubAgentMessage
        | EffectOperation::SubAgentAsk => true,
        _ => false,
    };
    let result = if needs_current_envelope {
        let error = match operation {
            EffectOperation::AgentAsk => agent_error_type(),
            EffectOperation::ModelRequest => model_error_type(),
            EffectOperation::UserAsk => user_error_type(),
            EffectOperation::SubAgentCreate
            | EffectOperation::SubAgentRun
            | EffectOperation::SubAgentMessage
            | EffectOperation::SubAgentAsk => sub_agent_error_type(),
            _ => unreachable!("guarded typed effect"),
        };
        ValueType::Result(Box::new(result), Box::new(error))
    } else {
        result
    };
    Ok(EffectOperationSignature { parameters, result })
}

fn verify_call_arguments<'a>(
    caller: &Function,
    initialized: &[bool],
    function_index: usize,
    instruction_index: usize,
    arguments: &[Register],
    expected: impl ExactSizeIterator<Item = &'a ValueType>,
) -> Result<(), VerifyError> {
    if arguments.len() != expected.len() {
        return Err(error(
            function_index,
            instruction_index,
            "call has wrong argument count",
        ));
    }
    for (argument, expected) in arguments.iter().zip(expected) {
        let actual = initialized_type(
            caller,
            initialized,
            function_index,
            instruction_index,
            *argument,
        )?;
        require_type(
            function_index,
            instruction_index,
            actual,
            expected,
            "call argument",
        )?;
    }
    Ok(())
}

fn require_effect_subset(
    module: &Module,
    function_index: usize,
    instruction_index: usize,
    required: EffectSetId,
    available: EffectSetId,
    message: &'static str,
) -> Result<(), VerifyError> {
    let required = &module.effect_sets[required as usize];
    let available = &module.effect_sets[available as usize];
    if required
        .iter()
        .all(|effect| available.binary_search(effect).is_ok())
    {
        Ok(())
    } else {
        Err(error(function_index, instruction_index, message))
    }
}

fn verify_module_value_type(
    module: &Module,
    value_type: &ValueType,
    depth: usize,
) -> Result<(), &'static str> {
    if depth > MAX_VALUE_NESTING {
        return Err("value type nesting exceeds limit");
    }
    match value_type {
        ValueType::List(element) | ValueType::Option(element) => {
            if contains_affine_type(element)
                || contains_workspace(element)
                || contains_stored_sub_agent(element)
            {
                return Err("Future and Task cannot be stored in aggregates");
            }
            verify_module_value_type(module, element, depth + 1)
        }
        ValueType::Map(key, value) => {
            if !key.is_map_key() {
                return Err("map key type is not allowed");
            }
            if contains_affine_type(key)
                || contains_affine_type(value)
                || contains_workspace(key)
                || contains_workspace(value)
                || contains_stored_sub_agent(key)
                || contains_stored_sub_agent(value)
            {
                return Err("Future and Task cannot be stored in aggregates");
            }
            verify_module_value_type(module, key, depth + 1)?;
            verify_module_value_type(module, value, depth + 1)
        }
        ValueType::Result(ok, error) => {
            let is_sub_agent_result =
                ok.as_ref() == &ValueType::SubAgent && error.as_ref() == &sub_agent_error_type();
            if contains_affine_type(ok)
                || contains_affine_type(error)
                || (contains_workspace(ok) || contains_workspace(error))
                    && !is_external_permission_result(value_type)
                || contains_stored_sub_agent(ok) && !is_sub_agent_result
                || contains_stored_sub_agent(error)
            {
                return Err("Future and Task cannot be stored in aggregates");
            }
            verify_module_value_type(module, ok, depth + 1)?;
            verify_module_value_type(module, error, depth + 1)
        }
        ValueType::Function {
            parameters,
            return_type,
            effects,
        } => {
            if *effects as usize >= module.effect_sets.len() {
                return Err("function type effect set ID is out of range");
            }
            for parameter in parameters {
                if parameter == &ValueType::Never {
                    return Err("function parameter cannot have type Never");
                }
                verify_module_value_type(module, parameter, depth + 1)?;
            }
            verify_module_value_type(module, return_type, depth + 1)
        }
        ValueType::Tuple(elements) => {
            if elements.is_empty() {
                return Err("empty tuple type must use Void");
            }
            for element in elements {
                if contains_affine_type(element)
                    || contains_workspace(element)
                    || contains_stored_sub_agent(element)
                {
                    return Err("Future and Task cannot be stored in aggregates");
                }
                verify_module_value_type(module, element, depth + 1)?;
            }
            Ok(())
        }
        ValueType::Record(fields) => verify_record_layout(module, fields, depth),
        ValueType::Future(value) | ValueType::Task(value) => {
            verify_module_value_type(module, value, depth + 1)
        }
        ValueType::Enum(id) => {
            if usize::try_from(*id).is_ok_and(|id| id < module.enum_types.len()) {
                Ok(())
            } else {
                Err("enum type ID is out of range")
            }
        }
        ValueType::Int
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::Unit
        | ValueType::Never
        | ValueType::Workspace
        | ValueType::ExternalFsAccess
        | ValueType::SubAgent
        | ValueType::Unknown => Ok(()),
    }
}

fn contains_affine_type(value_type: &ValueType) -> bool {
    match value_type {
        ValueType::Future(_) | ValueType::Task(_) => true,
        ValueType::List(value) | ValueType::Option(value) => contains_affine_type(value),
        ValueType::Map(left, right) | ValueType::Result(left, right) => {
            contains_affine_type(left) || contains_affine_type(right)
        }
        ValueType::Tuple(elements) => elements.iter().any(contains_affine_type),
        ValueType::Record(fields) => fields
            .iter()
            .any(|field| contains_affine_type(&field.value_type)),
        _ => false,
    }
}

fn contains_sub_agent(value_type: &ValueType) -> bool {
    match value_type {
        ValueType::SubAgent => true,
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value) => contains_sub_agent(value),
        ValueType::Map(left, right) | ValueType::Result(left, right) => {
            contains_sub_agent(left) || contains_sub_agent(right)
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

/// True only when the runtime value can store a handle. Function signatures
/// may mention `SubAgent`, but the function value itself stores no handle.
pub(crate) fn contains_stored_sub_agent(value_type: &ValueType) -> bool {
    match value_type {
        ValueType::SubAgent => true,
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value) => contains_stored_sub_agent(value),
        ValueType::Map(left, right) | ValueType::Result(left, right) => {
            contains_stored_sub_agent(left) || contains_stored_sub_agent(right)
        }
        ValueType::Tuple(values) => values.iter().any(contains_stored_sub_agent),
        ValueType::Record(fields) => fields
            .iter()
            .any(|field| contains_stored_sub_agent(&field.value_type)),
        _ => false,
    }
}

fn contains_workspace(value_type: &ValueType) -> bool {
    match value_type {
        ValueType::Workspace => true,
        ValueType::List(value)
        | ValueType::Option(value)
        | ValueType::Future(value)
        | ValueType::Task(value) => contains_workspace(value),
        ValueType::Map(left, right) | ValueType::Result(left, right) => {
            contains_workspace(left) || contains_workspace(right)
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
        _ => false,
    }
}

fn is_external_permission_result(value_type: &ValueType) -> bool {
    matches!(
        value_type,
        ValueType::Result(ok, error)
            if ok.as_ref() == &ValueType::Workspace && error.as_ref() == &file_error_type()
    )
}

const fn is_affine_type(value_type: &ValueType) -> bool {
    matches!(value_type, ValueType::Future(_) | ValueType::Task(_))
}

fn verify_record_layout(
    module: &Module,
    fields: &[RecordField],
    depth: usize,
) -> Result<(), &'static str> {
    let mut previous: Option<&[u8]> = None;
    for field in fields {
        if field.name.is_empty() {
            return Err("record field name must be nonempty");
        }
        let current = field.name.as_bytes();
        if previous.is_some_and(|previous| previous >= current) {
            return Err("record fields must be unique and sorted by UTF-8 bytes");
        }
        verify_module_value_type(module, &field.value_type, depth + 1)?;
        if contains_affine_type(&field.value_type) {
            return Err("Future and Task cannot be stored in aggregates");
        }
        if contains_workspace(&field.value_type) {
            return Err("Workspace cannot be stored in aggregates");
        }
        if contains_stored_sub_agent(&field.value_type) {
            return Err("SubAgent cannot be stored in aggregates");
        }
        previous = Some(current);
    }
    Ok(())
}

fn enum_variant_count(module: &Module, value_type: &ValueType) -> Option<usize> {
    match value_type {
        ValueType::Enum(id) => module
            .enum_types
            .get(*id as usize)
            .map(|ty| ty.variants.len()),
        ValueType::Option(_) | ValueType::Result(_, _) => Some(2),
        _ => None,
    }
}

fn enum_payload_types(
    module: &Module,
    value_type: &ValueType,
    variant: u32,
) -> Option<Vec<ValueType>> {
    match value_type {
        ValueType::Enum(id) => {
            let variant = module
                .enum_types
                .get(*id as usize)?
                .variants
                .get(variant as usize)?;
            Some(match &variant.payload {
                EnumPayloadType::Unit => Vec::new(),
                EnumPayloadType::Tuple(elements) => elements.clone(),
                EnumPayloadType::Record(fields) => fields
                    .iter()
                    .map(|field| field.value_type.clone())
                    .collect(),
            })
        }
        ValueType::Option(value) => match variant {
            0 => Some(Vec::new()),
            1 => Some(vec![(**value).clone()]),
            _ => None,
        },
        ValueType::Result(ok, error) => match variant {
            0 => Some(vec![(**ok).clone()]),
            1 => Some(vec![(**error).clone()]),
            _ => None,
        },
        _ => None,
    }
}

fn is_concrete_type(
    module: &Module,
    value_type: &ValueType,
    enum_cache: &mut [Option<bool>],
) -> bool {
    match value_type {
        ValueType::Workspace
        | ValueType::SubAgent
        | ValueType::Unknown
        | ValueType::Never
        | ValueType::Function { .. }
        | ValueType::Future(_)
        | ValueType::Task(_) => false,
        ValueType::List(element) | ValueType::Option(element) => {
            is_concrete_type(module, element, enum_cache)
        }
        ValueType::Map(key, value) | ValueType::Result(key, value) => {
            is_concrete_type(module, key, enum_cache) && is_concrete_type(module, value, enum_cache)
        }
        ValueType::Tuple(elements) => elements
            .iter()
            .all(|element| is_concrete_type(module, element, enum_cache)),
        ValueType::Record(fields) => fields
            .iter()
            .all(|field| is_concrete_type(module, &field.value_type, enum_cache)),
        ValueType::Enum(id) => {
            let index = *id as usize;
            if let Some(concrete) = enum_cache.get(index).and_then(|value| *value) {
                return concrete;
            }
            let Some(enum_type) = module.enum_types.get(index) else {
                return false;
            };
            let concrete = enum_type
                .variants
                .iter()
                .all(|variant| match &variant.payload {
                    EnumPayloadType::Unit => true,
                    EnumPayloadType::Tuple(elements) => elements
                        .iter()
                        .all(|element| is_concrete_type(module, element, enum_cache)),
                    EnumPayloadType::Record(fields) => fields
                        .iter()
                        .all(|field| is_concrete_type(module, &field.value_type, enum_cache)),
                });
            enum_cache[index] = Some(concrete);
            concrete
        }
        ValueType::Int
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::ExternalFsAccess
        | ValueType::Unit => true,
    }
}

fn is_equatable_in_module(
    module: &Module,
    value_type: &ValueType,
    enum_cache: &mut [Option<bool>],
) -> bool {
    match value_type {
        ValueType::Workspace
        | ValueType::SubAgent
        | ValueType::Unknown
        | ValueType::Function { .. }
        | ValueType::Future(_)
        | ValueType::Task(_) => false,
        ValueType::Enum(id) => {
            let index = *id as usize;
            if let Some(equatable) = enum_cache.get(index).and_then(|value| *value) {
                return equatable;
            }
            let Some(enum_type) = module.enum_types.get(index) else {
                return false;
            };
            let equatable = enum_type
                .variants
                .iter()
                .all(|variant| match &variant.payload {
                    EnumPayloadType::Unit => true,
                    EnumPayloadType::Tuple(elements) => elements
                        .iter()
                        .all(|element| is_equatable_in_module(module, element, enum_cache)),
                    EnumPayloadType::Record(fields) => fields
                        .iter()
                        .all(|field| is_equatable_in_module(module, &field.value_type, enum_cache)),
                });
            enum_cache[index] = Some(equatable);
            equatable
        }
        ValueType::List(element) | ValueType::Option(element) => {
            is_equatable_in_module(module, element, enum_cache)
        }
        ValueType::Map(key, value) | ValueType::Result(key, value) => {
            is_equatable_in_module(module, key, enum_cache)
                && is_equatable_in_module(module, value, enum_cache)
        }
        ValueType::Tuple(elements) => elements
            .iter()
            .all(|element| is_equatable_in_module(module, element, enum_cache)),
        ValueType::Record(fields) => fields
            .iter()
            .all(|field| is_equatable_in_module(module, &field.value_type, enum_cache)),
        ValueType::Int
        | ValueType::Bool
        | ValueType::Float
        | ValueType::String
        | ValueType::Bytes
        | ValueType::ExternalFsAccess
        | ValueType::Unit
        | ValueType::Never => true,
    }
}

fn error(function: usize, instruction: usize, message: &'static str) -> VerifyError {
    VerifyError::instruction(function, instruction, message)
}

fn destination_type(
    function: &Function,
    function_index: usize,
    instruction_index: usize,
    destination: Register,
) -> Result<&ValueType, VerifyError> {
    let destination_type = register_type(function, function_index, instruction_index, destination)?;
    if destination_type == &ValueType::Never {
        Err(error(
            function_index,
            instruction_index,
            "cannot initialize Never register",
        ))
    } else {
        Ok(destination_type)
    }
}

fn initialized_type<'a>(
    function: &'a Function,
    initialized: &[bool],
    function_index: usize,
    instruction_index: usize,
    register: Register,
) -> Result<&'a ValueType, VerifyError> {
    let value_type = register_type(function, function_index, instruction_index, register)?;
    if initialized[register as usize] {
        Ok(value_type)
    } else {
        Err(error(
            function_index,
            instruction_index,
            "register is not initialized",
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn require_unary_type(
    function: &Function,
    initialized: &[bool],
    function_index: usize,
    instruction_index: usize,
    destination: Register,
    source: Register,
    expected: &ValueType,
    operation: &'static str,
) -> Result<(), VerifyError> {
    let destination_type =
        destination_type(function, function_index, instruction_index, destination)?;
    let source_type = initialized_type(
        function,
        initialized,
        function_index,
        instruction_index,
        source,
    )?;
    require_type(
        function_index,
        instruction_index,
        destination_type,
        expected,
        operation,
    )?;
    require_type(
        function_index,
        instruction_index,
        source_type,
        expected,
        operation,
    )
}

#[allow(clippy::too_many_arguments)]
fn require_binary_type(
    function: &Function,
    initialized: &[bool],
    function_index: usize,
    instruction_index: usize,
    destination: Register,
    left: Register,
    right: Register,
    expected: &ValueType,
    operation: &'static str,
) -> Result<(), VerifyError> {
    let destination_type =
        destination_type(function, function_index, instruction_index, destination)?;
    let left_type = initialized_type(
        function,
        initialized,
        function_index,
        instruction_index,
        left,
    )?;
    let right_type = initialized_type(
        function,
        initialized,
        function_index,
        instruction_index,
        right,
    )?;
    require_type(
        function_index,
        instruction_index,
        destination_type,
        expected,
        operation,
    )?;
    require_type(
        function_index,
        instruction_index,
        left_type,
        expected,
        operation,
    )?;
    require_type(
        function_index,
        instruction_index,
        right_type,
        expected,
        operation,
    )
}

fn require_type(
    function_index: usize,
    instruction_index: usize,
    actual: &ValueType,
    expected: &ValueType,
    context: &'static str,
) -> Result<(), VerifyError> {
    if actual == expected {
        Ok(())
    } else {
        Err(error(function_index, instruction_index, context))
    }
}

fn register_type(
    function: &Function,
    function_index: usize,
    instruction_index: usize,
    register: Register,
) -> Result<&ValueType, VerifyError> {
    function.registers.get(register as usize).ok_or_else(|| {
        error(
            function_index,
            instruction_index,
            "register is out of range",
        )
    })
}
