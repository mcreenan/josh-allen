//! VM effect origins: loop activations, call frames, and spawn chains that the
//! observer receives when an effect request starts.
#![allow(clippy::needless_raw_string_hashes)]

use allen_bytecode::{DebugInfo, EffectOperation, verify};
use allen_compiler::compile_source;
use allen_vm::{
    CancellationSource, Checkpoint, CheckpointObserver, EffectOrigin, EffectProvider,
    ExecutionLimits, MonotonicClock, OriginFrame, PendingEffectId, Value, VmError, WorkspaceValue,
    execute_entry_with_runtime_context,
};
use std::time::Duration;

struct FixedClock;

impl MonotonicClock for FixedClock {
    fn now(&mut self) -> Duration {
        Duration::ZERO
    }
}

struct NeverCancelled;

impl CancellationSource for NeverCancelled {
    fn is_cancelled(&mut self) -> bool {
        false
    }
}

/// Every provider is unavailable, so each effect returns its typed `Err`.
struct Unavailable;

impl EffectProvider for Unavailable {
    fn workspace(&mut self) -> Result<WorkspaceValue, VmError> {
        Err(VmError::CapabilityMissing)
    }

    fn call(
        &mut self,
        _operation: EffectOperation,
        _arguments: &[Value],
    ) -> Result<Value, VmError> {
        Err(VmError::CapabilityMissing)
    }
}

struct Recorder {
    wants: bool,
    origins: Vec<EffectOrigin>,
}

impl CheckpointObserver for Recorder {
    fn checkpoint(&mut self, _checkpoint: Checkpoint) {}

    fn observes_effect_origins(&self) -> bool {
        self.wants
    }

    fn effect_started(&mut self, _pending: PendingEffectId, origin: &EffectOrigin) {
        self.origins.push(origin.clone());
    }
}

const PROBE: &str = r#"async fn probe(value: Int) returns Int effects [model.request] {
  match await model.request<Int>(prompt { system: "probe" output: Int }) {
    Ok(result) => result
    Err(_) => value
  }
}
"#;

