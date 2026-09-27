//! Join VM effect origins with an artifact's debug tables.
//!
//! [`ProgramDebug`] is built once per loaded program. It renders the static
//! construct and effect-site tables for `program/load` and turns each VM
//! [`EffectOrigin`] into the self-describing wire `origin` attached to provider
//! requests. Origins are diagnostic: they never enter request digests, replay
//! journals, or execution decisions.

use std::collections::BTreeMap;
use std::sync::Mutex;

use allen_bytecode::{
    DebugConstruct, DebugConstructKind, DebugInfo, DebugRegionKind, EffectOperation, FunctionId,
    Instruction, Module,
};
use allen_vm::{EffectOrigin, OriginFrame, PendingEffectId};
use josh_protocol::{
    EffectOriginPayload, EffectScopePayload, EffectSiteKind, EffectSitePayload, IfBranch,
    MAX_ORIGIN_SCOPE, ProgramConstructPayload, ProgramDebugPayload, ProgramEffectSitePayload,
    ProgramRegionPayload, ScopeKind, ScopeRegion, SourceSpan,
};

/// Origins retained for typed-response retries of recent effects.
const ORIGIN_STORE_CAPACITY: usize = 1_024;

/// Source text indexed by line for 1-based line and column positions.
#[derive(Debug)]
struct LineIndex {
    text: String,
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
        );
        Self {
            text: text.to_owned(),
            line_starts,
        }
    }

    /// The 1-based line and column (in Unicode scalar values) of a byte offset.
    fn position(&self, offset: u32) -> Option<(u32, u32)> {
        let mut offset = usize::try_from(offset).ok()?.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.line_starts.partition_point(|start| *start <= offset);
        let line_start = self.line_starts[line - 1];
        let column = self.text[line_start..offset].chars().count() + 1;
        Some((u32::try_from(line).ok()?, u32::try_from(column).ok()?))
    }
}

#[derive(Debug)]
struct SourceView {
    display: String,
    lines: Option<LineIndex>,
}

#[derive(Debug)]
struct StaticSite {
    kind: EffectSiteKind,
    operation: String,
    tool: Option<String>,
    function: FunctionId,
    instruction: u32,
    start_instruction: Option<u32>,
}

/// One program's debug tables prepared for origin rendering.
#[derive(Debug)]
pub struct ProgramDebug {
    function_names: Vec<String>,
    debug: DebugInfo,
    sources: Vec<SourceView>,
    /// Construct index range per function.
    function_constructs: Vec<(usize, usize)>,
    sites: Vec<StaticSite>,
    site_by_start: BTreeMap<(FunctionId, u32), u32>,
}

