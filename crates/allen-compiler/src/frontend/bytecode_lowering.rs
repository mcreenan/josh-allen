//! Coordinated bytecode-v13 lowering after resolution and checking.
//!
//! HIR and MIR are emitted from the same typed expression walk so bytecode,
//! debug spans, ownership transitions, and both IRs cannot observe different
//! control-flow or evaluation order. Their public representations and MIR
//! structural validation live in the focused `hir` and `mir` modules.

use super::checking::{
    SemanticType, concrete_type, contains_affine, contains_stored_sub_agent, contains_sub_agent,
    contains_workspace, effect_id, expected_type_diagnostic_code, is_affine, literal_map_key,
};
use super::resolution::{
    CollectionBuiltin, FunctionInfo, ResolvedBundle, StandardBuiltin, capability_builtin_callee,
    collection_builtin_callee, direct_capability_inspection_body_span, effect_operation_signature,
    is_task_snapshot_callee, required_body_effects, resolve_function_name, resolve_named_type,
    semantic_type, standard_builtin_callee, string_builtin_callee, sub_agent_projection_type,
    tool_callee,
};
use super::{
    BTreeMap, BTreeSet, Binary, CapabilityOperation, CheckedIntOperation, CompareOp, Constant,
    Conversion, DebugLocation, Diagnostic, EffectOperation, EffectSetId, EnumPayloadType,
    ExternalFsAccess, Function, FunctionId, HirExpr, HirExprKind, HirForSource, HirFunction,
    HirLoopBinding, HirLoopBindingElement, HirTemplatePart, Instruction, LocalBinding, LoweredBody,
    LoweredElse, LoweredEnumValuePayload, LoweredExpr, LoweredExprKind, LoweredForSource,
    LoweredFunction, LoweredLoopBinding, LoweredPattern, LoweredStatement, LoweredTemplatePart,
    LoweredType, MirBlock, MirCleanupKind, MirFunction, MirOperation, MirOwnership,
    MirOwnershipState, MirSuspension, MirTaskScope, MirTerminator, NumericBinaryOp, RecordField,
    Register, SafeCollectionOperation, SourceSpan, Span, SpanId, StringOperation, SymbolId, TypeId,
    Unary, ValueType, agent_error_type, is_strict_schema_type, model_error_type,
    prompt_output_type, prompt_type, sub_agent_error_type, task_snapshot_type,
    template_interpolations, user_error_type,
};

pub(super) struct GlobalLowering<'a> {
    pub(super) bundle: &'a ResolvedBundle,
    pub(super) effect_sets: Vec<Vec<String>>,
    pub(super) constants: Vec<Constant>,
    pub(super) functions: Vec<Option<Function>>,
    pub(super) monomorphs: Vec<(SymbolId, Vec<ValueType>, FunctionId)>,
    pub(super) hir_modules: BTreeMap<String, Vec<HirFunction>>,
    pub(super) mir_functions: Vec<MirFunction>,
    pub(super) types: Vec<ValueType>,
    pub(super) spans: Vec<SourceSpan>,
    pub(super) debug_sources: Vec<String>,
    pub(super) debug_locations: Vec<DebugLocation>,
    pub(super) next_symbol: SymbolId,
    pub(super) async_functions: BTreeSet<FunctionId>,
}

impl GlobalLowering<'_> {
    pub(super) fn allocate_symbol(&mut self) -> SymbolId {
        let symbol = self.next_symbol;
        self.next_symbol = self.next_symbol.checked_add(1).expect("symbol ID fits");
        symbol
    }
    pub(super) fn intern_type(&mut self, value_type: ValueType) -> TypeId {
        if let Some(index) = self.types.iter().position(|item| item == &value_type) {
            return u32::try_from(index).expect("type index fits");
        }
        let id = u32::try_from(self.types.len()).expect("type index fits");
        self.types.push(value_type);
        id
    }

    pub(super) fn intern_span(&mut self, module: &str, span: Span) -> SpanId {
        let source_span = SourceSpan {
            module: module.to_owned(),
            span,
        };
        if let Some(index) = self.spans.iter().position(|item| item == &source_span) {
            return u32::try_from(index).expect("span index fits");
        }
        let id = u32::try_from(self.spans.len()).expect("span index fits");
        self.spans.push(source_span);
        id
    }

    pub(super) fn constant(&mut self, constant: Constant) -> Result<u32, Diagnostic> {
        if let Some(index) = self.constants.iter().position(|item| item == &constant) {
            return u32::try_from(index).map_err(|_| {
                Diagnostic::new("E3005", "too many constants", Span { start: 0, end: 0 })
            });
        }
        let id = u32::try_from(self.constants.len()).map_err(|_| {
            Diagnostic::new("E3005", "too many constants", Span { start: 0, end: 0 })
        })?;
        self.constants.push(constant);
        Ok(id)
    }
}

pub(super) struct CompiledExpr {
    register: Register,
    value_type: ValueType,
    effects: EffectSetId,
    hir: HirExpr,
}

#[derive(Clone, Copy)]
pub(super) struct MirRegionCapture {
    outer_tail: Option<u32>,
    entry_start: usize,
}

#[derive(Clone, Copy, Default)]
pub(super) struct CapturedMirRegion {
    entry: Option<u32>,
    tail: Option<u32>,
}

pub(super) struct FunctionLowering<'a, 'b> {
    global: &'a mut GlobalLowering<'b>,
    info: FunctionInfo,
    return_type: ValueType,
    registers: Vec<ValueType>,
    parameters: Vec<Register>,
    captures: Vec<Register>,
    bindings: BTreeMap<String, LocalBinding>,
    code: Vec<Instruction>,
    instruction_spans: BTreeMap<usize, Span>,
    mir: Vec<MirOperation>,
    mir_blocks: Vec<MirBlock>,
    mir_suspensions: Vec<MirSuspension>,
    mir_task_scopes: Vec<MirTaskScope>,
    mir_ownership: Vec<MirOwnership>,
    ownership_states: BTreeMap<Register, OwnershipRecord>,
    active_scopes: Vec<u32>,
    next_scope: u32,
    mir_continuations: BTreeSet<u32>,
    mir_entries: Vec<u32>,
    mir_tail: Option<u32>,
    loops: Vec<LoopContext>,
    control_reachable: bool,
    runtime_terminal_values: BTreeSet<Register>,
    sub_agent_value_scopes: BTreeMap<Register, u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OwnershipRecord {
    scope: u32,
    state: MirOwnershipState,
    must_consume: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BindingState {
    moved: bool,
    value_scope: u32,
}

impl From<&LocalBinding> for BindingState {
    fn from(binding: &LocalBinding) -> Self {
        Self {
            moved: binding.moved,
            value_scope: binding.value_scope,
        }
    }
}

#[derive(Clone)]
pub(super) struct LoopEdgeState {
    bindings: BTreeMap<String, BindingState>,
    ownership: BTreeMap<Register, OwnershipRecord>,
}

#[derive(Clone, Copy)]
pub(super) struct LoopEntryStates<'a> {
    repeat: &'a LoopEdgeState,
    exit: &'a LoopEdgeState,
}

#[derive(Clone)]
pub(super) struct LoopContext {
    scope_depth: usize,
    outer_bindings: BTreeMap<String, LocalBinding>,
    outer_ownership: BTreeMap<Register, OwnershipRecord>,
    break_jumps: Vec<usize>,
    continue_jumps: Vec<usize>,
    break_mir_blocks: Vec<u32>,
    continue_mir_blocks: Vec<u32>,
    break_edges: Vec<LoopEdgeState>,
    continue_edges: Vec<LoopEdgeState>,
}

pub(super) struct CompiledLoop {
    hir: HirExpr,
    register: Register,
    falls_through: bool,
}

impl FunctionLowering<'_, '_> {
    pub(super) fn runtime_falls_through(&self, value: &CompiledExpr) -> bool {
        value.value_type != ValueType::Never
            && !self.runtime_terminal_values.contains(&value.register)
    }

    pub(super) fn validate_loop_body_type(
        body: &LoweredBody,
        value: &CompiledExpr,
    ) -> Result<(), Diagnostic> {
        if matches!(value.value_type, ValueType::Unit | ValueType::Never) {
            return Ok(());
        }
        Err(Diagnostic::new(
            "E3007",
            format!(
                "loop body must have type Void or Never, found {}",
                value.value_type
            ),
            body.tail.as_ref().map_or(body.span, |tail| tail.span),
        ))
    }

    pub(super) fn current_scope(&self) -> u32 {
        self.active_scopes.last().copied().unwrap_or(0)
    }

    pub(super) fn sub_agent_value_scope(&self, value: &CompiledExpr) -> u32 {
        value
            .hir
            .symbol
            .and_then(|symbol| {
                self.bindings
                    .values()
                    .find(|binding| binding.symbol == symbol)
                    .map(|binding| binding.value_scope)
            })
            .or_else(|| self.sub_agent_value_scopes.get(&value.register).copied())
            .or_else(|| {
                self.bindings
                    .values()
                    .find(|binding| binding.register == value.register)
                    .map(|binding| binding.value_scope)
            })
            .unwrap_or_else(|| self.current_scope())
    }

    pub(super) fn scope_outlives(&self, source: u32, target: u32) -> bool {
        matches!(
            (self.scope_depth(source), self.scope_depth(target)),
            (Some(source), Some(target)) if source <= target
        )
    }

    pub(super) fn scope_depth(&self, scope: u32) -> Option<usize> {
        if scope == 0 {
            Some(0)
        } else {
            self.active_scopes
                .iter()
                .position(|active| *active == scope)
                .map(|index| index + 1)
        }
    }

    pub(super) fn deeper_scope(&self, left: u32, right: u32) -> u32 {
        if self.scope_depth(left).unwrap_or(usize::MAX)
            >= self.scope_depth(right).unwrap_or(usize::MAX)
        {
            left
        } else {
            right
        }
    }

    pub(super) fn mark_instruction(&mut self, instruction: usize, span: Span) {
        self.instruction_spans.insert(instruction, span);
    }

    pub(super) fn mark_last_instruction(&mut self, span: Span) {
        self.mark_instruction(self.code.len() - 1, span);
    }

    pub(super) fn allocate(&mut self, value_type: ValueType) -> Result<Register, Diagnostic> {
        let register = u16::try_from(self.registers.len()).map_err(|_| {
            Diagnostic::new(
                "E3005",
                "function needs too many registers",
                self.info.lowered.name_span,
            )
        })?;
        self.registers.push(value_type);
        Ok(register)
    }

    pub(super) fn empty_effects(&self) -> EffectSetId {
        effect_id(&self.global.effect_sets, &[])
    }

    pub(super) fn union_effects(
        &self,
        effects: impl IntoIterator<Item = EffectSetId>,
    ) -> EffectSetId {
        let mut union = BTreeSet::new();
        for effect_set in effects {
            union.extend(self.global.effect_sets[effect_set as usize].iter().cloned());
        }
        effect_id(
            &self.global.effect_sets,
            &union.into_iter().collect::<Vec<_>>(),
        )
    }

    pub(super) fn record_ownership(
        &mut self,
        register: Register,
        scope: u32,
        state: MirOwnershipState,
        must_consume: bool,
    ) {
        self.ownership_states.insert(
            register,
            OwnershipRecord {
                scope,
                state,
                must_consume,
            },
        );
        self.mir_ownership.push(MirOwnership {
            temporary: u32::from(register),
            scope,
            state,
            must_consume,
        });
    }

    pub(super) fn consume_ownership(&mut self, register: Register, state: MirOwnershipState) {
        if let Some(ownership) = self.ownership_states.get(&register).copied() {
            self.record_ownership(register, ownership.scope, state, ownership.must_consume);
        }
    }

    pub(super) fn must_consume(&self, register: Register) -> bool {
        self.ownership_states
            .get(&register)
            .is_some_and(|ownership| ownership.must_consume)
    }

    pub(super) fn next_mir_block(&self) -> u32 {
        u32::try_from(self.mir_blocks.len() + 1).expect("MIR block ID fits")
    }

    pub(super) fn cleanup_operations(&self, kind: MirCleanupKind) -> Vec<MirOperation> {
        self.active_scopes
            .iter()
            .rev()
            .map(|scope| MirOperation::TaskScopeCleanup {
                scope: *scope,
                kind,
            })
            .collect()
    }

    pub(super) fn invalidate_scope_local_sub_agents(&mut self, scope: u32) {
        self.bindings.retain(|_, binding| {
            binding.scope != scope || !contains_stored_sub_agent(&binding.value_type)
        });
    }

    pub(super) fn terminate_source_dead_path(
        &mut self,
        ownership_at_entry: &BTreeSet<Register>,
        span: Span,
    ) -> Result<Register, Diagnostic> {
        let path_local_live = self
            .ownership_states
            .iter()
            .filter_map(|(register, ownership)| {
                (!ownership_at_entry.contains(register)
                    && ownership.state == MirOwnershipState::Live)
                    .then_some(*register)
            })
            .collect::<Vec<_>>();
        for register in path_local_live {
            self.consume_ownership(register, MirOwnershipState::ScopeJoined);
        }
        let reason = self.allocate(ValueType::String)?;
        let constant = self.global.constant(Constant::String(
            "source-unreachable control flow".to_owned(),
        ))?;
        self.code.push(Instruction::Const {
            destination: reason,
            constant,
        });
        self.mark_last_instruction(span);
        self.code.push(Instruction::Stop { reason });
        self.mark_last_instruction(span);
        Ok(reason)
    }

    pub(super) fn register_mir_region(&mut self, entry: u32, continuation: u32) {
        if let Some(previous) = self.mir_tail {
            self.mir_blocks[previous as usize - 1].terminator =
                MirTerminator::Goto { target: entry };
        }
        self.mir_entries.push(entry);
        self.mir_continuations.insert(continuation);
        self.mir_tail = Some(continuation);
    }

    pub(super) fn begin_nested_mir_region(&mut self) -> MirRegionCapture {
        MirRegionCapture {
            outer_tail: self.mir_tail.take(),
            entry_start: self.mir_entries.len(),
        }
    }

    pub(super) fn finish_nested_mir_region(
        &mut self,
        capture: MirRegionCapture,
    ) -> CapturedMirRegion {
        let entries = self.mir_entries.split_off(capture.entry_start);
        let region = CapturedMirRegion {
            entry: entries.first().copied(),
            tail: self.mir_tail.take(),
        };
        self.mir_tail = capture.outer_tail;
        region
    }

    pub(super) fn set_mir_handoff(&mut self, block: u32, terminator: MirTerminator) {
        self.mir_blocks[block as usize - 1].terminator = terminator;
        self.mir_continuations.remove(&block);
    }

    pub(super) fn compile_without_runtime<T>(
        &mut self,
        compile: impl FnOnce(&mut Self) -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        let global_constants_len = self.global.constants.len();
        let global_functions_len = self.global.functions.len();
        let global_monomorphs_len = self.global.monomorphs.len();
        let global_hir_modules = self.global.hir_modules.clone();
        let global_mir_functions_len = self.global.mir_functions.len();
        let global_debug_locations_len = self.global.debug_locations.len();
        let global_async_functions = self.global.async_functions.clone();
        let code_len = self.code.len();
        let register_len = self.registers.len();
        let mir_len = self.mir.len();
        let mir_block_len = self.mir_blocks.len();
        let suspension_len = self.mir_suspensions.len();
        let task_scope_len = self.mir_task_scopes.len();
        let mir_ownership_len = self.mir_ownership.len();
        let entry_len = self.mir_entries.len();
        let bindings = self.bindings.clone();
        let ownership_states = self.ownership_states.clone();
        let loops = self.loops.clone();
        let active_scopes = self.active_scopes.clone();
        let continuations = self.mir_continuations.clone();
        let tail = self.mir_tail.take();
        let runtime_terminal_values = self.runtime_terminal_values.clone();
        let sub_agent_value_scopes = self.sub_agent_value_scopes.clone();
        let control_reachable = self.control_reachable;
        self.control_reachable = false;

        let result = compile(self);

        self.global.constants.truncate(global_constants_len);
        self.global.functions.truncate(global_functions_len);
        self.global.monomorphs.truncate(global_monomorphs_len);
        self.global.hir_modules = global_hir_modules;
        self.global.mir_functions.truncate(global_mir_functions_len);
        self.global
            .debug_locations
            .truncate(global_debug_locations_len);
        self.global.async_functions = global_async_functions;
        self.code.truncate(code_len);
        self.instruction_spans
            .retain(|instruction, _| *instruction < code_len);
        self.registers.truncate(register_len);
        self.mir.truncate(mir_len);
        self.mir_blocks.truncate(mir_block_len);
        self.mir_suspensions.truncate(suspension_len);
        self.mir_task_scopes.truncate(task_scope_len);
        self.mir_ownership.truncate(mir_ownership_len);
        self.mir_entries.truncate(entry_len);
        self.bindings = bindings;
        self.ownership_states = ownership_states;
        self.loops = loops;
        self.active_scopes = active_scopes;
        self.mir_continuations = continuations;
        self.mir_tail = tail;
        self.runtime_terminal_values = runtime_terminal_values;
        self.sub_agent_value_scopes = sub_agent_value_scopes;
        self.control_reachable = control_reachable;
        result
    }

    pub(super) fn compile_static_loop_body(
        &mut self,
        body: &LoweredBody,
    ) -> Result<(HirExpr, EffectSetId), Diagnostic> {
        self.compile_without_runtime(|lowering| {
            lowering.push_loop();
            let (body_hir, body_value, _) = lowering.compile_block_value(body)?;
            Self::validate_loop_body_type(body, &body_value)?;
            let effects = body_hir.effects;
            let context = lowering.loops.pop().expect("static loop remains active");
            lowering.bindings = context.outer_bindings;
            lowering.ownership_states = context.outer_ownership;
            Ok((body_hir, effects))
        })
    }

    pub(super) fn compile_static_for_body(
        &mut self,
        binding: &LoweredLoopBinding,
        yielded_type: &ValueType,
        body: &LoweredBody,
    ) -> Result<(HirLoopBinding, HirExpr, EffectSetId), Diagnostic> {
        self.compile_without_runtime(|lowering| {
            lowering.push_loop();
            let yielded = lowering.allocate(yielded_type.clone())?;
            let hir_binding = lowering.install_loop_binding(binding, yielded, yielded_type)?;
            let (body_hir, body_value, _) = lowering.compile_block_value(body)?;
            Self::validate_loop_body_type(body, &body_value)?;
            let effects = body_hir.effects;
            let context = lowering
                .loops
                .pop()
                .expect("static for loop remains active");
            lowering.bindings = context.outer_bindings;
            lowering.ownership_states = context.outer_ownership;
            Ok((hir_binding, body_hir, effects))
        })
    }

    pub(super) fn hir(
        &mut self,
        kind: HirExprKind,
        symbol: Option<SymbolId>,
        value_type: &ValueType,
        effects: EffectSetId,
        span: Span,
    ) -> HirExpr {
        let ty = self.global.intern_type(value_type.clone());
        let span = self.global.intern_span(&self.info.module, span);
        HirExpr {
            kind,
            symbol,
            ty,
            effects,
            span,
        }
    }

    pub(super) fn loop_entry_state(&self) -> LoopEdgeState {
        LoopEdgeState {
            bindings: self
                .bindings
                .iter()
                .map(|(name, binding)| (name.clone(), BindingState::from(binding)))
                .collect(),
            ownership: self.ownership_states.clone(),
        }
    }

    pub(super) fn loop_edge_state(
        &self,
        context: &LoopContext,
        span: Span,
    ) -> Result<LoopEdgeState, Diagnostic> {
        if self.ownership_states.iter().any(|(register, ownership)| {
            !context.outer_ownership.contains_key(register)
                && ownership.state == MirOwnershipState::Live
                && matches!(
                    self.registers[*register as usize],
                    ValueType::Future(_) | ValueType::Task(_) | ValueType::SubAgent
                )
        }) || self.bindings.iter().any(|(name, binding)| {
            !context.outer_bindings.contains_key(name)
                && contains_stored_sub_agent(&binding.value_type)
        }) {
            return Err(Diagnostic::new(
                "E3011",
                "loop edge leaves a live affine future, task, or SubAgent obligation",
                span,
            ));
        }
        Ok(LoopEdgeState {
            bindings: context
                .outer_bindings
                .keys()
                .filter_map(|name| {
                    self.bindings
                        .get(name)
                        .map(|binding| (name.clone(), BindingState::from(binding)))
                })
                .collect(),
            ownership: context
                .outer_ownership
                .keys()
                .filter_map(|register| {
                    self.ownership_states
                        .get(register)
                        .map(|ownership| (*register, *ownership))
                })
                .collect(),
        })
    }

    pub(super) fn validate_loop_edge(
        expected: &LoopEdgeState,
        found: &LoopEdgeState,
        kind: &str,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if expected.bindings != found.bindings || expected.ownership != found.ownership {
            return Err(Diagnostic::new(
                "E3011",
                format!("{kind} must preserve the loop's affine ownership state"),
                span,
            ));
        }
        Ok(())
    }