fn run(source: &str, debug: bool, wants: bool) -> (Vec<EffectOrigin>, DebugInfo, Vec<String>) {
    let compilation = compile_source(source).expect("origin fixture compiles");
    let names = compilation
        .module
        .functions
        .iter()
        .map(|function| {
            function
                .name
                .rsplit("::")
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    let entry = compilation.module.entry;
    let verified = verify(compilation.module).expect("origin fixture verifies");
    let mut recorder = Recorder {
        wants,
        origins: Vec::new(),
    };
    execute_entry_with_runtime_context(
        &verified,
        debug.then_some(&compilation.debug as &dyn allen_vm::DebugSourceMap),
        entry,
        &[],
        ExecutionLimits::default(),
        &mut FixedClock,
        &mut recorder,
        &mut NeverCancelled,
        &mut Unavailable,
    )
    .expect("origin fixture runs");
    (recorder.origins, compilation.debug, names)
}

/// `function[kind#instance.iteration ...]` for each frame, outermost first.
fn frames(frames: &[OriginFrame], debug: &DebugInfo, names: &[String]) -> String {
    frames
        .iter()
        .map(|frame| {
            let loops = frame
                .loops
                .iter()
                .map(|active| {
                    format!(
                        "{}#{}.{}",
                        debug.constructs[active.construct as usize].kind.name(),
                        active.instance,
                        active.iteration
                    )
                })
                .collect::<Vec<_>>();
            format!("{}[{}]", names[frame.function as usize], loops.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" > ")
}

fn describe(origins: &[EffectOrigin], debug: &DebugInfo, names: &[String]) -> Vec<String> {
    origins
        .iter()
        .map(|origin| {
            let mut parts = vec![format!(
                "task {}{}",
                origin.task_id,
                origin
                    .parent_task_id
                    .map_or_else(String::new, |parent| format!(" of {parent}"))
            )];
            for spawn in &origin.spawns {
                parts.push(format!(
                    "spawn {} from {}",
                    spawn.task_id,
                    frames(&spawn.frames, debug, names)
                ));
            }
            parts.push(frames(&origin.frames, debug, names));
            parts.join(": ")
        })
        .collect()
}

#[test]
fn loop_iterations_count_continue_and_stop_at_break_in_every_loop_form() {
    let source = format!(
        "{PROBE}{}",
        r#"export async fn main() returns Int effects [model.request] {
  mut total = 0;
  for index in 0..5 {
    if (index == 1) { continue; }
    if (index == 3) { break; }
    total += await probe(index);
  }
  mut count = 0;
  while (count < 3) {
    count += 1;
    if (count == 2) { continue; }
    total += await probe(count);
  }
  loop {
    count += 1;
    if (count == 5) { continue; }
    if (count > 6) { break; }
    total += await probe(count);
  }
  total
}
"#
    );
    let (origins, debug, names) = run(&source, true, true);
    assert_eq!(
        describe(&origins, &debug, &names),
        [
            "task 0: main[for#1.1] > probe[]",
            "task 0: main[for#1.3] > probe[]",
            "task 0: main[while#1.1] > probe[]",
            "task 0: main[while#1.3] > probe[]",
            "task 0: main[loop#1.1] > probe[]",
            "task 0: main[loop#1.3] > probe[]",
        ]
    );
    let site = origins[0].frames.last().expect("site frame");
    let location = debug
        .locations
        .iter()
        .find(|location| {
            location.function == site.function && location.instruction == site.instruction
        })
        .expect("site location");
    assert_eq!(
        &source[location.start as usize..location.end as usize],
        r#"await model.request<Int>(prompt { system: "probe" output: Int })"#
    );
}

#[test]
fn nested_loops_sharing_a_header_get_a_fresh_instance_per_entry() {
    let source = format!(
        "{PROBE}{}",
        r#"export async fn main() returns Int effects [model.request] {
  mut total = 0;
  mut inner = 0;
  mut outer = 0;
  loop {
    while (inner < 2) {
      inner += 1;
      total += await probe(inner);
    }
    inner = 0;
    outer += 1;
    if (outer == 3) { break; }
  }
  total
}
"#
    );
    let (origins, debug, names) = run(&source, true, true);
    let main = debug
        .constructs
        .iter()
        .filter(|construct| names[construct.function as usize] == "main")
        .collect::<Vec<_>>();
    assert_eq!(
        main[0].first_instruction, main[1].first_instruction,
        "loop and while share a header"
    );
    assert_eq!(
        describe(&origins, &debug, &names),
        [
            "task 0: main[loop#1.1 while#1.1] > probe[]",
            "task 0: main[loop#1.1 while#1.2] > probe[]",
            "task 0: main[loop#1.2 while#2.1] > probe[]",
            "task 0: main[loop#1.2 while#2.2] > probe[]",
            "task 0: main[loop#1.3 while#3.1] > probe[]",
            "task 0: main[loop#1.3 while#3.2] > probe[]",
        ]
    );
}

#[test]
fn recursive_frames_keep_their_own_loop_activations() {
    let source = format!(
        "{PROBE}{}",
        r#"async fn countdown(n: Int) returns Int effects [model.request] {
  mut total = 0;
  for step in 0..2 {
    if (step == 1 && n > 0) {
      total += await countdown(n - 1);
    } else {
      total += await probe(n);
    }
  }
  total
}
export async fn main() returns Int effects [model.request] {
  await countdown(1)
}
"#
    );
    let (origins, debug, names) = run(&source, true, true);
    assert_eq!(
        describe(&origins, &debug, &names),
        [
            "task 0: main[] > countdown[for#1.1] > probe[]",
            "task 0: main[] > countdown[for#1.2] > countdown[for#2.1] > probe[]",
            "task 0: main[] > countdown[for#1.2] > countdown[for#2.2] > probe[]",
        ]
    );
}

#[test]
fn spawned_tasks_and_spawned_effects_carry_their_spawn_origin() {
    let source = format!(
        "{PROBE}{}",
        r#"export async fn main() returns Int effects [model.request, task.spawn] {
  mut total = 0;
  for round in 0..2 {
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
  total
}
"#
    );
    let (origins, debug, names) = run(&source, true, true);
    assert_eq!(
        describe(&origins, &debug, &names),
        [
            "task 1 of 0: spawn 1 from main[for#1.1]: probe[]",
            "task 2 of 0: main[for#1.1]",
            "task 3 of 0: spawn 3 from main[for#1.2]: probe[]",
            "task 4 of 0: main[for#1.2]",
        ]
    );
    // The spawned effect's site is the spawn itself.
    let site = origins[1].frames.last().expect("site frame");
    let location = debug
        .locations
        .iter()
        .find(|location| {
            location.function == site.function && location.instruction == site.instruction
        })
        .expect("site location");
    assert!(
        source[location.start as usize..location.end as usize].starts_with("spawn model.request")
    );
}

#[test]
fn origins_are_not_reported_without_debug_information_or_an_interested_observer() {
    let source = format!(
        "{PROBE}{}",
        r#"export async fn main() returns Int effects [model.request] {
  mut total = 0;
  for index in 0..2 {
    total += await probe(index);
  }
  total
}
"#
    );
    assert!(run(&source, false, true).0.is_empty());
    assert!(run(&source, true, false).0.is_empty());
    assert_eq!(run(&source, true, true).0.len(), 2);
}
