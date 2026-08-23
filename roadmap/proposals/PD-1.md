# PD-1: durable workflows

Status: Proposed

Depends on: [PD-2](PD-2.md), [PD-4](PD-4.md)

Back to the [roadmap](../../ROADMAP.md#proposed).

## Decision summary

Add a durable workflow profile to ALLEN. This profile stores typed workflow
state at explicit checkpoints. It also supports durable events, timers, and
child-work references.

A durable workflow can continue after a process stops. It does not replay a
completed mutation when it resumes.

## Problem

ALLEN `0.1` owns tasks and opaque handles for one execution. The runtime closes
these values when the execution ends. This rule is correct for a bounded task.

Long work has a different failure model. A workflow can wait for an approval
for several days. A host can stop after a tool changes external state. A new
host must then continue the work without loss or duplicate actions.

Restarting the entry function is not sufficient. The entry function cannot
know which prior effects completed. Replay also does not execute a live effect.
Replay only reproduces a recorded boundary result.

## Scope

This proposal adds:

- A stable workflow identity.
- Versioned workflow state.
- Explicit atomic checkpoints.
- Durable external events.
- Durable timers.
- Durable references to supported child work.
- Resume, cancel, complete, and fail operations.
- A canonical workflow history.
- State migration between approved artifact versions.

This proposal does not make all runtime values durable. `Future`, `Task`,
`Workspace`, and other execution-local handles remain invalid at a checkpoint.

## Terms

A workflow is one durable program instance.

A run is one process execution of that workflow.

A checkpoint is an atomic record of state and history position.

An event supplies typed external input to one waiting workflow.

A workflow history is the ordered record of checkpoints, events, timers, and
effect receipts.

## Proposed contract

The manifest declares each durable workflow. It also declares its input,
output, state type, effects, and limits.

The compiler checks that workflow state has a canonical encoding. The compiler
rejects an execution-local handle in workflow state.

The runtime gives each workflow an opaque `WorkflowId`. Source can compare a
workflow ID only when the contract permits comparison. Source cannot construct
or modify an ID.

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
record ReleaseState {
  approval: Option<ApprovalReceipt<ReleasePlan>>
  published: Option<EffectReceipt<PublishResult>>
}

durable workflow release(input: ReleasePlan)
  state ReleaseState
  returns ReleaseResult
  effects [workflow.checkpoint, workflow.event, tool.release.publish@1] {
  let approval = await event.wait<ApprovalReceipt<ReleasePlan>>(
    "release-approval"
  );

  checkpoint ReleaseState {
    approval: Some(approval),
    published: None,
  };
}
```

## Checkpoint rules

The runtime commits these items as one operation:

- The new workflow state.
- The workflow sequence number.
- The artifact digest.
- The language profile.
- Each new event reference.
- Each new effect receipt.
- The previous history digest.
- The new history digest.

The runtime must commit all items or no items. A partial checkpoint is invalid.

The runtime resumes after the last valid checkpoint. It must not resume from an
uncommitted state image.

A checkpoint does not copy live tasks into storage. The workflow must join,
cancel, or convert supported work to a durable reference before the checkpoint.

## Events and timers

An event has an exact schema. The host validates the event before delivery.
The event also identifies its issuer and target workflow.

The runtime accepts one event ID once. A duplicate event with the same content
returns the prior acceptance result. A duplicate ID with different content is
a protocol failure.

A timer records a logical due time and a clock policy. The host creates a timer
event when the due condition occurs. Replay uses the recorded timer event. It
does not read the current clock.

## Durable child work

A durable child reference is not a live `SubAgent` handle. It identifies child
work through a provider contract.

The reference records:

- The child principal.
- The delegation receipt.
- The request digest.
- The current child state.
- The result schema.
- The expiration policy.

The provider must support lookup after a host restart. If it cannot do this,
the program cannot checkpoint the child reference.

## Resume and migration

The default resume rule requires the same artifact digest. This rule prevents
new code from reading old state with different semantics.

An approved migration can move state to a new artifact. The migration is a
pure function. It accepts the old state and returns the new state.

The migration declaration names both state schemas and both artifact profiles.
The runtime records the migration result in workflow history.

The runtime must not run an effect during migration.

## Effect safety

PD-2 defines the mutation rules. PD-1 uses those rules at each resume point.

The workflow must store each committed mutation receipt in the next atomic
checkpoint. If the runtime cannot determine the mutation result, it enters an
ambiguous state. The workflow then waits for reconciliation.

The runtime must not guess that an effect failed because a response is absent.

## Recording and replay

The replay contract includes the workflow ID, checkpoint sequence, prior
history digest, artifact digest, and effective capabilities.

Replay verifies every event and receipt in order. Replay rejects a missing,
duplicate, changed, or reordered item.

Replay does not claim that a timer fired again. It does not claim that a tool
ran again. It reproduces the validated recorded values.

## Security rules

- Source cannot construct a workflow ID or history position.
- A host cannot resume a workflow with more authority than the checkpoint
  permits.
- A resume cannot restore an expired capability.
- An event must name its target workflow and expected schema.
- A durable child cannot receive more authority after resume.
- Workflow storage must preserve the labels from PD-3.
- Workflow history must use the receipts from PD-4.

## Failure cases

The runtime must reject these conditions before user code resumes:

- The state schema does not match.
- The artifact digest does not match.
- The history chain has a gap or fork.
- A checkpoint contains an execution-local handle.
- A required receipt does not verify.
- An event has an incorrect workflow ID.
- A migration performs an effect.
- A resume requests more authority than the stored contract.

## Implementation work

1. Define the workflow manifest and state-schema rules.
2. Define canonical workflow history entries.
3. Add checkpoint instructions to bytecode and verification.
4. Add a host-neutral durable-store provider.
5. Add event and timer provider contracts.
6. Add resume and cancellation protocol messages.
7. Add pure state migrations.
8. Add crash tests at each checkpoint boundary.

## Acceptance tests

- Stop a host after a tool commits but before the next source instruction.
- Resume the workflow without a duplicate tool mutation.
- Reject a changed state schema without a migration.
- Apply an approved pure migration and continue the workflow.
- Reject a duplicate event ID that has different content.
- Replay a timer event without a live clock read.
- Reject a checkpoint that contains a live task or workspace.
- Cancel durable child work with bounded cleanup.

## Open decisions

1. Must the runtime own workflow storage, or can a provider own it?
2. Which durable child providers are portable?
3. Can one event wake more than one workflow?
4. How does a workflow query an ambiguous external mutation?
5. Which state changes require a new language profile?
