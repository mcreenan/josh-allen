# JOSH/ALLEN implementation specification

Status: Early alpha

This document describes the one implementation shipped by the current
repository. The language semantics are defined by
[`language-spec.md`](language-spec.md). Source, artifacts, replay logs, and JOSH
messages produced by earlier repository states are unsupported.

## 1. Components and dependency boundaries

ALLEN is the language, compiler, verifier, VM, and host-neutral runtime. JOSH is
the optional JSON-Oriented Session Host that connects an execution to an agent
harness. ALLEN can run without JOSH; JOSH depends on ALLEN.

The workspace uses these crates:

| Crate | Responsibility |
|---|---|
| `allen-syntax` | Lossless lexing, bounded recovery parsing, immutable concrete trees, typed syntax views, and syntax-only incremental reparse |
| `allen-compiler` | Lower typed syntax, resolve modules and types, check effects and ownership, produce HIR/MIR, and lower current source to bytecode |
| `allen-bytecode` | Current artifact model, canonical encoder and decoder, independent verifier, and artifact inspection |
| `allen-vm` | Values, canonical value encoding, scheduler, instruction execution, budgets, and provider boundary validation |
| `allen-runtime` | Launch preflight, host-neutral providers, capability routing, policy, JSON boundaries, and terminal outcomes |
| `allen-package` | Manifest, lockfile, local dependency graph, and source loading |
| `allen-schema` | Strict schema parsing, canonicalization, catalog freezing, and descriptor conversion |
| `allen-sandbox-fs` | Descriptor-relative filesystem operations |
| `allen-http-get` | Restricted HTTPS GET and destination policy |
| `allen-testkit` | Deterministic recording, replay, redaction, and provider test support |
| `allen-cli` | `check`, `build`, `inspect`, `run`, and package commands |
| `josh-protocol` | The current `josh/1.3` message, payload, framing, and connection-state contract |
| `josh-host` | JOSH session lifecycle, provider adapters, stdio connection, and execution supervision |
| `josh` | The `serve` and `run` executable entry points |

Crates expose small public facades and keep implementation details in focused
modules. A module owns one coherent concern such as parsing, verification,
value conversion, provider routing, protocol state, or command execution.
Tests live with the module they exercise or in crate integration tests when
they validate a public boundary.

All crates forbid unsafe Rust except a narrowly reviewed platform adapter when
descriptor-relative operating-system access requires it. Untrusted input must
remain bounded before allocation, recursion, work scheduling, or host dispatch.

## 2. Trust model

The implementation has four trust zones:

1. Source, packages, entry input, artifacts, schemas, and external responses
   are untrusted.
2. The compiler, verifier, VM, runtime, schema layer, filesystem broker, and
   HTTP broker are trusted language components.
3. The host is trusted to enforce its own identity, authorization, redaction,
   and tool-dispatch claims, but every host value is validated.
4. Agents, models, tools, DNS, HTTP servers, and package sources may be faulty
   or hostile.

The runtime is a capability broker, not an operating-system sandbox. The CLI
runs verified code in a separate worker process where supported and applies OS
resource limits in addition to language-level limits. Embedded use preserves
the language checks but has weaker crash and address-space isolation.

## 3. Compilation pipeline

All source modes use one pipeline:

```text
UTF-8 sources and manifest
  -> mode-aware lossless lexing
  -> immutable concrete syntax tree and typed syntax views
  -> module and import resolution
  -> type, effect, and ownership checking
  -> typed HIR
  -> control-flow MIR
  -> current bytecode module
  -> canonical artifact encoder
```

Loose source receives a synthesized capability-free manifest. Inline manifests
and package manifests enter the same semantic pipeline after parsing. No source
text, command, file layout, environment variable, or feature flag selects a
different language grammar.

Diagnostics carry a stable code, source identity, and half-open UTF-8 byte
range. The compiler bounds source bytes, token count, nesting, declarations,
types, monomorphizations, functions, blocks, registers, constants, and emitted
instructions before it commits an artifact.

