# JOSH protocol reference for AI agents

Status: Operational guide for the current `josh/1.3` protocol

Entry point: [`docs/agents/README.md`](../README.md). The implementation
specification is authoritative for runtime behavior.

JOSH means JSON-Oriented Session Host. It is the optional reference host that
connects ALLEN execution to an invoking agent, model, user interface,
sub-agents, tools, permission decisions, and execution events. ALLEN itself does
not require JOSH.

## 1. Transport

JOSH uses one framed UTF-8 JSON message per record:

```text
<ASCII decimal byte length>:<exact JSON bytes>,
```

The length counts bytes, not characters. A frame has no whitespace before the
length, no leading zero except `0`, and no trailing bytes after the comma.
Readers bound the decimal header and payload length before allocation. A clean
EOF between frames ends the connection; EOF inside a frame is fatal.

JSON objects reject duplicate and unknown fields. Numbers, strings, IDs,
methods, enum values, and nested payloads use their exact declared shape. Do
not add optional fields speculatively.

## 2. Message envelope

Every message uses protocol `josh/1` and exactly one shape:

```json
{"protocol":"josh/1","kind":"request","id":"1","method":"initialize","params":{}}
```

```json
{"protocol":"josh/1","kind":"response","id":"1","result":{}}
```

```json
{"protocol":"josh/1","kind":"response","id":"1","error":{"code":"request.invalid","message":"..."}}
```

```json
{"protocol":"josh/1","kind":"notification","method":"execution/event","params":{}}
```

A request has `id`, `method`, and `params`. A response has the matching `id`
and exactly one of `result` or `error`. A notification has `method` and
`params` but no `id`. IDs are scoped by direction, so the same text may be
active once in each direction.

Unknown responses, duplicate active IDs, invalid directions, methods used in
the wrong connection state, late responses after cancellation, and ambiguous
envelopes are fatal protocol violations.

## 3. Initialization

The host sends `initialize` first and exactly once. It offers only the current
negotiated protocol version:

```json
{
  "protocol": "josh/1",
  "kind": "request",
  "id": "init-1",
  "method": "initialize",
  "params": {
    "host": {"name": "allen-host", "version": "0.1.0"},
    "protocol_versions": ["josh/1.3"],
    "language_versions": ["0.1.0"],
    "execution_mode": "attached",
    "invoking_session_id": "session-1",
    "limits": {
      "max_frame_bytes": 1048576,
      "max_active_requests": 64,
      "max_loaded_programs": 32,
      "max_total_executions": 256,
      "max_catalog_tools": 128,
      "max_catalog_bytes": 1048576
    },
    "standard_capabilities": [],
    "extensions": []
  }
}
```

`execution_mode` is `attached` or `unattended`. Attached mode requires one
stable `invoking_session_id`; unattended mode sends it as `null`. Limits and
standard capability names are canonical, bounded, sorted, and unique.

The runtime either selects `josh/1.3` or rejects initialization. Its result
returns the runtime identity, effective limits, and these exact feature strings:
`agent`, `external-fs-grants`, `model`, `record-replay`, `structured-prompts`,
`structured-transcript`, `sub-agents`, `typed-responses`, `typed-tools`, and
`user-interaction`.
There is no minor-version fallback or alternate feature set.

## 4. Connection lifecycle

After initialization, the host may:

1. freeze the tool catalog;
2. load source, a package source bundle, or one current artifact;
3. start an execution with entry input, capabilities, limits, origins, workdir,
   and optional replay binding;
4. service runtime-to-host provider requests while execution runs;
5. receive ordered execution events; and
6. receive exactly one terminal execution result.

Program and execution IDs are connection-local, opaque, bounded, and never
reused. A failed load creates no program ID. A failed start creates no reusable
execution ID. A prepared execution is consumed exactly once.

`execution/cancel` is idempotent for an active execution. Cancellation records
bounded response tombstones so a late provider response remains detectable and
cannot resume another operation. Disconnect cancels all active execution and
provider work.

## 5. Program loading

`program/load` accepts exactly one input form:

- current canonical `.allenb` bytes;
- one loose source file;
- an inline-manifest source file; or
- a complete canonical package source bundle.

Source bundle paths are normalized relative UTF-8 paths, sorted and unique,
with no absolute, empty, `.`, or `..` component. Source bytes, manifest bytes,
file count, and aggregate bundle size are bounded. The runtime compiles source
through the same compiler path used by the CLI.

Artifacts must use the one current format and pass decoding plus independent
verification. The result identifies the loaded program and its entries,
capabilities, limits, origins, required tools, and boundary schemas.

## 6. Tool catalog

The host freezes at most one tool catalog before loading a program that needs
tools. Entries are sorted by canonical dotted name and contain exact input,
output, declared-error schemas, selected tool version, generated effect,
idempotency metadata, and limits.