impl ProgramDebug {
    /// Prepare debug tables for one verified module.
    ///
    /// `tool_names` is the manifest's required-tool order, which
    /// `ToolInvoke` indexes. `texts` maps host source paths to their text;
    /// a debug source matches a host path when it equals it or ends with
    /// `/` followed by it, and is then reported under the host path.
    #[must_use]
    pub fn new(
        module: &Module,
        debug: &DebugInfo,
        tool_names: &[String],
        texts: &BTreeMap<String, String>,
    ) -> Self {
        let sources = debug
            .sources
            .iter()
            .map(|source| {
                let matched = texts.iter().find(|(path, _)| {
                    source == *path
                        || source
                            .strip_suffix(path.as_str())
                            .is_some_and(|prefix| prefix.ends_with('/'))
                });
                SourceView {
                    display: matched.map_or_else(|| source.clone(), |(path, _)| path.clone()),
                    lines: matched.map(|(_, text)| LineIndex::new(text)),
                }
            })
            .collect::<Vec<_>>();
        let function_names = module
            .functions
            .iter()
            .map(|function| readable_function_name(&function.name))
            .collect();
        let mut function_constructs = vec![(0, 0); module.functions.len()];
        for (index, construct) in debug.constructs.iter().enumerate() {
            if let Some(range) = function_constructs.get_mut(construct.function as usize) {
                if range.0 == range.1 {
                    *range = (index, index + 1);
                } else {
                    range.1 = index + 1;
                }
            }
        }
        let mut sites = Vec::new();
        for (function_id, function) in module.functions.iter().enumerate() {
            let function_id = FunctionId::try_from(function_id).unwrap_or(FunctionId::MAX);
            for (index, instruction) in function.code.iter().enumerate() {
                let (destination, kind, operation, tool) = match instruction {
                    Instruction::EffectCall {
                        destination,
                        operation,
                        ..
                    } => {
                        let (kind, name) = effect_operation(*operation);
                        (*destination, kind, name.to_owned(), None)
                    }
                    Instruction::ToolInvoke {
                        destination, tool, ..
                    } => {
                        let name = tool_names
                            .get(*tool as usize)
                            .cloned()
                            .unwrap_or_else(|| format!("#{tool}"));
                        (
                            *destination,
                            EffectSiteKind::Tool,
                            format!("tools.{name}.call"),
                            Some(name),
                        )
                    }
                    _ => continue,
                };
                let start_instruction = function.code[index + 1..]
                    .iter()
                    .position(|candidate| {
                        matches!(
                            candidate,
                            Instruction::Await { source, .. } | Instruction::Spawn { future: source, .. }
                                if *source == destination
                        )
                    })
                    .and_then(|offset| u32::try_from(index + 1 + offset).ok());
                sites.push(StaticSite {
                    kind,
                    operation,
                    tool,
                    function: function_id,
                    instruction: u32::try_from(index).unwrap_or(u32::MAX),
                    start_instruction,
                });
            }
        }
        let site_by_start = sites
            .iter()
            .enumerate()
            .filter_map(|(id, site)| {
                Some((
                    (site.function, site.start_instruction?),
                    u32::try_from(id).ok()?,
                ))
            })
            .collect();
        Self {
            function_names,
            debug: debug.clone(),
            sources,
            function_constructs,
            sites,
            site_by_start,
        }
    }

    fn span(&self, source: u32, start: u32, end: u32) -> SourceSpan {
        let view = self.sources.get(source as usize);
        let positions = view
            .and_then(|view| view.lines.as_ref())
            .and_then(|lines| Some((lines.position(start)?, lines.position(end)?)));
        SourceSpan {
            source: view.map_or_else(String::new, |view| view.display.clone()),
            start,
            end,
            line: positions.map(|((line, _), _)| line),
            column: positions.map(|((_, column), _)| column),
            end_line: positions.map(|(_, (line, _))| line),
            end_column: positions.map(|(_, (_, column))| column),
        }
    }

    /// Render a span in one debug source path, with host paths and positions.
    #[must_use]
    pub fn source_span(&self, module_path: &str, start: u32, end: u32) -> Option<SourceSpan> {
        let source = self
            .debug
            .sources
            .iter()
            .position(|source| source == module_path)?;
        Some(self.span(u32::try_from(source).ok()?, start, end))
    }

    /// The source span of one instruction.
    fn location(&self, function: FunctionId, instruction: u32) -> Option<SourceSpan> {
        let index = self
            .debug
            .locations
            .binary_search_by_key(&(function, instruction), |location| {
                (location.function, location.instruction)
            })
            .ok()?;
        let location = &self.debug.locations[index];
        Some(self.span(location.source, location.start, location.end))
    }

    fn function_name(&self, function: FunctionId) -> String {
        self.function_names
            .get(function as usize)
            .cloned()
            .unwrap_or_else(|| format!("#{function}"))
    }