Checked syntax lowering retains record, enum, and transparent type-alias
declarations in one module type namespace. Resolution validates duplicate
names, import visibility, unknown alias targets, and the complete alias
dependency graph before compiling functions. A deterministic ordered worklist
rejects direct and indirect alias cycles without recursive alias expansion.
Successful aliases erase to their underlying structural or nominal semantic
type before HIR and therefore add no artifact or runtime representation.

### 3.1 Syntax and incremental reparse

`allen-syntax` is compiler-independent and owns the one canonical lexer and
parser. Concrete tokens reproduce the original UTF-8 source exactly, including
trivia and malformed input. Parser recovery is bounded and produces ordered
syntax diagnostics without semantic state. Its frozen default resource contract
allows at most 1,048,576 lossless tokens (including EOF) and 2,097,152 concrete
nodes; `SyntaxLimits::DEFAULT` is the implementation source of truth.

`TextEdit` applies a validated UTF-8 byte replacement to an immutable source
snapshot. Incremental reparse is correctness-first: a clean horizontal
whitespace or quoted-literal token may be relexed and replaced only when its
complete edited text still produces exactly the same token kind with no
diagnostics. Newlines always use full parsing because adjacent CR and LF bytes
can coalesce. Identifiers, comments, templates, interpolation, operators,
delimiters, token boundaries, recovery trees, resource uncertainty, and every
other unproved context also use a fresh full parse. Both paths produce exactly
the same green tree, source text, and ordered diagnostics as full parsing.
`Parse` retains its immutable source snapshot and cached error state, and local
token selection uses positional tree lookup rather than a full-tree scan.
Deterministic work statistics report the edit range, token-or-source entry
point, source bytes copied, selected relex bytes, snapshot/error checks,
positional lookup work, rebuilt old and new nodes, and fallback status; they
contain no timing, address, or host state and are not an artifact, CLI, replay,
or JOSH contract.

### 3.2 HIR and MIR

HIR contains resolved symbol, type, effect-set, and span IDs. It preserves the
semantic structure needed for inspection without retaining parser ambiguity.

MIR contains typed temporaries, basic blocks, explicit terminators, task-scope
metadata, suspension points, cleanup edges, and affine ownership state. MIR
validation rejects use before initialization, inconsistent joins, invalid
back-edges, lost or duplicated task handles, escaped task scopes, and malformed
cleanup control flow before bytecode lowering.

### 3.3 Effects and callbacks

The compiler interns sorted closed effect sets. Named functions and closures
without an effect clause receive the empty maximum contract, exactly as if they
declared an empty effect clause. Explicit clauses are maximum contracts, and
callback types carry exact contracts, with omission denoting the empty set. Direct calls, closures, callback calls,
async producers, and generated tool calls all participate in effect checking.

## 4. Current artifact format

The repository defines one canonical bytecode-v13 `.allenb` format. The header contains the
fixed magic, the single current format identifier, language and compiler
versions, target profile, flags, section count, and SHA-256 digest. Integers are
little-endian. The digest covers the complete artifact with its digest field
zeroed.

Mandatory sections contain strings, constants, types, functions, effect sets,
entry contracts, strict schemas, imports, and the manifest contract. Debug
information is the only optional section. Sections have canonical order,
unique IDs, bounded sizes, and no trailing data.

Artifacts include the complete current value model and instruction set:

- scalars, tuples, lists, maps, records, enums, unknown values, callbacks,
  futures, tasks, workspaces, external-access modes, and sub-agent handles;
- computation, conversions, comparisons, String and collection operations;
- calls, branches, switches, loops, early return, and stopped outcomes;
- async call, spawn, explicit await, task scopes, task snapshots, and cleanup;
- filesystem, network, permission, invoking-agent, model, user, sub-agent, and
  generated typed-tool effects; and
- safe collection access and checked integer operations with closed results.

The encoder emits only this format. The decoder rejects any other identifier,
unknown tag, noncanonical table, invalid UTF-8 scalar, duplicate or unsorted
member, malformed reference, excess resource, or trailing byte.

## 5. Independent verification

Decoding does not establish safety. The verifier independently checks:

- canonical constants, types, enum layouts, function symbols, source paths,
  effect sets, entries, schemas, imports, manifests, and tool contracts;
