# Use ALLEN through JOSH

Read this file first. Load a reference document only when the routing table
below tells you to.

## Your operating rule

Write task logic and helper programs in ALLEN. Run ordinary files with the
first-party `josh run` adapter. Do not write a Python, JavaScript, or other host
adapter merely to execute an ALLEN file.

Default to:

- language `0.1` (the current evolving early-alpha profile);
- protocol `josh/1.6`;
- unattended mode when no existing invoking-agent session was supplied;
- attached mode only when the host supplied a real opaque invoking-session ID;
- one loose `.allen` file and no package manifest until the program actually
  needs package metadata, capabilities, tools, or multiple modules;
- an empty tool catalog and no grants until the program actually requires them;
- exact types, explicit effects, least authority, and bounded limits; and
- one active execution per JOSH connection.

JOSH does not create the invoking-agent session. It binds a session already
owned by the host. Never invent a session ID.

## Start here

### 1. Create one ALLEN file

Create `task.allen`:

```allen
export fn main() returns Int {
  42
}
```

For a loose file, `josh run` synthesizes a capability-free `inline` package and
selects exported `main`. This form cannot authorize an external effect.

### 2. Run it

With an installed binary:

```sh
josh run task.allen
```

From this repository:

```sh
cargo run --quiet --bin josh -- run task.allen
```

The command prints one terminal JSON outcome:

```json
{"outcome":"completed","output":42}
```

Pass exact entry JSON directly, from a file, or through stdin:

```sh
josh run --input '{"value":41}' task.allen
josh run --input @input.json task.allen
josh run --input - task.allen
```

`josh run` also accepts a directory containing `allen.toml` or a verified
`.allenb` artifact. Use `--entry`, `--workdir`, `--grant`,
`--allow-net-origin`, `--wall-ms`, or `--trace-events` only when needed. Run
`josh run --help` for their exact forms.

### 3. Add an inline manifest only when needed

A single file with a non-`main` entry, capabilities, HTTP origins, or required
tools uses one leading inline `manifest` block. The input and output boundary
of a loose `main` entry is inferred from its exact function signature. Keep task
logic in the same `.allen` file. Use `allen.toml` only for a package directory
or multiple modules.

An effectful function must declare its maximum effect set; omission means the
exact empty set. A manifest request
is not a grant; `josh run` grants only the explicitly selected standard
capabilities and origins.

### 4. Choose unattended or attached execution

`josh run` is unattended. Use the raw `josh serve` endpoint only when an
existing host or agent harness must bind a real invoking-agent session:

```json
"execution_mode": "unattended",
"invoking_session_id": null
```

When a real session ID was supplied, use attached mode:

```json
"execution_mode": "attached",
"invoking_session_id": "opaque-host-session-id"
```

Attached mode is required for `agent.message`, `agent.ask`,
`agent.transcript`, and external-filesystem permission requests. Models, user
interaction, tools, and sub-agents use independent providers.

### 5. Add only the authority the program needs

For each external operation, align all applicable layers:

1. the ALLEN function's declared effect;
2. the package manifest request;
3. the initialized protocol feature/provider;
4. the catalog contract for a tool;
5. the `execution/start` capability, tool, or HTTP-origin grant; and
6. host/runtime policy and limits.

If one layer is absent, do not bypass it in host code. Either remove the
operation or supply the missing contract explicitly.

### 6. Interpret the terminal outcome

`execution/start` ends with exactly one result:

- `completed`: use the schema-validated `output`;
- `stopped`: the ALLEN execution ended intentionally; the host and invoking
  agent remain alive;
- `failed`: inspect the safe structured runtime error; or
- `cancelled`: the start request or connection was cancelled.

Do not treat an event as the final result. Wait for the response to the original
`execution/start` request.

## Reference router

Open only the document and section needed for the current question.

| Need | Read |
|---|---|
| Write ALLEN syntax, declarations, functions, records, enums, type aliases, matching, or generics | [ALLEN language reference sections 1 to 6](reference/allen-language.md#1-rules-for-agents) |
| Use futures, tasks, `spawn`, `await`, or structured concurrency | [ALLEN async reference §7](reference/allen-language.md#7-async-execution-and-task-ownership) |
| Call filesystem, HTTP, invoking-agent, model, user, sub-agent, or tool operations | [ALLEN standard operations §8](reference/allen-language.md#8-standard-operations-and-providers) |
| Understand entry-boundary JSON | [ALLEN boundary JSON §5.8](reference/allen-language.md#58-source-boundary-json) |
| Write `allen.toml`, entries, or capabilities | [ALLEN packages and capabilities §9](reference/allen-language.md#9-packages-manifests-and-capabilities) |
| Handle ALLEN errors, `stop`, lifecycle, determinism, or limits | [ALLEN errors and lifecycle §10](reference/allen-language.md#10-errors-outcomes-and-lifecycle) |
| Check whether a desired language feature exists | [Unsupported syntax and operations section 11](reference/allen-language.md#11-unsupported-syntax-and-operations) |
| Implement exact length framing or envelopes | [Transport and envelope sections 1 and 2](reference/josh-protocol.md#1-transport) |
| Understand initialization and the connection lifecycle | [Initialization and lifecycle sections 3 and 4](reference/josh-protocol.md#3-initialization) |
| Build a catalog or load source/artifacts | [Loading and catalogs sections 5 and 6](reference/josh-protocol.md#5-program-loading) |
| Construct `execution/start` grants or interpret outcomes | [Execution start and result §7](reference/josh-protocol.md#7-execution-start-and-result) |
| Bind and service an invoking-agent session | [Invoking agent §8](reference/josh-protocol.md#8-invoking-agent) |
| Respond to model, user, transcript, sub-agent, or permission requests | [Provider routes sections 9 to 12](reference/josh-protocol.md#9-model-and-user-providers) |
| Handle events, wire errors, cancellation, or disconnect | [Events and wire errors sections 13 and 14](reference/josh-protocol.md#13-events) |
| Review a JOSH client before use | [Agent checklist §15](reference/josh-protocol.md#15-agent-checklist) |

## Non-negotiable checks

Before running:

- source and manifest types agree with the selected entry;
- every effectful function declares its effects;
- package files are normalized, unique, and UTF-8 sorted for `program/load`;
- one complete honest host projection is frozen after initialization;
- the catalog is frozen before program loading;
- `program_id` and `artifact_digest` come from the successful load response;
- capability, tool, and origin grants are sorted, unique, and no wider than the
  manifest;
- entry input exactly matches its schema;
- every limit is positive and implemented; and
- the host is prepared to service every effect the selected entry can execute.

While running:

- preserve active request IDs and respond in the correct direction;
- match runtime requests to the active execution and bound session;
- validate provider results against their exact schemas;
- never expose hidden instructions, reasoning, credentials, or secrets;
- keep reading stdout until the original start request returns; and
- treat disconnect as permanent and let JOSH perform bounded cleanup.

If a desired behavior is missing from the references, do not invent it. Use a
typed declared tool when appropriate, revise the program to use supported
behavior, or stop and request a language/protocol decision.