    /// Constructs of `function` that enclose the instruction at `at`,
    /// outermost first, with their IDs.
    ///
    /// A construct encloses an instruction inside its instruction range,
    /// except that an `if` or `match` whose condition or scrutinee contains
    /// the instruction's span does not: that effect decides the branch rather
    /// than running inside one. Loops always enclose their condition.
    fn enclosing<'a>(
        &'a self,
        function: FunctionId,
        instruction: u32,
        at: &'a SourceSpan,
    ) -> impl Iterator<Item = (u32, &'a DebugConstruct)> {
        let (start, end) = self
            .function_constructs
            .get(function as usize)
            .copied()
            .unwrap_or_default();
        self.debug.constructs[start..end]
            .iter()
            .enumerate()
            .filter(move |(_, construct)| {
                construct.contains_instruction(instruction)
                    && (construct.kind.is_loop()
                        || !construct
                            .region_for_span(at.start, at.end)
                            .is_some_and(|region| {
                                matches!(
                                    region.kind,
                                    DebugRegionKind::Condition | DebugRegionKind::Scrutinee
                                )
                            }))
            })
            .map(move |(offset, construct)| {
                (u32::try_from(start + offset).unwrap_or(u32::MAX), construct)
            })
    }

    /// Render the static tables for `program/load`, stopping before the
    /// serialized tables exceed `budget` bytes.
    #[must_use]
    pub fn tables(&self, budget: usize) -> ProgramDebugPayload {
        let mut used = 0_usize;
        let mut truncated = false;
        let mut fits = |bytes: usize| {
            if truncated || used.saturating_add(bytes) > budget {
                truncated = true;
                false
            } else {
                used += bytes;
                true
            }
        };
        let mut constructs = Vec::new();
        for (id, construct) in self.debug.constructs.iter().enumerate() {
            let mut payload = ProgramConstructPayload {
                id: u32::try_from(id).unwrap_or(u32::MAX),
                kind: scope_kind(construct.kind),
                function: self.function_name(construct.function),
                parent: construct.parent,
                source: String::new(),
                start: 0,
                end: 0,
                line: None,
                column: None,
                end_line: None,
                end_column: None,
                regions: construct
                    .regions
                    .iter()
                    .map(|region| {
                        let mut payload = ProgramRegionPayload {
                            region: scope_region(region.kind),
                            arm: (region.kind == DebugRegionKind::Arm).then_some(region.index),
                            source: String::new(),
                            start: 0,
                            end: 0,
                            line: None,
                            column: None,
                            end_line: None,
                            end_column: None,
                        };
                        payload.set_span(self.span(construct.source, region.start, region.end));
                        payload
                    })
                    .collect(),
            };
            payload.set_span(self.span(construct.source, construct.start, construct.end));
            if !fits(serialized_len(&payload)) {
                break;
            }
            constructs.push(payload);
        }
        let mut effect_sites = Vec::new();
        for (id, site) in self.sites.iter().enumerate() {
            let Some(span) = self.location(site.function, site.instruction) else {
                continue;
            };
            let mut payload = ProgramEffectSitePayload {
                id: u32::try_from(id).unwrap_or(u32::MAX),
                kind: site.kind,
                operation: site.operation.clone(),
                tool: site.tool.clone(),
                function: self.function_name(site.function),
                instruction: site.instruction,
                start_instruction: site.start_instruction,
                source: String::new(),
                start: 0,
                end: 0,
                line: None,
                column: None,
                end_line: None,
                end_column: None,
                constructs: self
                    .enclosing(site.function, site.instruction, &span)
                    .map(|(id, _)| id)
                    .collect(),
            };
            payload.set_span(span);
            if !fits(serialized_len(&payload)) {
                break;
            }
            effect_sites.push(payload);
        }
        ProgramDebugPayload {
            constructs,
            effect_sites,
            truncated,
        }
    }

    /// Render one VM origin, or `None` when the site has no debug location.
    #[must_use]
    pub fn origin(&self, origin: &EffectOrigin) -> Option<EffectOriginPayload> {
        let site_frame = origin.frames.last()?;
        let mut site = EffectSitePayload {
            id: self
                .site_by_start
                .get(&(site_frame.function, site_frame.instruction))
                .copied(),
            function: self.function_name(site_frame.function),
            instruction: site_frame.instruction,
            source: String::new(),
            start: 0,
            end: 0,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
        };
        site.set_span(self.location(site_frame.function, site_frame.instruction)?);
        let mut scope = Vec::new();
        for (index, spawn) in origin.spawns.iter().enumerate() {
            self.push_frames(&spawn.frames, &mut scope);
            let spawned = origin
                .spawns
                .get(index + 1)
                .and_then(|next| next.frames.first())
                .or_else(|| origin.frames.first());
            if let (Some(frame), Some(spawned)) = (spawn.frames.last(), spawned) {
                if let Some(span) = self.location(frame.function, frame.instruction) {
                    scope.push(boundary(
                        ScopeKind::Spawn,
                        self.function_name(spawned.function),
                        frame.instruction,
                        Some(spawn.task_id),
                        span,
                    ));
                }
            }
        }
        self.push_frames(&origin.frames, &mut scope);
        let truncated = scope.len() > MAX_ORIGIN_SCOPE;
        if truncated {
            scope.drain(..scope.len() - MAX_ORIGIN_SCOPE);
        }
        let payload = EffectOriginPayload {
            task: origin.task_id,
            parent_task: origin.parent_task_id,
            site,
            scope,
            truncated,
        };
        payload.validate().ok().map(|()| payload)
    }

    fn push_frames(&self, frames: &[OriginFrame], scope: &mut Vec<EffectScopePayload>) {
        for (index, frame) in frames.iter().enumerate() {
            let Some(at) = self.location(frame.function, frame.instruction) else {
                continue;
            };
            for (id, construct) in self.enclosing(frame.function, frame.instruction, &at) {
                if let Some(entry) = self.construct_entry(id, construct, frame, &at) {
                    scope.push(entry);
                }
            }
            if let Some(callee) = frames.get(index + 1) {
                scope.push(boundary(
                    ScopeKind::Call,
                    self.function_name(callee.function),
                    frame.instruction,
                    None,
                    at,
                ));
            }
        }
    }

    fn construct_entry(
        &self,
        id: u32,
        construct: &DebugConstruct,
        frame: &OriginFrame,
        at: &SourceSpan,
    ) -> Option<EffectScopePayload> {
        let kind = scope_kind(construct.kind);
        let (instance, iteration) = if construct.kind.is_loop() {
            // A loop without an activation is not rendered rather than guessed.
            let active = frame.loops.iter().find(|active| active.construct == id)?;
            (Some(active.instance), Some(active.iteration))
        } else {
            (None, None)
        };
        let region = construct.region_for_span(at.start, at.end);
        let mut entry = EffectScopePayload {
            kind,
            construct: Some(id),
            function: None,
            instruction: None,
            task: None,
            source: String::new(),
            start: 0,
            end: 0,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
            region: region.map(|region| scope_region(region.kind)),
            branch: region.and_then(|region| match region.kind {
                DebugRegionKind::Then => Some(IfBranch::Then),
                DebugRegionKind::Else => Some(IfBranch::Else),
                _ => None,
            }),
            arm: region
                .filter(|region| region.kind == DebugRegionKind::Arm)
                .map(|region| region.index),
            instance,
            iteration,
        };
        entry.set_span(self.span(construct.source, construct.start, construct.end));
        Some(entry)
    }
}