The runtime validates and canonicalizes all schemas, rejects generated-name or
effect collisions, computes the catalog digest, and creates the current typed
ALLEN tool bindings. A loaded program binds the exact catalog digest; execution
cannot change it.

`tool/invoke` carries the execution ID, operation ID, tool name, exact input,
and deadline. Its response is one of validated output, validated declared
error, unavailable, denied, cancelled, or a protocol failure. Schema failures
become the generated tool `Error.Schema` result only where the language
contract allows it.

## 7. Execution start and result

`execution/start` selects one loaded program entry and supplies exact JSON
input. It may request only authority declared by the artifact. The runtime
intersects capabilities, origins, tools, and limits with initialized host
policy and rejects any broadening.

The terminal result has exactly one channel:

- `completed` with validated entry output;
- `stopped` with an optional bounded reason;
- `failed` with one registered sanitized runtime error; or
- `cancelled`.

The runtime sends the final lifecycle event before the terminal response.
Provider detail, credentials, paths, prompts, transcript content, tool data,
and hidden reasoning never appear in public error messages.

## 8. Invoking agent

Attached execution may use:

- `agent/message` for accepted-delivery notification;
- `agent/ask` for a typed response;
- `agent/transcript` for a bounded structured snapshot.

Every request carries the initialized `session_id`. The host must reject a
different session. `agent/ask` carries a structured prompt, strict response
schema, bounded attempt number, validation issues for repair attempts, and a
deadline. Responses are validated locally before the VM resumes.

Unattended execution cannot use these methods. Their absence does not disable
model, user, sub-agent, or tool providers.

## 9. Model and user providers

`model/request` and `user/ask` use the same structured prompt and strict typed
response machinery but do not carry invoking-agent identity. They are
independent providers. The host may supply either in attached or unattended
mode.

Each request has an operation ID, prompt, response schema, attempt, validation
issues, and deadline. Valid output returns the ordinary typed result. Expected
denial, unavailability, and exhausted validation use the source-visible closed
error type. Late or malformed responses are protocol violations.

## 10. Structured transcript

`agent/transcript` takes a bounded oldest-first query with `limit`. A snapshot
contains structured messages and parts, stable sequence order, optional role
and identity metadata, and no hidden reasoning. The runtime validates the full
shape, bounds text and item counts, and converts it to the corresponding ALLEN
nominal values.

## 11. Sub-agents

The current protocol supports:

- `sub_agent/create`;
- `sub_agent/run`;
- `sub_agent/message`; and
- `sub_agent/ask`.

Create and run carry a typed prompt plus a closed projection of context, tools,
capabilities, and limits. Handles are opaque execution-local tokens. They
cannot cross executions, be fabricated, serialized as ordinary data, or escape
their ownership scope.

Message and ask require a live handle from the current execution. Ask and run
use strict typed response validation and bounded repair attempts. Cancellation
must cancel pending child-provider work before terminal completion.

## 12. Permission decisions

`permission/request` carries one structured external file or directory request.
The host returns deny or an equal-or-narrower grant. It must not broaden access,
path scope, capability kind, or lifetime. The runtime revalidates the decision,
opens or upgrades the descriptor-relative object, and returns an opaque
workspace handle only after identity checks succeed.

`permission/revoke` invalidates one active grant. A revoked or cross-execution
handle cannot authorize access.

## 13. Events

`execution/event` notifications carry the execution ID, monotonically
increasing sequence, injected monotonic timestamp, event kind, and exact
kind-specific payload. Current events cover accepted, started, task lifecycle,
provider lifecycle, resource warnings, permission decisions, replay provenance,
stopped, completed, failed, and cancelled state.

Events are bounded and redacted. Replayed effects are marked `replayed: true`;
the protocol never claims that an external effect happened again.

## 14. Wire errors

Wire errors describe request or connection failures, not ALLEN entry values.
Current families include invalid request, invalid state, unsupported protocol,
catalog or program rejection, limit failure, cancellation, and internal safe
failure. Provider denials and unavailable outcomes use their exact provider
response envelope and become source-visible closed results where specified.

A protocol violation closes the connection after active work is cancelled. It
must not produce a second terminal response or expose internal error detail.

## 15. Agent checklist

When writing or reviewing a JOSH client:

1. Offer only `josh/1.3`.
2. Send `initialize` exactly once.
3. Use strict length framing and exact JSON shapes.
4. Keep request IDs unique per direction while active.
5. Freeze catalogs before loading tool-dependent programs.
6. Never request authority absent from the loaded program contract.
7. Preserve attached session identity on all `agent/*` calls.
8. Treat opaque handles as execution-local and nonserializable.
9. Keep servicing reentrant provider requests while execution is active.
10. Cancel active work on disconnect and reject late responses.
