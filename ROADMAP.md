# ALLEN roadmap

Status: Planning document

ALLEN `0.1` supports bounded agent programs. It has exact types, closed
effects, typed prompts, typed tools, capability checks, structured concurrency,
and replay.

The proposals below address work that continues across process boundaries.
They also address external mutations, data controls, actor identity, and host
differences.

The proposals do not change ALLEN `0.1`. Each proposal needs a separate design
decision before implementation starts.

## PROPOSED

| ID | Proposal | Summary | Main dependencies |
|---|---|---|---|
| PD-1 | [Durable workflows](roadmap/proposals/PD-1.md) | Add checkpoints, durable events, timers, and restart-safe workflow state. | PD-2, PD-4 |
| PD-2 | [Idempotent and transactional effects](roadmap/proposals/PD-2.md) | Prevent duplicate external mutations after retries, crashes, or lost responses. | PD-4 |
| PD-3 | [Information-flow types](roadmap/proposals/PD-3.md) | Track confidentiality, integrity, and data origin through program values. | None |
| PD-4 | [Authenticated actors and verifiable receipts](roadmap/proposals/PD-4.md) | Identify actors and bind signed evidence to exact actions and approvals. | None |
| PD-5 | [Typed capability and tool negotiation](roadmap/proposals/PD-5.md) | Select compatible tools before execution without an untyped call path. | PD-3, PD-4 |
| PD-6 | [Source-controlled cancellation and supervision](roadmap/proposals/PD-6.md) | Add local deadlines, races, cancellation, retry control, and task supervision. | PD-2 |
| PD-7 | [Typed streams and multimodal artifacts](roadmap/proposals/PD-7.md) | Add bounded streams and typed references for images, audio, video, and documents. | PD-3, PD-6 |
| PD-8 | [Capability-scoped persistent memory](roadmap/proposals/PD-8.md) | Add typed persistent memory with explicit ownership, access, retention, and origin. | PD-1, PD-2, PD-3, PD-4 |
| PD-9 | [Provider-independent model policy](roadmap/proposals/PD-9.md) | Let source set model cost, privacy, time, content, and review requirements. | PD-3, PD-4, PD-6, PD-7 |

## Proposed order

Start with PD-4 and PD-2. These proposals define proof and safe mutation.

Implement PD-1 after those contracts are stable. A durable workflow must not
repeat an external mutation after a restart.

PD-3 can proceed in parallel as a separate type-system design. Apply its rules
to PD-5, PD-7, PD-8, and PD-9 before those proposals become language changes.

Implement PD-5 and PD-6 after the first group. Then implement PD-7. Implement
PD-8 and PD-9 last because they depend on most of the earlier contracts.
