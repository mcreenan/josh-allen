//! Diagnostic effect origins: where an effect request started.
//!
//! The VM keeps a per-frame stack of active loop executions derived from the
//! artifact's debug construct table and reports it, with the task and call
//! frames, when an effect request starts. Tracking never changes executable
//! meaning: it adds no instructions, charges no budget, and is enabled only
//! when an observer asks for origins and the artifact has loop constructs.

use allen_bytecode::{DebugConstruct, FunctionId};

/// One active execution of a loop construct in one call frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoopActivation {
    /// The loop's construct ID in the artifact debug table.
    pub construct: u32,
    /// Distinguishes separate executions of the same loop within one
    /// execution. It starts at 1 and increases each time the loop is entered.
    pub instance: u64,
    /// The 1-based iteration, advanced when the loop takes a repeat edge.
    pub iteration: u64,
}

/// One call frame at the moment an effect started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OriginFrame {
    pub function: FunctionId,
    /// The executing instruction: the call for an outer frame, or the
    /// instruction that starts the effect for the innermost frame.
    pub instruction: u32,
    /// Active loops in this frame, outermost first.
    pub loops: Vec<LoopActivation>,
}

/// The spawn that created one task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnOrigin {
    /// The spawned task.
    pub task_id: u64,
    /// The task that executed the spawn.
    pub parent_task_id: u64,
    /// The parent's frames at the spawn, outermost first. The innermost
    /// frame's instruction is the `Spawn`.
    pub frames: Vec<OriginFrame>,
}

/// Where one effect request started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectOrigin {
    /// The task that owns the effect. A spawned effect future is its own task.
    pub task_id: u64,
    /// The task that spawned `task_id`, if it is not the root task.
    pub parent_task_id: Option<u64>,
    /// Spawns leading to the frames below, outermost first.
    pub spawns: Vec<SpawnOrigin>,
    /// Frames that executed the effect site, outermost first. For a spawned
    /// effect future these are the spawning task's frames and the site is
    /// the `Spawn` instruction; otherwise the site is the starting `Await`.
    pub frames: Vec<OriginFrame>,
}

/// One loop's instruction range in a function.
#[derive(Clone, Copy, Debug)]
struct LoopRange {
    construct: u32,
    first: u32,
    end: u32,
}

/// A loop activation in a frame, with its function-local loop index.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ActiveLoop {
    local: usize,
    activation: LoopActivation,
}

impl ActiveLoop {
    pub(crate) const fn activation(&self) -> LoopActivation {
        self.activation
    }
}

#[derive(Debug, Default)]
struct FunctionLoops {
    /// Loops in construct pre-order, so loops sharing a header are outermost first.
    loops: Vec<LoopRange>,
    /// `(instruction, local loop)` sorted by instruction.
    repeat_edges: Vec<(u32, usize)>,
}

/// Per-function loop ranges and repeat edges, built once per execution.
#[derive(Debug, Default)]
pub(crate) struct LoopIndex {
    functions: Vec<Option<FunctionLoops>>,
    instances: Vec<u64>,
}

impl LoopIndex {
    /// Build the index, or `None` when the table has no loops.
    pub(crate) fn new(constructs: &[DebugConstruct]) -> Option<Self> {
        let mut index = Self {
            functions: Vec::new(),
            instances: vec![0; constructs.len()],
        };
        for (id, construct) in constructs.iter().enumerate() {
            if !construct.kind.is_loop() {
                continue;
            }
            let function = construct.function as usize;
            if index.functions.len() <= function {
                index.functions.resize_with(function + 1, || None);
            }
            let loops = index.functions[function].get_or_insert_with(FunctionLoops::default);
            let local = loops.loops.len();
            loops.loops.push(LoopRange {
                construct: u32::try_from(id).ok()?,
                first: construct.first_instruction,
                end: construct.end_instruction,
            });
            loops
                .repeat_edges
                .extend(construct.repeat_edges.iter().map(|edge| (*edge, local)));
        }
        if index.functions.iter().all(Option::is_none) {
            return None;
        }
        for loops in index.functions.iter_mut().flatten() {
            loops.repeat_edges.sort_unstable();
        }
        Some(index)
    }

    /// Update a frame's active loops before it executes `instruction`.
    ///
    /// Loops whose range no longer contains the instruction are exited; loops
    /// whose header is the instruction and are not active are entered with a
    /// fresh instance; a repeat edge advances its loop's iteration.
    pub(crate) fn enter_instruction(
        &mut self,
        function: FunctionId,
        instruction: u32,
        active: &mut Vec<ActiveLoop>,
    ) {
        let Some(Some(loops)) = self.functions.get(function as usize) else {
            return;
        };
        while let Some(top) = active.last() {
            let range = loops.loops[top.local];
            if range.first <= instruction && instruction < range.end {
                break;
            }
            active.pop();
        }
        let first_entry = loops
            .loops
            .partition_point(|range| range.first < instruction);
        for (local, range) in loops.loops.iter().enumerate().skip(first_entry) {
            if range.first != instruction {
                break;
            }
            if active.iter().any(|entry| entry.local == local) {
                continue;
            }
            let counter = &mut self.instances[range.construct as usize];
            *counter = counter.saturating_add(1);
            active.push(ActiveLoop {
                local,
                activation: LoopActivation {
                    construct: range.construct,
                    instance: *counter,
                    iteration: 1,
                },
            });
        }
        if let Ok(position) = loops
            .repeat_edges
            .binary_search_by_key(&instruction, |(edge, _)| *edge)
        {
            let local = loops.repeat_edges[position].1;
            if let Some(entry) = active.iter_mut().rev().find(|entry| entry.local == local) {
                entry.activation.iteration = entry.activation.iteration.saturating_add(1);
            }
        }
    }
}
