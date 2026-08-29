//! Public mid-level control-flow IR and its structural validator.

use super::{SymbolId, TypeId};
use allen_bytecode::{
    CapabilityOperation, CheckedIntOperation, CollectionOperation, FsOperation, ListCombinator,
    SafeCollectionOperation, StandardOperation, StringOperation,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MirBundle {
    pub constants: Vec<MirConstant>,
    pub functions: Vec<MirFunction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MirConstant {
    pub symbol: SymbolId,
    pub name: String,
    pub value_type: TypeId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MirFunction {
    pub symbol: SymbolId,
    pub name: String,
    pub is_async: bool,
    pub temporaries: Vec<TypeId>,
    pub blocks: Vec<MirBlock>,
    pub suspensions: Vec<MirSuspension>,
    pub task_scopes: Vec<MirTaskScope>,
    pub ownership: Vec<MirOwnership>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MirSuspension {
    pub destination: u32,
    pub source: u32,
    pub resume: u32,
    pub exceptional_cancel: u32,
    pub timeout_cancel: u32,
    pub external_cancel: u32,
    pub permanent_stop: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MirTaskScope {
    pub scope: u32,
    pub normal_join: u32,
    pub exceptional_cancel: u32,
    pub timeout_cancel: u32,
    pub external_cancel: u32,
    pub permanent_stop: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MirOwnershipState {
    Live,
    Moved,
    Awaited,
    ScopeJoined,
    Returned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MirOwnership {
    pub temporary: u32,
    pub scope: u32,
    pub state: MirOwnershipState,
    pub must_consume: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MirCleanupKind {
    NormalJoin,
    ExceptionalCancel,
    TimeoutCancel,
    ExternalCancel,
    PermanentStop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MirBlock {
    pub operations: Vec<MirOperation>,
    pub terminator: MirTerminator,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MirListItem {
    Element(u32),
    Spread(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MirMapItem {
    Entry { key: u32, value: u32 },
    Spread(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MirOperation {
    Constant {
        destination: u32,
    },
    Binary {
        destination: u32,
    },
    Tuple {
        destination: u32,
    },
    List {
        destination: u32,
        items: Vec<MirListItem>,
    },
    Length {
        destination: u32,
        collection: u32,
    },
    Range {
        destination: u32,
        start: u32,
        end: u32,
        inclusive: bool,
    },
    Slice {
        destination: u32,
        collection: u32,
        start: u32,
        end: u32,
    },
    SequenceFromList {
        destination: u32,
        values: u32,
    },
    SequenceMap {
        destination: u32,
        sequence: u32,
        callback: u32,
    },
    SequenceFilter {
        destination: u32,
        sequence: u32,
        callback: u32,
    },
    SequenceTake {
        destination: u32,
        sequence: u32,
        count: u32,
    },
    SequenceFind {
        destination: u32,
        sequence: u32,
        callback: u32,
    },
    SequenceAny {
        destination: u32,
        sequence: u32,
        callback: u32,
    },
    SequenceAll {
        destination: u32,
        sequence: u32,
        callback: u32,
    },
    SequenceFold {
        destination: u32,
        sequence: u32,
        initial: u32,
        callback: u32,
    },
    SequenceToList {
        destination: u32,
        sequence: u32,
    },
    StringOperation {
        destination: u32,
        operation: StringOperation,
        arguments: Vec<u32>,
    },
    TemplateRender {
        destination: u32,
        template: u32,
        arguments: Vec<u32>,
    },
    StandardOperation {
        destination: u32,
        operation: StandardOperation,
        arguments: Vec<u32>,
    },
    CapabilityInspect {
        destination: u32,
        operation: CapabilityOperation,
        arguments: Vec<u32>,
    },
    SafeCollectionOperation {
        destination: u32,
        operation: SafeCollectionOperation,
        arguments: Vec<u32>,
    },
    CheckedIntOperation {
        destination: u32,
        operation: CheckedIntOperation,
        arguments: Vec<u32>,
    },
    CollectionOperation {
        destination: u32,
        operation: CollectionOperation,
        arguments: Vec<u32>,
    },
    ListFold {
        destination: u32,
        values: u32,
        initial: u32,
        callback: u32,
    },
    ListCombinator {
        destination: u32,
        operation: ListCombinator,
        values: u32,
        initial: Option<u32>,
        callback: u32,
        callback_result: u32,
    },
    ListAppend {
        destination: u32,
        values: u32,
        value: u32,
    },
    ListSet {
        destination: u32,
        values: u32,
        index: u32,
        value: u32,
    },
    Map {
        destination: u32,
        items: Vec<MirMapItem>,
    },
    MapEntryAt {
        destination: u32,
        map: u32,
        index: u32,
    },
    Record {
        destination: u32,
    },
    Enum {
        destination: u32,
    },
    NewtypeWrap {
        destination: u32,
        source: u32,
    },
    NewtypeUnwrap {
        destination: u32,
        source: u32,
    },
    FieldGet {
        destination: u32,
        record: u32,
    },
    DirectCall {
        destination: u32,
        function: SymbolId,
        arguments: Vec<u32>,
    },
    AsyncCall {
        destination: u32,
        function: SymbolId,
        arguments: Vec<u32>,
    },
    Spawn {
        destination: u32,
        future: u32,
        scope: u32,
    },
    TaskSnapshot {
        destination: u32,
        source: u32,
    },
    WorkspaceGet {
        destination: u32,
    },
    EffectCall {
        destination: u32,
        operation: FsOperation,
        arguments: Vec<u32>,
    },
    ToolCall {
        destination: u32,
        tool: u32,
        input: u32,
    },
    Await {
        destination: u32,
        source: u32,
    },
    TaskScopeEnter {
        scope: u32,
    },
    TaskScopeExit {
        scope: u32,
    },
    TaskScopeCleanup {
        scope: u32,
        kind: MirCleanupKind,
    },
    ClosureEnvironment {
        destination: u32,
        function: SymbolId,
        captures: Vec<u32>,
    },
    ClosureCall {
        destination: u32,
        closure: u32,
        arguments: Vec<u32>,
    },
    Move {
        destination: u32,
        source: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MirTerminator {
    Goto {
        target: u32,
    },
    SwitchBool {
        false_target: u32,
        true_target: u32,
    },
    SwitchEnum {
        targets: Vec<u32>,
    },
    TryResult {
        success: u32,
        error: u32,
    },
    TryOption {
        some: u32,
        none: u32,
    },
    Return {
        source: u32,
    },
    Suspend {
        destination: u32,
        source: u32,
        resume: u32,
        exceptional_cancel: u32,
        timeout_cancel: u32,
        external_cancel: u32,
        permanent_stop: u32,
    },
    TaskScopeExit {
        scope: u32,
        normal_join: u32,
        exceptional_cancel: u32,
        timeout_cancel: u32,
        external_cancel: u32,
        permanent_stop: u32,
    },
    Stop {
        reason: u32,
    },
    Fail {
        reason: u32,
    },
    Unreachable,
}

impl MirFunction {
    /// Validate MIR target integrity and structured cleanup reachability.
    ///
    /// # Errors
    ///
    /// Returns one stable message when a target, cleanup edge, or reachable
    /// block is malformed.
    #[allow(clippy::too_many_lines)]
    pub fn validate_cfg(&self) -> Result<(), &'static str> {
        if self.blocks.is_empty() {
            return Err("MIR function has no entry block");
        }
        let block_count = self.blocks.len();
        let targets = |terminator: &MirTerminator| -> Vec<u32> {
            match terminator {
                MirTerminator::Goto { target } => vec![*target],
                MirTerminator::SwitchBool {
                    false_target,
                    true_target,
                } => vec![*false_target, *true_target],
                MirTerminator::SwitchEnum { targets } => targets.clone(),
                MirTerminator::TryResult { success, error } => vec![*success, *error],
                MirTerminator::TryOption { some, none } => vec![*some, *none],
                MirTerminator::Suspend {
                    resume,
                    exceptional_cancel,
                    timeout_cancel,
                    external_cancel,
                    permanent_stop,
                    ..
                } => vec![
                    *resume,
                    *exceptional_cancel,
                    *timeout_cancel,
                    *external_cancel,
                    *permanent_stop,
                ],
                MirTerminator::TaskScopeExit {
                    normal_join,
                    exceptional_cancel,
                    timeout_cancel,
                    external_cancel,
                    permanent_stop,
                    ..
                } => vec![
                    *normal_join,
                    *exceptional_cancel,
                    *timeout_cancel,
                    *external_cancel,
                    *permanent_stop,
                ],
                MirTerminator::Return { .. }
                | MirTerminator::Stop { .. }
                | MirTerminator::Fail { .. }
                | MirTerminator::Unreachable => Vec::new(),
            }
        };
        for block in &self.blocks {
            let mut temporaries = Vec::new();
            for operation in &block.operations {
                match operation {
                    MirOperation::Constant { destination }
                    | MirOperation::Binary { destination }
                    | MirOperation::Tuple { destination }
                    | MirOperation::List { destination, .. }
                    | MirOperation::Map { destination, .. }
                    | MirOperation::Record { destination }
                    | MirOperation::Enum { destination }
                    | MirOperation::WorkspaceGet { destination } => temporaries.push(*destination),
                    MirOperation::Length {
                        destination,
                        collection,
                    } => temporaries.extend([*destination, *collection]),
                    MirOperation::Range {
                        destination,
                        start,
                        end,
                        ..
                    } => temporaries.extend([*destination, *start, *end]),
                    MirOperation::Slice {
                        destination,
                        collection,
                        start,
                        end,
                    } => temporaries.extend([*destination, *collection, *start, *end]),
                    MirOperation::SequenceFromList {
                        destination,
                        values,
                    } => temporaries.extend([*destination, *values]),
                    MirOperation::SequenceMap {
                        destination,
                        sequence,
                        callback,
                    }
                    | MirOperation::SequenceFilter {
                        destination,
                        sequence,
                        callback,
                    }
                    | MirOperation::SequenceFind {
                        destination,
                        sequence,
                        callback,
                    }
                    | MirOperation::SequenceAny {
                        destination,
                        sequence,
                        callback,
                    }
                    | MirOperation::SequenceAll {
                        destination,
                        sequence,
                        callback,
                    } => temporaries.extend([*destination, *sequence, *callback]),
                    MirOperation::SequenceTake {
                        destination,
                        sequence,
                        count,
                    } => temporaries.extend([*destination, *sequence, *count]),
                    MirOperation::SequenceFold {
                        destination,
                        sequence,
                        initial,
                        callback,
                    } => {
                        temporaries.extend([*destination, *sequence, *initial, *callback]);
                    }
                    MirOperation::SequenceToList {
                        destination,
                        sequence,
                    } => temporaries.extend([*destination, *sequence]),
                    MirOperation::MapEntryAt {
                        destination,
                        map,
                        index,
                    } => temporaries.extend([*destination, *map, *index]),
                    MirOperation::ListAppend {
                        destination,
                        values,
                        value,
                    } => temporaries.extend([*destination, *values, *value]),
                    MirOperation::ListSet {
                        destination,
                        values,
                        index,
                        value,
                    } => temporaries.extend([*destination, *values, *index, *value]),
                    MirOperation::FieldGet {
                        destination,
                        record,
                    } => temporaries.extend([*destination, *record]),
                    MirOperation::StringOperation {
                        destination,
                        arguments,
                        ..
                    }
                    | MirOperation::TemplateRender {
                        destination,
                        arguments,
                        ..
                    }
                    | MirOperation::StandardOperation {
                        destination,
                        arguments,
                        ..
                    }
                    | MirOperation::CapabilityInspect {
                        destination,
                        arguments,
                        ..
                    }
                    | MirOperation::SafeCollectionOperation {
                        destination,
                        arguments,
                        ..
                    }
                    | MirOperation::CheckedIntOperation {
                        destination,
                        arguments,
                        ..
                    }
                    | MirOperation::CollectionOperation {
                        destination,
                        arguments,
                        ..
                    }
                    | MirOperation::DirectCall {
                        destination,
                        arguments,
                        ..
                    }
                    | MirOperation::AsyncCall {
                        destination,
                        arguments,
                        ..
                    }
                    | MirOperation::EffectCall {
                        destination,
                        arguments,
                        ..
                    } => {
                        temporaries.push(*destination);
                        temporaries.extend(arguments);
                    }
                    MirOperation::Spawn {
                        destination,
                        future,
                        ..
                    }
                    | MirOperation::TaskSnapshot {
                        destination,
                        source: future,
                    }
                    | MirOperation::Await {
                        destination,
                        source: future,
                    }
                    | MirOperation::Move {
                        destination,
                        source: future,
                    }
                    | MirOperation::NewtypeWrap {
                        destination,
                        source: future,
                    }
                    | MirOperation::NewtypeUnwrap {
                        destination,
                        source: future,
                    }
                    | MirOperation::ToolCall {
                        destination,
                        input: future,
                        ..
                    } => temporaries.extend([*destination, *future]),
                    MirOperation::ClosureEnvironment {
                        destination,
                        captures,
                        ..
                    } => {
                        temporaries.push(*destination);
                        temporaries.extend(captures);
                    }
                    MirOperation::ClosureCall {
                        destination,
                        closure,
                        arguments,
                    } => {
                        temporaries.extend([*destination, *closure]);
                        temporaries.extend(arguments);
                    }
                    MirOperation::ListFold {
                        destination,
                        values,
                        initial,
                        callback,
                    } => {
                        temporaries.extend([*destination, *values, *initial, *callback]);
                    }
                    MirOperation::ListCombinator {
                        destination,
                        values,
                        initial,
                        callback,
                        callback_result,
                        ..
                    } => {
                        temporaries.extend([*destination, *values, *callback, *callback_result]);
                        temporaries.extend(initial);
                    }
                    MirOperation::TaskScopeEnter { .. }
                    | MirOperation::TaskScopeExit { .. }
                    | MirOperation::TaskScopeCleanup { .. } => {}
                }
            }
            match &block.terminator {
                MirTerminator::Return { source } => temporaries.push(*source),
                MirTerminator::Suspend {
                    destination,
                    source,
                    ..
                } => temporaries.extend([*destination, *source]),
                MirTerminator::Stop { reason } | MirTerminator::Fail { reason } => {
                    temporaries.push(*reason);
                }
                _ => {}
            }
            if temporaries
                .iter()
                .any(|temporary| *temporary as usize >= self.temporaries.len())
            {
                return Err("MIR temporary is out of range");
            }
            let outgoing = targets(&block.terminator);
            if outgoing
                .iter()
                .any(|target| *target as usize >= block_count)
            {
                return Err("MIR control-flow target is out of range");
            }
            if matches!(
                block.terminator,
                MirTerminator::Suspend { .. } | MirTerminator::TaskScopeExit { .. }
            ) && outgoing.iter().copied().collect::<BTreeSet<_>>().len() != 5
            {
                return Err("MIR suspend or scope cleanup edges must be distinct");
            }
            match &block.terminator {
                MirTerminator::Suspend {
                    destination,
                    source,
                    ..
                } if !block.operations.iter().any(|operation| {
                    matches!(
                        operation,
                        MirOperation::Await {
                            destination: operation_destination,
                            source: operation_source,
                        } if operation_destination == destination && operation_source == source
                    )
                }) =>
                {
                    return Err("MIR suspend block does not contain its await operation");
                }
                MirTerminator::TaskScopeExit {
                    scope,
                    permanent_stop,
                    ..
                } if !block.operations.iter().any(|operation| {
                    matches!(
                        operation,
                        MirOperation::TaskScopeExit { scope: operation_scope }
                            if operation_scope == scope
                    )
                }) && !matches!(
                    self.blocks[*permanent_stop as usize].terminator,
                    MirTerminator::Stop { .. } | MirTerminator::Fail { .. }
                ) =>
                {
                    return Err("MIR scope-exit block does not contain its exit operation");
                }
                _ => {}
            }
        }
        let mut reachable = BTreeSet::from([0_u32]);
        let mut work = vec![0_u32];
        while let Some(block) = work.pop() {
            for target in targets(&self.blocks[block as usize].terminator) {
                if reachable.insert(target) {
                    work.push(target);
                }
            }
        }
        if reachable.len() != block_count {
            return Err("MIR contains an unreachable block");
        }
        for scope in &self.task_scopes {
            for (target, kind) in [
                (scope.normal_join, MirCleanupKind::NormalJoin),
                (scope.exceptional_cancel, MirCleanupKind::ExceptionalCancel),
                (scope.timeout_cancel, MirCleanupKind::TimeoutCancel),
                (scope.external_cancel, MirCleanupKind::ExternalCancel),
                (scope.permanent_stop, MirCleanupKind::PermanentStop),
            ] {
                let has_cleanup = self.blocks[target as usize]
                    .operations
                    .iter()
                    .any(|operation| {
                        matches!(
                            operation,
                            MirOperation::TaskScopeCleanup {
                                scope: operation_scope,
                                kind: operation_kind,
                            } if *operation_scope == scope.scope && *operation_kind == kind
                        )
                    });
                if !has_cleanup {
                    return Err("MIR task scope edge does not name its cleanup operation");
                }
            }
        }
        for suspension in &self.suspensions {
            let matches_block = self.blocks.iter().any(|block| {
                matches!(
                    block.terminator,
                    MirTerminator::Suspend {
                        destination,
                        source,
                        resume,
                        exceptional_cancel,
                        timeout_cancel,
                        external_cancel,
                        permanent_stop,
                    } if destination == suspension.destination
                        && source == suspension.source
                        && resume == suspension.resume
                        && exceptional_cancel == suspension.exceptional_cancel
                        && timeout_cancel == suspension.timeout_cancel
                        && external_cancel == suspension.external_cancel
                        && permanent_stop == suspension.permanent_stop
                )
            });
            if !matches_block {
                return Err("MIR suspension metadata has no matching block");
            }
        }
        let mut final_ownership = BTreeMap::new();
        for ownership in &self.ownership {
            if ownership.temporary as usize >= self.temporaries.len() {
                return Err("MIR ownership temporary is out of range");
            }
            final_ownership.insert(ownership.temporary, ownership);
        }
        if final_ownership
            .values()
            .any(|ownership| ownership.must_consume && ownership.state == MirOwnershipState::Live)
        {
            return Err("MIR leaves a live affine obligation");
        }
        Ok(())
    }
}
