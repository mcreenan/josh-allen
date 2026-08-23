use std::collections::BTreeMap;
use std::time::Instant;

use josh_protocol::{EventKind, ExecutionEventParams};
use serde_json::Value;

pub(crate) trait EventClock: Send + Sync {
    fn elapsed_ms(&self) -> u64;
}

pub(crate) struct SystemEventClock {
    started: Instant,
}

impl SystemEventClock {
    pub(crate) const fn new(started: Instant) -> Self {
        Self { started }
    }
}

impl EventClock for SystemEventClock {
    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

pub(crate) fn execution_event(
    execution_id: &str,
    sequence: u64,
    elapsed_ms: u64,
    kind: EventKind,
    replayed: bool,
    fields: BTreeMap<String, Value>,
) -> ExecutionEventParams {
    ExecutionEventParams {
        execution_id: execution_id.to_owned(),
        sequence,
        elapsed_ms,
        kind,
        replayed,
        fields,
    }
}