fn boundary(
    kind: ScopeKind,
    function: String,
    instruction: u32,
    task: Option<u64>,
    span: SourceSpan,
) -> EffectScopePayload {
    let mut entry = EffectScopePayload {
        kind,
        construct: None,
        function: Some(function),
        instruction: Some(instruction),
        task,
        source: String::new(),
        start: 0,
        end: 0,
        line: None,
        column: None,
        end_line: None,
        end_column: None,
        region: None,
        branch: None,
        arm: None,
        instance: None,
        iteration: None,
    };
    entry.set_span(span);
    entry
}

fn serialized_len<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len().saturating_add(1))
}

const fn scope_kind(kind: DebugConstructKind) -> ScopeKind {
    match kind {
        DebugConstructKind::For => ScopeKind::For,
        DebugConstructKind::While => ScopeKind::While,
        DebugConstructKind::Loop => ScopeKind::Loop,
        DebugConstructKind::If => ScopeKind::If,
        DebugConstructKind::Match => ScopeKind::Match,
        DebugConstructKind::AwaitBlock => ScopeKind::AwaitBlock,
    }
}

const fn scope_region(kind: DebugRegionKind) -> ScopeRegion {
    match kind {
        DebugRegionKind::Condition => ScopeRegion::Condition,
        DebugRegionKind::Body => ScopeRegion::Body,
        DebugRegionKind::Then => ScopeRegion::Then,
        DebugRegionKind::Else => ScopeRegion::Else,
        DebugRegionKind::Scrutinee => ScopeRegion::Scrutinee,
        DebugRegionKind::Arm => ScopeRegion::Arm,
    }
}