- all register, block, function, constant, schema, enum, and type references;
- instruction operand and result types;
- exact declared effects for external operations and calls;
- structured control flow, initialized registers, and compatible joins;
- affine `Future`, `Task`, `Workspace`, and `SubAgent` restrictions;
- task ownership, scope cleanup, and transfer rules;
- boundary-safe entry and provider types;
- closed standard and generated-tool error envelopes; and
- configured complexity, nesting, state, and allocation limits.

Only a `VerifiedArtifact` can enter launch preflight. A verified value cannot be
constructed by public field mutation.

## 6. VM and structured concurrency

The VM is a deterministic register machine. It executes one instruction at a
time through a scheduler checkpoint, charges work before mutation, and uses
stable task IDs for scheduling ties.

An async call creates a lazy `Future<T>`. `spawn` transfers a future or task
producer into a child task and returns one owned `Task<T>` handle. `await`
consumes one future or task layer. Awaiting a task handle twice is impossible
because the compiler and independent verifier enforce affine moves across
locals, calls, branches, loops, and closure boundaries. Nested `await { ... }`
scopes assign each spawned task to the innermost current scope. Normal exit,
early return, `?`, `break`, and `continue` join live children without
cancelling them. Terminal failure, timeout, external cancellation, and `stop`
cancel then join unfinished work before the terminal outcome is reported.

The VM validates every provider completion against the exact result type and
operation-specific error-code allowlist before resuming a task. A missing
provider produces the operation's documented unavailable result when that
condition is recoverable. Malformed, late, duplicate, or wrong-operation
provider results terminate with `protocol.violation` after cleanup.

### 6.1 Values and canonical encoding

Runtime values use explicit variants and opaque execution-local handles.
Canonical value encoding is deterministic, bounded, and independent of memory
addresses. Records use sorted field names; maps use the language's canonical
key ordering; NaNs use the canonical NaN bits; enum identity includes its
resolved nominal identity.

Closures, futures, tasks, workspaces, and sub-agent handles are not serializable
entry or replay data unless the replay layer replaces an opaque handle with its
documented execution-local token representation.

### 6.2 Budgets

Limits cover instructions, cleanup instructions, cumulative logical allocation,
maximum single allocation, call depth, tasks, concurrent effects, wall time,
input/output bytes, schema work, provider requests, redirects, DNS answers,
filesystem entries, and protocol frames. The VM charges before performing the
bounded action. Cleanup has its own reserve and cannot create new user work.

Resource exhaustion terminates with `resource.limit`; wall deadlines terminate
with `runtime.timeout`. A stopped outcome remains distinct from success and
failure and wins only according to the scheduler's documented terminal-order
rules.

## 7. Manifests, packages, and preflight

`allen.toml` declares the package identity, language requirement, entries,
capabilities, limits, HTTPS origins, dependencies, and required tools.
`allen.lock` contains the canonical local dependency graph and source digests.
Inline manifests expose the same execution contract for one standalone file.

Package loading rejects path traversal, absolute source paths, symlink escapes,
cycles, duplicate identities, stale locks, digest mismatches, unknown fields,
and contract disagreement. Imports resolve within the locked graph; no runtime
source loading occurs after artifact verification.

Launch preflight:

1. verifies the artifact and selects one declared entry;
2. parses and validates exact entry JSON against the strict boundary type;
3. checks the current language and artifact identifiers;
4. intersects requested capabilities, origins, tools, and limits with host
   policy without granting undeclared authority;
5. validates working-directory and provider requirements; and
6. produces an immutable prepared launch consumed exactly once.

## 8. Host-neutral providers

The runtime defines separate provider interfaces for filesystem, HTTP,
permission grants, the invoking agent, model requests, user interaction,
sub-agents, and tools. Supplying one provider does not imply another. Provider
calls are lazy until awaited or spawned.

Expected operational failures use the source-visible closed `Result` type.
Cancellation, limits, deadlines, invariant failures, replay divergence after
execution starts, and provider protocol violations are terminal channels. No
general source exception or catch mechanism exists.

Prompts and responses are converted through strict schemas. Repair attempts are
bounded. Provider detail, credentials, paths, transcript content, prompt data,
and hidden reasoning never enter public error messages.

