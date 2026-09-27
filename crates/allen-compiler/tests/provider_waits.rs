//! The VM waits for provider progress instead of polling in a tight loop.

use allen_bytecode::{EffectOperation, verify};
use allen_compiler::compile_source;
use allen_vm::{
    CancellationSource, Checkpoint, CheckpointObserver, EffectPoll, EffectProvider,
    ExecutionLimits, MonotonicClock, PendingEffectId, Value, VmError, WorkspaceValue,
    execute_entry_with_runtime_context,
};
use std::rc::Rc;
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

struct Quiet;

impl CheckpointObserver for Quiet {
    fn checkpoint(&mut self, _checkpoint: Checkpoint) {}
}

/// A model provider that answers only after a number of polls and records
/// every wait the VM asks for in between.
struct SlowModel {
    polls_until_ready: usize,
    polls: usize,
    waits: Vec<Duration>,
}

impl EffectProvider for SlowModel {
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

    fn start_agent(
        &mut self,
        _pending: PendingEffectId,
        _operation: EffectOperation,
        _arguments: &[Value],
        _result_type: &allen_bytecode::ValueType,
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        Ok(EffectPoll::Pending)
    }

    fn poll_effect(
        &mut self,
        _pending: PendingEffectId,
        _cancellation: &mut dyn CancellationSource,
    ) -> Result<EffectPoll, VmError> {
        self.polls += 1;
        if self.polls < self.polls_until_ready {
            return Ok(EffectPoll::Pending);
        }
        Ok(EffectPoll::Ready(Value::Enum(Rc::new(
            allen_vm::EnumValue {
                identity: allen_vm::EnumIdentity::Result,
                type_name: Rc::from("Result"),
                variant: 0,
                variant_name: Rc::from("Ok"),
                payload: allen_vm::EnumPayload::Tuple(vec![Value::Int(7)].into()),
            },
        ))))
    }

    fn wait_for_effects(&mut self, timeout: Duration) {
        self.waits.push(timeout);
    }
}

#[test]
fn idle_scheduler_waits_with_growing_bounded_timeouts_between_polls() {
    let source = r#"export async fn main() returns Int effects [model.request] {
  match await model.request<Int>(prompt { system: "slow" output: Int }) {
    Ok(value) => value
    Err(_) => 0
  }
}
"#;
    let compilation = compile_source(source).expect("fixture compiles");
    let entry = compilation.module.entry;
    let verified = verify(compilation.module).expect("fixture verifies");
    let mut model = SlowModel {
        polls_until_ready: 14,
        polls: 0,
        waits: Vec::new(),
    };
    let outcome = execute_entry_with_runtime_context(
        &verified,
        None,
        entry,
        &[],
        ExecutionLimits::default(),
        &mut FixedClock,
        &mut Quiet,
        &mut NeverCancelled,
        &mut model,
    )
    .expect("fixture runs");
    assert!(matches!(
        outcome,
        allen_vm::ExecutionOutcome::Completed(result) if result.value == Value::Int(7)
    ));
    // One wait between consecutive unready polls, doubling from 50
    // microseconds to a 10 millisecond ceiling.
    let micros = model
        .waits
        .iter()
        .map(|wait| u64::try_from(wait.as_micros()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        micros,
        [
            50, 100, 200, 400, 800, 1_600, 3_200, 6_400, 10_000, 10_000, 10_000, 10_000, 10_000
        ]
    );
}
