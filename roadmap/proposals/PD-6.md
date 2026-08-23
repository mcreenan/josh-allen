# PD-6: source-controlled cancellation and supervision

Status: Proposed

Depends on: [PD-2](PD-2.md)

Back to the [roadmap](../../ROADMAP.md#proposed).

## Decision summary

Add local deadlines, cancellation tokens, typed races, bounded retries, quorum
collection, and task supervisors.

Keep every task inside a structured owner scope. Do not add detached tasks.

## Problem

ALLEN `0.1` cancels tasks after a terminal error, execution timeout, external
cancellation, or `stop`. Source cannot cancel one slow child and continue with
another result.

Source also cannot set a local deadline or select the first valid provider
result. A host-wide timeout is too large for this purpose. It ends the complete
execution.

Agent programs often call providers in parallel. They need explicit and bounded
control for slow, failed, and redundant work.

## Scope

This proposal adds:

- Deadlines for one scope or operation.
- Owned cancellation tokens.
- Typed `select` and `race` operations.
- First-success and quorum collection.
- Bounded retry policies.
- Task supervisors with restart rules.
- Source-visible local cancellation results.
- Recording and replay for each scheduling decision.

The proposal keeps affine task ownership and structured cleanup.

## Terms

A deadline is a time limit for one owned operation or scope.

A cancellation token lets an owner request cancellation of child work.

A race selects one result and cancels the remaining work.

A quorum accepts a declared number of compatible results.

A supervisor owns child tasks and applies a bounded failure policy.

## Concrete example: PagerDuty incident triage

This example starts when PagerDuty opens a production incident. The workflow
collects evidence from Datadog and Amazon CloudWatch.

The tool and event names are proposed ALLEN adapters.

| Proposal concept | Named service, tool, or event | Concrete use |
|---|---|---|
| Start event | `pagerduty.incident.triggered@1` | Start one triage workflow. |
| Local deadline | `tools.datadog.logs.query@1` | Stop one log query after eight seconds. |
| First-success | Datadog and `tools.aws_cloudwatch_logs.query@1` | Use the first valid log sample. |
| Cancellation | `tools.aws_cloudwatch_logs.stop_query@1` | Stop the losing CloudWatch query. |
| Retry | `tools.aws_s3.get_object@1` | Retry a read-only runbook download. |
| Supervisor | GitHub Issues and Slack | Keep the incident issue and notification tasks in one scope. |
| Quorum | Snyk, VirusTotal, and ClamAV | Require two matching malware decisions. |

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
await scope {
  let token = cancel.token();

  let datadog = spawn deadline.after_seconds(8, token.child(), fn() {
    tools.datadog.logs.query.call({
      service: event.service,
      start_time: event.started_at,
    })
  });

  let cloudwatch = spawn deadline.after_seconds(8, token.child(), fn() {
    tools.aws_cloudwatch_logs.query.call({
      log_group: event.log_group,
      start_time: event.started_at,
    })
  });

  let logs = await first_success([datadog, cloudwatch], {
    retryable: ["datadog.timeout", "cloudwatch.throttled"],
  })?;
  token.cancel_remaining();

  let runbook = await retry({
    attempts: 3,
    call: tools.aws_s3.get_object.call({
      bucket: "production-runbooks",
      key: event.service + ".md",
    }),
    retryable: ["s3.slow_down", "s3.unavailable"],
  })?;

  await supervisor.all([
    tools.github.issues.create.call(make_issue(event, logs, runbook)),
    tools.slack.chat.post_message.call(make_alert(event, logs)),
  ])?;
}
```

The Datadog and CloudWatch operations are read-only in this example. The
runtime can cancel the losing query without an ambiguous mutation.

The Amazon S3 retry keeps one request digest. Each attempt stays inside the
total incident deadline.

The GitHub Issues and Slack calls change external state. Their adapters must
meet PD-2 before the supervisor can retry them.

For a suspected malicious file, the workflow starts Snyk, VirusTotal, and
ClamAV scans. A pure function compares their typed decisions. The workflow
accepts a two-of-three quorum and retains all three receipts.

## Cancellation model

The first version uses owner-driven cancellation. A task cannot cancel an
unrelated task.

A scope creates a token. The token can move to child work, but it cannot leave
the owner scope or cross an entry boundary.

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
await scope {
  let token = cancel.token();
  let left = spawn tools.datadog.logs.query.call(
    datadog_query,
    token.child(),
  );
  let right = spawn tools.aws_cloudwatch_logs.query.call(
    cloudwatch_query,
    token.child(),
  );

  let result = await race.first_ok([left, right]);
  token.cancel_remaining();
  result
}
```

The scope waits for bounded cleanup before it returns.

## Local cancellation result

Local cancellation is not a terminal execution cancellation. The affected
operation returns a typed cancellation state when its contract permits this.

The type must distinguish:

- The operation returned a value.
- The operation returned a documented error.
- The owner cancelled the operation.
- The local deadline expired.
- Cleanup failed and caused a terminal runtime error.

The runtime must not convert an ambiguous external mutation to a local
cancellation result. PD-2 defines that case.

## Deadlines

A deadline can use a relative duration or an inherited logical due time. The
host clock remains outside deterministic evaluation.

The runtime records the deadline event. Replay uses that event instead of the
live clock.

A child deadline cannot exceed its parent deadline. A host can lower any
requested deadline.

## Select and race

`select` returns the first completed case under deterministic scheduler rules.
It does not cancel other cases unless source requests cancellation.

`race` selects a result and cancels all losing cases. The runtime consumes each
losing affine handle during cleanup.

When more than one case is ready at the same scheduler point, the lowest stable
case ID wins. Recording stores the selected case and ready set.

The compiler checks that every case has a compatible result type.

## First-success

First-success ignores documented recoverable errors until one task returns a
valid value. It returns the ordered errors when all tasks fail.

Source declares which error codes first-success can ignore. It cannot ignore a
terminal error, protocol failure, or ambiguous mutation.

The result records every attempted provider and the selected result receipt.

## Quorum

A quorum operation states:

- The number of requested tasks.
- The minimum accepted result count.
- The exact compatibility function.
- The deadline.
- The cancellation rule for remaining tasks.

The compatibility function must be pure and deterministic. A model cannot
declare its own answer compatible without a source rule or review operation.

Quorum collection keeps all contributing receipts.

## Retry

A retry policy includes:

- A maximum attempt count.
- Exact retryable error codes.
- A total deadline.
- A deterministic backoff schedule.
- A provider-change rule.
- An idempotency requirement from PD-2.

The runtime checks the policy before the first attempt. It charges every
attempt to execution limits.

A retry cannot change a state-changing request, key, or provider unless the
effect contract permits that change.

## Supervision

A supervisor owns a fixed or bounded set of child tasks. Its policy states how
one child failure affects the other children.

The first version should support these policies:

- Stop the group after one child fails.
- Keep other children and report all results.
- Restart one child for listed recoverable errors.
- Require a quorum before group completion.

Each restart creates a new task ID and attempt record. The supervisor cannot
increase child authority or limits.

## Provider cancellation

The runtime sends a cancellation request when the provider supports it. The
runtime then waits for bounded cancellation confirmation or cleanup timeout.

A late provider result cannot resume source work. The runtime records and
discards the result.

Provider cancellation does not prove that an external mutation did not commit.
PD-2 controls the final state for mutations.

## Recording and replay

Recording stores:

- Token creation and cancellation.
- Deadline events.
- Ready case sets.
- Selected race and select cases.
- Retry attempts and delays.
- Supervisor restarts.
- Quorum members.
- Provider cancellation results.

Replay releases task completions in the recorded order. It verifies the same
selection and cleanup result.

## Security rules

- A token controls only work in its owner scope.
- Source cannot cancel another workflow or host execution.
- A child cannot extend a deadline or limit.
- Retry cannot bypass effect authority or PD-2 rules.
- A race cannot leak losing protected results.
- A supervisor cannot widen child authority.
- Cancellation cleanup remains bounded.

## Failure cases

The compiler or runtime must reject these conditions:

- A token leaves its owner scope.
- A race loses a live affine handle.
- A retry policy includes a denied error code.
- A state-changing retry lacks an applicable PD-2 contract.
- A child deadline exceeds its parent deadline.
- A quorum function performs an effect.
- Cleanup exceeds its reserved budget.
- Replay selects a different ready case.

## Implementation work

1. Define token ownership and local cancellation result types.
2. Add deadline scopes and recorded timer events.
3. Add `select`, `race`, and first-success instructions.
4. Add quorum collection with pure compatibility functions.
5. Add retry policies linked to PD-2.
6. Add supervisor state to MIR and bytecode verification.
7. Extend provider cancellation and JOSH messages.
8. Add deterministic scheduling and cleanup tests.

## Acceptance tests

- Start triage from `pagerduty.incident.triggered@1`.
- Select the first valid Datadog or CloudWatch log result.
- Cancel the losing CloudWatch query through its named stop tool.
- Retry an Amazon S3 runbook read within one total deadline.
- Keep GitHub Issues and Slack mutations under one supervisor.
- Retain Snyk, VirusTotal, and ClamAV quorum receipts.
- Race two read-only providers and cancel the loser.
- Resolve two cases that become ready at one scheduler point.
- Reject a race that loses task ownership.
- Stop retry after the total deadline.
- Reject a mutation retry without idempotency support.
- Restart one child for one listed error code.
- Collect a quorum and retain all contributing receipts.
- Replay the same race and retry decisions.

## Open decisions

1. Is local cancellation a `Result` error or a separate tagged outcome?
2. Can `select` accept streams from PD-7?
3. Which backoff schedules belong in the portable profile?
4. Can a supervisor change providers between attempts?
5. How much ready-set detail must replay record?