## 9. Filesystem and HTTP brokers

Filesystem access is descriptor-relative to an execution-scoped workspace.
Paths are relative UTF-8 components with no empty, `.` or `..` segment, NUL,
absolute prefix, or platform separator escape. Symlink components are rejected.
The broker validates object type and identity around every open and applies
entry, byte, and traversal limits.

External file or directory access requires a structured permission request.
The host may deny or narrow it but cannot broaden it. An approved object becomes
an opaque workspace handle stored in the execution capability table.

HTTP supports absolute HTTPS GET only. The manifest and host policy both allow
the origin. Redirects are revalidated. DNS answers, destination addresses,
ports, response framing, decompression, body size, redirects, and timeouts are
bounded. Loopback, link-local, multicast, unspecified, private, documentation,
and otherwise denied address classes are rejected according to policy.

## 10. Current JOSH protocol

JOSH uses the fixed envelope marker `josh/1` and negotiates the sole current
protocol version `josh/1.3`. Transport is an ASCII
decimal byte length, `:`, exactly that many UTF-8 JSON bytes, and `,`. Frames are
bounded before allocation. JSON objects reject duplicate and unknown fields;
request IDs and methods are validated before entering connection state.

The host sends `initialize` once with `protocol_versions: ["josh/1.3"]`, one
execution mode, an invoking-session identity or `null`, limits, and standard
capabilities.
The runtime selects `josh/1.3` or rejects initialization. The result contains
the runtime identity, effective limits, and the current feature set. There is no
minor-version fallback.

After initialization the host may freeze a tool catalog, load source or one
current artifact, start and cancel executions, answer provider requests, and
receive ordered execution events and one terminal response. Direction-scoped
request IDs, active-request bounds, cancellation tombstones, and strict method
state prevent duplicate, late, or cross-direction responses from resuming work.

The protocol routes invoking-agent messages, typed agent questions, transcript
snapshots, model and user prompts, sub-agent creation/run/message/ask, permission
decisions, and typed tool invocation. Attached operations carry the initialized
session identity. Unattended execution cannot use invoking-agent operations but
may use separately supplied model, user, sub-agent, and tool providers.

The `josh` executable requires an explicit `serve` or `run` command. `serve`
owns the framed stdio lifecycle; `run` is a local client that drives the same
current protocol contract for source, packages, or artifacts.

## 11. Replay and deterministic testing

The testkit defines one current canonical replay format. Its header binds the
artifact digest, language and runtime identities, contract and policy digests,
catalog and capability digests, error registry, scheduler completion order,
and final execution channel. Entries contain ordered operation identity,
request and schema digests, validated values or closed provider errors.

Replay validates the binding before dispatch, never invokes a live provider,
rechecks every value and error against the current operation contract, releases
completions only in the recorded order, and requires exact exhaustion and final
channel agreement. Secret-bearing semantic data cannot be replaced by a
schema-agnostic redactor.

Tests cover source modes, canonical artifacts, verifier rejection, provider
routes, deterministic scheduling, limits, filesystem and HTTP policy, protocol
framing/state, record/replay, editor fixtures, conformance data, hostile inputs,
and fuzz targets. Golden data represents only the current implementation.

## 12. Diagnostics, events, and redaction

Compiler and preflight failures are structured diagnostics before execution.
Runtime terminal failures use bounded registered codes and sanitized messages.
`runtime.panic` is a safe host boundary for an implementation invariant breach;
a Rust panic must not escape the supervisor.

Events use an execution-local sequence number and injected monotonic clock.
They expose lifecycle, task, resource-warning, provider, permission-decision,
replay provenance, stopped, completed, and failed state without leaking secret
input or provider detail. Cancellation performs provider cancellation and task
cleanup before the final event and response.

## 13. Required verification

Repository changes must keep these checks green:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo test --workspace --doc
npm --prefix editors/vscode test
git diff --check
```

The current source and host conformance runners, examples, Rosetta programs,
artifact fuzz target, parser fuzz target, protocol decoder fuzz target, replay
fuzz target, and schema conversion fuzz target complete the acceptance checks.