const fn effect_operation(operation: EffectOperation) -> (EffectSiteKind, &'static str) {
    match operation {
        EffectOperation::ReadText => (EffectSiteKind::Fs, "fs.read_text"),
        EffectOperation::ReadBytes => (EffectSiteKind::Fs, "fs.read_bytes"),
        EffectOperation::WriteText => (EffectSiteKind::Fs, "fs.write_text"),
        EffectOperation::WriteBytes => (EffectSiteKind::Fs, "fs.write_bytes"),
        EffectOperation::List => (EffectSiteKind::Fs, "fs.list"),
        EffectOperation::Search => (EffectSiteKind::Fs, "fs.search"),
        EffectOperation::HttpGet => (EffectSiteKind::Http, "http.get"),
        EffectOperation::ExecRun => (EffectSiteKind::Exec, "exec.run"),
        EffectOperation::PermissionRequestFile => {
            (EffectSiteKind::Permission, "permission.request_file")
        }
        EffectOperation::PermissionRequestDirectory => {
            (EffectSiteKind::Permission, "permission.request_directory")
        }
        EffectOperation::AgentMessage => (EffectSiteKind::Agent, "agent.message"),
        EffectOperation::AgentAsk => (EffectSiteKind::Agent, "agent.ask"),
        EffectOperation::AgentTranscript => (EffectSiteKind::Agent, "agent.transcript"),
        EffectOperation::ModelRequest => (EffectSiteKind::Model, "model.request"),
        EffectOperation::UserAsk => (EffectSiteKind::User, "user.ask"),
        EffectOperation::SubAgentCreate => (EffectSiteKind::SubAgent, "sub_agent.create"),
        EffectOperation::SubAgentRun => (EffectSiteKind::SubAgent, "sub_agent.run"),
        EffectOperation::SubAgentMessage => (EffectSiteKind::SubAgent, "sub_agent.message"),
        EffectOperation::SubAgentAsk => (EffectSiteKind::SubAgent, "sub_agent.ask"),
    }
}

/// Decode a bytecode function symbol into `source-path::name`.
///
/// Package symbols hex-escape each module path component, as in
/// `pkg/x696e6c696e65/x302e312e30/x737263/x6d61696e.allen::main`, which
/// becomes `src/main.allen::main`. Other symbols are returned unchanged.
#[must_use]
pub fn readable_function_name(symbol: &str) -> String {
    let decode = |component: &str| -> Option<String> {
        let hex = component.strip_prefix('x')?;
        if hex.len() % 2 != 0 {
            return None;
        }
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(hex.get(index..index + 2)?, 16).ok())
            .collect::<Option<Vec<_>>>()?;
        String::from_utf8(bytes).ok()
    };
    let readable = || -> Option<String> {
        let (module, name) = symbol.split_once("::")?;
        let components = module.strip_prefix("pkg/")?.split('/').collect::<Vec<_>>();
        let (file, directories) = components.get(2..)?.split_last()?;
        let file = file.strip_suffix(".allen")?;
        let mut path = Vec::new();
        for component in directories {
            path.push(decode(component)?);
        }
        path.push(format!("{}.allen", decode(file)?));
        Some(format!("{}::{name}", path.join("/")))
    };
    readable().unwrap_or_else(|| symbol.to_owned())
}

/// Rendered origins shared by the observer and the wire providers of one
/// execution, keyed by pending effect.
///
/// An origin stays available for typed-response retries of the same effect;
/// the oldest origins are dropped beyond a fixed capacity.
#[derive(Debug, Default)]
pub(crate) struct OriginStore {
    origins: Mutex<BTreeMap<u64, EffectOriginPayload>>,
}

impl OriginStore {
    pub(crate) fn insert(&self, pending: PendingEffectId, origin: EffectOriginPayload) {
        if let Ok(mut origins) = self.origins.lock() {
            origins.insert(pending.0, origin);
            while origins.len() > ORIGIN_STORE_CAPACITY {
                origins.pop_first();
            }
        }
    }

