# ALLEN roadmap

Status: Planning document

ALLEN `0.1` supports bounded agent programs. It has exact types, closed
effects, typed prompts, typed tools, capability checks, structured concurrency,
and replay.

The roadmap has two tracks. Selected language features change the current
ALLEN language. Protocol proposals address work across process boundaries,
external mutations, data controls, actor identity, host differences, and
bounded host integrations.

A language feature ships only when its implementation, specifications, tests,
conformance data, editor support, examples, and agent references land in one
change. Protocol proposals need an accepted design before implementation.

## Implemented MVPs

| ID | Decision | Shipped scope |
|---|---|---|
| PD-10 | [Opt-in Executor CLI tool provider](roadmap/proposals/PD-10.md) | `josh run` dispatches explicitly granted typed tools through a user-installed Executor CLI without an agent or model turn. |

## Implemented language features

These 21 features are implemented in ALLEN 0.1.1. The stable IDs also name
their focused proposals and the entries in
[`TODO.md`](TODO.md#selected-language-features). The shared design rules remain
in the [language feature proposal](docs/plans/allen-language-features-proposal.md).

| ID | Proposal | Batch | Depends on |
|---|---|---|---|
| `LIT-RAW-STRING` | [Raw string literals](roadmap/proposals/language/LIT-RAW-STRING.md) | L1 | None |
| `LIT-MULTILINE` | [Indentation-trimmed multiline strings](roadmap/proposals/language/LIT-MULTILINE.md) | L1 | None |
| `FUN-LABELED-ARGS` | [Labeled arguments](roadmap/proposals/language/FUN-LABELED-ARGS.md) | L1 | None |
| `FUN-LAMBDA-SHORT` | [Concise inferred lambda](roadmap/proposals/language/FUN-LAMBDA-SHORT.md) | L1 | None |
| `COL-COMBINATORS` | [Eager list transform and predicate combinators](roadmap/proposals/language/COL-COMBINATORS.md) | L1 | None |
| `FLOW-OPTION-QUESTION` | [Postfix question operator for Option](roadmap/proposals/language/FLOW-OPTION-QUESTION.md) | L1 | None |
| `FUN-DEFAULT-ARGS` | [Default arguments](roadmap/proposals/language/FUN-DEFAULT-ARGS.md) | L2 | `FUN-LABELED-ARGS` |
| `FUN-TRAILING-CALLBACK` | [Trailing callback block](roadmap/proposals/language/FUN-TRAILING-CALLBACK.md) | L2 | `FUN-LAMBDA-SHORT` |
| `FUN-PARTIAL` | [Placeholder partial application](roadmap/proposals/language/FUN-PARTIAL.md) | L2 | `FUN-LAMBDA-SHORT`, `FUN-LABELED-ARGS` |
| `FUN-COMPOSE` | [Function composition](roadmap/proposals/language/FUN-COMPOSE.md) | L2 | `FUN-LAMBDA-SHORT` |
| `FUN-PIPE` | [Forward pipe](roadmap/proposals/language/FUN-PIPE.md) | L2 | `FUN-PARTIAL`, `COL-COMBINATORS` |
| `FUN-EXTENSION-CALL` | [Uniform extension-call sugar](roadmap/proposals/language/FUN-EXTENSION-CALL.md) | L2 | `COL-COMBINATORS` |
| `COL-RECORD-UPDATE` | [Immutable record update](roadmap/proposals/language/COL-RECORD-UPDATE.md) | L2 | None |
| `COL-LITERAL-SPREAD` | [List and map spread](roadmap/proposals/language/COL-LITERAL-SPREAD.md) | L2 | None |
| `FLOW-OPTION-CHAIN` | [Optional member and call chaining](roadmap/proposals/language/FLOW-OPTION-CHAIN.md) | L2 | `FLOW-OPTION-QUESTION` |
| `COL-RANGE-VALUES` | [First-class ranges](roadmap/proposals/language/COL-RANGE-VALUES.md) | L3 | None |
| `COL-SLICES` | [Bracket slicing](roadmap/proposals/language/COL-SLICES.md) | L3 | `COL-RANGE-VALUES` |
| `PAT-RANGE` | [Range patterns](roadmap/proposals/language/PAT-RANGE.md) | L3 | `COL-RANGE-VALUES` |
| `PAT-OR` | [OR patterns](roadmap/proposals/language/PAT-OR.md) | L3 | None |
| `FUN-LOCAL` | [Local named functions](roadmap/proposals/language/FUN-LOCAL.md) | L3 | None |
| `COL-LAZY-SEQUENCE` | [Lazy sequences or iterators](roadmap/proposals/language/COL-LAZY-SEQUENCE.md) | L3 | `COL-COMBINATORS` |

### Language feature order

Batch L1 fixes lexical, call-contract, callback-inference, collection-library,
and `Option` propagation foundations. Batch L2 adds sugar that expands through
those contracts. Batch L3 adds new value and pattern semantics, local compiler
identities, and affine lazy sequences.

Finish one feature as a complete current-language change before marking its
checkbox complete. A parser-only implementation does not complete a feature.

## Accepted

| ID | Proposal | Summary | Delivery state |
|---|---|---|---|
| PD-11 | [Authenticated host projection and native provider routing](roadmap/proposals/PD-11.md) | Project the host's complete tools, context facilities, providers, authority surfaces, limits, and telemetry into JOSH through one frozen contract; then replace prompt-assisted forwarding with native adapters. | Phase 1 implemented |

## Proposed

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

## Current deployment boundary

Use cron or Orca to start independent bounded `josh run` executions on a
schedule. This starts new runs. It does not preserve workflow state, resume a
stopped run, or provide restart-safe durability.

A scheduler must not automatically retry a headless-tool mutation, or a future
exec mutation, after a timeout, lost response, or other ambiguous result.
Authenticated receipts from [PD-4](roadmap/proposals/PD-4.md) and the
idempotency and reconciliation semantics from
[PD-2](roadmap/proposals/PD-2.md) must be accepted before such retries can be
safe. Until then, a new run requires external reconciliation or an explicit
operator decision.

[SHOUT and durable workflows](roadmap/proposals/PD-1.md) remain a separate
future milestone after PD-4 and PD-2. The current implementation has no durable
checkpoint, restart-safe resume, or exactly-once effect guarantee.

PD-10 shipped independently against the current language and protocol. Its MVP
keeps catalog supply, tool grants, and Executor selection explicit. Automatic
catalog import, mutation semantics, receipts, negotiation, stronger
cancellation, and streams remain future work tied to the related proposals.