    pub(super) fn validate_loop_repeat_edge(
        &self,
        context: &LoopContext,
        entry: &LoopEdgeState,
        found: &LoopEdgeState,
        kind: &str,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let live_binding = found.bindings.iter().any(|(name, state)| {
            context.outer_bindings.get(name).is_some_and(|binding| {
                (!state.moved && is_affine(&binding.value_type))
                    || contains_stored_sub_agent(&binding.value_type)
            })
        });
        let live_ownership = found.ownership.iter().any(|(register, ownership)| {
            ownership.state == MirOwnershipState::Live
                && matches!(
                    self.registers[*register as usize],
                    ValueType::Future(_) | ValueType::Task(_) | ValueType::SubAgent
                )
        });
        if live_binding || live_ownership {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "live affine future, task, or SubAgent ownership cannot cross a reachable {kind}"
                ),
                span,
            ));
        }
        Self::validate_loop_edge(entry, found, kind, span)
    }

    pub(super) fn exit_scopes_for_loop_control(
        &mut self,
        scope_depth: usize,
        span: Span,
    ) -> Result<(), Diagnostic> {
        for scope in self.active_scopes[scope_depth..]
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>()
        {
            let live = self
                .ownership_states
                .iter()
                .filter_map(|(register, ownership)| {
                    (ownership.scope == scope && ownership.state == MirOwnershipState::Live)
                        .then_some(*register)
                })
                .collect::<Vec<_>>();
            for register in live {
                let nested_task_result = matches!(
                    &self.registers[register as usize],
                    ValueType::Task(result) if is_affine(result)
                );
                let hidden_future_obligation =
                    matches!(self.registers[register as usize], ValueType::Future(_))
                        && self.must_consume(register);
                if nested_task_result || hidden_future_obligation {
                    return Err(Diagnostic::new(
                        "E3011",
                        "nested affine result must be awaited before loop control exits its scope",
                        span,
                    ));
                }
                self.record_ownership(register, scope, MirOwnershipState::ScopeJoined, true);
            }
            self.code.push(Instruction::TaskScopeExit { scope });
            self.mark_last_instruction(span);
            self.mir.push(MirOperation::TaskScopeExit { scope });
            self.mir.push(MirOperation::TaskScopeCleanup {
                scope,
                kind: MirCleanupKind::NormalJoin,
            });
            self.invalidate_scope_local_sub_agents(scope);
        }
        Ok(())
    }

    pub(super) fn compile_loop_control(
        &mut self,
        is_break: bool,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let Some(context) = self.loops.last() else {
            return Err(Diagnostic::new(
                "E3005",
                if is_break {
                    "break is only valid inside a loop"
                } else {
                    "continue is only valid inside a loop"
                },
                span,
            ));
        };
        if !self.control_reachable {
            // The source-dead branch is still present in bytecode for static HIR, effect, and
            // diagnostic checking. Terminate that impossible verifier path instead of inventing
            // a break/continue edge, and account for values created only on the dead path as
            // permanently cancelled without changing the loop-entry ownership snapshot.
            let ownership_at_entry = context.outer_ownership.keys().copied().collect();
            let reason = self.terminate_source_dead_path(&ownership_at_entry, span)?;
            self.mir.push(MirOperation::Constant {
                destination: u32::from(reason),
            });
            let register = self.allocate(ValueType::Unit)?;
            let effects = self.empty_effects();
            return Ok(CompiledExpr {
                register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    if is_break {
                        HirExprKind::Break
                    } else {
                        HirExprKind::Continue
                    },
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        let scope_depth = context.scope_depth;
        self.exit_scopes_for_loop_control(scope_depth, span)?;
        let context = self.loops.last().expect("loop context remains active");
        let edge = self.loop_edge_state(context, span)?;
        let jump = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        self.mark_last_instruction(span);
        let mir_block = self.next_mir_block();
        self.mir_blocks.push(MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        });
        if let Some(previous) = self.mir_tail.take() {
            self.set_mir_handoff(previous, MirTerminator::Goto { target: mir_block });
        }
        self.mir_entries.push(mir_block);
        let context = self.loops.last_mut().expect("loop context remains active");
        if is_break {
            context.break_jumps.push(jump);
            context.break_mir_blocks.push(mir_block);
            context.break_edges.push(edge);
        } else {
            context.continue_jumps.push(jump);
            context.continue_mir_blocks.push(mir_block);
            context.continue_edges.push(edge);
        }
        let register = self.allocate(ValueType::Unit)?;
        let effects = self.empty_effects();
        Ok(CompiledExpr {
            register,
            value_type: ValueType::Never,
            effects,
            hir: self.hir(
                if is_break {
                    HirExprKind::Break
                } else {
                    HirExprKind::Continue
                },
                None,
                &ValueType::Never,
                effects,
                span,
            ),
        })
    }

    pub(super) fn push_loop(&mut self) -> LoopEdgeState {
        let entry = self.loop_entry_state();
        self.loops.push(LoopContext {
            scope_depth: self.active_scopes.len(),
            outer_bindings: self.bindings.clone(),
            outer_ownership: self.ownership_states.clone(),
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            break_mir_blocks: Vec::new(),
            continue_mir_blocks: Vec::new(),
            break_edges: Vec::new(),
            continue_edges: Vec::new(),
        });
        entry
    }

    pub(super) fn restore_loop_depth(&mut self, depth: usize) {
        while self.loops.len() > depth {
            let context = self.loops.pop().expect("loop context exceeds saved depth");
            self.bindings = context.outer_bindings;
            self.ownership_states = context.outer_ownership;
        }
    }

    pub(super) fn finish_loop(
        &mut self,
        entries: LoopEntryStates<'_>,
        body_falls_through: bool,
        continue_target: u32,
        break_target: u32,
        zero_iteration: bool,
        span: Span,
    ) -> Result<(LoopContext, bool), Diagnostic> {
        let context = self.loops.pop().expect("loop context is active");
        if body_falls_through {
            let edge = self.loop_edge_state(&context, span)?;
            self.validate_loop_repeat_edge(
                &context,
                entries.repeat,
                &edge,
                "loop back-edge",
                span,
            )?;
        }
        for edge in &context.continue_edges {
            self.validate_loop_repeat_edge(&context, entries.repeat, edge, "continue", span)?;
        }
        let mut joined = zero_iteration.then(|| entries.exit.clone());
        for edge in &context.break_edges {
            if let Some(expected) = &joined {
                Self::validate_loop_edge(expected, edge, "break", span)?;
            } else {
                joined = Some(edge.clone());
            }
        }
        for jump in &context.continue_jumps {
            self.code[*jump] = Instruction::Jump {
                target: continue_target,
            };
        }
        for jump in &context.break_jumps {
            self.code[*jump] = Instruction::Jump {
                target: break_target,
            };
        }
        self.bindings = context.outer_bindings.clone();
        self.ownership_states = context.outer_ownership.clone();
        if let Some(joined) = joined {
            for (name, state) in joined.bindings {
                if let Some(binding) = self.bindings.get_mut(&name) {
                    binding.moved = state.moved;
                    binding.value_scope = state.value_scope;
                }
            }
            self.ownership_states.extend(joined.ownership);
        }
        let falls_through = zero_iteration || !context.break_edges.is_empty();
        Ok((context, falls_through))
    }

    pub(super) fn install_loop_binding(
        &mut self,
        binding: &LoweredLoopBinding,
        value: Register,
        value_type: &ValueType,
    ) -> Result<HirLoopBinding, Diagnostic> {
        let element_types = if binding.tuple {
            let ValueType::Tuple(elements) = value_type else {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("tuple loop binding requires a Tuple value, found {value_type}"),
                    binding.span,
                ));
            };
            if elements.len() != binding.elements.len() {
                return Err(Diagnostic::new(
                    "E3007",
                    format!(
                        "tuple loop binding has {} elements, but the iterator yields {}",
                        binding.elements.len(),
                        elements.len()
                    ),
                    binding.span,
                ));
            }
            elements.clone()
        } else {
            vec![value_type.clone()]
        };
        let mut hir_elements = Vec::with_capacity(binding.elements.len());
        for (index, (element, element_type)) in binding
            .elements
            .iter()
            .zip(element_types.into_iter())
            .enumerate()
        {
            let register = if binding.tuple {
                let register = self.allocate(element_type.clone())?;
                self.code.push(Instruction::TupleGet {
                    destination: register,
                    tuple: value,
                    index: u32::try_from(index).expect("loop tuple binding index fits"),
                });
                self.mark_last_instruction(element.span);
                self.mir.push(MirOperation::Binary {
                    destination: u32::from(register),
                });
                register
            } else {
                value
            };
            let symbol = if let Some(name) = &element.name {
                if self.bindings.contains_key(name) {
                    return Err(Diagnostic::new(
                        "E3005",
                        format!("duplicate local binding '{name}'"),
                        element.span,
                    ));
                }
                let symbol = self.global.allocate_symbol();
                let scope = self.active_scopes.last().copied().unwrap_or(0);
                self.bindings.insert(
                    name.clone(),
                    LocalBinding {
                        register,
                        symbol,
                        value_type: element_type.clone(),
                        scope,
                        value_scope: scope,
                        mutable: false,
                        moved: false,
                    },
                );
                Some(symbol)
            } else {
                None
            };
            hir_elements.push(HirLoopBindingElement {
                symbol,
                ty: self.global.intern_type(element_type),
                span: self.global.intern_span(&self.info.module, element.span),
            });
        }
        Ok(HirLoopBinding {
            elements: hir_elements,
            tuple: binding.tuple,
            span: self.global.intern_span(&self.info.module, binding.span),
        })
    }

    #[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
    pub(super) fn register_loop_mir_region(
        &mut self,
        header: u32,
        repeat_target: u32,
        condition_operations: Vec<MirOperation>,
        body_operations: Vec<MirOperation>,
        body_region: CapturedMirRegion,
        body_falls_through: bool,
        backedge_reachable: bool,
        continue_mir_blocks: &[u32],
        break_mir_blocks: &[u32],
        has_zero_iteration: bool,
    ) {
        let body = header + 1;
        let exit = self.next_mir_block();
        let has_continue = !continue_mir_blocks.is_empty();
        let has_break = !break_mir_blocks.is_empty();
        for block in continue_mir_blocks {
            self.mir_blocks[*block as usize - 1].terminator = MirTerminator::Goto {
                target: repeat_target,
            };
        }
        for block in break_mir_blocks {
            self.mir_blocks[*block as usize - 1].terminator = MirTerminator::Goto { target: exit };
        }
        let synthetic_stop = body_falls_through
            && !backedge_reachable
            && matches!(self.code.last(), Some(Instruction::Stop { .. }));
        let body_terminator = if synthetic_stop {
            let Some(Instruction::Stop { reason }) = self.code.last() else {
                unreachable!("synthetic stop was matched")
            };
            MirTerminator::Stop {
                reason: u32::from(*reason),
            }
        } else if backedge_reachable || has_continue {
            MirTerminator::Goto {
                target: repeat_target,
            }
        } else if body_falls_through && has_zero_iteration {
            MirTerminator::Goto { target: exit }
        } else if !body_falls_through {
            match self.code.last() {
                Some(Instruction::Return { source }) => MirTerminator::Return {
                    source: u32::from(*source),
                },
                Some(Instruction::Stop { reason }) => MirTerminator::Stop {
                    reason: u32::from(*reason),
                },
                _ => MirTerminator::Unreachable,
            }
        } else {
            MirTerminator::Unreachable
        };
        self.mir_blocks[header as usize - 1] = MirBlock {
            operations: condition_operations,
            terminator: if has_zero_iteration {
                MirTerminator::SwitchBool {
                    false_target: exit,
                    true_target: body,
                }
            } else {
                MirTerminator::Goto { target: body }
            },
        };
        self.mir_blocks[body as usize - 1] = MirBlock {
            operations: body_operations,
            terminator: body_region.entry.map_or_else(
                || body_terminator.clone(),
                |target| MirTerminator::Goto { target },
            ),
        };
        if let Some(tail) = body_region.tail {
            self.set_mir_handoff(tail, body_terminator);
        }
        if has_zero_iteration || has_break {
            self.mir_blocks.push(MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            });
            self.register_mir_region(header, exit);
        } else {
            if let Some(previous) = self.mir_tail {
                self.mir_blocks[previous as usize - 1].terminator =
                    MirTerminator::Goto { target: header };
            }
            self.mir_entries.push(header);
            if backedge_reachable || has_continue || synthetic_stop || !body_falls_through {
                self.mir_tail = None;
            } else {
                self.mir_continuations.insert(body);
                self.mir_tail = Some(body);
            }
        }
    }

    pub(super) fn compile_while(
        &mut self,
        condition: &LoweredExpr,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledLoop, Diagnostic> {
        let loop_depth = self.loops.len();
        let control_reachable = self.control_reachable;
        let result = self.compile_while_scoped(condition, body, span);
        self.restore_loop_depth(loop_depth);
        self.control_reachable = control_reachable;
        result
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_while_scoped(
        &mut self,
        condition: &LoweredExpr,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledLoop, Diagnostic> {
        let constant_condition = match &condition.kind {
            LoweredExprKind::Bool(value) => Some(*value),
            _ => None,
        };
        let has_zero_iteration = constant_condition != Some(true);
        let outer_control_reachable = self.control_reachable;
        let register = self.allocate(ValueType::Unit)?;
        let header = u32::try_from(self.code.len()).expect("instruction index fits");
        let repeat_entry = self.push_loop();
        let condition_operations_start = self.mir.len();
        let condition_value = self.compile_expression(condition)?;
        if condition_value.value_type == ValueType::Never {
            let (body_hir, body_effects) = self.compile_static_loop_body(body)?;
            let effects = self.union_effects([condition_value.effects, body_effects]);
            return Ok(CompiledLoop {
                register,
                falls_through: false,
                hir: self.hir(
                    HirExprKind::While {
                        condition: Box::new(condition_value.hir),
                        body: Box::new(body_hir),
                    },
                    None,
                    &ValueType::Unit,
                    effects,
                    span,
                ),
            });
        }
        let condition_operations = self.mir.split_off(condition_operations_start);
        if condition_value.value_type != ValueType::Bool {
            return Err(Diagnostic::new(
                "E3007",
                format!(
                    "while condition must be Bool, found {}",
                    condition_value.value_type
                ),
                condition.span,
            ));
        }
        let exit_entry = self.loop_edge_state(
            self.loops
                .last()
                .expect("while loop context remains active"),
            condition.span,
        )?;
        let branch = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        let body_target = u32::try_from(self.code.len()).expect("instruction index fits");
        let mir_header = self.next_mir_block();
        self.mir_blocks.extend((0..2).map(|_| MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        }));
        self.control_reachable = outer_control_reachable && constant_condition != Some(false);
        let body_region_capture = self.begin_nested_mir_region();
        let body_operations_start = self.mir.len();
        let (body_hir, body_value, _) = self.compile_block_value(body)?;
        let body_region = self.finish_nested_mir_region(body_region_capture);
        self.control_reachable = outer_control_reachable;
        Self::validate_loop_body_type(body, &body_value)?;
        let mut body_operations = self.mir.split_off(body_operations_start);
        let body_falls_through = body_value.value_type != ValueType::Never;
        let body_runtime_falls_through = self.runtime_falls_through(&body_value);
        let backedge_reachable = body_runtime_falls_through
            && outer_control_reachable
            && constant_condition != Some(false);
        if backedge_reachable {
            self.code.push(Instruction::Jump { target: header });
            self.mark_last_instruction(body.span);
        } else if body_falls_through && outer_control_reachable && constant_condition != Some(false)
        {
            let ownership_at_entry = repeat_entry.ownership.keys().copied().collect();
            let reason = self.terminate_source_dead_path(&ownership_at_entry, body.span)?;
            body_operations.push(MirOperation::Constant {
                destination: u32::from(reason),
            });
        }
        let emitted_exit = u32::try_from(self.code.len()).expect("instruction index fits");
        let edge_span = Span {
            start: body.span.end.saturating_sub(1),
            end: body.span.end,
        };
        let (context, falls_through) = self.finish_loop(
            LoopEntryStates {
                repeat: &repeat_entry,
                exit: &exit_entry,
            },
            backedge_reachable,
            header,
            emitted_exit,
            has_zero_iteration,
            edge_span,
        )?;
        let exit = if constant_condition == Some(false) {
            self.code.truncate(body_target as usize);
            self.instruction_spans
                .retain(|instruction, _| *instruction < body_target as usize);
            body_target
        } else {
            emitted_exit
        };
        self.code[branch] = Instruction::BranchBool {
            condition: condition_value.register,
            false_target: if has_zero_iteration {
                exit
            } else {
                body_target
            },
            true_target: if constant_condition == Some(false) {
                exit
            } else {
                body_target
            },
        };
        self.mark_instruction(branch, condition.span);
        self.register_loop_mir_region(
            mir_header,
            mir_header,
            condition_operations,
            body_operations,
            body_region,
            body_falls_through,
            backedge_reachable,
            &context.continue_mir_blocks,
            &context.break_mir_blocks,
            has_zero_iteration,
        );
        let effects = self.union_effects([condition_value.effects, body_hir.effects]);
        Ok(CompiledLoop {
            register,
            falls_through,
            hir: self.hir(
                HirExprKind::While {
                    condition: Box::new(condition_value.hir),
                    body: Box::new(body_hir),
                },
                None,
                &ValueType::Unit,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_infinite_loop(
        &mut self,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledLoop, Diagnostic> {
        let loop_reachable = self.control_reachable;
        let register = self.allocate(ValueType::Unit)?;
        let header = u32::try_from(self.code.len()).expect("instruction index fits");
        let entry = self.push_loop();
        let mir_header = self.next_mir_block();
        self.mir_blocks.extend((0..2).map(|_| MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        }));
        let body_region_capture = self.begin_nested_mir_region();
        let body_operations_start = self.mir.len();
        let (body_hir, body_value, _) = self.compile_block_value(body)?;
        let body_region = self.finish_nested_mir_region(body_region_capture);
        Self::validate_loop_body_type(body, &body_value)?;
        let mut body_operations = self.mir.split_off(body_operations_start);
        let body_falls_through = body_value.value_type != ValueType::Never;
        let body_runtime_falls_through = self.runtime_falls_through(&body_value);
        let backedge_reachable = body_runtime_falls_through && loop_reachable;
        if backedge_reachable {
            self.code.push(Instruction::Jump { target: header });
            self.mark_last_instruction(body.span);
        } else if body_falls_through && loop_reachable {
            let ownership_at_entry = entry.ownership.keys().copied().collect();
            let reason = self.terminate_source_dead_path(&ownership_at_entry, body.span)?;
            body_operations.push(MirOperation::Constant {
                destination: u32::from(reason),
            });
        }
        let exit = u32::try_from(self.code.len()).expect("instruction index fits");
        let edge_span = Span {
            start: body.span.end.saturating_sub(1),
            end: body.span.end,
        };
        let (context, falls_through) = self.finish_loop(
            LoopEntryStates {
                repeat: &entry,
                exit: &entry,
            },
            backedge_reachable,
            header,
            exit,
            false,
            edge_span,
        )?;
        self.register_loop_mir_region(
            mir_header,
            mir_header,
            Vec::new(),
            body_operations,
            body_region,
            body_falls_through,
            backedge_reachable,
            &context.continue_mir_blocks,
            &context.break_mir_blocks,
            false,
        );
        let effects = body_hir.effects;
        Ok(CompiledLoop {
            register,
            falls_through,
            hir: self.hir(
                HirExprKind::Loop {
                    body: Box::new(body_hir),
                },
                None,
                &ValueType::Unit,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_terminal_for(
        &mut self,
        register: Register,
        binding: &LoweredLoopBinding,
        source: HirForSource,
        source_effects: EffectSetId,
        yielded_type: &ValueType,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledLoop, Diagnostic> {
        let (hir_binding, body_hir, body_effects) =
            self.compile_static_for_body(binding, yielded_type, body)?;
        let effects = self.union_effects([source_effects, body_effects]);
        Ok(CompiledLoop {
            register,
            falls_through: false,
            hir: self.hir(
                HirExprKind::For {
                    binding: hir_binding,
                    source,
                    body: Box::new(body_hir),
                },
                None,
                &ValueType::Unit,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_for(
        &mut self,
        binding: &LoweredLoopBinding,
        source: &LoweredForSource,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledLoop, Diagnostic> {
        let loop_reachable = self.control_reachable;
        let register = self.allocate(ValueType::Unit)?;
        let (
            source_hir,
            source_effects,
            yielded,
            yielded_type,
            length,
            cursor,
            step,
            string_iterable,
        ) = match source {
            LoweredForSource::Range { start, end } => {
                let start_value = self.compile_expected(start, &ValueType::Int, "range start")?;
                if start_value.value_type == ValueType::Never {
                    let end_value = self.compile_without_runtime(|lowering| {
                        lowering.compile_expected(end, &ValueType::Int, "range end")
                    })?;
                    let effects = self.union_effects([start_value.effects, end_value.effects]);
                    return self.finish_terminal_for(
                        register,
                        binding,
                        HirForSource::Range {
                            start: Box::new(start_value.hir),
                            end: Box::new(end_value.hir),
                        },
                        effects,
                        &ValueType::Int,
                        body,
                        span,
                    );
                }
                let start_snapshot = self.allocate(ValueType::Int)?;
                self.code.push(Instruction::Move {
                    destination: start_snapshot,
                    source: start_value.register,
                });
                self.mark_last_instruction(start.span);
                self.mir.push(MirOperation::Move {
                    destination: u32::from(start_snapshot),
                    source: u32::from(start_value.register),
                });
                let end_value = self.compile_expected(end, &ValueType::Int, "range end")?;
                if end_value.value_type == ValueType::Never {
                    let effects = self.union_effects([start_value.effects, end_value.effects]);
                    return self.finish_terminal_for(
                        register,
                        binding,
                        HirForSource::Range {
                            start: Box::new(start_value.hir),
                            end: Box::new(end_value.hir),
                        },
                        effects,
                        &ValueType::Int,
                        body,
                        span,
                    );
                }
                let end_snapshot = self.allocate(ValueType::Int)?;
                self.code.push(Instruction::Move {
                    destination: end_snapshot,
                    source: end_value.register,
                });
                self.mark_last_instruction(end.span);
                self.mir.push(MirOperation::Move {
                    destination: u32::from(end_snapshot),
                    source: u32::from(end_value.register),
                });
                let cursor = self.allocate(ValueType::Int)?;
                self.code.push(Instruction::Move {
                    destination: cursor,
                    source: start_snapshot,
                });
                self.mark_last_instruction(start.span);
                self.mir.push(MirOperation::Move {
                    destination: u32::from(cursor),
                    source: u32::from(start_snapshot),
                });
                (
                    HirForSource::Range {
                        start: Box::new(start_value.hir),
                        end: Box::new(end_value.hir),
                    },
                    self.union_effects([start_value.effects, end_value.effects]),
                    cursor,
                    ValueType::Int,
                    end_snapshot,
                    cursor,
                    true,
                    false,
                )
            }
            LoweredForSource::Iterable(value) => {
                let value_span = value.span;
                let value = self.compile_expression(value)?;
                if value.value_type == ValueType::Never {
                    let yielded_type = if binding.tuple {
                        ValueType::Tuple(vec![ValueType::Never; binding.elements.len()])
                    } else {
                        ValueType::Never
                    };
                    return self.finish_terminal_for(
                        register,
                        binding,
                        HirForSource::Iterable(Box::new(value.hir)),
                        value.effects,
                        &yielded_type,
                        body,
                        span,
                    );
                }
                let yielded_type = match &value.value_type {
                    ValueType::List(element) => element.as_ref().clone(),
                    ValueType::Bytes => ValueType::Int,
                    ValueType::String => ValueType::String,
                    ValueType::Map(key, value) => {
                        ValueType::Tuple(vec![key.as_ref().clone(), value.as_ref().clone()])
                    }
                    found => {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!(
                                "for iterable must be String, List<T>, Bytes, or Map<K, V>, found {found}"
                            ),
                            value_span,
                        ));
                    }
                };
                let iterable_type = value.value_type.clone();
                let iterable = self.allocate(iterable_type.clone())?;
                self.code.push(Instruction::Move {
                    destination: iterable,
                    source: value.register,
                });
                self.mark_last_instruction(span);
                self.mir.push(MirOperation::Move {
                    destination: u32::from(iterable),
                    source: u32::from(value.register),
                });
                let length = self.allocate(ValueType::Int)?;
                self.code.push(Instruction::Length {
                    destination: length,
                    collection: iterable,
                });
                self.mark_last_instruction(span);
                self.mir.push(MirOperation::Length {
                    destination: u32::from(length),
                    collection: u32::from(iterable),
                });
                let zero = self.compile_expression(&LoweredExpr {
                    kind: LoweredExprKind::Int(0),
                    span,
                })?;
                let cursor = self.allocate(ValueType::Int)?;
                self.code.push(Instruction::Move {
                    destination: cursor,
                    source: zero.register,
                });
                self.mark_last_instruction(span);
                self.mir.push(MirOperation::Move {
                    destination: u32::from(cursor),
                    source: u32::from(zero.register),
                });
                (
                    HirForSource::Iterable(Box::new(value.hir)),
                    value.effects,
                    iterable,
                    yielded_type,
                    length,
                    cursor,
                    false,
                    matches!(iterable_type, ValueType::String),
                )
            }
        };
        let entry = self.push_loop();
        let header = u32::try_from(self.code.len()).expect("instruction index fits");
        let condition_operations_start = self.mir.len();
        let condition = self.allocate(ValueType::Bool)?;
        self.code.push(Instruction::Compare {
            destination: condition,
            left: cursor,
            right: length,
            operation: CompareOp::Less,
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::Binary {
            destination: u32::from(condition),
        });
        let condition_operations = self.mir.split_off(condition_operations_start);
        let branch = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        let iteration_target = u32::try_from(self.code.len()).expect("instruction index fits");
        let body_operations_start = self.mir.len();
        let string_value = string_iterable
            .then(|| self.allocate(ValueType::String))
            .transpose()?;
        let mut string_none_jump = None;
        if let Some(string_value) = string_value {
            let option = self.allocate(ValueType::Option(Box::new(ValueType::String)))?;
            self.code.push(Instruction::StringCall {
                destination: option,
                operation: StringOperation::Get,
                arguments: vec![yielded, cursor],
            });
            self.mark_last_instruction(binding.span);
            self.mir.push(MirOperation::StringOperation {
                destination: u32::from(option),
                operation: StringOperation::Get,
                arguments: vec![u32::from(yielded), u32::from(cursor)],
            });
            let switch = self.code.len();
            self.code.push(Instruction::Jump { target: 0 });
            let none_target = u32::try_from(self.code.len()).expect("instruction index fits");
            string_none_jump = Some(self.code.len());
            self.code.push(Instruction::Jump { target: 0 });
            let some_target = u32::try_from(self.code.len()).expect("instruction index fits");
            self.code[switch] = Instruction::SwitchEnum {
                source: option,
                arms: vec![
                    allen_bytecode::EnumSwitchArm {
                        variant: 0,
                        target: none_target,
                        bindings: Vec::new(),
                    },
                    allen_bytecode::EnumSwitchArm {
                        variant: 1,
                        target: some_target,
                        bindings: vec![string_value],
                    },
                ],
            };
            self.mark_instruction(switch, binding.span);
            self.mark_last_instruction(binding.span);
            self.mir.push(MirOperation::Binary {
                destination: u32::from(string_value),
            });
        }
        let mir_header = self.next_mir_block();
        self.mir_blocks.extend((0..2).map(|_| MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        }));
        let yielded = if let Some(string_value) = string_value {
            string_value
        } else if step {
            yielded
        } else {
            let value = self.allocate(yielded_type.clone())?;
            match &source_hir {
                HirForSource::Iterable(_) => {
                    if matches!(self.registers[yielded as usize], ValueType::Map(_, _)) {
                        self.code.push(Instruction::MapEntryAt {
                            destination: value,
                            map: yielded,
                            index: cursor,
                        });
                        self.mir.push(MirOperation::MapEntryAt {
                            destination: u32::from(value),
                            map: u32::from(yielded),
                            index: u32::from(cursor),
                        });
                    } else {
                        self.code.push(Instruction::IndexGet {
                            destination: value,
                            collection: yielded,
                            index: cursor,
                        });
                        self.mir.push(MirOperation::Binary {
                            destination: u32::from(value),
                        });
                    }
                }
                HirForSource::Range { .. } => unreachable!("range yields its cursor"),
            }
            self.mark_last_instruction(binding.span);
            value
        };
        let hir_binding = self.install_loop_binding(binding, yielded, &yielded_type)?;
        let body_region_capture = self.begin_nested_mir_region();
        let (body_hir, body_value, _) = self.compile_block_value(body)?;
        let body_region = self.finish_nested_mir_region(body_region_capture);
        Self::validate_loop_body_type(body, &body_value)?;
        let body_falls_through = body_value.value_type != ValueType::Never;
        let body_runtime_falls_through = self.runtime_falls_through(&body_value);
        let step_target = u32::try_from(self.code.len()).expect("instruction index fits");
        let has_continue = self
            .loops
            .last()
            .is_some_and(|loop_| !loop_.continue_jumps.is_empty());
        let backedge_reachable = loop_reachable && (body_runtime_falls_through || has_continue);
        let step_operations_start = self.mir.len();
        if backedge_reachable {
            let one = self.compile_expression(&LoweredExpr {
                kind: LoweredExprKind::Int(1),
                span,
            })?;
            self.code.push(Instruction::IntBinary {
                destination: cursor,
                left: cursor,
                right: one.register,
                operation: NumericBinaryOp::Add,
            });
            self.mark_last_instruction(span);
            self.mir.push(MirOperation::Binary {
                destination: u32::from(cursor),
            });
            self.code.push(Instruction::Jump { target: header });
            self.mark_last_instruction(span);
        }
        let step_operations = self.mir.split_off(step_operations_start);
        let mut body_operations = self.mir.split_off(body_operations_start);
        if body_falls_through && !backedge_reachable && loop_reachable {
            let ownership_at_entry = entry.ownership.keys().copied().collect();
            let reason = self.terminate_source_dead_path(&ownership_at_entry, body.span)?;
            body_operations.push(MirOperation::Constant {
                destination: u32::from(reason),
            });
        }
        let exit = u32::try_from(self.code.len()).expect("instruction index fits");
        if let Some(jump) = string_none_jump {
            self.code[jump] = Instruction::Jump { target: exit };
        }
        let edge_span = Span {
            start: body.span.end.saturating_sub(1),
            end: body.span.end,
        };
        let (context, falls_through) = self.finish_loop(
            LoopEntryStates {
                repeat: &entry,
                exit: &entry,
            },
            backedge_reachable,
            step_target,
            exit,
            true,
            edge_span,
        )?;
        self.code[branch] = Instruction::BranchBool {
            condition,
            false_target: exit,
            true_target: iteration_target,
        };
        self.mark_instruction(branch, span);
        let mir_step = if backedge_reachable {
            let step = self.next_mir_block();
            self.mir_blocks.push(MirBlock {
                operations: step_operations,
                terminator: MirTerminator::Goto { target: mir_header },
            });
            step
        } else {
            mir_header
        };
        self.register_loop_mir_region(
            mir_header,
            mir_step,
            condition_operations,
            body_operations,
            body_region,
            body_falls_through,
            backedge_reachable,
            &context.continue_mir_blocks,
            &context.break_mir_blocks,
            true,
        );
        let effects = self.union_effects([source_effects, body_hir.effects]);
        Ok(CompiledLoop {
            register,
            falls_through,
            hir: self.hir(
                HirExprKind::For {
                    binding: hir_binding,
                    source: source_hir,
                    body: Box::new(body_hir),
                },
                None,
                &ValueType::Unit,
                effects,
                span,
            ),
        })
    }

    pub(super) fn collection_value_is_valid(
        value_type: &ValueType,
        collection: &str,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if contains_affine(value_type) || contains_stored_sub_agent(value_type) {
            return Err(Diagnostic::new(
                "E3011",
                format!("future, task, or SubAgent values cannot be stored in {collection}"),
                span,
            ));
        }
        if contains_workspace(value_type) {
            return Err(Diagnostic::new(
                "E3011",
                format!("Workspace cannot be stored in {collection}"),
                span,
            ));
        }
        Ok(())
    }

    pub(super) fn annotation_type(
        &self,
        annotation: &LoweredType,
    ) -> Result<ValueType, Diagnostic> {
        let SemanticType::Value(value_type) = semantic_type(
            annotation,
            &BTreeSet::new(),
            &self.info.module,
            &self.global.bundle.modules,
            &self.global.bundle.types,
        )?
        else {
            return Err(Diagnostic::new(
                "E3007",
                "local binding type must be concrete",
                annotation.span(),
            ));
        };
        Ok(value_type)
    }

    pub(super) fn compile_list(
        &mut self,
        expression: &LoweredExpr,
        elements: &[LoweredExpr],
        expected: Option<&ValueType>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let mut values = Vec::with_capacity(elements.len());
        let element_type = match expected {
            Some(ValueType::List(element_type)) => (**element_type).clone(),
            Some(_) => {
                return Err(Diagnostic::new(
                    "E3010",
                    "list literal requires a List expected type",
                    expression.span,
                ));
            }
            None => {
                let Some(first) = elements.first() else {
                    return Err(Diagnostic::new(
                        "E3010",
                        "empty List requires an expected List type",
                        expression.span,
                    ));
                };
                let first = self.compile_expression(first)?;
                let element_type = first.value_type.clone();
                values.push(first);
                element_type
            }
        };
        Self::collection_value_is_valid(&element_type, "List", expression.span)?;
        for element in elements.iter().skip(usize::from(expected.is_none())) {
            values.push(self.compile_expected(element, &element_type, "list element")?);
        }
        let value_type = ValueType::List(Box::new(element_type));
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::ListNew {
            destination: register,
            elements: values.iter().map(|value| value.register).collect(),
        });
        self.mir.push(MirOperation::List {
            destination: u32::from(register),
        });
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::List(values.into_iter().map(|value| value.hir).collect()),
                None,
                &value_type,
                effects,
                expression.span,
            ),
        })
    }

    pub(super) fn compile_map(
        &mut self,
        expression: &LoweredExpr,
        entries: &[(LoweredExpr, LoweredExpr)],
        expected: Option<&ValueType>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let mut values = Vec::with_capacity(entries.len());
        let (key_type, map_value_type) = match expected {
            Some(ValueType::Map(key, value)) => ((**key).clone(), (**value).clone()),
            Some(_) => {
                return Err(Diagnostic::new(
                    "E3010",
                    "map literal requires a Map expected type",
                    expression.span,
                ));
            }
            None => {
                let Some((key, value)) = entries.first() else {
                    return Err(Diagnostic::new(
                        "E3010",
                        "empty Map requires an expected Map type",
                        expression.span,
                    ));
                };
                let key = self.compile_expression(key)?;
                let value = self.compile_expression(value)?;
                let types = (key.value_type.clone(), value.value_type.clone());
                values.push((key, value));
                types
            }
        };
        if !key_type.is_map_key() {
            return Err(Diagnostic::new(
                "E3011",
                format!("Map key type {key_type} is not allowed"),
                expression.span,
            ));
        }
        Self::collection_value_is_valid(&key_type, "Map", expression.span)?;
        Self::collection_value_is_valid(&map_value_type, "Map", expression.span)?;
        let mut seen = BTreeSet::new();
        if expected.is_none() {
            if let Some((key, _)) = entries.first() {
                if let Some(key) = literal_map_key(key) {
                    seen.insert(key);
                }
            }
        }
        for (key, value) in entries.iter().skip(usize::from(expected.is_none())) {
            if let Some(key) = literal_map_key(key) {
                if !seen.insert(key) {
                    return Err(Diagnostic::new("E3011", "duplicate Map key", value.span));
                }
            }
            values.push((
                self.compile_expected(key, &key_type, "map key")?,
                self.compile_expected(value, &map_value_type, "map value")?,
            ));
        }
        let value_type = ValueType::Map(Box::new(key_type), Box::new(map_value_type));
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::MapNew {
            destination: register,
            entries: values
                .iter()
                .map(|(key, value)| (key.register, value.register))
                .collect(),
        });
        self.mir.push(MirOperation::Map {
            destination: u32::from(register),
        });
        let effects = self.union_effects(
            values
                .iter()
                .flat_map(|(key, value)| [key.effects, value.effects]),
        );
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Map(
                    values
                        .into_iter()
                        .map(|(key, value)| (key.hir, value.hir))
                        .collect(),
                ),
                None,
                &value_type,
                effects,
                expression.span,
            ),
        })
    }

    pub(super) fn compile_collection_builtin(
        &mut self,
        builtin: CollectionBuiltin,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let (name, arity) = match builtin {
            CollectionBuiltin::Length => ("length", 1),
            CollectionBuiltin::ListAppend => ("list.append", 2),
            CollectionBuiltin::ListSet => ("list.set", 3),
            CollectionBuiltin::Safe(SafeCollectionOperation::ListGet) => ("list.get", 2),
            CollectionBuiltin::Safe(SafeCollectionOperation::ListTrySet) => ("list.try_set", 3),
            CollectionBuiltin::Safe(SafeCollectionOperation::BytesGet) => ("bytes.get", 2),
            CollectionBuiltin::Safe(SafeCollectionOperation::MapGet) => ("map.get", 2),
            CollectionBuiltin::CheckedInt(CheckedIntOperation::Negate) => ("int.checked_neg", 1),
            CollectionBuiltin::CheckedInt(_) => ("checked integer operation", 2),
        };
        if arguments.len() != arity {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "{name} requires exactly {arity} argument{}",
                    if arity == 1 { "" } else { "s" }
                ),
                span,
            ));
        }
        match builtin {
            CollectionBuiltin::Length => {
                let values = self.compile_expression(&arguments[0])?;
                self.compile_length_builtin(values, arguments[0].span, span)
            }
            CollectionBuiltin::ListAppend | CollectionBuiltin::ListSet => {
                let values = self.compile_expression(&arguments[0])?;
                self.compile_list_builtin(builtin, values, arguments, span, name)
            }
            CollectionBuiltin::Safe(operation) => {
                self.compile_safe_collection_builtin(operation, arguments, span, name)
            }
            CollectionBuiltin::CheckedInt(operation) => {
                self.compile_checked_int_builtin(operation, arguments, span, name)
            }
        }
    }

    pub(super) fn compile_safe_collection_builtin(
        &mut self,
        operation: SafeCollectionOperation,
        arguments: &[LoweredExpr],
        span: Span,
        name: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let values = arguments
            .iter()
            .map(|argument| self.compile_expression(argument))
            .collect::<Result<Vec<_>, _>>()?;
        let result = match (operation, values.as_slice()) {
            (SafeCollectionOperation::ListGet, [list, index])
                if matches!(index.value_type, ValueType::Int) =>
            {
                let ValueType::List(item) = &list.value_type else {
                    return Err(Diagnostic::new("E3011", "list.get requires List<T>", span));
                };
                ValueType::Option(item.clone())
            }
            (SafeCollectionOperation::ListTrySet, [list, index, replacement])
                if matches!(index.value_type, ValueType::Int) =>
            {
                let ValueType::List(item) = &list.value_type else {
                    return Err(Diagnostic::new(
                        "E3011",
                        "list.try_set requires List<T>",
                        span,
                    ));
                };
                if item.as_ref() != &replacement.value_type {
                    return Err(Diagnostic::new(
                        "E3011",
                        "list.try_set replacement must match the list element type",
                        span,
                    ));
                }
                ValueType::Option(Box::new(list.value_type.clone()))
            }
            (SafeCollectionOperation::BytesGet, [bytes, index])
                if bytes.value_type == ValueType::Bytes && index.value_type == ValueType::Int =>
            {
                ValueType::Option(Box::new(ValueType::Int))
            }
            (SafeCollectionOperation::MapGet, [map, key]) => {
                let ValueType::Map(expected_key, value) = &map.value_type else {
                    return Err(Diagnostic::new("E3011", "map.get requires Map<K, V>", span));
                };
                if expected_key.as_ref() != &key.value_type {
                    return Err(Diagnostic::new(
                        "E3011",
                        "map.get key must match the map key type",
                        span,
                    ));
                }
                ValueType::Option(value.clone())
            }
            _ => {
                return Err(Diagnostic::new(
                    "E3011",
                    format!("{name} arguments do not match its exact signature"),
                    span,
                ));
            }
        };
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        let register = self.allocate(result.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::SafeCollectionCall {
            destination: register,
            operation,
            arguments: argument_registers.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::SafeCollectionOperation {
            destination: u32::from(register),
            operation,
            arguments: argument_registers.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::SafeCollectionOperation {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_checked_int_builtin(
        &mut self,
        operation: CheckedIntOperation,
        arguments: &[LoweredExpr],
        span: Span,
        name: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let values = arguments
            .iter()
            .map(|argument| self.compile_expected(argument, &ValueType::Int, name))
            .collect::<Result<Vec<_>, _>>()?;
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        let result = ValueType::Option(Box::new(ValueType::Int));
        let register = self.allocate(result.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::CheckedIntCall {
            destination: register,
            operation,
            arguments: argument_registers.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::CheckedIntOperation {
            destination: u32::from(register),
            operation,
            arguments: argument_registers.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::CheckedIntOperation {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_string_builtin(
        &mut self,
        operation: StringOperation,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let (name, expected, result) = match operation {
            StringOperation::ByteLength => (
                "string.byte_length",
                vec![ValueType::String],
                ValueType::Int,
            ),
            StringOperation::Concat => (
                "string.concat",
                vec![ValueType::String, ValueType::String],
                ValueType::String,
            ),
            StringOperation::Get => (
                "string.get",
                vec![ValueType::String, ValueType::Int],
                ValueType::Option(Box::new(ValueType::String)),
            ),
            StringOperation::Slice => (
                "string.slice",
                vec![ValueType::String, ValueType::Int, ValueType::Int],
                ValueType::Option(Box::new(ValueType::String)),
            ),
            StringOperation::Find => (
                "string.find",
                vec![ValueType::String, ValueType::String],
                ValueType::Option(Box::new(ValueType::Int)),
            ),
            StringOperation::Contains => (
                "string.contains",
                vec![ValueType::String, ValueType::String],
                ValueType::Bool,
            ),
            StringOperation::StartsWith => (
                "string.starts_with",
                vec![ValueType::String, ValueType::String],
                ValueType::Bool,
            ),
            StringOperation::EndsWith => (
                "string.ends_with",
                vec![ValueType::String, ValueType::String],
                ValueType::Bool,
            ),
            StringOperation::Split => (
                "string.split",
                vec![ValueType::String, ValueType::String],
                ValueType::Option(Box::new(ValueType::List(Box::new(ValueType::String)))),
            ),
            StringOperation::Join => (
                "string.join",
                vec![
                    ValueType::List(Box::new(ValueType::String)),
                    ValueType::String,
                ],
                ValueType::String,
            ),
            StringOperation::TrimAscii => (
                "string.trim_ascii",
                vec![ValueType::String],
                ValueType::String,
            ),
            StringOperation::FromUtf8 => (
                "string.from_utf8",
                vec![ValueType::Bytes],
                ValueType::Option(Box::new(ValueType::String)),
            ),
            StringOperation::TemplateConcat => {
                unreachable!("template concatenation is not a source builtin")
            }
        };
        if arguments.len() != expected.len() {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "{name} requires exactly {} argument{}",
                    expected.len(),
                    if expected.len() == 1 { "" } else { "s" }
                ),
                span,
            ));
        }

        let mut values = Vec::with_capacity(arguments.len());
        let mut terminal = None;
        for (argument, expected) in arguments.iter().zip(&expected) {
            let value = if terminal.is_some() {
                self.compile_without_runtime(|lowering| {
                    lowering.compile_expected(argument, expected, name)
                })?
            } else {
                self.compile_expected(argument, expected, name)?
            };
            if terminal.is_none() && value.value_type == ValueType::Never {
                terminal = Some(value.register);
            }
            values.push(value);
        }
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        if let Some(register) = terminal {
            return Ok(CompiledExpr {
                register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::StringOperation {
                        operation,
                        arguments: values.into_iter().map(|value| value.hir).collect(),
                    },
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }

        let register = self.allocate(result.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::StringCall {
            destination: register,
            operation,
            arguments: argument_registers.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::StringOperation {
            destination: u32::from(register),
            operation,
            arguments: argument_registers.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::StringOperation {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_capability_builtin(
        &mut self,
        operation: CapabilityOperation,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let (name, expected, result) = match operation {
            CapabilityOperation::IsGranted => (
                "capability.is_granted",
                vec![ValueType::String],
                ValueType::Bool,
            ),
            CapabilityOperation::Granted => (
                "capability.granted",
                Vec::new(),
                ValueType::List(Box::new(ValueType::String)),
            ),
        };
        if arguments.len() != expected.len() {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "{name} requires exactly {} argument{}",
                    expected.len(),
                    if expected.len() == 1 { "" } else { "s" }
                ),
                span,
            ));
        }
        let values = arguments
            .iter()
            .zip(&expected)
            .map(|(argument, expected)| self.compile_expected(argument, expected, name))
            .collect::<Result<Vec<_>, _>>()?;
        let inspect_effect =
            effect_id(&self.global.effect_sets, &["capability.inspect".to_owned()]);
        let effects = self.union_effects(
            values
                .iter()
                .map(|value| value.effects)
                .chain(std::iter::once(inspect_effect)),
        );
        if let Some(value) = values
            .iter()
            .find(|value| value.value_type == ValueType::Never)
        {
            return Ok(CompiledExpr {
                register: value.register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::CapabilityInspect {
                        operation,
                        arguments: values.into_iter().map(|value| value.hir).collect(),
                    },
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        let register = self.allocate(result.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::CapabilityInspect {
            destination: register,
            operation,
            arguments: argument_registers.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::CapabilityInspect {
            destination: u32::from(register),
            operation,
            arguments: argument_registers.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: result.clone(),
            effects,
            hir: self.hir(
                HirExprKind::CapabilityInspect {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &result,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_length_builtin(
        &mut self,
        values: CompiledExpr,
        argument_span: Span,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if !matches!(
            values.value_type,
            ValueType::String | ValueType::Bytes | ValueType::List(_) | ValueType::Map(_, _)
        ) {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "length requires String, Bytes, List<T>, or Map<K, V>, found {}",
                    values.value_type
                ),
                argument_span,
            ));
        }
        let register = self.allocate(ValueType::Int)?;
        self.code.push(Instruction::Length {
            destination: register,
            collection: values.register,
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::Length {
            destination: u32::from(register),
            collection: u32::from(values.register),
        });
        let effects = values.effects;
        Ok(CompiledExpr {
            register,
            value_type: ValueType::Int,
            effects,
            hir: self.hir(
                HirExprKind::Length(Box::new(values.hir)),
                None,
                &ValueType::Int,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_list_builtin(
        &mut self,
        builtin: CollectionBuiltin,
        values: CompiledExpr,
        arguments: &[LoweredExpr],
        span: Span,
        name: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let ValueType::List(element_type) = &values.value_type else {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "{name} requires List<T> as its first argument, found {}",
                    values.value_type
                ),
                arguments[0].span,
            ));
        };
        let element_type = element_type.as_ref().clone();
        let value_type = values.value_type.clone();
        match builtin {
            CollectionBuiltin::ListAppend => {
                let value = self.compile_expected(&arguments[1], &element_type, "list value")?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::ListAppend {
                    destination: register,
                    values: values.register,
                    value: value.register,
                });
                self.mir.push(MirOperation::ListAppend {
                    destination: u32::from(register),
                    values: u32::from(values.register),
                    value: u32::from(value.register),
                });
                let effects = self.union_effects([values.effects, value.effects]);
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::ListAppend {
                            values: Box::new(values.hir),
                            value: Box::new(value.hir),
                        },
                        None,
                        &value_type,
                        effects,
                        span,
                    ),
                })
            }
            CollectionBuiltin::ListSet => {
                let index = self.compile_expected(&arguments[1], &ValueType::Int, "list index")?;
                let value = self.compile_expected(&arguments[2], &element_type, "list value")?;
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::ListSet {
                    destination: register,
                    values: values.register,
                    index: index.register,
                    value: value.register,
                });
                self.mir.push(MirOperation::ListSet {
                    destination: u32::from(register),
                    values: u32::from(values.register),
                    index: u32::from(index.register),
                    value: u32::from(value.register),
                });
                let effects = self.union_effects([values.effects, index.effects, value.effects]);
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::ListSet {
                            values: Box::new(values.hir),
                            index: Box::new(index.hir),
                            value: Box::new(value.hir),
                        },
                        None,
                        &value_type,
                        effects,
                        span,
                    ),
                })
            }
            CollectionBuiltin::Length
            | CollectionBuiltin::Safe(_)
            | CollectionBuiltin::CheckedInt(_) => {
                unreachable!("length builtin is handled before list lowering")
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_user_enum(
        &mut self,
        name: &str,
        variant_name: &str,
        payload: &LoweredEnumValuePayload,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if name == "TranscriptPart" && self.global.bundle.transcript_part.is_some() {
            return Err(Diagnostic::new(
                "E3007",
                "TranscriptPart is a read-only standard type",
                span,
            ));
        }
        if name == "ExternalFsAccess" {
            if !matches!(payload, LoweredEnumValuePayload::Unit) {
                return Err(Diagnostic::new(
                    "E3007",
                    "ExternalFsAccess variants do not accept a payload",
                    span,
                ));
            }
            let access = match variant_name {
                "Read" => ExternalFsAccess::Read,
                "Write" => ExternalFsAccess::Write,
                "ReadWrite" => ExternalFsAccess::ReadWrite,
                _ => {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("ExternalFsAccess has no variant '{variant_name}'"),
                        span,
                    ));
                }
            };
            let register = self.allocate(ValueType::ExternalFsAccess)?;
            let constant = self.global.constant(Constant::ExternalFsAccess(access))?;
            self.code.push(Instruction::Const {
                destination: register,
                constant,
            });
            self.mir.push(MirOperation::Constant {
                destination: u32::from(register),
            });
            let effects = self.empty_effects();
            return Ok(CompiledExpr {
                register,
                value_type: ValueType::ExternalFsAccess,
                effects,
                hir: self.hir(
                    HirExprKind::Enum,
                    None,
                    &ValueType::ExternalFsAccess,
                    effects,
                    span,
                ),
            });
        }
        let value_type = resolve_named_type(
            &self.global.bundle.modules,
            &self.global.bundle.types,
            &self.info.module,
            name,
            span,
        )?;
        let ValueType::Enum(enum_id) = value_type else {
            return Err(Diagnostic::new(
                "E3007",
                format!("'{name}' is not an enum type"),
                span,
            ));
        };
        let metadata = &self.global.bundle.enum_types[enum_id as usize];
        let variant = metadata
            .variants
            .iter()
            .position(|candidate| candidate.name == variant_name)
            .ok_or_else(|| {
                Diagnostic::new(
                    "E3007",
                    format!("enum '{name}' has no variant '{variant_name}'"),
                    span,
                )
            })?;
        let declared_payload = metadata.variants[variant].payload.clone();
        let values = match (declared_payload, payload) {
            (EnumPayloadType::Unit, LoweredEnumValuePayload::Unit) => Vec::new(),
            (EnumPayloadType::Tuple(expected), LoweredEnumValuePayload::Tuple(values)) => {
                if expected.len() != values.len() {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("enum variant '{variant_name}' has the wrong payload count"),
                        span,
                    ));
                }
                values
                    .iter()
                    .zip(&expected)
                    .map(|(value, expected)| self.compile_expected(value, expected, "enum payload"))
                    .collect::<Result<Vec<_>, _>>()?
            }
            (EnumPayloadType::Record(expected), LoweredEnumValuePayload::Record(fields)) => {
                if expected.len() != fields.len() {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("enum variant '{variant_name}' requires every field exactly once"),
                        span,
                    ));
                }
                let mut supplied = BTreeMap::new();
                for (field, value, field_span) in fields {
                    if supplied
                        .insert(field.clone(), (value, *field_span))
                        .is_some()
                    {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!("duplicate enum field '{field}'"),
                            *field_span,
                        ));
                    }
                }
                let mut values = Vec::with_capacity(expected.len());
                for field in &expected {
                    let (value, field_span) = supplied.remove(&field.name).ok_or_else(|| {
                        Diagnostic::new(
                            "E3007",
                            format!("missing enum field '{}'", field.name),
                            span,
                        )
                    })?;
                    values.push(
                        self.compile_expected(value, &field.value_type, "enum field")
                            .map_err(|diagnostic| {
                                if diagnostic.span == value.span {
                                    Diagnostic::new(diagnostic.code, diagnostic.message, field_span)
                                } else {
                                    diagnostic
                                }
                            })?,
                    );
                }
                if let Some((field, (_, field_span))) = supplied.into_iter().next() {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("unknown enum field '{field}'"),
                        field_span,
                    ));
                }
                values
            }
            _ => {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("enum variant '{variant_name}' uses the wrong payload form"),
                    span,
                ));
            }
        };
        let value_type = ValueType::Enum(enum_id);
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::EnumNew {
            destination: register,
            variant: u32::try_from(variant).expect("variant index fits"),
            payload: values.iter().map(|value| value.register).collect(),
        });
        self.mir.push(MirOperation::Enum {
            destination: u32::from(register),
        });
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(HirExprKind::Enum, None, &value_type, effects, span),
        })
    }

    pub(super) fn compile_template(
        &mut self,
        parts: &[LoweredTemplatePart],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let mut values = Vec::with_capacity(parts.len());
        let mut hir_parts = Vec::with_capacity(parts.len());
        let mut terminal = None;
        for part in parts {
            let value = match part {
                LoweredTemplatePart::Literal {
                    value,
                    span: literal_span,
                } => {
                    let literal = LoweredExpr {
                        kind: LoweredExprKind::String(value.clone()),
                        span: *literal_span,
                    };
                    let compiled = if terminal.is_some() {
                        self.compile_without_runtime(|lowering| {
                            lowering.compile_expression(&literal)
                        })?
                    } else {
                        self.compile_expression(&literal)?
                    };
                    hir_parts.push(HirTemplatePart::Literal {
                        value: value.clone(),
                        span: self.global.intern_span(&self.info.module, *literal_span),
                    });
                    compiled
                }
                LoweredTemplatePart::Interpolation(expression) => {
                    let compiled = if terminal.is_some() {
                        self.compile_without_runtime(|lowering| {
                            lowering.compile_expression(expression)
                        })?
                    } else {
                        self.compile_expression(expression)?
                    };
                    if compiled.value_type != ValueType::Never
                        && compiled.value_type != ValueType::String
                    {
                        return Err(Diagnostic::new(
                            "E3011",
                            format!(
                                "template interpolation must be String, found {}",
                                compiled.value_type
                            ),
                            expression.span,
                        ));
                    }
                    hir_parts.push(HirTemplatePart::Interpolation(compiled.hir.clone()));
                    compiled
                }
            };
            if terminal.is_none() && value.value_type == ValueType::Never {
                terminal = Some(value.register);
            }
            values.push(value);
        }
        let effects = self.union_effects(values.iter().map(|value| value.effects));
        if let Some(register) = terminal {
            return Ok(CompiledExpr {
                register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::Template(hir_parts),
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }

        let register = self.allocate(ValueType::String)?;
        let arguments = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        self.code.push(Instruction::StringCall {
            destination: register,
            operation: StringOperation::TemplateConcat,
            arguments: arguments.clone(),
        });
        self.mark_last_instruction(span);
        self.mir.push(MirOperation::StringOperation {
            destination: u32::from(register),
            operation: StringOperation::TemplateConcat,
            arguments: arguments.into_iter().map(u32::from).collect(),
        });
        Ok(CompiledExpr {
            register,
            value_type: ValueType::String,
            effects,
            hir: self.hir(
                HirExprKind::Template(hir_parts),
                None,
                &ValueType::String,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_expression(
        &mut self,
        expression: &LoweredExpr,
    ) -> Result<CompiledExpr, Diagnostic> {
        match &expression.kind {
            LoweredExprKind::Unit => {
                let register = self.allocate(ValueType::Unit)?;
                let constant = self.global.constant(Constant::Unit)?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Unit,
                    effects,
                    hir: self.hir(
                        HirExprKind::Unit,
                        None,
                        &ValueType::Unit,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Int(value) => {
                let register = self.allocate(ValueType::Int)?;
                let constant = self.global.constant(Constant::Int(*value))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Int,
                    effects,
                    hir: self.hir(
                        HirExprKind::Int(*value),
                        None,
                        &ValueType::Int,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Float(value) => {
                let register = self.allocate(ValueType::Float)?;
                let constant = self.global.constant(Constant::float_bits(*value))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Float,
                    effects,
                    hir: self.hir(
                        HirExprKind::Float(*value),
                        None,
                        &ValueType::Float,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Bool(value) => {
                let register = self.allocate(ValueType::Bool)?;
                let constant = self.global.constant(Constant::Bool(*value))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Bool,
                    effects,
                    hir: self.hir(
                        HirExprKind::Bool(*value),
                        None,
                        &ValueType::Bool,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::String(value) => {
                let register = self.allocate(ValueType::String)?;
                let constant = self.global.constant(Constant::String(value.clone()))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::String,
                    effects,
                    hir: self.hir(
                        HirExprKind::String(value.clone()),
                        None,
                        &ValueType::String,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Template(parts) => self.compile_template(parts, expression.span),
            LoweredExprKind::Bytes(value) => {
                let register = self.allocate(ValueType::Bytes)?;
                let constant = self.global.constant(Constant::Bytes(value.clone()))?;
                self.code.push(Instruction::Const {
                    destination: register,
                    constant,
                });
                self.mir.push(MirOperation::Constant {
                    destination: u32::from(register),
                });
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Bytes,
                    effects,
                    hir: self.hir(
                        HirExprKind::Bytes(value.clone()),
                        None,
                        &ValueType::Bytes,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Variable(name) => {
                let binding = self.bindings.get_mut(name).ok_or_else(|| {
                    Diagnostic::new(
                        "E3005",
                        format!("unknown local value '{name}'"),
                        expression.span,
                    )
                })?;
                if is_affine(&binding.value_type) {
                    if binding.moved {
                        return Err(Diagnostic::new(
                            "E3011",
                            format!("use of moved {} value '{name}'", binding.value_type),
                            expression.span,
                        ));
                    }
                    binding.moved = true;
                }
                let binding = binding.clone();
                let effects = self.empty_effects();
                Ok(CompiledExpr {
                    register: binding.register,
                    value_type: binding.value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Variable,
                        Some(binding.symbol),
                        &binding.value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Record { name, fields } => {
                if name == "$anonymous" {
                    let mut seen = BTreeSet::new();
                    let mut compiled = Vec::with_capacity(fields.len());
                    for (field, value, field_span) in fields {
                        if !seen.insert(field.clone()) {
                            return Err(Diagnostic::new(
                                "E3007",
                                format!("duplicate record field '{field}'"),
                                *field_span,
                            ));
                        }
                        compiled.push((field.clone(), self.compile_expression(value)?));
                    }
                    compiled.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                    let value_type = ValueType::Record(
                        compiled
                            .iter()
                            .map(|(field, value)| RecordField {
                                name: field.clone(),
                                value_type: value.value_type.clone(),
                            })
                            .collect(),
                    );
                    let register = self.allocate(value_type.clone())?;
                    self.code.push(Instruction::RecordNew {
                        destination: register,
                        fields: compiled
                            .iter()
                            .enumerate()
                            .map(|(index, (_, value))| {
                                (
                                    u32::try_from(index).expect("record field index fits"),
                                    value.register,
                                )
                            })
                            .collect(),
                    });
                    self.mir.push(MirOperation::Record {
                        destination: u32::from(register),
                    });
                    let effects =
                        self.union_effects(compiled.iter().map(|(_, value)| value.effects));
                    return Ok(CompiledExpr {
                        register,
                        value_type: value_type.clone(),
                        effects,
                        hir: self.hir(
                            HirExprKind::Record(
                                compiled.into_iter().map(|(_, value)| value.hir).collect(),
                            ),
                            None,
                            &value_type,
                            effects,
                            expression.span,
                        ),
                    });
                }
                if self.global.bundle.transcript_part.is_some()
                    && matches!(name.as_str(), "TranscriptSnapshot" | "TranscriptMessage")
                {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("{name} is a read-only standard type"),
                        expression.span,
                    ));
                }
                let value_type = resolve_named_type(
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                    &self.info.module,
                    name,
                    expression.span,
                )?;
                let ValueType::Record(layout) = &value_type else {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("'{name}' is not a record type"),
                        expression.span,
                    ));
                };
                if fields.len() != layout.len() {
                    return Err(Diagnostic::new(
                        "E3007",
                        format!("record '{name}' requires every field exactly once"),
                        expression.span,
                    ));
                }
                let mut seen = BTreeSet::new();
                let mut compiled = Vec::new();
                for (field, value, field_span) in fields {
                    if !seen.insert(field.clone()) {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!("duplicate record field '{field}'"),
                            *field_span,
                        ));
                    }
                    let index = layout
                        .iter()
                        .position(|candidate| candidate.name == *field)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                "E3007",
                                format!("record '{name}' has no field '{field}'"),
                                *field_span,
                            )
                        })?;
                    let value =
                        self.compile_expected(value, &layout[index].value_type, "record field")?;
                    if value.value_type != layout[index].value_type {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!(
                                "record field '{field}' expects {}, found {}",
                                layout[index].value_type, value.value_type
                            ),
                            *field_span,
                        ));
                    }
                    compiled.push((index, value));
                }
                compiled.sort_by_key(|(index, _)| *index);
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::RecordNew {
                    destination: register,
                    fields: compiled
                        .iter()
                        .map(|(index, value)| {
                            (
                                u32::try_from(*index).expect("field index fits"),
                                value.register,
                            )
                        })
                        .collect(),
                });
                self.mir.push(MirOperation::Record {
                    destination: u32::from(register),
                });
                let effects = self.union_effects(compiled.iter().map(|(_, value)| value.effects));
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Record(
                            compiled.into_iter().map(|(_, value)| value.hir).collect(),
                        ),
                        None,
                        &value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Prompt {
                system,
                context,
                data,
                output,
                max_attempts,
            } => self.compile_prompt(
                system,
                context.as_deref(),
                data.as_deref(),
                output,
                *max_attempts,
                expression.span,
            ),
            LoweredExprKind::Enum {
                name,
                variant,
                payload,
            } => self.compile_user_enum(name, variant, payload, expression.span),
            LoweredExprKind::FieldGet {
                record,
                field,
                field_span,
            } => {
                if let LoweredExprKind::Variable(name) = &record.kind {
                    if name == "ExternalFsAccess"
                        || resolve_named_type(
                            &self.global.bundle.modules,
                            &self.global.bundle.types,
                            &self.info.module,
                            name,
                            record.span,
                        )
                        .is_ok_and(|value_type| matches!(value_type, ValueType::Enum(_)))
                    {
                        return self.compile_user_enum(
                            name,
                            field,
                            &LoweredEnumValuePayload::Unit,
                            expression.span,
                        );
                    }
                }
                self.compile_field_get(record, field, *field_span, expression.span)
            }
            LoweredExprKind::Try(value) => self.compile_try(value, expression.span),
            LoweredExprKind::Match { source, arms } => {
                self.compile_match(source, arms, expression.span)
            }
            LoweredExprKind::If {
                condition,
                then_body,
                else_branch,
            } => self.compile_if(
                condition,
                then_body,
                else_branch.as_ref(),
                expression.span,
                None,
            ),
            LoweredExprKind::List(elements) => self.compile_list(expression, elements, None),
            LoweredExprKind::Map(entries) => self.compile_map(expression, entries, None),
            LoweredExprKind::Tuple(elements) => {
                let values = elements
                    .iter()
                    .map(|element| self.compile_expression(element))
                    .collect::<Result<Vec<_>, _>>()?;
                if values.iter().any(|value| is_affine(&value.value_type)) {
                    return Err(Diagnostic::new(
                        "E3011",
                        "future or task values cannot be stored in a tuple",
                        expression.span,
                    ));
                }
                if values
                    .iter()
                    .any(|value| contains_workspace(&value.value_type))
                {
                    return Err(Diagnostic::new(
                        "E3011",
                        "Workspace cannot be stored in a tuple",
                        expression.span,
                    ));
                }
                let value_type = if values.is_empty() {
                    ValueType::Unit
                } else {
                    ValueType::Tuple(
                        values
                            .iter()
                            .map(|value| value.value_type.clone())
                            .collect(),
                    )
                };
                let register = self.allocate(value_type.clone())?;
                if values.is_empty() {
                    let constant = self.global.constant(Constant::Unit)?;
                    self.code.push(Instruction::Const {
                        destination: register,
                        constant,
                    });
                    self.mir.push(MirOperation::Constant {
                        destination: u32::from(register),
                    });
                } else {
                    self.code.push(Instruction::TupleNew {
                        destination: register,
                        elements: values.iter().map(|value| value.register).collect(),
                    });
                    self.mir.push(MirOperation::Tuple {
                        destination: u32::from(register),
                    });
                }
                let effects = self.union_effects(values.iter().map(|value| value.effects));
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Tuple(values.into_iter().map(|value| value.hir).collect()),
                        None,
                        &value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Unary { operation, operand } => {
                let operand = self.compile_expression(operand)?;
                let value_type = match operation {
                    Unary::Not if operand.value_type == ValueType::Bool => ValueType::Bool,
                    Unary::Negate
                        if matches!(operand.value_type, ValueType::Int | ValueType::Float) =>
                    {
                        operand.value_type.clone()
                    }
                    Unary::Not => {
                        return Err(Diagnostic::new(
                            "E2003",
                            format!("operand of '!' must be Bool, found {}", operand.value_type),
                            expression.span,
                        ));
                    }
                    Unary::Negate => {
                        return Err(Diagnostic::new(
                            "E2003",
                            format!(
                                "operand of '-' must be Int or Float, found {}",
                                operand.value_type
                            ),
                            expression.span,
                        ));
                    }
                };
                let register = self.allocate(value_type.clone())?;
                self.code.push(match (operation, &value_type) {
                    (Unary::Not, _) => Instruction::BoolNot {
                        destination: register,
                        source: operand.register,
                    },
                    (Unary::Negate, ValueType::Int) => Instruction::IntNegate {
                        destination: register,
                        source: operand.register,
                    },
                    (Unary::Negate, ValueType::Float) => Instruction::FloatNegate {
                        destination: register,
                        source: operand.register,
                    },
                    _ => unreachable!("unary type was validated"),
                });
                self.mir.push(MirOperation::Binary {
                    destination: u32::from(register),
                });
                let effects = operand.effects;
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Unary(Box::new(operand.hir)),
                        None,
                        &value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Index { collection, index } => {
                let collection = self.compile_expression(collection)?;
                let (index, value_type, tuple_index) = match &collection.value_type {
                    ValueType::List(element) => (
                        self.compile_expected(index, &ValueType::Int, "list index")?,
                        element.as_ref().clone(),
                        None,
                    ),
                    ValueType::Bytes => (
                        self.compile_expected(index, &ValueType::Int, "bytes index")?,
                        ValueType::Int,
                        None,
                    ),
                    ValueType::Map(key, value) => (
                        self.compile_expected(index, key, "map index")?,
                        value.as_ref().clone(),
                        None,
                    ),
                    ValueType::Tuple(elements) => {
                        let LoweredExprKind::Int(index_value) = index.kind else {
                            return Err(Diagnostic::new(
                                "E2009",
                                "tuple index must be a nonnegative integer literal",
                                index.span,
                            ));
                        };
                        let tuple_index = usize::try_from(index_value)
                            .ok()
                            .filter(|value| *value < elements.len())
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    "E2009",
                                    format!(
                                        "tuple index {index_value} is out of range for {} elements",
                                        elements.len()
                                    ),
                                    index.span,
                                )
                            })?;
                        (
                            self.compile_expression(index)?,
                            elements[tuple_index].clone(),
                            Some(u32::try_from(tuple_index).expect("tuple index fits")),
                        )
                    }
                    found => {
                        return Err(Diagnostic::new(
                            "E2009",
                            format!("cannot index a value of type {found}"),
                            expression.span,
                        ));
                    }
                };
                let register = self.allocate(value_type.clone())?;
                self.code.push(if let Some(tuple_index) = tuple_index {
                    Instruction::TupleGet {
                        destination: register,
                        tuple: collection.register,
                        index: tuple_index,
                    }
                } else {
                    Instruction::IndexGet {
                        destination: register,
                        collection: collection.register,
                        index: index.register,
                    }
                });
                self.mir.push(MirOperation::Binary {
                    destination: u32::from(register),
                });
                let effects = self.union_effects([collection.effects, index.effects]);
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Index {
                            collection: Box::new(collection.hir),
                            index: Box::new(index.hir),
                        },
                        None,
                        &value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Binary {
                operation: operation @ (Binary::And | Binary::Or),
                left,
                right,
            } => self.compile_short_circuit_binary(*operation, left, right, expression.span),
            LoweredExprKind::Binary {
                operation,
                left,
                right,
            } => {
                let left_span = left.span;
                let right_span = right.span;
                let left = self.compile_expression(left)?;
                if left.value_type == ValueType::Unknown {
                    return Err(Diagnostic::new(
                        "E2018",
                        "unknown must be narrowed before use in an operator",
                        left_span,
                    ));
                }
                let right_needs_expected_type = matches!(
                    &right.kind,
                    LoweredExprKind::Variable(name) if name == "None"
                ) || matches!(
                    &right.kind,
                    LoweredExprKind::Call { callee, .. }
                        if matches!(&callee.kind, LoweredExprKind::Variable(name)
                            if matches!(name.as_str(), "Some" | "Ok" | "Err"))
                );
                let right = if right_needs_expected_type {
                    self.compile_expected(right, &left.value_type, "binary operand")?
                } else {
                    self.compile_expression(right)?
                };
                if right.value_type == ValueType::Unknown {
                    return Err(Diagnostic::new(
                        "E2018",
                        "unknown must be narrowed before use in an operator",
                        right_span,
                    ));
                }
                if matches!(operation, Binary::Remainder) {
                    if left.value_type != ValueType::Int {
                        return Err(Diagnostic::new(
                            "E2003",
                            "remainder requires Int operands",
                            left_span,
                        ));
                    }
                    if right.value_type != ValueType::Int {
                        return Err(Diagnostic::new(
                            "E2003",
                            "remainder requires Int operands",
                            right_span,
                        ));
                    }
                }
                if left.value_type != right.value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "binary operands must have one exact type",
                        right_span,
                    ));
                }
                let value_type = match operation {
                    Binary::Add | Binary::Subtract | Binary::Multiply | Binary::Divide => {
                        if !matches!(left.value_type, ValueType::Int | ValueType::Float) {
                            return Err(Diagnostic::new(
                                "E2003",
                                "arithmetic requires Int or Float",
                                expression.span,
                            ));
                        }
                        left.value_type.clone()
                    }
                    Binary::Remainder => ValueType::Int,
                    Binary::Equal | Binary::NotEqual => {
                        if !left.value_type.is_equatable() {
                            return Err(Diagnostic::new(
                                "E3008",
                                format!("type {} does not satisfy Eq", left.value_type),
                                expression.span,
                            ));
                        }
                        ValueType::Bool
                    }
                    Binary::Less | Binary::LessEqual | Binary::Greater | Binary::GreaterEqual => {
                        if !matches!(
                            left.value_type,
                            ValueType::Int
                                | ValueType::Float
                                | ValueType::String
                                | ValueType::Bytes
                        ) {
                            return Err(Diagnostic::new(
                                "E2003",
                                "ordered comparison requires Int, Float, String, or Bytes",
                                expression.span,
                            ));
                        }
                        ValueType::Bool
                    }
                    Binary::And | Binary::Or => {
                        unreachable!("Boolean binary operations lower through branch control flow")
                    }
                };
                let register = self.allocate(value_type.clone())?;
                let instruction = match operation {
                    Binary::Add | Binary::Subtract | Binary::Multiply | Binary::Divide => {
                        let operation = match operation {
                            Binary::Add => NumericBinaryOp::Add,
                            Binary::Subtract => NumericBinaryOp::Subtract,
                            Binary::Multiply => NumericBinaryOp::Multiply,
                            Binary::Divide => NumericBinaryOp::Divide,
                            _ => unreachable!(),
                        };
                        if left.value_type == ValueType::Int {
                            Instruction::IntBinary {
                                destination: register,
                                left: left.register,
                                right: right.register,
                                operation,
                            }
                        } else {
                            Instruction::FloatBinary {
                                destination: register,
                                left: left.register,
                                right: right.register,
                                operation,
                            }
                        }
                    }
                    Binary::Remainder => Instruction::IntRemainder {
                        destination: register,
                        left: left.register,
                        right: right.register,
                    },
                    _ => Instruction::Compare {
                        destination: register,
                        left: left.register,
                        right: right.register,
                        operation: match operation {
                            Binary::Equal => CompareOp::Equal,
                            Binary::NotEqual => CompareOp::NotEqual,
                            Binary::Less => CompareOp::Less,
                            Binary::LessEqual => CompareOp::LessEqual,
                            Binary::Greater => CompareOp::Greater,
                            Binary::GreaterEqual => CompareOp::GreaterEqual,
                            _ => unreachable!(),
                        },
                    },
                };
                self.code.push(instruction);
                if *operation == Binary::Remainder {
                    self.mark_last_instruction(expression.span);
                }
                self.mir.push(MirOperation::Binary {
                    destination: u32::from(register),
                });
                let effects = self.union_effects([left.effects, right.effects]);
                Ok(CompiledExpr {
                    register,
                    value_type: value_type.clone(),
                    effects,
                    hir: self.hir(
                        HirExprKind::Binary(vec![left.hir, right.hir]),
                        None,
                        &value_type,
                        effects,
                        expression.span,
                    ),
                })
            }
            LoweredExprKind::Call {
                callee,
                type_arguments,
                arguments,
            } => self.compile_call(callee, type_arguments, arguments, expression.span),
            LoweredExprKind::Spawn(value) => self.compile_spawn(value, expression.span),
            LoweredExprKind::Await(value) => self.compile_await(value, expression.span),
            LoweredExprKind::AwaitBlock(body) => self.compile_await_block(body, expression.span),
            LoweredExprKind::Closure {
                parameters,
                return_type,
                declared_effects,
                body,
            } => self.compile_closure(
                parameters,
                return_type,
                declared_effects.as_deref(),
                body,
                expression.span,
            ),
        }
    }

    pub(super) fn compile_short_circuit_binary(
        &mut self,
        operation: Binary,
        left: &LoweredExpr,
        right: &LoweredExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let compiled_left = self.compile_expected(left, &ValueType::Bool, "Boolean operand")?;
        if compiled_left.value_type == ValueType::Never {
            let right = self.compile_without_runtime(|lowering| {
                lowering.compile_expected(right, &ValueType::Bool, "Boolean operand")
            })?;
            let effects = self.union_effects([compiled_left.effects, right.effects]);
            return Ok(CompiledExpr {
                register: compiled_left.register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::Binary(vec![compiled_left.hir, right.hir]),
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        let right_body = LoweredBody {
            statements: Vec::new(),
            tail: Some(right.clone()),
            span: right.span,
        };
        let literal_body = |value| LoweredBody {
            statements: Vec::new(),
            tail: Some(LoweredExpr {
                kind: LoweredExprKind::Bool(value),
                span,
            }),
            span,
        };
        let mut lowered = match operation {
            Binary::And => {
                let false_branch = LoweredElse::Body(Box::new(literal_body(false)));
                self.compile_if(
                    left,
                    &right_body,
                    Some(&false_branch),
                    span,
                    Some(compiled_left),
                )?
            }
            Binary::Or => {
                let true_branch = literal_body(true);
                let false_branch = LoweredElse::Body(Box::new(right_body));
                self.compile_if(
                    left,
                    &true_branch,
                    Some(&false_branch),
                    span,
                    Some(compiled_left),
                )?
            }
            _ => unreachable!("only Boolean short-circuit operations are lowered here"),
        };
        let HirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = lowered.hir.kind
        else {
            unreachable!("short-circuit lowering produces conditional HIR")
        };
        let right = match operation {
            Binary::And => *then_branch,
            Binary::Or => *else_branch.expect("short-circuit lowering always has an else branch"),
            _ => unreachable!("only Boolean short-circuit operations are lowered here"),
        };
        lowered.hir = self.hir(
            HirExprKind::Binary(vec![*condition, right]),
            None,
            &lowered.value_type,
            lowered.effects,
            span,
        );
        Ok(lowered)
    }

    pub(super) fn compile_spawn(
        &mut self,
        value: &LoweredExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let future = self.compile_expression(value)?;
        let ValueType::Future(result_type) = &future.value_type else {
            return Err(Diagnostic::new(
                "E3011",
                format!("spawn requires Future<T>, found {}", future.value_type),
                span,
            ));
        };
        self.consume_ownership(future.register, MirOwnershipState::Moved);
        let value_type = ValueType::Task(result_type.clone());
        let register = self.allocate(value_type.clone())?;
        let scope = self.active_scopes.last().copied().unwrap_or(0);
        self.code.push(Instruction::Spawn {
            destination: register,
            future: future.register,
            scope,
        });
        self.mir.push(MirOperation::Spawn {
            destination: u32::from(register),
            future: u32::from(future.register),
            scope,
        });
        self.record_ownership(register, scope, MirOwnershipState::Live, true);
        let spawn_effects = effect_id(&self.global.effect_sets, &["task.spawn".to_owned()]);
        let effects = self.union_effects([future.effects, spawn_effects]);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Spawn {
                    future: Box::new(future.hir),
                    scope,
                },
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_observed_task(
        &mut self,
        expression: &LoweredExpr,
    ) -> Result<CompiledExpr, Diagnostic> {
        let LoweredExprKind::Variable(name) = &expression.kind else {
            return self.compile_expression(expression);
        };
        let binding = self.bindings.get(name).cloned().ok_or_else(|| {
            Diagnostic::new(
                "E3005",
                format!("unknown local value '{name}'"),
                expression.span,
            )
        })?;
        if is_affine(&binding.value_type) && binding.moved {
            return Err(Diagnostic::new(
                "E3011",
                format!("use of moved {} value '{name}'", binding.value_type),
                expression.span,
            ));
        }
        let effects = self.empty_effects();
        Ok(CompiledExpr {
            register: binding.register,
            value_type: binding.value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Variable,
                Some(binding.symbol),
                &binding.value_type,
                effects,
                expression.span,
            ),
        })
    }

    pub(super) fn compile_task_snapshot(
        &mut self,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if arguments.len() != 1 {
            return Err(Diagnostic::new(
                "E3011",
                "allen.internal.task_snapshot requires exactly one Task<T> argument",
                span,
            ));
        }
        let source = self.compile_observed_task(&arguments[0])?;
        if !matches!(&source.value_type, ValueType::Task(_)) {
            return Err(Diagnostic::new(
                "E3011",
                format!(
                    "allen.internal.task_snapshot requires Task<T>, found {}",
                    source.value_type
                ),
                arguments[0].span,
            ));
        }
        let value_type = task_snapshot_type();
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::TaskSnapshot {
            destination: register,
            source: source.register,
        });
        self.mir.push(MirOperation::TaskSnapshot {
            destination: u32::from(register),
            source: u32::from(source.register),
        });
        let inspect_effects = effect_id(&self.global.effect_sets, &["debug.inspect".to_owned()]);
        let effects = self.union_effects([source.effects, inspect_effects]);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::TaskSnapshot(Box::new(source.hir)),
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_workspace_get(
        &mut self,
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if !arguments.is_empty() {
            return Err(Diagnostic::new(
                "E3011",
                "fs.workspace requires no arguments",
                span,
            ));
        }
        let value_type = ValueType::Workspace;
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::WorkspaceGet {
            destination: register,
        });
        self.mir.push(MirOperation::WorkspaceGet {
            destination: u32::from(register),
        });
        let effects = self.empty_effects();
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(HirExprKind::WorkspaceGet, None, &value_type, effects, span),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_effect_call(
        &mut self,
        operation: EffectOperation,
        type_arguments: &[LoweredType],
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if matches!(
            operation,
            EffectOperation::AgentAsk
                | EffectOperation::ModelRequest
                | EffectOperation::UserAsk
                | EffectOperation::SubAgentRun
                | EffectOperation::SubAgentAsk
        ) {
            let request_index = usize::from(operation == EffectOperation::SubAgentAsk);
            let expected_arguments = if operation == EffectOperation::SubAgentRun {
                2
            } else {
                request_index + 1
            };
            if arguments.len() != expected_arguments {
                return Err(Diagnostic::new(
                    "E3011",
                    "typed request has the wrong argument count",
                    span,
                ));
            }
            let mut values = Vec::with_capacity(arguments.len());
            for argument in arguments {
                values.push(self.compile_expression(argument)?);
            }
            if operation == EffectOperation::SubAgentAsk
                && values[0].value_type != ValueType::SubAgent
            {
                return Err(Diagnostic::new(
                    "E3011",
                    format!(
                        "sub_agent.ask expected SubAgent, found {}",
                        values[0].value_type
                    ),
                    arguments[0].span,
                ));
            }
            if operation == EffectOperation::SubAgentRun
                && values[1].value_type != sub_agent_projection_type()
            {
                return Err(Diagnostic::new(
                    "E3011",
                    "sub_agent.run requires the exact authority projection record",
                    arguments[1].span,
                ));
            }
            let value = &values[request_index];
            let result = if let Some(output) = prompt_output_type(&value.value_type) {
                output.clone()
            } else {
                return Err(Diagnostic::new(
                    "E3011",
                    "typed request requires Prompt<T>",
                    arguments[request_index].span,
                ));
            };
            if let Some(type_argument) = type_arguments.first() {
                let SemanticType::Value(expected) = semantic_type(
                    type_argument,
                    &BTreeSet::new(),
                    &self.info.module,
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                )?
                else {
                    return Err(Diagnostic::new(
                        "E3007",
                        "typed request response type must be concrete",
                        type_argument.span(),
                    ));
                };
                if expected != result {
                    return Err(Diagnostic::new(
                        "E3010",
                        format!(
                            "typed request declares response {expected}, but its prompt produces {result}"
                        ),
                        type_argument.span(),
                    ));
                }
            }
            let error = match operation {
                EffectOperation::AgentAsk => agent_error_type(),
                EffectOperation::ModelRequest => model_error_type(),
                EffectOperation::UserAsk => user_error_type(),
                EffectOperation::SubAgentRun | EffectOperation::SubAgentAsk => {
                    sub_agent_error_type()
                }
                _ => unreachable!("guarded typed request"),
            };
            let result = ValueType::Result(Box::new(result), Box::new(error));
            let value_type = ValueType::Future(Box::new(result));
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::EffectCall {
                destination: register,
                operation,
                arguments: values.iter().map(|value| value.register).collect(),
            });
            self.mir.push(MirOperation::EffectCall {
                destination: u32::from(register),
                operation,
                arguments: values
                    .iter()
                    .map(|value| u32::from(value.register))
                    .collect(),
            });
            self.record_ownership(register, 0, MirOwnershipState::Live, true);
            let call_effect = effect_id(
                &self.global.effect_sets,
                &[operation.required_effect().to_owned()],
            );
            let effects = self.union_effects(
                values
                    .iter()
                    .map(|value| value.effects)
                    .chain(std::iter::once(call_effect)),
            );
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::EffectCall {
                        operation,
                        arguments: values.into_iter().map(|value| value.hir).collect(),
                    },
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }
        if matches!(
            operation,
            EffectOperation::SubAgentCreate | EffectOperation::SubAgentMessage
        ) {
            if !type_arguments.is_empty() {
                return Err(Diagnostic::new(
                    "E3005",
                    "this sub_agent operation does not take type arguments",
                    span,
                ));
            }
            if arguments.len() != 2 {
                return Err(Diagnostic::new(
                    "E3011",
                    "sub_agent operation requires exactly two arguments",
                    span,
                ));
            }
            let values = arguments
                .iter()
                .map(|argument| self.compile_expression(argument))
                .collect::<Result<Vec<_>, _>>()?;
            let valid = match operation {
                EffectOperation::SubAgentCreate => {
                    prompt_output_type(&values[0].value_type) == Some(&ValueType::Unit)
                        && values[1].value_type == sub_agent_projection_type()
                }
                EffectOperation::SubAgentMessage => {
                    values[0].value_type == ValueType::SubAgent
                        && values[1].value_type == ValueType::String
                }
                _ => unreachable!(),
            };
            if !valid {
                return Err(Diagnostic::new(
                    "E3011",
                    "sub_agent operation arguments do not match its exact signature",
                    span,
                ));
            }
            let result = if operation == EffectOperation::SubAgentCreate {
                ValueType::SubAgent
            } else {
                ValueType::Unit
            };
            let result = ValueType::Result(Box::new(result), Box::new(sub_agent_error_type()));
            let value_type = ValueType::Future(Box::new(result));
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::EffectCall {
                destination: register,
                operation,
                arguments: values.iter().map(|value| value.register).collect(),
            });
            self.mir.push(MirOperation::EffectCall {
                destination: u32::from(register),
                operation,
                arguments: values
                    .iter()
                    .map(|value| u32::from(value.register))
                    .collect(),
            });
            self.record_ownership(register, 0, MirOwnershipState::Live, true);
            let call_effect = effect_id(
                &self.global.effect_sets,
                &[operation.required_effect().to_owned()],
            );
            let effects = self.union_effects(
                values
                    .iter()
                    .map(|value| value.effects)
                    .chain(std::iter::once(call_effect)),
            );
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::EffectCall {
                        operation,
                        arguments: values.into_iter().map(|value| value.hir).collect(),
                    },
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }
        let (parameters, result, effect, label) =
            effect_operation_signature(operation, self.global.bundle.transcript_part)
                .expect("transcript operation has a synthetic transcript part enum");
        if arguments.len() != parameters.len() {
            return Err(Diagnostic::new(
                "E3011",
                format!("{label} has the wrong argument count"),
                span,
            ));
        }
        if operation == EffectOperation::AgentTranscript {
            if let LoweredExprKind::Record { fields, .. } = &arguments[0].kind {
                if let Some((
                    _,
                    LoweredExpr {
                        kind: LoweredExprKind::Int(limit),
                        ..
                    },
                    _,
                )) = fields.iter().find(|(name, _, _)| name == "limit")
                {
                    if !(1..=100).contains(limit) {
                        return Err(Diagnostic::new(
                            "E3011",
                            "agent.transcript limit must be from 1 through 100",
                            arguments[0].span,
                        ));
                    }
                }
            }
        }
        let values = if operation == EffectOperation::AgentTranscript {
            arguments
                .iter()
                .zip(&parameters)
                .map(|(argument, parameter)| {
                    self.compile_expected(argument, parameter, "agent.transcript query")
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            arguments
                .iter()
                .map(|argument| self.compile_expression(argument))
                .collect::<Result<Vec<_>, _>>()?
        };
        for (value, parameter) in values.iter().zip(&parameters) {
            if value.value_type != *parameter {
                return Err(Diagnostic::new(
                    "E3011",
                    format!("{label} expected {parameter}, found {}", value.value_type),
                    span,
                ));
            }
        }
        let value_type = ValueType::Future(Box::new(result));
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::EffectCall {
            destination: register,
            operation,
            arguments: values.iter().map(|value| value.register).collect(),
        });
        self.mir.push(MirOperation::EffectCall {
            destination: u32::from(register),
            operation,
            arguments: values
                .iter()
                .map(|value| u32::from(value.register))
                .collect(),
        });
        self.record_ownership(register, 0, MirOwnershipState::Live, true);
        let call_effect = effect_id(&self.global.effect_sets, &[effect.to_owned()]);
        let effects = self.union_effects(
            values
                .iter()
                .map(|value| value.effects)
                .chain(std::iter::once(call_effect)),
        );
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::EffectCall {
                    operation,
                    arguments: values.into_iter().map(|value| value.hir).collect(),
                },
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_await(
        &mut self,
        value: &LoweredExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if !self.info.lowered.is_async {
            return Err(Diagnostic::new(
                "E3011",
                "await requires an async function",
                span,
            ));
        }
        let source = self.compile_expression(value)?;
        let value_type = match &source.value_type {
            ValueType::Future(value) | ValueType::Task(value) => value.as_ref().clone(),
            _ => {
                return Err(Diagnostic::new(
                    "E3011",
                    format!(
                        "await requires Future<T> or Task<T>, found {}",
                        source.value_type
                    ),
                    span,
                ));
            }
        };
        self.consume_ownership(source.register, MirOwnershipState::Awaited);
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::Await {
            destination: register,
            source: source.register,
        });
        if is_affine(&value_type) {
            let scope = if matches!(value_type, ValueType::Task(_)) {
                self.active_scopes.last().copied().unwrap_or(0)
            } else {
                0
            };
            self.record_ownership(register, scope, MirOwnershipState::Live, true);
        }
        if contains_stored_sub_agent(&value_type) {
            self.sub_agent_value_scopes
                .insert(register, self.current_scope());
        }
        let suspension = self.next_mir_block();
        let resume = suspension + 1;
        let exceptional_cancel = suspension + 2;
        let timeout_cancel = suspension + 3;
        let external_cancel = suspension + 4;
        let permanent_stop = suspension + 5;
        self.mir_suspensions.push(MirSuspension {
            destination: u32::from(register),
            source: u32::from(source.register),
            resume,
            exceptional_cancel,
            timeout_cancel,
            external_cancel,
            permanent_stop,
        });
        self.mir_blocks.push(MirBlock {
            operations: vec![MirOperation::Await {
                destination: u32::from(register),
                source: u32::from(source.register),
            }],
            terminator: MirTerminator::Suspend {
                destination: u32::from(register),
                source: u32::from(source.register),
                resume,
                exceptional_cancel,
                timeout_cancel,
                external_cancel,
                permanent_stop,
            },
        });
        self.mir_blocks.push(MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        });
        for kind in [
            MirCleanupKind::ExceptionalCancel,
            MirCleanupKind::TimeoutCancel,
            MirCleanupKind::ExternalCancel,
            MirCleanupKind::PermanentStop,
        ] {
            let operations = self.cleanup_operations(kind);
            self.mir_blocks.push(MirBlock {
                operations,
                terminator: MirTerminator::Unreachable,
            });
        }
        self.register_mir_region(suspension, resume);
        let effects = source.effects;
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Await(Box::new(source.hir)),
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_await_block(
        &mut self,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if !self.info.lowered.is_async {
            return Err(Diagnostic::new(
                "E3011",
                "await block requires an async function",
                span,
            ));
        }
        let scope = self.next_scope;
        self.next_scope = self
            .next_scope
            .checked_add(1)
            .ok_or_else(|| Diagnostic::new("E3011", "too many task scopes", span))?;
        self.code.push(Instruction::TaskScopeEnter { scope });
        self.mir.push(MirOperation::TaskScopeEnter { scope });
        self.active_scopes.push(scope);
        let outer_bindings = self.bindings.clone();
        let body_mir_block_start = self.mir_blocks.len();
        let body_mir_entry_start = self.mir_entries.len();
        let (body_hir, result, mut returns_from_function) = self.compile_block_value(body)?;
        let result_value_scope = self.sub_agent_value_scope(&result);
        let outer_scope = self.active_scopes.iter().rev().nth(1).copied().unwrap_or(0);
        if contains_stored_sub_agent(&result.value_type)
            && !self.scope_outlives(result_value_scope, outer_scope)
        {
            return Err(Diagnostic::new(
                "E3011",
                "SubAgent-containing value cannot escape an await block",
                body.tail.as_ref().map_or(body.span, |tail| tail.span),
            ));
        }
        returns_from_function |= result.value_type == ValueType::Never
            && matches!(self.code.last(), Some(Instruction::Return { .. }));
        self.active_scopes.pop();
        let exits_through_scope = result.value_type != ValueType::Never
            || returns_from_function
            || matches!(self.code.last(), Some(Instruction::Jump { .. }));
        if matches!(result.value_type, ValueType::Task(_))
            && self
                .ownership_states
                .get(&result.register)
                .is_some_and(|ownership| ownership.scope == scope)
        {
            return Err(Diagnostic::new(
                "E3011",
                "task ownership cannot escape an await block",
                span,
            ));
        }
        let joined = self
            .ownership_states
            .iter()
            .filter_map(|(register, ownership)| {
                (ownership.scope == scope && ownership.state == MirOwnershipState::Live)
                    .then_some(*register)
            })
            .collect::<Vec<_>>();
        for register in joined {
            let nested_task_result = matches!(
                &self.registers[register as usize],
                ValueType::Task(result) if is_affine(result)
            );
            if nested_task_result
                || (matches!(self.registers[register as usize], ValueType::Future(_))
                    && self.must_consume(register))
            {
                return Err(Diagnostic::new(
                    "E3011",
                    "nested affine result must be awaited before scope exit",
                    span,
                ));
            }
            if matches!(self.registers[register as usize], ValueType::Task(_)) {
                self.record_ownership(register, scope, MirOwnershipState::ScopeJoined, true);
            }
        }
        if result.value_type != ValueType::Never {
            self.code.push(Instruction::TaskScopeExit { scope });
            self.mark_last_instruction(span);
        }
        let normal_join = self.next_mir_block();
        let exceptional_cancel = normal_join + 1;
        let timeout_cancel = normal_join + 2;
        let external_cancel = normal_join + 3;
        let permanent_stop = normal_join + 4;
        self.mir_task_scopes.push(MirTaskScope {
            scope,
            normal_join,
            exceptional_cancel,
            timeout_cancel,
            external_cancel,
            permanent_stop,
        });
        let exit_dispatch = normal_join;
        let normal_cleanup = exit_dispatch + 1;
        let exceptional_cancel = exit_dispatch + 2;
        let timeout_cancel = exit_dispatch + 3;
        let external_cancel = exit_dispatch + 4;
        let permanent_stop = exit_dispatch + 5;
        let continuation = exit_dispatch + 6;
        if let Some(metadata) = self.mir_task_scopes.last_mut() {
            metadata.normal_join = normal_cleanup;
            metadata.exceptional_cancel = exceptional_cancel;
            metadata.timeout_cancel = timeout_cancel;
            metadata.external_cancel = external_cancel;
            metadata.permanent_stop = permanent_stop;
        }
        self.mir_blocks.push(MirBlock {
            operations: exits_through_scope
                .then_some(MirOperation::TaskScopeExit { scope })
                .into_iter()
                .collect(),
            terminator: MirTerminator::TaskScopeExit {
                scope,
                normal_join: normal_cleanup,
                exceptional_cancel,
                timeout_cancel,
                external_cancel,
                permanent_stop,
            },
        });
        self.mir_blocks.push(MirBlock {
            operations: vec![MirOperation::TaskScopeCleanup {
                scope,
                kind: MirCleanupKind::NormalJoin,
            }],
            terminator: MirTerminator::Goto {
                target: continuation,
            },
        });
        for kind in [
            MirCleanupKind::ExceptionalCancel,
            MirCleanupKind::TimeoutCancel,
            MirCleanupKind::ExternalCancel,
            MirCleanupKind::PermanentStop,
        ] {
            self.mir_blocks.push(MirBlock {
                operations: vec![MirOperation::TaskScopeCleanup { scope, kind }],
                terminator: if kind == MirCleanupKind::PermanentStop
                    && result.value_type == ValueType::Never
                    && !returns_from_function
                {
                    match self.code.last() {
                        Some(Instruction::Stop { reason }) => MirTerminator::Stop {
                            reason: u32::from(*reason),
                        },
                        _ => MirTerminator::Unreachable,
                    }
                } else {
                    MirTerminator::Unreachable
                },
            });
        }
        self.mir_blocks.push(MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        });
        let terminal_terminator = if result.value_type == ValueType::Never {
            if returns_from_function {
                Some(MirTerminator::Return {
                    source: u32::from(result.register),
                })
            } else if let Some(Instruction::Stop { reason }) = self.code.last() {
                Some(MirTerminator::Stop {
                    reason: u32::from(*reason),
                })
            } else {
                None
            }
        } else {
            None
        };
        let terminal_child_region = self.mir_tail.is_none()
            && self.mir_entries.len() > body_mir_entry_start
            && result.value_type == ValueType::Never;
        let mut emitted_scope_region = true;
        if terminal_child_region {
            let mut rewired = false;
            for block in &mut self.mir_blocks[body_mir_block_start..exit_dispatch as usize - 1] {
                if matches!(
                    (&block.terminator, &terminal_terminator),
                    (
                        MirTerminator::Return { .. },
                        Some(MirTerminator::Return { .. })
                    ) | (MirTerminator::Stop { .. }, Some(MirTerminator::Stop { .. }))
                ) {
                    block.terminator = MirTerminator::Goto {
                        target: exit_dispatch,
                    };
                    rewired = true;
                }
            }
            if !rewired {
                self.mir_blocks.truncate(exit_dispatch as usize - 1);
                self.mir_task_scopes.pop();
                emitted_scope_region = false;
            }
        } else {
            self.register_mir_region(exit_dispatch, continuation);
        }
        if terminal_child_region && emitted_scope_region && terminal_terminator.is_some() {
            self.mir_blocks[continuation as usize - 1].terminator =
                terminal_terminator.expect("terminal await block has a terminator");
        } else if terminal_terminator.is_some() && !terminal_child_region {
            self.set_mir_handoff(
                continuation,
                terminal_terminator.expect("terminal await block has a terminator"),
            );
            self.mir_tail = None;
        }
        let mut restored = outer_bindings;
        for (name, binding) in &mut restored {
            if let Some(inner) = self.bindings.get(name) {
                binding.moved |= inner.moved;
                binding.value_scope = inner.value_scope;
            }
        }
        self.bindings = restored;
        let effects = result.effects;
        if returns_from_function {
            if result.value_type != ValueType::Never && result.value_type != self.return_type {
                return Err(Diagnostic::new(
                    "E3007",
                    format!(
                        "function returns {}, expected {}",
                        result.value_type, self.return_type
                    ),
                    body.span,
                ));
            }
            if result.value_type != ValueType::Never {
                self.prepare_return(&result, body.span)?;
                self.code.push(Instruction::Return {
                    source: result.register,
                });
            }
            return Ok(CompiledExpr {
                register: result.register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::AwaitBlock {
                        scope,
                        body: Box::new(body_hir),
                    },
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        Ok(CompiledExpr {
            register: result.register,
            value_type: result.value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::AwaitBlock {
                    scope,
                    body: Box::new(body_hir),
                },
                None,
                &result.value_type,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_block_value(
        &mut self,
        body: &LoweredBody,
    ) -> Result<(HirExpr, CompiledExpr, bool), Diagnostic> {
        let mut expressions = Vec::new();
        let mut result = None;
        let mut returns_from_function = false;
        let mut runtime_falls_through = true;
        for (index, statement) in body.statements.iter().enumerate() {
            match statement {
                LoweredStatement::Let {
                    name,
                    name_span,
                    mutable,
                    annotation,
                    value,
                } => {
                    if self.bindings.contains_key(name) {
                        return Err(Diagnostic::new(
                            "E3005",
                            format!("duplicate local binding '{name}'"),
                            *name_span,
                        ));
                    }
                    let value = if let Some(annotation) = annotation {
                        let expected = self.annotation_type(annotation)?;
                        self.compile_expected(value, &expected, "binding")?
                    } else {
                        self.compile_expression(value)?
                    };
                    let value_scope = self.sub_agent_value_scope(&value);
                    if *mutable && is_affine(&value.value_type) {
                        return Err(Diagnostic::new(
                            "E3011",
                            "future or task values cannot use mutable bindings",
                            *name_span,
                        ));
                    }
                    let symbol = self.global.allocate_symbol();
                    let scope = self.active_scopes.last().copied().unwrap_or(0);
                    self.bindings.insert(
                        name.clone(),
                        LocalBinding {
                            register: value.register,
                            symbol,
                            value_type: value.value_type.clone(),
                            scope,
                            value_scope,
                            mutable: *mutable,
                            moved: false,
                        },
                    );
                    let terminates = value.value_type == ValueType::Never;
                    runtime_falls_through &= self.runtime_falls_through(&value);
                    expressions.push(value.hir.clone());
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                *name_span,
                            ));
                        }
                        result = Some(value);
                        returns_from_function = true;
                    }
                }
                LoweredStatement::Assignment {
                    name,
                    name_span,
                    operation,
                    value,
                } => {
                    let assignment =
                        self.compile_assignment(name, *name_span, *operation, value)?;
                    let terminates = assignment.value_type == ValueType::Never;
                    runtime_falls_through &= self.runtime_falls_through(&assignment);
                    expressions.push(assignment.hir.clone());
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                value.span,
                            ));
                        }
                        result = Some(assignment);
                    }
                }
                LoweredStatement::ControlFlow(expression) => {
                    let value = self.compile_expression(expression)?;
                    if !matches!(value.value_type, ValueType::Unit | ValueType::Never) {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!(
                                "control-flow statement must have type Void, found {}",
                                value.value_type
                            ),
                            expression.span,
                        ));
                    }
                    let terminates = value.value_type == ValueType::Never;
                    runtime_falls_through &= self.runtime_falls_through(&value);
                    expressions.push(value.hir.clone());
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                expression.span,
                            ));
                        }
                        result = Some(value);
                        returns_from_function =
                            matches!(self.code.last(), Some(Instruction::Return { .. }));
                    }
                }
                LoweredStatement::Return(value, statement_span) => {
                    if index + 1 != body.statements.len() || body.tail.is_some() {
                        return Err(Diagnostic::new(
                            "E3005",
                            "code after return is unreachable",
                            *statement_span,
                        ));
                    }
                    let value = self.compile_return(value.as_ref(), *statement_span)?;
                    runtime_falls_through = false;
                    expressions.push(value.hir.clone());
                    result = Some(value);
                    returns_from_function = true;
                }
                LoweredStatement::While {
                    condition,
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_while(condition, loop_body, *span)?;
                    runtime_falls_through &= value.falls_through;
                    let effects = value.hir.effects;
                    expressions.push(value.hir.clone());
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after non-terminating loop is unreachable",
                                *span,
                            ));
                        }
                        result = Some(CompiledExpr {
                            register: value.register,
                            value_type: ValueType::Never,
                            effects,
                            hir: value.hir,
                        });
                    }
                }
                LoweredStatement::Loop {
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_infinite_loop(loop_body, *span)?;
                    runtime_falls_through &= value.falls_through;
                    let effects = value.hir.effects;
                    expressions.push(value.hir.clone());
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after non-terminating loop is unreachable",
                                *span,
                            ));
                        }
                        result = Some(CompiledExpr {
                            register: value.register,
                            value_type: ValueType::Never,
                            effects,
                            hir: value.hir,
                        });
                    }
                }
                LoweredStatement::For {
                    binding,
                    source,
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_for(binding, source, loop_body, *span)?;
                    runtime_falls_through &= value.falls_through;
                    let effects = value.hir.effects;
                    expressions.push(value.hir.clone());
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating loop header is unreachable",
                                *span,
                            ));
                        }
                        result = Some(CompiledExpr {
                            register: value.register,
                            value_type: ValueType::Never,
                            effects,
                            hir: value.hir,
                        });
                    }
                }
                LoweredStatement::Break(span) | LoweredStatement::Continue(span) => {
                    let value = self.compile_loop_control(
                        matches!(statement, LoweredStatement::Break(_)),
                        *span,
                    )?;
                    runtime_falls_through = false;
                    if index + 1 != body.statements.len() || body.tail.is_some() {
                        return Err(Diagnostic::new(
                            "E3005",
                            "code after loop control is unreachable",
                            *span,
                        ));
                    }
                    expressions.push(value.hir.clone());
                    result = Some(value);
                }
            }
        }
        let result = if let Some(result) = result {
            result
        } else if let Some(tail) = &body.tail {
            self.compile_expression(tail)?
        } else {
            self.compile_expression(&LoweredExpr {
                kind: LoweredExprKind::Tuple(Vec::new()),
                span: body.span,
            })?
        };
        runtime_falls_through &= self.runtime_falls_through(&result);
        if !runtime_falls_through {
            self.runtime_terminal_values.insert(result.register);
        }
        if !returns_from_function {
            expressions.push(result.hir.clone());
        }
        let effects = self.union_effects(
            expressions
                .iter()
                .map(|expression| expression.effects)
                .chain(std::iter::once(result.effects)),
        );
        let value_type = result.value_type.clone();
        let hir = self.hir(
            HirExprKind::Block(expressions),
            None,
            &value_type,
            effects,
            body.span,
        );
        Ok((hir, result, returns_from_function))
    }

    pub(super) fn compile_return(
        &mut self,
        expression: Option<&LoweredExpr>,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let value = if let Some(expression) = expression {
            self.compile_expected(expression, &self.return_type.clone(), "return")?
        } else {
            if self.return_type != ValueType::Unit {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("bare return requires Void, found {}", self.return_type),
                    span,
                ));
            }
            self.compile_expression(&LoweredExpr {
                kind: LoweredExprKind::Unit,
                span,
            })?
        };
        if value.value_type == ValueType::Never {
            return Ok(value);
        }

        for scope in self.active_scopes.iter().rev().copied().collect::<Vec<_>>() {
            let live = self
                .ownership_states
                .iter()
                .filter_map(|(register, ownership)| {
                    (ownership.scope == scope && ownership.state == MirOwnershipState::Live)
                        .then_some(*register)
                })
                .collect::<Vec<_>>();
            for register in live {
                let nested_task_result = matches!(
                    &self.registers[register as usize],
                    ValueType::Task(result) if is_affine(result)
                );
                let hidden_future_obligation =
                    matches!(self.registers[register as usize], ValueType::Future(_))
                        && self.must_consume(register);
                if nested_task_result || hidden_future_obligation {
                    return Err(Diagnostic::new(
                        "E3011",
                        "nested affine result must be awaited before scope exit",
                        span,
                    ));
                }
                self.record_ownership(register, scope, MirOwnershipState::ScopeJoined, true);
            }
            self.code.push(Instruction::TaskScopeExit { scope });
            self.mir.push(MirOperation::TaskScopeExit { scope });
            self.mir.push(MirOperation::TaskScopeCleanup {
                scope,
                kind: MirCleanupKind::NormalJoin,
            });
            self.invalidate_scope_local_sub_agents(scope);
        }
        self.prepare_return(&value, span)?;
        self.code.push(Instruction::Return {
            source: value.register,
        });
        self.mark_last_instruction(span);
        let effects = value.effects;
        Ok(CompiledExpr {
            register: value.register,
            value_type: ValueType::Never,
            effects,
            hir: self.hir(
                HirExprKind::Return(Box::new(value.hir)),
                None,
                &ValueType::Never,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_field_get(
        &mut self,
        record: &LoweredExpr,
        field: &str,
        field_span: Span,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let record = self.compile_expression(record)?;
        let ValueType::Record(layout) = &record.value_type else {
            return Err(Diagnostic::new(
                "E3007",
                "field access requires a record value",
                field_span,
            ));
        };
        let index = layout
            .iter()
            .position(|candidate| candidate.name == field)
            .ok_or_else(|| {
                Diagnostic::new(
                    "E3007",
                    format!("record has no field '{field}'"),
                    field_span,
                )
            })?;
        let value_type = layout[index].value_type.clone();
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::FieldGet {
            destination: register,
            record: record.register,
            field: u32::try_from(index).expect("field index fits"),
        });
        self.mir.push(MirOperation::FieldGet {
            destination: u32::from(register),
            record: u32::from(record.register),
        });
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects: record.effects,
            hir: self.hir(
                HirExprKind::FieldGet(Box::new(record.hir)),
                None,
                &value_type,
                record.effects,
                span,
            ),
        })
    }

    pub(super) fn compile_try(
        &mut self,
        value: &LoweredExpr,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let value = self.compile_expression(value)?;
        let ValueType::Result(ok, error) = &value.value_type else {
            return Err(Diagnostic::new(
                "E2017",
                "'?' requires a Result value",
                span,
            ));
        };
        let ValueType::Result(_, return_error) = &self.return_type else {
            return Err(Diagnostic::new(
                "E2017",
                "a function that uses '?' must return Result",
                span,
            ));
        };
        if error != return_error {
            return Err(Diagnostic::new(
                "E2017",
                "'?' error type must match the function return error type",
                span,
            ));
        }
        if self.ownership_states.iter().any(|(register, ownership)| {
            ownership.state == MirOwnershipState::Live
                && ownership.must_consume
                && (matches!(self.registers[*register as usize], ValueType::Future(_))
                    || ownership.scope == 0)
        }) {
            return Err(Diagnostic::new(
                "E3011",
                "try error path would discard a live affine obligation",
                span,
            ));
        }
        let value_type = ok.as_ref().clone();
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::TryResult {
            destination: register,
            source: value.register,
        });
        let base = self.next_mir_block();
        let cleanup_operations = self.cleanup_operations(MirCleanupKind::NormalJoin);
        let success = base + 1;
        let error = base + 2;
        let continuation = base + 3;
        self.mir_blocks.extend([
            MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::TryResult { success, error },
            },
            MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Goto {
                    target: continuation,
                },
            },
            MirBlock {
                operations: cleanup_operations,
                terminator: MirTerminator::Return {
                    source: u32::from(value.register),
                },
            },
            MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            },
        ]);
        self.register_mir_region(base, continuation);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects: value.effects,
            hir: self.hir(
                HirExprKind::Try(Box::new(value.hir)),
                None,
                &value_type,
                value.effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_record_match(
        &mut self,
        source: CompiledExpr,
        layout: &[RecordField],
        arms: &[(LoweredPattern, LoweredExpr, Span)],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if arms.len() != 1 {
            return Err(Diagnostic::new(
                "E3007",
                "a structural record match has exactly one reachable arm",
                span,
            ));
        }
        let (pattern, arm, pattern_span) = &arms[0];
        let fields = match pattern {
            LoweredPattern::Wildcard => Vec::new(),
            LoweredPattern::Record { name, fields } => {
                let expected = resolve_named_type(
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                    &self.info.module,
                    name,
                    *pattern_span,
                )?;
                if expected != source.value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "record pattern has a different structural type",
                        *pattern_span,
                    ));
                }
                if fields.len() != layout.len() {
                    return Err(Diagnostic::new(
                        "E3007",
                        "record pattern must contain exactly the declared fields",
                        *pattern_span,
                    ));
                }
                fields.clone()
            }
            _ => {
                return Err(Diagnostic::new(
                    "E3007",
                    "record match requires a record pattern or '_'",
                    *pattern_span,
                ));
            }
        };
        let outer = self.bindings.clone();
        let mut seen_fields = BTreeSet::new();
        let mut seen_bindings = BTreeSet::new();
        for (field, field_span, binding) in fields {
            if !seen_fields.insert(field.clone()) {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("duplicate record pattern field '{field}'"),
                    field_span,
                ));
            }
            let index = layout
                .iter()
                .position(|candidate| candidate.name == field)
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E3007",
                        format!("record pattern has no field '{field}'"),
                        field_span,
                    )
                })?;
            if let Some(binding) = binding {
                if !seen_bindings.insert(binding.clone()) || self.bindings.contains_key(&binding) {
                    return Err(Diagnostic::new(
                        "E3005",
                        format!("duplicate pattern binding '{binding}'"),
                        field_span,
                    ));
                }
                let value_type = layout[index].value_type.clone();
                let register = self.allocate(value_type.clone())?;
                self.code.push(Instruction::FieldGet {
                    destination: register,
                    record: source.register,
                    field: u32::try_from(index).expect("field index fits"),
                });
                self.mir.push(MirOperation::FieldGet {
                    destination: u32::from(register),
                    record: u32::from(source.register),
                });
                let symbol = self.global.allocate_symbol();
                let scope = self.active_scopes.last().copied().unwrap_or(0);
                self.bindings.insert(
                    binding,
                    LocalBinding {
                        register,
                        symbol,
                        value_type,
                        scope,
                        value_scope: scope,
                        mutable: false,
                        moved: false,
                    },
                );
            }
        }
        let value = self.compile_expression(arm)?;
        self.bindings = outer;
        let effects = self.union_effects([source.effects, value.effects]);
        Ok(CompiledExpr {
            register: value.register,
            value_type: value.value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Match {
                    source: Box::new(source.hir),
                    arms: vec![value.hir],
                },
                None,
                &value.value_type,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_if(
        &mut self,
        condition: &LoweredExpr,
        then_body: &LoweredBody,
        else_branch: Option<&LoweredElse>,
        span: Span,
        compiled_condition: Option<CompiledExpr>,
    ) -> Result<CompiledExpr, Diagnostic> {
        let constant_condition = match &condition.kind {
            LoweredExprKind::Bool(value) => Some(*value),
            _ => None,
        };
        let false_branch_continues = constant_condition != Some(true);
        let true_branch_continues = constant_condition != Some(false);
        let outer_control_reachable = self.control_reachable;
        let condition_span = condition.span;
        let condition = match compiled_condition {
            Some(condition) => condition,
            None => self.compile_expression(condition)?,
        };
        if condition.value_type != ValueType::Bool {
            return Err(Diagnostic::new(
                "E3007",
                format!("if condition must be Bool, found {}", condition.value_type),
                condition_span,
            ));
        }

        let branch_index = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        let base = self.next_mir_block();
        let false_block = base + 1;
        let true_block = base + 2;
        self.mir_blocks.extend((0..3).map(|_| MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::Unreachable,
        }));
        let outer_bindings = self.bindings.clone();
        let outer_ownership = self.ownership_states.clone();
        let mut continuing_bindings: Option<BTreeMap<String, BindingState>> = None;
        let mut continuing_ownership: Option<BTreeMap<Register, OwnershipRecord>> = None;
        let mut static_bindings: Option<BTreeMap<String, BindingState>> = None;
        let mut static_ownership: Option<BTreeMap<Register, OwnershipRecord>> = None;
        let mut result: Option<(Register, ValueType)> = None;
        let mut never_register = None;
        let mut jumps = Vec::new();

        let false_target = u32::try_from(self.code.len()).expect("instruction index fits");
        self.bindings = outer_bindings.clone();
        self.ownership_states = outer_ownership.clone();
        self.control_reachable = outer_control_reachable && constant_condition != Some(true);
        let false_region_capture = self.begin_nested_mir_region();
        let false_operation_start = self.mir.len();
        let false_span = match else_branch {
            Some(LoweredElse::Body(body)) => body.span,
            Some(LoweredElse::If(expression)) => expression.span,
            None => span,
        };
        let (false_hir, false_value) = match else_branch {
            Some(LoweredElse::Body(body)) => {
                let (hir, value, _) = self.compile_block_value(body)?;
                (hir, value)
            }
            Some(LoweredElse::If(expression)) => {
                let value = self.compile_expression(expression)?;
                (value.hir.clone(), value)
            }
            None => {
                let value = self.compile_expression(&LoweredExpr {
                    kind: LoweredExprKind::Unit,
                    span,
                })?;
                (value.hir.clone(), value)
            }
        };
        let false_region = self.finish_nested_mir_region(false_region_capture);
        let false_value_scope = self.sub_agent_value_scope(&false_value);
        let false_runtime_falls_through =
            false_branch_continues && self.runtime_falls_through(&false_value);
        let mut false_operations = self.mir.split_off(false_operation_start);
        let false_terminal = if false_value.value_type == ValueType::Never {
            never_register.get_or_insert(false_value.register);
            if !false_branch_continues
                && !matches!(
                    self.code.last(),
                    Some(Instruction::Return { .. } | Instruction::Stop { .. })
                )
            {
                let ownership_at_entry = outer_ownership.keys().copied().collect();
                let reason = self.terminate_source_dead_path(&ownership_at_entry, false_span)?;
                false_operations.push(MirOperation::Constant {
                    destination: u32::from(reason),
                });
                Some(MirTerminator::Stop {
                    reason: u32::from(reason),
                })
            } else {
                match self.code.last() {
                    Some(Instruction::Return { source }) => Some(MirTerminator::Return {
                        source: u32::from(*source),
                    }),
                    Some(Instruction::Stop { reason }) => Some(MirTerminator::Stop {
                        reason: u32::from(*reason),
                    }),
                    Some(Instruction::Jump { .. }) if false_branch_continues => None,
                    _ => Some(MirTerminator::Unreachable),
                }
            }
        } else {
            let (result_register, result_type) = if let Some((register, value_type)) = &result {
                if *value_type != false_value.value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "if branches must have one exact result type",
                        span,
                    ));
                }
                (*register, value_type.clone())
            } else {
                let register = self.allocate(false_value.value_type.clone())?;
                result = Some((register, false_value.value_type.clone()));
                (register, false_value.value_type.clone())
            };
            self.code.push(Instruction::Move {
                destination: result_register,
                source: false_value.register,
            });
            self.mark_last_instruction(false_span);
            false_operations.push(MirOperation::Move {
                destination: u32::from(result_register),
                source: u32::from(false_value.register),
            });
            if is_affine(&result_type) {
                let ownership = self
                    .ownership_states
                    .get(&false_value.register)
                    .copied()
                    .unwrap_or(OwnershipRecord {
                        scope: 0,
                        state: MirOwnershipState::Live,
                        must_consume: matches!(result_type, ValueType::Task(_)),
                    });
                self.consume_ownership(false_value.register, MirOwnershipState::Moved);
                self.record_ownership(
                    result_register,
                    ownership.scope,
                    MirOwnershipState::Live,
                    ownership.must_consume,
                );
            }
            self.validate_conditional_branch_state(
                &outer_bindings,
                &outer_ownership,
                Some(result_register),
                span,
                &mut static_bindings,
                &mut static_ownership,
            )?;
            if false_runtime_falls_through {
                self.validate_conditional_branch_state(
                    &outer_bindings,
                    &outer_ownership,
                    Some(result_register),
                    span,
                    &mut continuing_bindings,
                    &mut continuing_ownership,
                )?;
            }
            jumps.push(self.code.len());
            self.code.push(Instruction::Jump { target: 0 });
            self.mark_last_instruction(false_span);
            None
        };

        let true_target = u32::try_from(self.code.len()).expect("instruction index fits");
        self.bindings = outer_bindings.clone();
        self.ownership_states = outer_ownership.clone();
        self.control_reachable = outer_control_reachable && constant_condition != Some(false);
        let true_region_capture = self.begin_nested_mir_region();
        let true_operation_start = self.mir.len();
        let (true_hir, true_value, _) = self.compile_block_value(then_body)?;
        let true_region = self.finish_nested_mir_region(true_region_capture);
        let true_value_scope = self.sub_agent_value_scope(&true_value);
        let true_runtime_falls_through =
            true_branch_continues && self.runtime_falls_through(&true_value);
        let mut true_operations = self.mir.split_off(true_operation_start);
        if else_branch.is_none()
            && !matches!(true_value.value_type, ValueType::Unit | ValueType::Never)
        {
            return Err(Diagnostic::new(
                "E3007",
                format!(
                    "if without else requires a Void true branch, found {}",
                    true_value.value_type
                ),
                then_body.span,
            ));
        }
        let true_terminal = if true_value.value_type == ValueType::Never {
            never_register.get_or_insert(true_value.register);
            if !true_branch_continues
                && !matches!(
                    self.code.last(),
                    Some(Instruction::Return { .. } | Instruction::Stop { .. })
                )
            {
                let ownership_at_entry = outer_ownership.keys().copied().collect();
                let reason =
                    self.terminate_source_dead_path(&ownership_at_entry, then_body.span)?;
                true_operations.push(MirOperation::Constant {
                    destination: u32::from(reason),
                });
                Some(MirTerminator::Stop {
                    reason: u32::from(reason),
                })
            } else {
                match self.code.last() {
                    Some(Instruction::Return { source }) => Some(MirTerminator::Return {
                        source: u32::from(*source),
                    }),
                    Some(Instruction::Stop { reason }) => Some(MirTerminator::Stop {
                        reason: u32::from(*reason),
                    }),
                    Some(Instruction::Jump { .. }) if true_branch_continues => None,
                    _ => Some(MirTerminator::Unreachable),
                }
            }
        } else {
            let (result_register, result_type) = if let Some((register, value_type)) = &result {
                if *value_type != true_value.value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "if branches must have one exact result type",
                        then_body.span,
                    ));
                }
                (*register, value_type.clone())
            } else {
                let register = self.allocate(true_value.value_type.clone())?;
                result = Some((register, true_value.value_type.clone()));
                (register, true_value.value_type.clone())
            };
            self.code.push(Instruction::Move {
                destination: result_register,
                source: true_value.register,
            });
            self.mark_last_instruction(then_body.span);
            true_operations.push(MirOperation::Move {
                destination: u32::from(result_register),
                source: u32::from(true_value.register),
            });
            if is_affine(&result_type) {
                let ownership = self
                    .ownership_states
                    .get(&true_value.register)
                    .copied()
                    .unwrap_or(OwnershipRecord {
                        scope: 0,
                        state: MirOwnershipState::Live,
                        must_consume: matches!(result_type, ValueType::Task(_)),
                    });
                self.consume_ownership(true_value.register, MirOwnershipState::Moved);
                self.record_ownership(
                    result_register,
                    ownership.scope,
                    MirOwnershipState::Live,
                    ownership.must_consume,
                );
            }
            self.validate_conditional_branch_state(
                &outer_bindings,
                &outer_ownership,
                Some(result_register),
                span,
                &mut static_bindings,
                &mut static_ownership,
            )?;
            if true_runtime_falls_through {
                self.validate_conditional_branch_state(
                    &outer_bindings,
                    &outer_ownership,
                    Some(result_register),
                    span,
                    &mut continuing_bindings,
                    &mut continuing_ownership,
                )?;
            }
            jumps.push(self.code.len());
            self.code.push(Instruction::Jump { target: 0 });
            self.mark_last_instruction(then_body.span);
            None
        };

        let join = u32::try_from(self.code.len()).expect("instruction index fits");
        for jump in jumps {
            self.code[jump] = Instruction::Jump { target: join };
        }
        self.code[branch_index] = Instruction::BranchBool {
            condition: condition.register,
            false_target,
            true_target,
        };
        self.mark_instruction(branch_index, condition_span);

        let has_continuation = false_terminal.is_none() || true_terminal.is_none();
        let join_block = self.next_mir_block();
        self.mir_blocks[base as usize - 1] = MirBlock {
            operations: Vec::new(),
            terminator: MirTerminator::SwitchBool {
                false_target: false_block,
                true_target: true_block,
            },
        };
        self.mir_blocks[false_block as usize - 1] = MirBlock {
            operations: false_operations,
            terminator: false_region.entry.map_or_else(
                || {
                    false_terminal
                        .clone()
                        .unwrap_or(MirTerminator::Goto { target: join_block })
                },
                |target| MirTerminator::Goto { target },
            ),
        };
        self.mir_blocks[true_block as usize - 1] = MirBlock {
            operations: true_operations,
            terminator: true_region.entry.map_or_else(
                || {
                    true_terminal
                        .clone()
                        .unwrap_or(MirTerminator::Goto { target: join_block })
                },
                |target| MirTerminator::Goto { target },
            ),
        };
        if let Some(tail) = false_region.tail {
            self.set_mir_handoff(
                tail,
                false_terminal
                    .clone()
                    .unwrap_or(MirTerminator::Goto { target: join_block }),
            );
        }
        if let Some(tail) = true_region.tail {
            self.set_mir_handoff(
                tail,
                true_terminal
                    .clone()
                    .unwrap_or(MirTerminator::Goto { target: join_block }),
            );
        }
        if has_continuation {
            self.mir_blocks.push(MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            });
            self.register_mir_region(base, join_block);
        } else {
            if let Some(previous) = self.mir_tail {
                self.mir_blocks[previous as usize - 1].terminator =
                    MirTerminator::Goto { target: base };
            }
            self.mir_entries.push(base);
            self.mir_tail = None;
        }

        self.bindings = outer_bindings;
        self.control_reachable = outer_control_reachable;
        if let Some(joined) = continuing_bindings {
            for (name, state) in joined {
                if let Some(binding) = self.bindings.get_mut(&name) {
                    binding.moved = state.moved;
                    binding.value_scope = state.value_scope;
                }
            }
        }
        self.ownership_states = outer_ownership;
        if let Some(joined) = continuing_ownership {
            self.ownership_states.extend(joined);
        }

        let runtime_falls_through = match constant_condition {
            Some(true) => true_runtime_falls_through,
            Some(false) => false_runtime_falls_through,
            None => false_runtime_falls_through || true_runtime_falls_through,
        };
        let (register, value_type) = result
            .or_else(|| never_register.map(|register| (register, ValueType::Never)))
            .expect("an if always has a true and false path");
        if contains_stored_sub_agent(&value_type) {
            let value_scope = match (
                false_value.value_type != ValueType::Never,
                true_value.value_type != ValueType::Never,
            ) {
                (true, true) => self.deeper_scope(false_value_scope, true_value_scope),
                (true, false) => false_value_scope,
                (false, true) => true_value_scope,
                (false, false) => self.current_scope(),
            };
            self.sub_agent_value_scopes.insert(register, value_scope);
        }
        if !runtime_falls_through {
            self.runtime_terminal_values.insert(register);
        }
        let effects =
            self.union_effects([condition.effects, false_value.effects, true_value.effects]);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::If {
                    condition: Box::new(condition.hir),
                    then_branch: Box::new(true_hir),
                    else_branch: else_branch.map(|_| Box::new(false_hir)),
                },
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn validate_conditional_branch_state(
        &self,
        outer_bindings: &BTreeMap<String, LocalBinding>,
        outer_ownership: &BTreeMap<Register, OwnershipRecord>,
        result: Option<Register>,
        span: Span,
        joined_bindings: &mut Option<BTreeMap<String, BindingState>>,
        joined_ownership: &mut Option<BTreeMap<Register, OwnershipRecord>>,
    ) -> Result<(), Diagnostic> {
        let binding_state = outer_bindings
            .keys()
            .filter_map(|name| {
                self.bindings
                    .get(name)
                    .map(|binding| (name.clone(), BindingState::from(binding)))
            })
            .collect::<BTreeMap<_, _>>();
        let ownership_state = outer_ownership
            .keys()
            .filter_map(|register| {
                self.ownership_states
                    .get(register)
                    .map(|ownership| (*register, *ownership))
            })
            .chain(result.and_then(|register| {
                self.ownership_states
                    .get(&register)
                    .map(|ownership| (register, *ownership))
            }))
            .collect::<BTreeMap<_, _>>();
        if self.ownership_states.iter().any(|(register, ownership)| {
            !outer_ownership.contains_key(register)
                && Some(*register) != result
                && ownership.state == MirOwnershipState::Live
                && ownership.must_consume
        }) {
            return Err(Diagnostic::new(
                "E3011",
                "conditional branch leaves a live affine obligation",
                span,
            ));
        }
        if joined_bindings
            .as_ref()
            .is_some_and(|joined| joined != &binding_state)
            || joined_ownership
                .as_ref()
                .is_some_and(|joined| joined != &ownership_state)
        {
            return Err(Diagnostic::new(
                "E3011",
                "conditional paths must leave the same affine ownership state",
                span,
            ));
        }
        joined_bindings.get_or_insert(binding_state);
        joined_ownership.get_or_insert(ownership_state);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_match(
        &mut self,
        source: &LoweredExpr,
        arms: &[(LoweredPattern, LoweredExpr, Span)],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let source = self.compile_expression(source)?;
        if let ValueType::Record(layout) = source.value_type.clone() {
            return self.compile_record_match(source, &layout, arms, span);
        }
        let variant_count = match &source.value_type {
            ValueType::Bool | ValueType::Option(_) | ValueType::Result(_, _) => 2,
            ValueType::Enum(id) => self.global.bundle.enum_types[*id as usize].variants.len(),
            _ => {
                return Err(Diagnostic::new(
                    "E3007",
                    "match requires Bool, Option, Result, or a nominal enum",
                    span,
                ));
            }
        };
        let mut planned = vec![None; variant_count];
        let mut wildcard = None;
        for (arm_index, (pattern, _, pattern_span)) in arms.iter().enumerate() {
            if wildcard.is_some() || planned.iter().all(Option::is_some) {
                return Err(Diagnostic::new(
                    "E2016",
                    "match case is unreachable",
                    *pattern_span,
                ));
            }
            let variant = match (&source.value_type, pattern) {
                (ValueType::Bool, LoweredPattern::Bool(false)) => Some(0),
                (ValueType::Bool, LoweredPattern::Bool(true)) => Some(1),
                (
                    ValueType::Enum(id),
                    LoweredPattern::Enum {
                        name,
                        variant,
                        bindings,
                        fields,
                    },
                ) => {
                    let expected = resolve_named_type(
                        &self.global.bundle.modules,
                        &self.global.bundle.types,
                        &self.info.module,
                        name,
                        *pattern_span,
                    )?;
                    if expected != ValueType::Enum(*id) {
                        return Err(Diagnostic::new(
                            "E3007",
                            "match pattern uses a different nominal enum",
                            *pattern_span,
                        ));
                    }
                    let variant_index = self.global.bundle.enum_types[*id as usize]
                        .variants
                        .iter()
                        .position(|candidate| candidate.name == *variant)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                "E3007",
                                format!("unknown enum variant '{variant}'"),
                                *pattern_span,
                            )
                        })?;
                    match (
                        &self.global.bundle.enum_types[*id as usize].variants[variant_index]
                            .payload,
                        fields,
                    ) {
                        (EnumPayloadType::Unit, None) if bindings.is_empty() => {}
                        (EnumPayloadType::Tuple(expected), None)
                            if expected.len() == bindings.len() => {}
                        (EnumPayloadType::Record(expected), Some(fields)) => {
                            let supplied = fields
                                .iter()
                                .map(|(name, _, _)| name)
                                .collect::<BTreeSet<_>>();
                            if supplied.len() != fields.len()
                                || supplied.len() != expected.len()
                                || expected.iter().any(|field| !supplied.contains(&field.name))
                            {
                                return Err(Diagnostic::new(
                                    "E3007",
                                    "enum record pattern must contain every field exactly once",
                                    *pattern_span,
                                ));
                            }
                        }
                        _ => {
                            return Err(Diagnostic::new(
                                "E3007",
                                "enum pattern uses the wrong payload form or count",
                                *pattern_span,
                            ));
                        }
                    }
                    Some(variant_index)
                }
                (ValueType::Option(_), LoweredPattern::Option { some, .. }) => {
                    Some(usize::from(*some))
                }
                (ValueType::Result(_, _), LoweredPattern::Result { ok, .. }) => {
                    Some(usize::from(!*ok))
                }
                (_, LoweredPattern::Wildcard) => None,
                _ => {
                    return Err(Diagnostic::new(
                        "E3007",
                        "match pattern does not match the source type",
                        *pattern_span,
                    ));
                }
            };
            if let Some(variant) = variant {
                if planned[variant].replace(arm_index).is_some() {
                    return Err(Diagnostic::new(
                        "E2016",
                        "duplicate match case is unreachable",
                        *pattern_span,
                    ));
                }
            } else if wildcard.replace(arm_index).is_some() {
                return Err(Diagnostic::new(
                    "E2016",
                    "duplicate match case is unreachable",
                    *pattern_span,
                ));
            }
        }
        let missing = if wildcard.is_some() {
            Vec::new()
        } else {
            let cases = match &source.value_type {
                ValueType::Bool => vec!["false".to_owned(), "true".to_owned()],
                ValueType::Option(_) => vec!["None".to_owned(), "Some".to_owned()],
                ValueType::Result(_, _) => vec!["Ok".to_owned(), "Err".to_owned()],
                ValueType::Enum(id) => self.global.bundle.enum_types[*id as usize]
                    .variants
                    .iter()
                    .map(|variant| variant.name.clone())
                    .collect(),
                _ => unreachable!("match source type was validated"),
            };
            cases
                .into_iter()
                .enumerate()
                .filter_map(|(index, name)| planned[index].is_none().then_some(name))
                .collect::<Vec<_>>()
        };
        if !missing.is_empty() {
            return Err(Diagnostic::new(
                "E2015",
                format!(
                    "non-exhaustive match; missing cases: {}",
                    missing.join(", ")
                ),
                span,
            ));
        }
        for plan in &mut planned {
            if plan.is_none() {
                *plan = wildcard;
            }
        }
        let branch_index = self.code.len();
        self.code.push(Instruction::Jump { target: 0 });
        let mir_arm_count = planned.len();
        let base = self.next_mir_block();
        self.mir_blocks
            .extend((0..=mir_arm_count).map(|_| MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            }));
        let mut targets = Vec::new();
        let mut target_bindings = Vec::new();
        let mut jumps = Vec::new();
        let mut result = None;
        let mut never_register = None;
        let mut hir_arms = Vec::new();
        let mut arm_effects = Vec::new();
        let mut arm_operations = Vec::new();
        let mut arm_stops = Vec::new();
        let mut arm_regions = Vec::new();
        let mut arm_value_scopes = Vec::new();
        let branch_bindings = self.bindings.clone();
        let branch_ownership = self.ownership_states.clone();
        let mut joined_bindings: Option<BTreeMap<String, BindingState>> = None;
        let mut joined_ownership: Option<BTreeMap<Register, OwnershipRecord>> = None;
        for arm_index in planned.into_iter().map(Option::unwrap) {
            self.bindings = branch_bindings.clone();
            for (register, state) in &branch_ownership {
                self.ownership_states.insert(*register, *state);
            }
            targets.push(u32::try_from(self.code.len()).expect("instruction index fits"));
            let binding_names = match (&source.value_type, &arms[arm_index].0) {
                (
                    ValueType::Option(_),
                    LoweredPattern::Option {
                        some: true,
                        binding,
                    },
                )
                | (ValueType::Result(_, _), LoweredPattern::Result { binding, .. }) => {
                    vec![binding.clone()]
                }
                (
                    ValueType::Enum(id),
                    LoweredPattern::Enum {
                        variant,
                        bindings,
                        fields,
                        ..
                    },
                ) => {
                    let metadata = self.global.bundle.enum_types[*id as usize]
                        .variants
                        .iter()
                        .find(|candidate| candidate.name == *variant)
                        .expect("validated enum match variant");
                    match (&metadata.payload, fields) {
                        (EnumPayloadType::Tuple(_), None) => bindings.clone(),
                        (EnumPayloadType::Record(expected), Some(fields)) => {
                            let supplied = fields
                                .iter()
                                .map(|(name, _, binding)| (name, binding.clone()))
                                .collect::<BTreeMap<_, _>>();
                            expected
                                .iter()
                                .map(|field| supplied[&field.name].clone())
                                .collect()
                        }
                        _ => Vec::new(),
                    }
                }
                _ => Vec::new(),
            };
            let mut payload_registers = Vec::new();
            let mut previous_bindings = Vec::new();
            let payload_types = match (&source.value_type, &arms[arm_index].0) {
                (ValueType::Option(payload), LoweredPattern::Option { some: true, .. }) => {
                    vec![payload.as_ref().clone()]
                }
                (ValueType::Result(ok, _), LoweredPattern::Result { ok: true, .. }) => {
                    vec![ok.as_ref().clone()]
                }
                (ValueType::Result(_, error), LoweredPattern::Result { ok: false, .. }) => {
                    vec![error.as_ref().clone()]
                }
                (ValueType::Enum(id), LoweredPattern::Enum { variant, .. }) => {
                    let metadata = &self.global.bundle.enum_types[*id as usize];
                    let payload = &metadata
                        .variants
                        .iter()
                        .find(|candidate| candidate.name == *variant)
                        .expect("validated enum match variant")
                        .payload;
                    match payload {
                        EnumPayloadType::Unit => vec![],
                        EnumPayloadType::Tuple(values) => values.clone(),
                        EnumPayloadType::Record(fields) => fields
                            .iter()
                            .map(|field| field.value_type.clone())
                            .collect(),
                    }
                }
                _ => vec![],
            };
            for (index, value_type) in payload_types.into_iter().enumerate() {
                let register = self.allocate(value_type.clone())?;
                payload_registers.push(register);
                if let Some(Some(name)) = binding_names.get(index) {
                    if binding_names[..index]
                        .iter()
                        .flatten()
                        .any(|previous| previous == name)
                    {
                        return Err(Diagnostic::new(
                            "E3005",
                            format!("duplicate pattern binding '{name}'"),
                            arms[arm_index].2,
                        ));
                    }
                    let symbol = self.global.allocate_symbol();
                    let scope = self.active_scopes.last().copied().unwrap_or(0);
                    let previous = self.bindings.insert(
                        name.clone(),
                        LocalBinding {
                            register,
                            symbol,
                            value_type,
                            scope,
                            value_scope: scope,
                            mutable: false,
                            moved: false,
                        },
                    );
                    previous_bindings.push((name.clone(), previous));
                }
            }
            let region_capture = self.begin_nested_mir_region();
            let operation_start = self.mir.len();
            let value = self.compile_expression(&arms[arm_index].1)?;
            let region = self.finish_nested_mir_region(region_capture);
            arm_operations.push(self.mir.split_off(operation_start));
            arm_regions.push(region);
            arm_value_scopes.push((
                value.value_type != ValueType::Never,
                self.sub_agent_value_scope(&value),
            ));
            arm_stops.push((value.value_type == ValueType::Never).then(
                || match self.code.last() {
                    Some(Instruction::Stop { reason }) => u32::from(*reason),
                    _ => u32::MAX,
                },
            ));
            if value.value_type != ValueType::Never {
                let affine_bindings = self
                    .bindings
                    .iter()
                    .filter(|(_, binding)| {
                        is_affine(&binding.value_type)
                            || contains_stored_sub_agent(&binding.value_type)
                    })
                    .map(|(name, binding)| (name.clone(), BindingState::from(binding)))
                    .collect::<BTreeMap<_, _>>();
                let affine_ownership = branch_ownership
                    .keys()
                    .filter_map(|register| {
                        self.ownership_states
                            .get(register)
                            .map(|state| (*register, *state))
                    })
                    .collect::<BTreeMap<_, _>>();
                if joined_bindings
                    .as_ref()
                    .is_some_and(|joined| joined != &affine_bindings)
                    || joined_ownership
                        .as_ref()
                        .is_some_and(|joined| joined != &affine_ownership)
                {
                    return Err(Diagnostic::new(
                        "E3011",
                        "match paths must leave the same affine ownership state",
                        span,
                    ));
                }
                joined_bindings.get_or_insert(affine_bindings);
                joined_ownership.get_or_insert(affine_ownership);
            }
            for (name, previous) in previous_bindings {
                if let Some(previous) = previous {
                    self.bindings.insert(name, previous);
                } else {
                    self.bindings.remove(&name);
                }
            }
            target_bindings.push(payload_registers);
            if value.value_type == ValueType::Never {
                never_register.get_or_insert(value.register);
            } else if let Some((result_register, result_type)) = &result {
                if *result_type != value.value_type {
                    return Err(Diagnostic::new(
                        "E3007",
                        "every match arm must have one exact result type",
                        arms[arm_index].2,
                    ));
                }
                self.code.push(Instruction::Move {
                    destination: *result_register,
                    source: value.register,
                });
            } else {
                let result_register = self.allocate(value.value_type.clone())?;
                self.code.push(Instruction::Move {
                    destination: result_register,
                    source: value.register,
                });
                result = Some((result_register, value.value_type.clone()));
            }
            hir_arms.push(value.hir);
            arm_effects.push(value.effects);
            if value.value_type != ValueType::Never {
                jumps.push(self.code.len());
                self.code.push(Instruction::Jump { target: 0 });
            }
        }
        let join = u32::try_from(self.code.len()).expect("instruction index fits");
        for jump in jumps {
            self.code[jump] = Instruction::Jump { target: join };
        }
        self.code[branch_index] = if source.value_type == ValueType::Bool {
            Instruction::BranchBool {
                condition: source.register,
                false_target: targets[0],
                true_target: targets[1],
            }
        } else {
            Instruction::SwitchEnum {
                source: source.register,
                arms: targets
                    .iter()
                    .zip(&target_bindings)
                    .enumerate()
                    .map(
                        |(variant, (target, bindings))| allen_bytecode::EnumSwitchArm {
                            variant: u32::try_from(variant).expect("variant index fits"),
                            target: *target,
                            bindings: bindings.clone(),
                        },
                    )
                    .collect(),
            }
        };
        let join_block = self.next_mir_block();
        self.mir_blocks[base as usize - 1] = MirBlock {
            operations: Vec::new(),
            terminator: if source.value_type == ValueType::Bool {
                MirTerminator::SwitchBool {
                    false_target: base + 1,
                    true_target: base + 2,
                }
            } else {
                MirTerminator::SwitchEnum {
                    targets: (0..targets.len())
                        .map(|index| base + 1 + u32::try_from(index).expect("arm ID fits"))
                        .collect(),
                }
            },
        };
        let has_continuation = arm_stops.iter().any(Option::is_none);
        for (arm, ((operations, stop), region)) in arm_operations
            .into_iter()
            .zip(arm_stops)
            .zip(arm_regions)
            .enumerate()
        {
            let terminal = match stop {
                Some(reason) if reason != u32::MAX => Some(MirTerminator::Stop { reason }),
                Some(_) => Some(MirTerminator::Unreachable),
                None => None,
            };
            let arm_block = base + 1 + u32::try_from(arm).expect("arm ID fits");
            self.mir_blocks[arm_block as usize - 1] = MirBlock {
                operations,
                terminator: region.entry.map_or_else(
                    || {
                        terminal
                            .clone()
                            .unwrap_or(MirTerminator::Goto { target: join_block })
                    },
                    |target| MirTerminator::Goto { target },
                ),
            };
            if let Some(tail) = region.tail {
                self.set_mir_handoff(
                    tail,
                    terminal.unwrap_or(MirTerminator::Goto { target: join_block }),
                );
            }
        }
        if has_continuation {
            self.mir_blocks.push(MirBlock {
                operations: Vec::new(),
                terminator: MirTerminator::Unreachable,
            });
            self.register_mir_region(base, join_block);
        } else {
            if let Some(previous) = self.mir_tail {
                self.set_mir_handoff(previous, MirTerminator::Goto { target: base });
            }
            self.mir_entries.push(base);
            self.mir_tail = None;
        }
        let (register, value_type) = result
            .or_else(|| never_register.map(|register| (register, ValueType::Never)))
            .ok_or_else(|| Diagnostic::new("E3007", "match must contain at least one arm", span))?;
        if is_affine(&value_type) {
            return Err(Diagnostic::new(
                "E3011",
                "match cannot produce a future or task value",
                span,
            ));
        }
        if contains_stored_sub_agent(&value_type) {
            let value_scope = arm_value_scopes
                .into_iter()
                .filter_map(|(continues, scope)| continues.then_some(scope))
                .reduce(|left, right| self.deeper_scope(left, right))
                .unwrap_or_else(|| self.current_scope());
            self.sub_agent_value_scopes.insert(register, value_scope);
        }
        self.bindings = branch_bindings;
        if let Some(joined) = joined_bindings {
            for (name, state) in joined {
                if let Some(binding) = self.bindings.get_mut(&name) {
                    binding.moved = state.moved;
                    binding.value_scope = state.value_scope;
                }
            }
        }
        if let Some(joined) = joined_ownership {
            for (register, state) in joined {
                self.ownership_states.insert(register, state);
            }
        }
        let effects = self.union_effects(
            arm_effects
                .into_iter()
                .chain(std::iter::once(source.effects)),
        );
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Match {
                    source: Box::new(source.hir),
                    arms: hir_arms,
                },
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_prompt(
        &mut self,
        system: &LoweredExpr,
        context: Option<&LoweredExpr>,
        data: Option<&LoweredExpr>,
        output: &LoweredType,
        max_attempts: u32,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let system = self.compile_expression(system)?;
        if system.value_type != ValueType::String {
            return Err(Diagnostic::new(
                "E3010",
                "prompt system must be String",
                span,
            ));
        }
        let SemanticType::Value(output_type) = semantic_type(
            output,
            &BTreeSet::new(),
            &self.info.module,
            &self.global.bundle.modules,
            &self.global.bundle.types,
        )?
        else {
            return Err(Diagnostic::new(
                "E3007",
                "prompt output type must be concrete",
                output.span(),
            ));
        };
        if !is_strict_schema_type(&output_type) {
            return Err(Diagnostic::new(
                "E3011",
                "prompt output is not supported by the strict schema profile",
                output.span(),
            ));
        }
        let context = self.compile_prompt_segment(context, "context")?;
        let data = self.compile_prompt_segment(data, "data")?;
        let output_option = ValueType::Option(Box::new(output_type.clone()));
        let output_register = self.allocate(output_option.clone())?;
        self.code.push(Instruction::EnumNew {
            destination: output_register,
            variant: 0,
            payload: Vec::new(),
        });
        self.mir.push(MirOperation::Enum {
            destination: u32::from(output_register),
        });
        let attempts_register = self.allocate(ValueType::Int)?;
        let attempts_constant = self
            .global
            .constant(Constant::Int(i64::from(max_attempts)))?;
        self.code.push(Instruction::Const {
            destination: attempts_register,
            constant: attempts_constant,
        });
        self.mir.push(MirOperation::Constant {
            destination: u32::from(attempts_register),
        });
        let value_type = prompt_type(output_type);
        let register = self.allocate(value_type.clone())?;
        let registers = [
            system.register,
            context.register,
            data.register,
            output_register,
            attempts_register,
        ];
        self.code.push(Instruction::RecordNew {
            destination: register,
            fields: registers
                .iter()
                .enumerate()
                .map(|(index, register)| {
                    (u32::try_from(index).expect("prompt field index"), *register)
                })
                .collect(),
        });
        self.mir.push(MirOperation::Record {
            destination: u32::from(register),
        });
        let effects = self.union_effects([system.effects, context.effects, data.effects]);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Prompt(vec![system.hir, context.hir, data.hir]),
                None,
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_prompt_segment(
        &mut self,
        expression: Option<&LoweredExpr>,
        label: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let option_type = ValueType::Option(Box::new(ValueType::Unknown));
        let Some(expression) = expression else {
            let register = self.allocate(option_type.clone())?;
            self.code.push(Instruction::EnumNew {
                destination: register,
                variant: 0,
                payload: Vec::new(),
            });
            self.mir.push(MirOperation::Enum {
                destination: u32::from(register),
            });
            let effects = self.empty_effects();
            return Ok(CompiledExpr {
                register,
                value_type: option_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::Enum,
                    None,
                    &option_type,
                    effects,
                    Span { start: 0, end: 0 },
                ),
            });
        };
        let value = self.compile_prompt_data_value(expression, label)?;
        if !is_strict_schema_type(&value.value_type) {
            return Err(Diagnostic::new(
                "E3011",
                format!("prompt {label} is not supported by the strict schema profile"),
                expression.span,
            ));
        }
        let unknown = self.allocate(ValueType::Unknown)?;
        self.code.push(Instruction::ToUnknown {
            destination: unknown,
            source: value.register,
        });
        let register = self.allocate(option_type.clone())?;
        self.code.push(Instruction::EnumNew {
            destination: register,
            variant: 1,
            payload: vec![unknown],
        });
        self.mir.push(MirOperation::Enum {
            destination: u32::from(register),
        });
        Ok(CompiledExpr {
            register,
            value_type: option_type.clone(),
            effects: value.effects,
            hir: self.hir(
                HirExprKind::Enum,
                None,
                &option_type,
                value.effects,
                expression.span,
            ),
        })
    }

    pub(super) fn compile_prompt_data_value(
        &mut self,
        expression: &LoweredExpr,
        label: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let LoweredExprKind::Record { name, fields } = &expression.kind else {
            return self.compile_expression(expression);
        };
        if name != "$anonymous" {
            return self.compile_expression(expression);
        }
        let mut compiled = Vec::new();
        let mut seen = BTreeSet::new();
        for (name, value, field_span) in fields {
            if !seen.insert(name.clone()) {
                return Err(Diagnostic::new(
                    "E3010",
                    format!("duplicate prompt {label} field '{name}'"),
                    *field_span,
                ));
            }
            compiled.push((name.clone(), self.compile_expression(value)?));
        }
        compiled.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let value_type = ValueType::Record(
            compiled
                .iter()
                .map(|(name, value)| RecordField {
                    name: name.clone(),
                    value_type: value.value_type.clone(),
                })
                .collect(),
        );
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::RecordNew {
            destination: register,
            fields: compiled
                .iter()
                .enumerate()
                .map(|(index, (_, value))| {
                    (
                        u32::try_from(index).expect("prompt data field index"),
                        value.register,
                    )
                })
                .collect(),
        });
        self.mir.push(MirOperation::Record {
            destination: u32::from(register),
        });
        let effects = self.union_effects(compiled.iter().map(|(_, value)| value.effects));
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Record(compiled.into_iter().map(|(_, value)| value.hir).collect()),
                None,
                &value_type,
                effects,
                expression.span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_expected(
        &mut self,
        expression: &LoweredExpr,
        expected: &ValueType,
        label: &str,
    ) -> Result<CompiledExpr, Diagnostic> {
        let constructor = match &expression.kind {
            LoweredExprKind::Variable(name) if name == "None" => Some((name.as_str(), &[][..])),
            LoweredExprKind::Call {
                callee, arguments, ..
            } => match &callee.kind {
                LoweredExprKind::Variable(name)
                    if matches!(name.as_str(), "Some" | "Ok" | "Err") =>
                {
                    Some((name.as_str(), arguments.as_slice()))
                }
                _ => None,
            },
            _ => None,
        };
        if let Some((name, arguments)) = constructor {
            let (variant, payload_type) = match (name, expected) {
                ("None", ValueType::Option(_)) => (0, None),
                ("Some", ValueType::Option(value)) | ("Err", ValueType::Result(_, value)) => {
                    (1, Some(value.as_ref()))
                }
                ("Ok", ValueType::Result(value, _)) => (0, Some(value.as_ref())),
                _ => {
                    return Err(Diagnostic::new(
                        "E2019",
                        format!("{name} is not valid for expected type {expected}"),
                        expression.span,
                    ));
                }
            };
            let values = match payload_type {
                None if arguments.is_empty() => Vec::new(),
                Some(payload_type) if arguments.len() == 1 => vec![self.compile_expected(
                    &arguments[0],
                    payload_type,
                    "constructor payload",
                )?],
                _ => {
                    return Err(Diagnostic::new(
                        "E2019",
                        format!("{name} has the wrong payload count"),
                        expression.span,
                    ));
                }
            };
            let register = self.allocate(expected.clone())?;
            self.code.push(Instruction::EnumNew {
                destination: register,
                variant,
                payload: values.iter().map(|value| value.register).collect(),
            });
            self.mir.push(MirOperation::Enum {
                destination: u32::from(register),
            });
            let effects = self.union_effects(values.iter().map(|value| value.effects));
            return Ok(CompiledExpr {
                register,
                value_type: expected.clone(),
                effects,
                hir: self.hir(HirExprKind::Enum, None, expected, effects, expression.span),
            });
        }
        if let LoweredExprKind::List(elements) = &expression.kind {
            return self.compile_list(expression, elements, Some(expected));
        }
        if let LoweredExprKind::Map(entries) = &expression.kind {
            return self.compile_map(expression, entries, Some(expected));
        }
        if let (LoweredExprKind::Tuple(elements), ValueType::Tuple(element_types)) =
            (&expression.kind, expected)
        {
            if elements.len() != element_types.len() {
                return Err(Diagnostic::new(
                    "E3010",
                    "tuple value has the wrong element count",
                    expression.span,
                ));
            }
            let values = elements
                .iter()
                .zip(element_types)
                .map(|(element, element_type)| {
                    self.compile_expected(element, element_type, "tuple element")
                })
                .collect::<Result<Vec<_>, _>>()?;
            if values.iter().any(|value| is_affine(&value.value_type)) {
                return Err(Diagnostic::new(
                    "E3011",
                    "future or task values cannot be stored in a tuple",
                    expression.span,
                ));
            }
            if values
                .iter()
                .any(|value| contains_workspace(&value.value_type))
            {
                return Err(Diagnostic::new(
                    "E3011",
                    "Workspace cannot be stored in a tuple",
                    expression.span,
                ));
            }
            let register = self.allocate(expected.clone())?;
            self.code.push(Instruction::TupleNew {
                destination: register,
                elements: values.iter().map(|value| value.register).collect(),
            });
            self.mir.push(MirOperation::Tuple {
                destination: u32::from(register),
            });
            let effects = self.union_effects(values.iter().map(|value| value.effects));
            return Ok(CompiledExpr {
                register,
                value_type: expected.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::Tuple(values.into_iter().map(|value| value.hir).collect()),
                    None,
                    expected,
                    effects,
                    expression.span,
                ),
            });
        }
        let LoweredExprKind::Record { name, fields } = &expression.kind else {
            let value = self.compile_expression(expression)?;
            if value.value_type != ValueType::Never && &value.value_type != expected {
                return Err(Diagnostic::new(
                    expected_type_diagnostic_code(label),
                    format!("expected {expected}, found {}", value.value_type),
                    expression.span,
                ));
            }
            return Ok(value);
        };
        if name != "$anonymous" {
            let value = self.compile_expression(expression)?;
            if value.value_type != ValueType::Never && &value.value_type != expected {
                return Err(Diagnostic::new(
                    expected_type_diagnostic_code(label),
                    format!("expected {expected}, found {}", value.value_type),
                    expression.span,
                ));
            }
            return Ok(value);
        }
        let ValueType::Record(layout) = expected else {
            return Err(Diagnostic::new(
                "E3010",
                format!("anonymous record is not valid for this {label}"),
                expression.span,
            ));
        };
        if fields.len() != layout.len() {
            return Err(Diagnostic::new(
                "E3010",
                format!("{label} record requires every field exactly once"),
                expression.span,
            ));
        }
        let mut seen = BTreeSet::new();
        let mut compiled = Vec::new();
        for (field, value, field_span) in fields {
            if !seen.insert(field.clone()) {
                return Err(Diagnostic::new(
                    "E3010",
                    format!("duplicate {label} field '{field}'"),
                    *field_span,
                ));
            }
            let index = layout
                .iter()
                .position(|candidate| candidate.name == *field)
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E3010",
                        format!("{label} has no field '{field}'"),
                        *field_span,
                    )
                })?;
            let value = self.compile_expected(value, &layout[index].value_type, label)?;
            compiled.push((index, value));
        }
        compiled.sort_by_key(|(index, _)| *index);
        let register = self.allocate(expected.clone())?;
        self.code.push(Instruction::RecordNew {
            destination: register,
            fields: compiled
                .iter()
                .map(|(index, value)| {
                    (
                        u32::try_from(*index).expect("field index fits"),
                        value.register,
                    )
                })
                .collect(),
        });
        self.mir.push(MirOperation::Record {
            destination: u32::from(register),
        });
        let effects = self.union_effects(compiled.iter().map(|(_, value)| value.effects));
        Ok(CompiledExpr {
            register,
            value_type: expected.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Record(compiled.into_iter().map(|(_, value)| value.hir).collect()),
                None,
                expected,
                effects,
                expression.span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_call(
        &mut self,
        callee: &LoweredExpr,
        type_arguments: &[LoweredType],
        arguments: &[LoweredExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if is_task_snapshot_callee(callee) {
            return self.compile_task_snapshot(arguments, span);
        }
        if let Some(path) = tool_callee(callee) {
            let binding = self
                .global
                .bundle
                .tools
                .get(&path)
                .cloned()
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E3005",
                        "tool call is not in the frozen catalog",
                        callee.span,
                    )
                })?;
            if arguments.len() != 1 {
                return Err(Diagnostic::new(
                    "E3010",
                    "tool call requires exactly one input",
                    span,
                ));
            }
            let input = self.compile_expected(&arguments[0], &binding.input, "tool input")?;
            let value_type = ValueType::Future(Box::new(ValueType::Result(
                Box::new(binding.output.clone()),
                Box::new(binding.error.clone()),
            )));
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::ToolInvoke {
                destination: register,
                tool: binding.contract,
                input: input.register,
            });
            self.mir.push(MirOperation::ToolCall {
                destination: u32::from(register),
                tool: binding.contract,
                input: u32::from(input.register),
            });
            self.record_ownership(register, 0, MirOwnershipState::Live, true);
            let call_effect = effect_id(&self.global.effect_sets, &[binding.effect]);
            let effects = self.union_effects([input.effects, call_effect]);
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::ToolCall {
                        tool: binding.contract,
                        input: Box::new(input.hir),
                    },
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }
        if let Some(builtin) = standard_builtin_callee(callee) {
            return match builtin {
                StandardBuiltin::Workspace => {
                    if !type_arguments.is_empty() {
                        return Err(Diagnostic::new(
                            "E3005",
                            "fs.workspace does not take type arguments",
                            span,
                        ));
                    }
                    self.compile_workspace_get(arguments, span)
                }
                StandardBuiltin::Operation(operation) => {
                    self.compile_effect_call(operation, type_arguments, arguments, span)
                }
            };
        }
        if let Some(builtin) = collection_builtin_callee(callee) {
            if !type_arguments.is_empty() {
                return Err(Diagnostic::new(
                    "E3005",
                    "collection builtins do not take type arguments",
                    span,
                ));
            }
            return self.compile_collection_builtin(builtin, arguments, span);
        }
        if let Some(operation) = string_builtin_callee(callee) {
            if !type_arguments.is_empty() {
                return Err(Diagnostic::new(
                    "E3005",
                    "String builtins do not take type arguments",
                    span,
                ));
            }
            return self.compile_string_builtin(operation, arguments, span);
        }
        if let Some(operation) = capability_builtin_callee(callee) {
            if !type_arguments.is_empty() {
                return Err(Diagnostic::new(
                    "E3005",
                    "capability inspection does not take type arguments",
                    span,
                ));
            }
            return self.compile_capability_builtin(operation, arguments, span);
        }
        if matches!(&callee.kind, LoweredExprKind::Variable(name) if name == "narrow") {
            if type_arguments.len() != 1 || arguments.len() != 1 {
                return Err(Diagnostic::new(
                    "E2018",
                    "narrow<T> requires one concrete target type and one argument",
                    span,
                ));
            }
            let target = self.annotation_type(&type_arguments[0])?;
            if matches!(
                target,
                ValueType::Never
                    | ValueType::Unknown
                    | ValueType::Function { .. }
                    | ValueType::Future(_)
                    | ValueType::Task(_)
                    | ValueType::Workspace
                    | ValueType::SubAgent
            ) || contains_workspace(&target)
                || contains_affine(&target)
                || contains_sub_agent(&target)
            {
                return Err(Diagnostic::new(
                    "E2018",
                    "narrow target must be a complete concrete value type",
                    type_arguments[0].span(),
                ));
            }
            let value =
                self.compile_expected(&arguments[0], &ValueType::Unknown, "narrow input")?;
            let value_type = ValueType::Option(Box::new(target.clone()));
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::Narrow {
                destination: register,
                source: value.register,
                target,
            });
            self.mir.push(MirOperation::Enum {
                destination: u32::from(register),
            });
            let effects = value.effects;
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::Narrow(Box::new(value.hir)),
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }
        if !type_arguments.is_empty() {
            return Err(Diagnostic::new(
                "E3005",
                "only typed response operations take explicit type arguments",
                span,
            ));
        }
        let LoweredExprKind::Variable(name) = &callee.kind else {
            if let LoweredExprKind::FieldGet { record, field, .. } = &callee.kind {
                if let LoweredExprKind::Variable(enum_name) = &record.kind {
                    if resolve_named_type(
                        &self.global.bundle.modules,
                        &self.global.bundle.types,
                        &self.info.module,
                        enum_name,
                        record.span,
                    )
                    .is_ok_and(|value_type| matches!(value_type, ValueType::Enum(_)))
                    {
                        return self.compile_user_enum(
                            enum_name,
                            field,
                            &LoweredEnumValuePayload::Tuple(arguments.to_vec()),
                            span,
                        );
                    }
                }
            }
            return Err(Diagnostic::new(
                "E3005",
                "call target must be a resolved name",
                callee.span,
            ));
        };
        if matches!(
            name.as_str(),
            "to_float" | "to_string" | "to_bytes" | "to_unknown"
        ) {
            if arguments.len() != 1 {
                return Err(Diagnostic::new(
                    "E2011",
                    format!("'{name}' expects exactly one argument"),
                    span,
                ));
            }
            let value = self.compile_expression(&arguments[0])?;
            if name == "to_unknown" {
                if matches!(
                    value.value_type,
                    ValueType::Never
                        | ValueType::Unknown
                        | ValueType::Function { .. }
                        | ValueType::Future(_)
                        | ValueType::Task(_)
                        | ValueType::Workspace
                        | ValueType::SubAgent
                ) || contains_workspace(&value.value_type)
                    || contains_affine(&value.value_type)
                    || contains_sub_agent(&value.value_type)
                {
                    return Err(Diagnostic::new(
                        "E2018",
                        "to_unknown requires a concrete encodable value",
                        arguments[0].span,
                    ));
                }
                let register = self.allocate(ValueType::Unknown)?;
                self.code.push(Instruction::ToUnknown {
                    destination: register,
                    source: value.register,
                });
                self.mir.push(MirOperation::Move {
                    destination: u32::from(register),
                    source: u32::from(value.register),
                });
                let effects = value.effects;
                return Ok(CompiledExpr {
                    register,
                    value_type: ValueType::Unknown,
                    effects,
                    hir: self.hir(
                        HirExprKind::ToUnknown(Box::new(value.hir)),
                        None,
                        &ValueType::Unknown,
                        effects,
                        span,
                    ),
                });
            }
            let (conversion, value_type) = match (name.as_str(), &value.value_type) {
                ("to_float", ValueType::Int) => (Conversion::IntToFloat, ValueType::Float),
                ("to_bytes", ValueType::String) => (Conversion::StringToBytes, ValueType::Bytes),
                (
                    "to_string",
                    ValueType::Bool | ValueType::Int | ValueType::Float | ValueType::String,
                ) => (Conversion::ToString, ValueType::String),
                _ => {
                    return Err(Diagnostic::new(
                        "E2011",
                        format!("'{name}' does not accept {}", value.value_type),
                        arguments[0].span,
                    ));
                }
            };
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::Convert {
                destination: register,
                source: value.register,
                conversion,
            });
            self.mir.push(MirOperation::Move {
                destination: u32::from(register),
                source: u32::from(value.register),
            });
            let effects = value.effects;
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::Convert(Box::new(value.hir)),
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }
        if matches!(name.as_str(), "Some" | "Ok" | "Err") {
            let values = arguments
                .iter()
                .map(|argument| self.compile_expression(argument))
                .collect::<Result<Vec<_>, _>>()?;
            return self.compile_builtin_enum(name, &values, span);
        }
        if name == "stop" {
            let values = arguments
                .iter()
                .map(|argument| self.compile_expression(argument))
                .collect::<Result<Vec<_>, _>>()?;
            if values.len() != 1 || values[0].value_type != ValueType::String {
                return Err(Diagnostic::new(
                    "E3011",
                    "stop requires one String reason",
                    span,
                ));
            }
            self.code.push(Instruction::Stop {
                reason: values[0].register,
            });
            let effects = values[0].effects;
            return Ok(CompiledExpr {
                register: values[0].register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::Stop(Box::new(values.into_iter().next().expect("one value").hir)),
                    None,
                    &ValueType::Never,
                    effects,
                    span,
                ),
            });
        }
        if let Some(binding) = self.bindings.get(name).cloned() {
            let ValueType::Function {
                parameters,
                return_type,
                effects,
            } = &binding.value_type
            else {
                return Err(Diagnostic::new(
                    "E3005",
                    format!("local value '{name}' is not callable"),
                    callee.span,
                ));
            };
            if arguments.len() != parameters.len() {
                return Err(Diagnostic::new(
                    "E3010",
                    "callback arguments do not match its exact type",
                    span,
                ));
            }
            let values = arguments
                .iter()
                .zip(parameters)
                .map(|(argument, parameter)| {
                    self.compile_expected(argument, parameter, "callback argument")
                })
                .collect::<Result<Vec<_>, _>>()?;
            for value in &values {
                if is_affine(&value.value_type) {
                    let scope = self
                        .ownership_states
                        .get(&value.register)
                        .map_or(0, |ownership| ownership.scope);
                    if matches!(value.value_type, ValueType::Task(_)) && scope != 0 {
                        return Err(Diagnostic::new(
                            "E3011",
                            "task ownership cannot escape an await block",
                            span,
                        ));
                    }
                    self.consume_ownership(value.register, MirOwnershipState::Moved);
                }
            }
            let value_type = return_type.as_ref().clone();
            let result_scope = if contains_stored_sub_agent(&value_type) {
                Some(
                    values
                        .iter()
                        .filter(|value| contains_stored_sub_agent(&value.value_type))
                        .map(|value| self.sub_agent_value_scope(value))
                        .reduce(|left, right| self.deeper_scope(left, right))
                        .ok_or_else(|| {
                            Diagnostic::new(
                                "E3011",
                                "closure call returning SubAgent requires a SubAgent-containing argument",
                                span,
                            )
                        })?,
                )
            } else {
                None
            };
            let register = self.allocate(value_type.clone())?;
            self.code.push(Instruction::ClosureCall {
                destination: register,
                closure: binding.register,
                arguments: values.iter().map(|value| value.register).collect(),
            });
            self.mir.push(MirOperation::ClosureCall {
                destination: u32::from(register),
                closure: u32::from(binding.register),
                arguments: values
                    .iter()
                    .map(|value| u32::from(value.register))
                    .collect(),
            });
            if is_affine(&value_type) {
                self.record_ownership(register, 0, MirOwnershipState::Live, true);
            }
            if let Some(result_scope) = result_scope {
                self.sub_agent_value_scopes.insert(register, result_scope);
            }
            let effects = self.union_effects(
                values
                    .iter()
                    .map(|value| value.effects)
                    .chain(std::iter::once(*effects)),
            );
            return Ok(CompiledExpr {
                register,
                value_type: value_type.clone(),
                effects,
                hir: self.hir(
                    HirExprKind::ClosureCall(values.into_iter().map(|value| value.hir).collect()),
                    None,
                    &value_type,
                    effects,
                    span,
                ),
            });
        }

        let symbol = resolve_function_name(self.global.bundle, &self.info.module, name)?
            .ok_or_else(|| {
                Diagnostic::new("E3005", format!("unknown function '{name}'"), callee.span)
            })?;
        let target = self.global.bundle.functions[symbol as usize].clone();
        if arguments.len() != target.parameters.len() {
            return Err(Diagnostic::new(
                "E3007",
                format!("function '{name}' has the wrong argument count"),
                span,
            ));
        }
        let mut substitutions = BTreeMap::new();
        let mut values = Vec::with_capacity(arguments.len());
        for (parameter, argument) in target.parameters.iter().zip(arguments) {
            let value = if matches!(parameter, SemanticType::Generic(_)) {
                self.compile_expression(argument)?
            } else {
                let expected = concrete_type(parameter, &substitutions, &self.global.effect_sets)?;
                self.compile_expected(argument, &expected, "function argument")?
            };
            if let SemanticType::Generic(generic) = parameter {
                if let Some(previous) =
                    substitutions.insert(generic.clone(), value.value_type.clone())
                {
                    if previous != value.value_type {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!("generic '{generic}' inferred as two different types"),
                            span,
                        ));
                    }
                }
            }
            values.push(value);
        }
        let captured_obligation = values.iter().any(|value| {
            self.must_consume(value.register) || matches!(value.value_type, ValueType::Task(_))
        });
        let captured_scope = values
            .iter()
            .filter_map(|value| self.ownership_states.get(&value.register))
            .map(|ownership| ownership.scope)
            .find(|scope| *scope != 0)
            .unwrap_or(0);
        for value in &values {
            if is_affine(&value.value_type) {
                self.consume_ownership(value.register, MirOwnershipState::Moved);
            }
        }
        for (generic, declaration_span) in &target.lowered.generics {
            let inferred = substitutions.get(generic).ok_or_else(|| {
                Diagnostic::new(
                    "E3007",
                    format!("cannot infer generic '{generic}'"),
                    *declaration_span,
                )
            })?;
            if !inferred.is_equatable() {
                return Err(Diagnostic::new(
                    "E3008",
                    format!("type {inferred} does not satisfy Eq for '{generic}'"),
                    span,
                )
                .with_label(*declaration_span, "Eq constraint declared here"));
            }
        }
        let declared_return_type = concrete_type(
            &target.return_type,
            &substitutions,
            &self.global.effect_sets,
        )?;
        let callee_effects = effect_id(&self.global.effect_sets, &target.effects);
        let function = if let Some(function) = target.bytecode {
            function
        } else {
            let arguments = target
                .lowered
                .generics
                .iter()
                .map(|(generic, _)| substitutions[generic].clone())
                .collect::<Vec<_>>();
            if let Some((_, _, function)) = self
                .global
                .monomorphs
                .iter()
                .find(|(callee, types, _)| *callee == symbol && *types == arguments)
            {
                *function
            } else {
                let function = u32::try_from(self.global.functions.len()).map_err(|_| {
                    Diagnostic::new(
                        "E3005",
                        "too many generic instances",
                        target.lowered.name_span,
                    )
                })?;
                self.global.functions.push(None);
                self.global.monomorphs.push((symbol, arguments, function));
                let mut instance = target.clone();
                instance.bytecode = Some(function);
                let (compiled, hir, mir) = lower_one_function(
                    self.global,
                    instance.clone(),
                    function,
                    Vec::new(),
                    &substitutions,
                )
                .map_err(|diagnostic| diagnostic.with_source(&target.module))?;
                self.global.functions[function as usize] = Some(compiled);
                self.global
                    .hir_modules
                    .entry(instance.module)
                    .or_default()
                    .push(hir);
                self.global.mir_functions.push(mir);
                function
            }
        };
        let value_type = if target.lowered.is_async {
            ValueType::Future(Box::new(declared_return_type))
        } else {
            declared_return_type
        };
        let register = self.allocate(value_type.clone())?;
        let argument_registers = values
            .iter()
            .map(|value| value.register)
            .collect::<Vec<_>>();
        if target.lowered.is_async {
            self.code.push(Instruction::AsyncCall {
                destination: register,
                function,
                arguments: argument_registers,
            });
            self.mir.push(MirOperation::AsyncCall {
                destination: u32::from(register),
                function: symbol,
                arguments: values
                    .iter()
                    .map(|value| u32::from(value.register))
                    .collect(),
            });
            self.record_ownership(
                register,
                captured_scope,
                MirOwnershipState::Live,
                captured_obligation,
            );
        } else {
            self.code.push(Instruction::DirectCall {
                destination: register,
                function,
                arguments: argument_registers,
            });
            self.mir.push(MirOperation::DirectCall {
                destination: u32::from(register),
                function: symbol,
                arguments: values
                    .iter()
                    .map(|value| u32::from(value.register))
                    .collect(),
            });
            if is_affine(&value_type) {
                let result_scope = if matches!(value_type, ValueType::Task(_)) {
                    self.active_scopes.last().copied().unwrap_or(captured_scope)
                } else {
                    captured_scope
                };
                self.record_ownership(
                    register,
                    result_scope,
                    MirOwnershipState::Live,
                    captured_obligation || matches!(value_type, ValueType::Task(_)),
                );
            }
            if contains_stored_sub_agent(&value_type) {
                let result_scope = values
                    .iter()
                    .filter(|value| contains_stored_sub_agent(&value.value_type))
                    .map(|value| self.sub_agent_value_scope(value))
                    .reduce(|left, right| self.deeper_scope(left, right))
                    .ok_or_else(|| {
                        Diagnostic::new(
                            "E3011",
                            "direct call returning SubAgent requires a SubAgent-containing argument",
                            span,
                        )
                    })?;
                self.sub_agent_value_scopes.insert(register, result_scope);
            }
        }
        let effects = self.union_effects(
            values
                .iter()
                .map(|value| value.effects)
                .chain(std::iter::once(callee_effects)),
        );
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                if target.lowered.is_async {
                    HirExprKind::AsyncCall(values.into_iter().map(|value| value.hir).collect())
                } else {
                    HirExprKind::DirectCall(values.into_iter().map(|value| value.hir).collect())
                },
                Some(symbol),
                &value_type,
                effects,
                span,
            ),
        })
    }

    pub(super) fn compile_builtin_enum(
        &mut self,
        name: &str,
        values: &[CompiledExpr],
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        if values.len() != 1 {
            return Err(Diagnostic::new(
                "E3007",
                format!("{name} requires one payload"),
                span,
            ));
        }
        let (value_type, variant, expected) = match (&self.return_type, name) {
            (_, "Some") => (
                ValueType::Option(Box::new(values[0].value_type.clone())),
                1,
                values[0].value_type.clone(),
            ),
            (ValueType::Result(ok, _), "Ok") => (self.return_type.clone(), 0, ok.as_ref().clone()),
            (ValueType::Result(_, error), "Err") => {
                (self.return_type.clone(), 1, error.as_ref().clone())
            }
            _ => {
                return Err(Diagnostic::new(
                    "E3007",
                    format!("cannot infer {name} from this function return type"),
                    span,
                ));
            }
        };
        if values[0].value_type != expected {
            return Err(Diagnostic::new(
                "E3007",
                format!("{name} payload has the wrong type"),
                span,
            ));
        }
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::EnumNew {
            destination: register,
            variant,
            payload: vec![values[0].register],
        });
        self.mir.push(MirOperation::Enum {
            destination: u32::from(register),
        });
        let effects = values[0].effects;
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(HirExprKind::Enum, None, &value_type, effects, span),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_assignment(
        &mut self,
        name: &str,
        name_span: Span,
        operation: Option<Binary>,
        expression: &LoweredExpr,
    ) -> Result<CompiledExpr, Diagnostic> {
        let binding = self.bindings.get(name).cloned().ok_or_else(|| {
            Diagnostic::new("E3005", format!("unknown local value '{name}'"), name_span)
        })?;
        if !binding.mutable {
            return Err(Diagnostic::new(
                "E3010",
                format!("cannot assign to immutable binding '{name}'"),
                name_span,
            ));
        }
        if is_affine(&binding.value_type) {
            return Err(Diagnostic::new(
                "E3011",
                "future or task values cannot use mutable bindings",
                name_span,
            ));
        }
        if let Some(operation) = operation {
            if !matches!(binding.value_type, ValueType::Int | ValueType::Float) {
                return Err(Diagnostic::new(
                    "E2003",
                    "compound assignment requires an Int or Float mutable local",
                    name_span,
                ));
            }
            if operation == Binary::Remainder && binding.value_type != ValueType::Int {
                return Err(Diagnostic::new(
                    "E2003",
                    "remainder compound assignment requires Int",
                    name_span,
                ));
            }

            let old = self.allocate(binding.value_type.clone())?;
            self.code.push(Instruction::Move {
                destination: old,
                source: binding.register,
            });
            self.mark_last_instruction(name_span);
            self.mir.push(MirOperation::Move {
                destination: u32::from(old),
                source: u32::from(binding.register),
            });

            let working = self.allocate(binding.value_type.clone())?;
            self.code.push(Instruction::Move {
                destination: working,
                source: old,
            });
            self.mark_last_instruction(name_span);
            self.mir.push(MirOperation::Move {
                destination: u32::from(working),
                source: u32::from(old),
            });
            self.bindings
                .get_mut(name)
                .expect("compound assignment binding remains available")
                .register = working;
            let value = self.compile_expected(
                expression,
                &binding.value_type,
                "compound assignment operand",
            );
            self.bindings
                .get_mut(name)
                .expect("compound assignment binding remains available")
                .register = binding.register;
            let value = value?;
            if value.value_type == ValueType::Never {
                let effects = value.effects;
                return Ok(CompiledExpr {
                    register: value.register,
                    value_type: ValueType::Never,
                    effects,
                    hir: self.hir(
                        HirExprKind::Assignment(Box::new(value.hir)),
                        Some(binding.symbol),
                        &ValueType::Never,
                        effects,
                        Span {
                            start: name_span.start,
                            end: expression.span.end,
                        },
                    ),
                });
            }

            let result = self.allocate(binding.value_type.clone())?;
            let instruction = match operation {
                Binary::Add | Binary::Subtract | Binary::Multiply | Binary::Divide => {
                    let numeric = match operation {
                        Binary::Add => NumericBinaryOp::Add,
                        Binary::Subtract => NumericBinaryOp::Subtract,
                        Binary::Multiply => NumericBinaryOp::Multiply,
                        Binary::Divide => NumericBinaryOp::Divide,
                        _ => unreachable!("compound numeric operation was matched"),
                    };
                    if binding.value_type == ValueType::Int {
                        Instruction::IntBinary {
                            destination: result,
                            left: old,
                            right: value.register,
                            operation: numeric,
                        }
                    } else {
                        Instruction::FloatBinary {
                            destination: result,
                            left: old,
                            right: value.register,
                            operation: numeric,
                        }
                    }
                }
                Binary::Remainder => Instruction::IntRemainder {
                    destination: result,
                    left: old,
                    right: value.register,
                },
                _ => unreachable!("parser only creates arithmetic compound assignments"),
            };
            self.code.push(instruction);
            self.mark_last_instruction(Span {
                start: name_span.start,
                end: expression.span.end,
            });
            self.mir.push(MirOperation::Binary {
                destination: u32::from(result),
            });
            self.code.push(Instruction::Move {
                destination: binding.register,
                source: result,
            });
            self.mark_last_instruction(Span {
                start: name_span.start,
                end: expression.span.end,
            });
            self.mir.push(MirOperation::Move {
                destination: u32::from(binding.register),
                source: u32::from(result),
            });

            let effects = value.effects;
            let old_hir = self.hir(
                HirExprKind::Variable,
                Some(binding.symbol),
                &binding.value_type,
                self.empty_effects(),
                name_span,
            );
            let binary_hir = self.hir(
                HirExprKind::Binary(vec![old_hir, value.hir]),
                None,
                &binding.value_type,
                effects,
                Span {
                    start: name_span.start,
                    end: expression.span.end,
                },
            );
            return Ok(CompiledExpr {
                register: binding.register,
                value_type: ValueType::Unit,
                effects,
                hir: self.hir(
                    HirExprKind::Assignment(Box::new(binary_hir)),
                    Some(binding.symbol),
                    &ValueType::Unit,
                    effects,
                    Span {
                        start: name_span.start,
                        end: expression.span.end,
                    },
                ),
            });
        }
        let value = self.compile_expected(expression, &binding.value_type, "assignment")?;
        let value_scope = self.sub_agent_value_scope(&value);
        if value.value_type == ValueType::Never {
            let effects = value.effects;
            return Ok(CompiledExpr {
                register: value.register,
                value_type: ValueType::Never,
                effects,
                hir: self.hir(
                    HirExprKind::Assignment(Box::new(value.hir)),
                    Some(binding.symbol),
                    &ValueType::Never,
                    effects,
                    Span {
                        start: name_span.start,
                        end: expression.span.end,
                    },
                ),
            });
        }
        if contains_stored_sub_agent(&binding.value_type)
            && !self.scope_outlives(value_scope, binding.scope)
        {
            return Err(Diagnostic::new(
                "E3011",
                "SubAgent-containing value cannot escape its lexical scope through assignment",
                expression.span,
            ));
        }
        self.code.push(Instruction::Move {
            destination: binding.register,
            source: value.register,
        });
        self.mir.push(MirOperation::Move {
            destination: u32::from(binding.register),
            source: u32::from(value.register),
        });
        if contains_stored_sub_agent(&binding.value_type) {
            self.bindings
                .get_mut(name)
                .expect("assignment binding remains available")
                .value_scope = value_scope;
        }
        let effects = value.effects;
        Ok(CompiledExpr {
            register: binding.register,
            value_type: ValueType::Unit,
            effects,
            hir: self.hir(
                HirExprKind::Assignment(Box::new(value.hir)),
                Some(binding.symbol),
                &ValueType::Unit,
                effects,
                Span {
                    start: name_span.start,
                    end: expression.span.end,
                },
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_closure(
        &mut self,
        parameters: &[(String, LoweredType, Span)],
        return_type: &LoweredType,
        declared_effects: Option<&[String]>,
        body: &LoweredBody,
        span: Span,
    ) -> Result<CompiledExpr, Diagnostic> {
        let free_names = free_variables(body, parameters);
        let mut capture_names = free_names
            .into_iter()
            .filter(|name| self.bindings.contains_key(name))
            .collect::<Vec<_>>();
        capture_names.sort();
        let mut outer_captures = Vec::new();
        let mut capture_bindings = Vec::new();
        for name in capture_names {
            let binding = self.bindings[&name].clone();
            if binding.mutable {
                return Err(Diagnostic::new(
                    "E3010",
                    format!("closure cannot capture mutable binding '{name}'"),
                    span,
                ));
            }
            if is_affine(&binding.value_type) {
                return Err(Diagnostic::new(
                    "E3011",
                    format!("closure cannot capture affine binding '{name}'"),
                    span,
                ));
            }
            if contains_sub_agent(&binding.value_type) {
                return Err(Diagnostic::new(
                    "E3011",
                    format!("closure cannot capture SubAgent-containing binding '{name}'"),
                    span,
                ));
            }
            outer_captures.push(binding.register);
            capture_bindings.push((name, binding.value_type, binding.symbol));
        }
        let generics = BTreeSet::new();
        let semantic_parameters = parameters
            .iter()
            .map(|(_, value_type, _)| {
                semantic_type(
                    value_type,
                    &generics,
                    &self.info.module,
                    &self.global.bundle.modules,
                    &self.global.bundle.types,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let semantic_return = semantic_type(
            return_type,
            &generics,
            &self.info.module,
            &self.global.bundle.modules,
            &self.global.bundle.types,
        )?;
        let current = self
            .global
            .bundle
            .functions
            .iter()
            .map(|function| function.effects.clone())
            .collect::<Vec<_>>();
        let fake_lowered = LoweredFunction {
            exported: false,
            is_async: false,
            name: format!("$closure@{}", span.start),
            name_span: span,
            generics: Vec::new(),
            parameters: parameters.to_vec(),
            return_type: return_type.clone(),
            declared_effects: declared_effects.map(<[String]>::to_vec),
            effects_span: None,
            body: body.clone(),
        };
        let closure_symbol = self.global.allocate_symbol();
        let mut fake = FunctionInfo {
            module: self.info.module.clone(),
            symbol: closure_symbol,
            bytecode: None,
            lowered: fake_lowered,
            parameters: semantic_parameters,
            return_type: semantic_return,
            effects: Vec::new(),
        };
        if direct_capability_inspection_body_span(body).is_some()
            && !declared_effects.is_some_and(|effects| {
                effects
                    .binary_search_by(|effect| effect.as_str().cmp("capability.inspect"))
                    .is_ok()
            })
        {
            return Err(Diagnostic::new(
                "E2403",
                "closure directly inspects capabilities and must explicitly declare effect 'capability.inspect'",
                span,
            ));
        }
        let required = required_body_effects(self.global.bundle, &fake, &current)?;
        if let Some(declared) = declared_effects {
            if required
                .iter()
                .any(|effect| declared.binary_search(effect).is_err())
            {
                return Err(Diagnostic::new(
                    "E2403",
                    format!(
                        "closure requires undeclared effects [{}]",
                        required.join(", ")
                    ),
                    span,
                ));
            }
            fake.effects = declared.to_vec();
        } else {
            fake.effects = required;
        }
        let function_id = u32::try_from(self.global.functions.len())
            .map_err(|_| Diagnostic::new("E3005", "too many closure functions", span))?;
        fake.bytecode = Some(function_id);
        self.global.functions.push(None);
        let capture_symbols = capture_bindings
            .iter()
            .map(|(_, _, symbol)| *symbol)
            .collect::<Vec<_>>();
        let (function, hir_function, mir_function) = lower_one_function(
            self.global,
            fake.clone(),
            function_id,
            capture_bindings,
            &BTreeMap::new(),
        )
        .map_err(|diagnostic| diagnostic.with_source(&fake.module))?;
        self.global.functions[function_id as usize] = Some(function);
        let closure_hir_body = hir_function.body.clone();
        self.global
            .hir_modules
            .entry(fake.module.clone())
            .or_default()
            .push(hir_function);
        self.global.mir_functions.push(mir_function);

        let value_type = ValueType::Function {
            parameters: fake
                .parameters
                .iter()
                .map(|value_type| {
                    concrete_type(value_type, &BTreeMap::new(), &self.global.effect_sets)
                })
                .collect::<Result<_, _>>()?,
            return_type: Box::new(concrete_type(
                &fake.return_type,
                &BTreeMap::new(),
                &self.global.effect_sets,
            )?),
            effects: effect_id(&self.global.effect_sets, &fake.effects),
        };
        let register = self.allocate(value_type.clone())?;
        self.code.push(Instruction::ClosureNew {
            destination: register,
            function: function_id,
            captures: outer_captures.clone(),
        });
        self.mir.push(MirOperation::ClosureEnvironment {
            destination: u32::from(register),
            function: closure_symbol,
            captures: outer_captures
                .iter()
                .map(|value| u32::from(*value))
                .collect(),
        });
        let effects = effect_id(&self.global.effect_sets, &fake.effects);
        Ok(CompiledExpr {
            register,
            value_type: value_type.clone(),
            effects,
            hir: self.hir(
                HirExprKind::Closure {
                    captures: capture_symbols,
                    body: Box::new(closure_hir_body),
                },
                Some(closure_symbol),
                &value_type,
                effects,
                span,
            ),
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn compile_body(
        &mut self,
        body: &LoweredBody,
        return_type: &ValueType,
    ) -> Result<(HirExpr, Register), Diagnostic> {
        let mut expressions = Vec::new();
        let mut returned = None;
        for (index, statement) in body.statements.iter().enumerate() {
            match statement {
                LoweredStatement::Let {
                    name,
                    name_span,
                    mutable,
                    annotation,
                    value,
                } => {
                    if self.bindings.contains_key(name) {
                        return Err(Diagnostic::new(
                            "E3005",
                            format!("duplicate local binding '{name}'"),
                            *name_span,
                        ));
                    }
                    if let LoweredExprKind::Closure {
                        parameters, body, ..
                    } = &value.kind
                    {
                        if free_variables(body, parameters).contains(name) {
                            return Err(Diagnostic::new(
                                "E3010",
                                format!("closure '{name}' cannot capture itself"),
                                *name_span,
                            ));
                        }
                    }
                    let value = if let Some(annotation) = annotation {
                        let expected = self.annotation_type(annotation)?;
                        self.compile_expected(value, &expected, "binding")?
                    } else {
                        self.compile_expression(value)?
                    };
                    let value_scope = self.sub_agent_value_scope(&value);
                    if *mutable && is_affine(&value.value_type) {
                        return Err(Diagnostic::new(
                            "E3011",
                            "future or task values cannot use mutable bindings",
                            *name_span,
                        ));
                    }
                    let symbol = self.global.allocate_symbol();
                    let scope = self.active_scopes.last().copied().unwrap_or(0);
                    self.bindings.insert(
                        name.clone(),
                        LocalBinding {
                            register: value.register,
                            symbol,
                            value_type: value.value_type.clone(),
                            scope,
                            value_scope,
                            mutable: *mutable,
                            moved: false,
                        },
                    );
                    let terminates = value.value_type == ValueType::Never;
                    expressions.push(value.hir);
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                *name_span,
                            ));
                        }
                        returned = Some(value.register);
                    }
                }
                LoweredStatement::Assignment {
                    name,
                    name_span,
                    operation,
                    value,
                } => {
                    let assignment =
                        self.compile_assignment(name, *name_span, *operation, value)?;
                    let terminates = assignment.value_type == ValueType::Never;
                    let register = assignment.register;
                    expressions.push(assignment.hir);
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                value.span,
                            ));
                        }
                        returned = Some(register);
                    }
                }
                LoweredStatement::ControlFlow(expression) => {
                    let value = self.compile_expression(expression)?;
                    if !matches!(value.value_type, ValueType::Unit | ValueType::Never) {
                        return Err(Diagnostic::new(
                            "E3007",
                            format!(
                                "control-flow statement must have type Void, found {}",
                                value.value_type
                            ),
                            expression.span,
                        ));
                    }
                    let terminates = value.value_type == ValueType::Never;
                    let register = value.register;
                    expressions.push(value.hir);
                    if terminates {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating expression is unreachable",
                                expression.span,
                            ));
                        }
                        returned = Some(register);
                    }
                }
                LoweredStatement::Return(value, statement_span) => {
                    if index + 1 != body.statements.len() || body.tail.is_some() {
                        return Err(Diagnostic::new(
                            "E3005",
                            "code after return is unreachable",
                            *statement_span,
                        ));
                    }
                    let value = self.compile_return(value.as_ref(), *statement_span)?;
                    expressions.push(value.hir);
                    returned = Some(value.register);
                }
                LoweredStatement::While {
                    condition,
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_while(condition, loop_body, *span)?;
                    expressions.push(value.hir);
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after non-terminating loop is unreachable",
                                *span,
                            ));
                        }
                        returned = Some(value.register);
                    }
                }
                LoweredStatement::Loop {
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_infinite_loop(loop_body, *span)?;
                    expressions.push(value.hir);
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after non-terminating loop is unreachable",
                                *span,
                            ));
                        }
                        returned = Some(value.register);
                    }
                }
                LoweredStatement::For {
                    binding,
                    source,
                    body: loop_body,
                    span,
                } => {
                    let value = self.compile_for(binding, source, loop_body, *span)?;
                    expressions.push(value.hir);
                    if !value.falls_through {
                        if index + 1 != body.statements.len() || body.tail.is_some() {
                            return Err(Diagnostic::new(
                                "E3005",
                                "code after terminating loop header is unreachable",
                                *span,
                            ));
                        }
                        returned = Some(value.register);
                    }
                }
                LoweredStatement::Break(span) | LoweredStatement::Continue(span) => {
                    let value = self.compile_loop_control(
                        matches!(statement, LoweredStatement::Break(_)),
                        *span,
                    )?;
                    if index + 1 != body.statements.len() || body.tail.is_some() {
                        return Err(Diagnostic::new(
                            "E3005",
                            "code after loop control is unreachable",
                            *span,
                        ));
                    }
                    expressions.push(value.hir);
                    returned = Some(value.register);
                }
            }
        }
        if returned.is_none() {
            let value = if let Some(tail) = &body.tail {
                self.compile_expected(tail, return_type, "function result")?
            } else if return_type == &ValueType::Unit {
                self.compile_expression(&LoweredExpr {
                    kind: LoweredExprKind::Unit,
                    span: body.span,
                })?
            } else {
                return Err(Diagnostic::new(
                    "E3005",
                    "function can end without returning a value",
                    body.span,
                ));
            };
            if value.value_type != ValueType::Never && &value.value_type != return_type {
                return Err(Diagnostic::new(
                    "E3007",
                    format!(
                        "function body has {}, expected {return_type}",
                        value.value_type
                    ),
                    body.span,
                ));
            }
            if value.value_type != ValueType::Never {
                self.prepare_return(&value, body.span)?;
                self.code.push(Instruction::Return {
                    source: value.register,
                });
            }
            returned = Some(value.register);
            expressions.push(value.hir);
        }
        let effects = effect_id(&self.global.effect_sets, &self.info.effects);
        let hir = self.hir(
            HirExprKind::Block(expressions),
            Some(self.info.symbol),
            return_type,
            effects,
            body.span,
        );
        Ok((hir, returned.expect("body always returns")))
    }

    pub(super) fn prepare_return(
        &mut self,
        value: &CompiledExpr,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if matches!(value.value_type, ValueType::Task(_)) {
            let scope = self
                .ownership_states
                .get(&value.register)
                .map_or(0, |ownership| ownership.scope);
            if scope != 0 {
                return Err(Diagnostic::new(
                    "E3011",
                    "task ownership cannot escape an await block",
                    span,
                ));
            }
            self.record_ownership(value.register, scope, MirOwnershipState::Returned, true);
        } else if matches!(value.value_type, ValueType::Future(_)) {
            self.consume_ownership(value.register, MirOwnershipState::Returned);
        }
        self.reject_live_tasks(Some(value.register), span)
    }

    pub(super) fn reject_live_tasks(
        &self,
        returned: Option<Register>,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if let Some((register, _)) = self.ownership_states.iter().find(|(register, ownership)| {
            Some(**register) != returned
                && ownership.state == MirOwnershipState::Live
                && ownership.must_consume
        }) {
            return Err(Diagnostic::new(
                "E3011",
                format!("live affine obligation in register {register} is discarded"),
                span,
            ));
        }
        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn free_variables(
    body: &LoweredBody,
    parameters: &[(String, LoweredType, Span)],
) -> BTreeSet<String> {
    pub(super) fn expression(
        value: &LoweredExpr,
        bound: &BTreeSet<String>,
        free: &mut BTreeSet<String>,
    ) {
        match &value.kind {
            LoweredExprKind::Template(parts) => {
                for interpolation in template_interpolations(parts) {
                    expression(interpolation, bound, free);
                }
            }
            LoweredExprKind::Variable(name) => {
                if !bound.contains(name) {
                    free.insert(name.clone());
                }
            }
            LoweredExprKind::List(values)
            | LoweredExprKind::Tuple(values)
            | LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Tuple(values),
                ..
            } => {
                for value in values {
                    expression(value, bound, free);
                }
            }
            LoweredExprKind::Map(entries) => {
                for (key, value) in entries {
                    expression(key, bound, free);
                    expression(value, bound, free);
                }
            }
            LoweredExprKind::Record { fields, .. }
            | LoweredExprKind::Enum {
                payload: LoweredEnumValuePayload::Record(fields),
                ..
            } => {
                for (_, value, _) in fields {
                    expression(value, bound, free);
                }
            }
            LoweredExprKind::Prompt {
                system,
                context,
                data,
                ..
            } => {
                expression(system, bound, free);
                if let Some(context) = context {
                    expression(context, bound, free);
                }
                if let Some(data) = data {
                    expression(data, bound, free);
                }
            }
            LoweredExprKind::Binary { left, right, .. } => {
                expression(left, bound, free);
                expression(right, bound, free);
            }
            LoweredExprKind::Index { collection, index } => {
                expression(collection, bound, free);
                expression(index, bound, free);
            }
            LoweredExprKind::FieldGet { record, .. }
            | LoweredExprKind::Try(record)
            | LoweredExprKind::Unary {
                operand: record, ..
            } => {
                expression(record, bound, free);
            }
            LoweredExprKind::Spawn(value) | LoweredExprKind::Await(value) => {
                expression(value, bound, free);
            }
            LoweredExprKind::AwaitBlock(body) => {
                let mut nested = bound.clone();
                body_free_variables(body, &mut nested, free);
            }
            LoweredExprKind::Match { source, arms } => {
                expression(source, bound, free);
                for (pattern, value, _) in arms {
                    let mut arm_bound = bound.clone();
                    match pattern {
                        LoweredPattern::Option { binding, .. }
                        | LoweredPattern::Result { binding, .. } => {
                            if let Some(binding) = binding {
                                arm_bound.insert(binding.clone());
                            }
                        }
                        LoweredPattern::Record { fields, .. } => {
                            arm_bound.extend(
                                fields.iter().filter_map(|(_, _, binding)| binding.clone()),
                            );
                        }
                        LoweredPattern::Enum {
                            bindings, fields, ..
                        } => {
                            arm_bound.extend(bindings.iter().filter_map(Clone::clone));
                            if let Some(fields) = fields {
                                arm_bound.extend(
                                    fields.iter().filter_map(|(_, _, binding)| binding.clone()),
                                );
                            }
                        }
                        LoweredPattern::Wildcard | LoweredPattern::Bool(_) => {}
                    }
                    expression(value, &arm_bound, free);
                }
            }
            LoweredExprKind::If {
                condition,
                then_body,
                else_branch,
            } => {
                expression(condition, bound, free);
                let mut then_bound = bound.clone();
                body_free_variables(then_body, &mut then_bound, free);
                match else_branch {
                    Some(LoweredElse::Body(body)) => {
                        let mut else_bound = bound.clone();
                        body_free_variables(body, &mut else_bound, free);
                    }
                    Some(LoweredElse::If(value)) => expression(value, bound, free),
                    None => {}
                }
            }
            LoweredExprKind::Call {
                callee, arguments, ..
            } => {
                if !is_task_snapshot_callee(callee)
                    && standard_builtin_callee(callee).is_none()
                    && collection_builtin_callee(callee).is_none()
                    && string_builtin_callee(callee).is_none()
                    && capability_builtin_callee(callee).is_none()
                    && tool_callee(callee).is_none()
                {
                    expression(callee, bound, free);
                }
                for argument in arguments {
                    expression(argument, bound, free);
                }
            }
            LoweredExprKind::Closure {
                parameters, body, ..
            } => {
                let mut nested = bound.clone();
                nested.extend(parameters.iter().map(|(name, _, _)| name.clone()));
                body_free_variables(body, &mut nested, free);
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
            } => {}
        }
    }

    pub(super) fn body_free_variables(
        body: &LoweredBody,
        bound: &mut BTreeSet<String>,
        free: &mut BTreeSet<String>,
    ) {
        for statement in &body.statements {
            match statement {
                LoweredStatement::Let { name, value, .. } => {
                    expression(value, bound, free);
                    bound.insert(name.clone());
                }
                LoweredStatement::Assignment { name, value, .. } => {
                    if !bound.contains(name) {
                        free.insert(name.clone());
                    }
                    expression(value, bound, free);
                }
                LoweredStatement::ControlFlow(value) => expression(value, bound, free),
                LoweredStatement::Return(value, _) => {
                    if let Some(value) = value {
                        expression(value, bound, free);
                    }
                }
                LoweredStatement::While {
                    condition, body, ..
                } => {
                    expression(condition, bound, free);
                    let mut nested = bound.clone();
                    body_free_variables(body, &mut nested, free);
                }
                LoweredStatement::Loop { body, .. } => {
                    let mut nested = bound.clone();
                    body_free_variables(body, &mut nested, free);
                }
                LoweredStatement::For {
                    binding,
                    source,
                    body,
                    ..
                } => {
                    match source {
                        LoweredForSource::Iterable(value) => expression(value, bound, free),
                        LoweredForSource::Range { start, end } => {
                            expression(start, bound, free);
                            expression(end, bound, free);
                        }
                    }
                    let mut nested = bound.clone();
                    nested.extend(
                        binding
                            .elements
                            .iter()
                            .filter_map(|element| element.name.clone()),
                    );
                    body_free_variables(body, &mut nested, free);
                }
                LoweredStatement::Break(_) | LoweredStatement::Continue(_) => {}
            }
        }
        if let Some(tail) = &body.tail {
            expression(tail, bound, free);
        }
    }

    let mut bound = parameters
        .iter()
        .map(|(name, _, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let mut free = BTreeSet::new();
    body_free_variables(body, &mut bound, &mut free);
    free
}

#[allow(clippy::too_many_lines)]
pub(super) fn lower_one_function(
    global: &mut GlobalLowering<'_>,
    info: FunctionInfo,
    function_id: FunctionId,
    capture_values: Vec<(String, ValueType, SymbolId)>,
    substitutions: &BTreeMap<String, ValueType>,
) -> Result<(Function, HirFunction, MirFunction), Diagnostic> {
    if info.lowered.is_async {
        global.async_functions.insert(function_id);
    }
    let return_type = concrete_type(&info.return_type, substitutions, &global.effect_sets)?;
    let effects = effect_id(&global.effect_sets, &info.effects);
    let mut lowering = FunctionLowering {
        global,
        info: info.clone(),
        return_type: return_type.clone(),
        registers: Vec::new(),
        parameters: Vec::new(),
        captures: Vec::new(),
        bindings: BTreeMap::new(),
        code: Vec::new(),
        instruction_spans: BTreeMap::new(),
        mir: Vec::new(),
        mir_blocks: Vec::new(),
        mir_suspensions: Vec::new(),
        mir_task_scopes: Vec::new(),
        mir_ownership: Vec::new(),
        ownership_states: BTreeMap::new(),
        active_scopes: Vec::new(),
        next_scope: 1,
        mir_continuations: BTreeSet::new(),
        mir_entries: Vec::new(),
        mir_tail: None,
        loops: Vec::new(),
        control_reachable: true,
        runtime_terminal_values: BTreeSet::new(),
        sub_agent_value_scopes: BTreeMap::new(),
    };
    for ((name, _, span), parameter_type) in info.lowered.parameters.iter().zip(&info.parameters) {
        let value_type =
            concrete_type(parameter_type, substitutions, &lowering.global.effect_sets)?;
        let register = lowering.allocate(value_type.clone())?;
        let symbol = lowering.global.allocate_symbol();
        lowering.parameters.push(register);
        if lowering
            .bindings
            .insert(
                name.clone(),
                LocalBinding {
                    register,
                    symbol,
                    value_type: value_type.clone(),
                    scope: 0,
                    value_scope: 0,
                    mutable: false,
                    moved: false,
                },
            )
            .is_some()
        {
            return Err(Diagnostic::new(
                "E3005",
                format!("duplicate parameter '{name}'"),
                *span,
            ));
        }
        if is_affine(&value_type) {
            lowering.record_ownership(register, 0, MirOwnershipState::Live, true);
        }
    }
    for (name, value_type, symbol) in capture_values {
        let register = lowering.allocate(value_type.clone())?;
        lowering.captures.push(register);
        lowering.bindings.insert(
            name,
            LocalBinding {
                register,
                symbol,
                value_type: value_type.clone(),
                scope: 0,
                value_scope: 0,
                mutable: false,
                moved: false,
            },
        );
        if is_affine(&value_type) {
            lowering.record_ownership(register, 0, MirOwnershipState::Live, true);
        }
    }
    let (body, return_register) = lowering.compile_body(&info.lowered.body, &return_type)?;
    let parameters = lowering
        .parameters
        .iter()
        .map(|register| {
            lowering
                .global
                .intern_type(lowering.registers[*register as usize].clone())
        })
        .collect();
    let return_type_id = lowering.global.intern_type(return_type.clone());
    let temporaries = lowering
        .registers
        .iter()
        .cloned()
        .map(|value_type| lowering.global.intern_type(value_type))
        .collect();
    // Bytecode function symbols use the verifier's normalized path grammar.
    // Source, import, and entry metadata retain the exact `pkg://` identity.
    let bytecode_module = bytecode_module_path(&info.module);
    let symbol_name = if substitutions.is_empty() {
        format!("{bytecode_module}::{}", info.lowered.name)
    } else {
        let arguments = substitutions
            .iter()
            .map(|(name, value_type)| format!("{name}={value_type}"))
            .collect::<Vec<_>>()
            .join(",");
        format!("{bytecode_module}::{}<{arguments}>", info.lowered.name)
    };
    let stop_reason = match lowering.code.last() {
        Some(Instruction::Stop { reason }) => Some(*reason),
        _ => None,
    };
    let instruction_spans = std::mem::take(&mut lowering.instruction_spans);
    let function = Function {
        name: symbol_name,
        parameters: lowering.parameters,
        captures: lowering.captures,
        registers: lowering.registers,
        return_type,
        effects,
        code: lowering.code,
    };
    let source = lowering
        .global
        .debug_sources
        .binary_search(&info.module)
        .expect("resolved module has a debug source ID");
    let source = u32::try_from(source).expect("debug source ID fits");
    let mut locations = Vec::with_capacity(function.code.len());
    for instruction in 0..function.code.len() {
        let span = instruction_spans
            .get(&instruction)
            .copied()
            .unwrap_or(info.lowered.body.span);
        let start = u32::try_from(span.start)
            .map_err(|_| Diagnostic::new("E3005", "source span exceeds artifact limits", span))?;
        let end = u32::try_from(span.end)
            .map_err(|_| Diagnostic::new("E3005", "source span exceeds artifact limits", span))?;
        locations.push(DebugLocation {
            function: function_id,
            instruction: u32::try_from(instruction).expect("instruction ID fits"),
            source,
            start,
            end,
        });
    }
    lowering.global.debug_locations.extend(locations);
    let hir = HirFunction {
        symbol: info.symbol,
        name: info.lowered.name.clone(),
        is_async: info.lowered.is_async,
        parameters,
        return_type: return_type_id,
        effects,
        body,
    };
    let final_terminator = if let Some(entry) = lowering.mir_entries.first() {
        MirTerminator::Goto { target: *entry }
    } else {
        match stop_reason {
            Some(reason) => MirTerminator::Stop {
                reason: u32::from(reason),
            },
            _ => MirTerminator::Return {
                source: u32::from(return_register),
            },
        }
    };
    let mut blocks = vec![MirBlock {
        operations: lowering.mir,
        terminator: final_terminator,
    }];
    blocks.extend(lowering.mir_blocks);
    for continuation in lowering.mir_continuations {
        let block = &mut blocks[continuation as usize];
        if matches!(block.terminator, MirTerminator::Unreachable) {
            block.terminator = match stop_reason {
                Some(reason) => MirTerminator::Stop {
                    reason: u32::from(reason),
                },
                _ => MirTerminator::Return {
                    source: u32::from(return_register),
                },
            };
        }
    }
    let mir = MirFunction {
        symbol: info.symbol,
        name: info.lowered.name,
        is_async: info.lowered.is_async,
        temporaries,
        blocks,
        suspensions: lowering.mir_suspensions,
        task_scopes: lowering.mir_task_scopes,
        ownership: lowering.mir_ownership,
    };
    mir.validate_cfg().map_err(|message| {
        Diagnostic::new(
            "E3011",
            format!("invalid generated MIR: {message}"),
            info.lowered.body.span,
        )
    })?;
    Ok((function, hir, mir))
}

pub(super) fn bytecode_module_path(module: &str) -> String {
    let Some(package_path) = module.strip_prefix("pkg://") else {
        return module.to_owned();
    };
    let Some((package, source_path)) = package_path.split_once('/') else {
        return module.to_owned();
    };
    let Some((name, version)) = package.rsplit_once('@') else {
        return module.to_owned();
    };
    let mut components = vec![
        "pkg".to_owned(),
        escape_package_symbol_component(name),
        escape_package_symbol_component(version),
    ];
    let mut source_components = source_path.split('/').collect::<Vec<_>>();
    let Some(file_name) = source_components.pop() else {
        return module.to_owned();
    };
    let Some(file_stem) = file_name.strip_suffix(".allen") else {
        return module.to_owned();
    };
    components.extend(
        source_components
            .into_iter()
            .map(escape_package_symbol_component),
    );
    // The bytecode verifier requires a normalized ASCII path ending in
    // `.allen`. Every canonical URI component is otherwise hex escaped.
    components.push(format!(
        "{}.allen",
        escape_package_symbol_component(file_stem)
    ));
    components.join("/")
}

pub(super) fn escape_package_symbol_component(component: &str) -> String {
    let mut escaped = String::with_capacity(1 + component.len() * 2);
    escaped.push('x');
    for byte in component.bytes() {
        use std::fmt::Write as _;
        write!(&mut escaped, "{byte:02x}").expect("writing into String cannot fail");
    }
    escaped
}