    pub(crate) fn get(&self, pending: PendingEffectId) -> Option<EffectOriginPayload> {
        self.origins.lock().ok()?.get(&pending.0).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use allen_vm::{
        CancellationSource, Checkpoint, CheckpointObserver, EffectProvider, ExecutionLimits,
        MonotonicClock, Value, VmError, WorkspaceValue,
    };
    use std::time::Duration;

    struct Clock;
    impl MonotonicClock for Clock {
        fn now(&mut self) -> Duration {
            Duration::ZERO
        }
    }
    struct Never;
    impl CancellationSource for Never {
        fn is_cancelled(&mut self) -> bool {
            false
        }
    }
    struct Unavailable;
    impl EffectProvider for Unavailable {
        fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
            Err(VmError::CapabilityMissing)
        }
        fn call(&mut self, _: EffectOperation, _: &[Value]) -> Result<Value, VmError> {
            Err(VmError::CapabilityMissing)
        }
    }
    #[derive(Default)]
    struct Recorder(Vec<EffectOrigin>);
    impl CheckpointObserver for Recorder {
        fn checkpoint(&mut self, _: Checkpoint) {}
        fn observes_effect_origins(&self) -> bool {
            true
        }
        fn effect_started(&mut self, _: PendingEffectId, origin: &EffectOrigin) {
            self.0.push(origin.clone());
        }
    }

    fn scope_text(origin: &EffectOriginPayload) -> Vec<String> {
        origin
            .scope
            .iter()
            .map(|entry| {
                let value = serde_json::to_value(entry).unwrap();
                let mut text = value["kind"].as_str().unwrap().to_owned();
                for key in ["function", "task", "region", "iteration"] {
                    if !value[key].is_null() {
                        text.push_str(&format!(" {key}={}", value[key]));
                    }
                }
                text.push_str(&format!(
                    " @{}:{}",
                    entry.line.unwrap(),
                    entry.column.unwrap()
                ));
                text
            })
            .collect()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn origins_render_spawn_boundaries_regions_and_positions() {
        let source = r#"async fn probe(value: Int) returns Int effects [model.request] {
  match await model.request<Int>(prompt { system: "probe" output: Int }) {
    Ok(result) => result
    Err(_) => value
  }
}
export async fn main() returns Int effects [model.request, task.spawn] {
  mut total = 0;
  for round in 0..2 {
    if (round == 1) {
      total += await {
        let first = spawn probe(round);
        let second = spawn model.request<Int>(prompt { system: "direct" output: Int });
        let a = await first;
        let b = match await second {
          Ok(value) => value
          Err(_) => 0
        };
        a + b
      };
    }
  }
  total
}
"#;
        let compilation = allen_compiler::compile_source(source).expect("fixture compiles");
        let debug = ProgramDebug::new(
            &compilation.module,
            &compilation.debug,
            &[],
            &BTreeMap::from([("main.allen".to_owned(), source.to_owned())]),
        );
        let entry = compilation.module.entry;
        let verified = allen_bytecode::verify(compilation.module.clone()).expect("verifies");
        let mut recorder = Recorder::default();
        allen_vm::execute_entry_with_runtime_context(
            &verified,
            Some(&compilation.debug),
            entry,
            &[],
            ExecutionLimits::default(),
            &mut Clock,
            &mut recorder,
            &mut Never,
            &mut Unavailable,
        )
        .expect("fixture runs");
        let origins = recorder
            .0
            .iter()
            .map(|origin| debug.origin(origin).expect("origin renders"))
            .collect::<Vec<_>>();
        assert_eq!(origins.len(), 2);
        assert_eq!((origins[0].task, origins[0].parent_task), (1, Some(0)));
        assert_eq!(
            scope_text(&origins[0]),
            [
                "for region=\"body\" iteration=2 @9:3",
                "if region=\"then\" @10:5",
                "await_block region=\"body\" @11:16",
                "spawn function=\"main.allen::probe\" task=1 @12:21",
            ]
        );
        assert_eq!(origins[0].site.function, "main.allen::probe");
        assert_eq!(origins[0].site.line, Some(2));
        assert_eq!((origins[1].task, origins[1].parent_task), (2, Some(0)));
        assert_eq!(
            scope_text(&origins[1]),
            [
                "for region=\"body\" iteration=2 @9:3",
                "if region=\"then\" @10:5",
                "await_block region=\"body\" @11:16",
            ]
        );
        assert_eq!(
            (origins[1].site.line, origins[1].site.column),
            (Some(13), Some(22))
        );
        let if_scope = &origins[0].scope[1];
        assert_eq!(if_scope.branch, Some(IfBranch::Then));

        let tables = debug.tables(usize::MAX);
        assert!(!tables.truncated);
        for origin in &origins {
            let site = &tables.effect_sites[origin.site.id.expect("static site") as usize];
            assert_eq!(site.function, origin.site.function);
            assert_eq!(site.operation, "model.request");
            assert_eq!(site.start_instruction, Some(origin.site.instruction));
        }
        assert_eq!(
            tables
                .constructs
                .iter()
                .map(|construct| (construct.kind, construct.parent))
                .collect::<Vec<_>>(),
            [
                (ScopeKind::For, None),
                (ScopeKind::If, Some(0)),
                (ScopeKind::AwaitBlock, Some(1)),
                (ScopeKind::Match, Some(2)),
                (ScopeKind::Match, None),
            ]
        );
        let spawned = &tables.effect_sites[origins[1].site.id.unwrap() as usize];
        assert_eq!(spawned.constructs, [0, 1, 2]);
        let small = debug.tables(300);
        assert!(small.truncated);
        assert!(small.constructs.len() < tables.constructs.len());
        tables.validate().unwrap();
        small.validate().unwrap();
    }

    #[test]
    fn deep_call_chains_keep_the_innermost_scope_entries() {
        let source = r#"async fn deep(n: Int) returns Int effects [model.request] {
  if (n == 0) {
    match await model.request<Int>(prompt { system: "deep" output: Int }) {
      Ok(value) => value
      Err(_) => 0
    }
  } else {
    await deep(n - 1)
  }
}
export async fn main() returns Int effects [model.request] { await deep(70) }
"#;
        let compilation = allen_compiler::compile_source(source).expect("fixture compiles");
        let debug = ProgramDebug::new(
            &compilation.module,
            &compilation.debug,
            &[],
            &BTreeMap::new(),
        );
        let entry = compilation.module.entry;
        let verified = allen_bytecode::verify(compilation.module.clone()).expect("verifies");
        let mut recorder = Recorder::default();
        allen_vm::execute_entry_with_runtime_context(
            &verified,
            Some(&compilation.debug),
            entry,
            &[],
            ExecutionLimits::default(),
            &mut Clock,
            &mut recorder,
            &mut Never,
            &mut Unavailable,
        )
        .expect("fixture runs");
        let origin = debug.origin(&recorder.0[0]).expect("origin renders");
        assert!(origin.truncated);
        assert_eq!(origin.scope.len(), MAX_ORIGIN_SCOPE);
        assert_eq!(origin.scope.last().unwrap().kind, ScopeKind::If);
        assert_eq!(origin.scope.last().unwrap().branch, Some(IfBranch::Then));
        assert_eq!(origin.site.source, "main.allen");
        assert_eq!(origin.site.line, None, "no source text, no positions");
    }

    #[test]
    fn package_symbols_decode_to_source_paths() {
        assert_eq!(
            readable_function_name("pkg/x696e6c696e65/x302e312e30/x737263/x6d61696e.allen::main"),
            "src/main.allen::main"
        );
        assert_eq!(
            readable_function_name("pkg/x74/x31/x737263/x6c6962/x7574696c.allen::$closure@7"),
            "src/lib/util.allen::$closure@7"
        );
        assert_eq!(
            readable_function_name("main.allen::main"),
            "main.allen::main"
        );
        assert_eq!(
            readable_function_name("pkg/x74/x31/xzz.allen::f"),
            "pkg/x74/x31/xzz.allen::f"
        );
    }
}
