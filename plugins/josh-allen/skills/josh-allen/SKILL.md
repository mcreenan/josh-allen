---
name: josh-allen
description: Author and run typed ALLEN programs through JOSH when a bounded workflow needs deterministic rules, narrow capabilities, or agent, model, user, tool, and subagent callbacks.
---

# JOSH/ALLEN

JOSH/ALLEN has two parts. ALLEN is the language and runtime. JOSH is the host
that runs ALLEN programs and connects them to an agent session.

## Load the language reference

Before authoring or changing ALLEN source, read the relevant sections of the
packaged [ALLEN language reference](references/allen-language.md):

- sections 1 through 6 for syntax, declarations, types, control flow, and
  effects;
- section 7 for futures, tasks, `spawn`, and `await`;
- section 8 for filesystem, HTTP, callback, and tool operations;
- section 9 for packages, manifests, entries, and capabilities; and
- sections 10 and 11 for lifecycle, errors, limits, and unsupported features.

Load only the sections required for the program. The packaged reference is
kept byte-for-byte in sync with the repository's canonical agent reference.

## Choose the runner

Choose from the effects used by the program.

- Use `josh run` for `fs.read`, `fs.write`, and `net.http_get`. Declare each
  capability in the manifest. Pass matching, narrow `--workdir`, `--grant`, or
  `--allow-net-origin` options.
- Use the `josh_allen` MCP tools for callbacks through the current agent host.
  This includes `agent.*`, `model.request`, `user.ask`, `tool.*`, and
  `sub_agent.*` effects. The MCP bridge does not grant filesystem or network
  access.
- If a task needs file or network evidence and a callback, use two ALLEN
  programs. Reduce the evidence with `josh run`, then pass that exact JSON to a
  callback program through MCP.

Never run callback effects with unattended `josh run`. That process is not
bound to the current agent session and will return an unavailable-provider
error.

The MCP bridge is prompt-assisted. It does not prove caller identity, provide
signed receipts, route providers natively, or isolate child-agent authority.
Its default projection contains only the built-in integration tool. A native
host adapter may inject a larger complete projection, catalog, and explicit
authorization list through `JOSH_ALLEN_HOST_PROJECTION_PATH`; that still does
not make the prompt-assisted session authenticated.

## Author the program

Own the program and execution details when this skill matches a natural work
request. The user should only need to describe the task.

Keep the input bounded and non-sensitive. Put task-specific source in a
workspace-contained `.josh-allen/` directory unless the repository already has
a better location for examples. Begin callback programs with an inline
`manifest`. Inspect the manifest and effects before running the program.

Use ALLEN when deterministic filtering, grouping, matching, aggregation, or
hard gates do real work. Leave ambiguous judgment or final approval to a typed
callback. Do not add an ALLEN program to a simple direct task when it would
only repeat the prompt.

Running the program is part of the result. Reading the source and answering by
hand is not enough.

## Run through MCP

Call `allen_session_start` with a workspace-relative `source_path`. Omit
`input` when the entry takes no parameter. When it takes one parameter, pass
the exact schema-valid JSON value. `entry` defaults to `main`.

Model, reasoning effort, and wall-time settings are optional host defaults.
Include them only when the user or source requests them. Do not restart a
session to add optional defaults.

When the server returns `next_action`, perform that action and no other. Copy
`next_action.resume_arguments_shape` as the complete arguments for
`allen_session_resume`. Replace its placeholder value. Keep the provider
response under the outer `result` key.

Use these provider result shapes:

- For `tool/invoke`, call the named host tool with the requested input. Resume
  with `{"outcome":"ok","value":<tool output>}`. By default, the packaged
  bridge projects and authorizes only `allen_integration_echo`. A host-injected
  catalog may name other tools, but call one only when it is actually available
  in the current host and appears in the frozen, host-authorized projection.
- For `agent/message`, show the message in the current task and resume with
  `{"accepted":true}`.
- For `agent/ask` or `model/request`, return a value that satisfies
  `response_schema`. Do not invent user input.
- For `user/ask`, stop and ask the user. Resume on their next message with
  `{"value":<schema-valid answer>}`.
- For `sub_agent.*`, use the host's native child-agent tools. Treat the model
  and effort fields as advice unless the user requested them. Resume with the
  exact JOSH result shape in the action.

If the host cannot perform an action, return the matching JOSH provider error.
Never fabricate success. Continue while `state` is `waiting`. Stop on
`terminal` or `cancelled`, then report the nested terminal outcome. Cancel an
abandoned session with `allen_session_cancel`.

Keep the session token out of ALLEN source, program input, transcript values,
and child-agent prompts.

## Tool catalog example

`examples/josh-allen/tool-echo.allen` takes the runtime-confirmed projection of
the frozen tool catalog. Start it through MCP with `catalog_input: true` and no
explicit `input`. For unattended execution, use `josh run --catalog
<catalog.json> --catalog-input`.

The catalog producer owns enumeration. It supplies sorted typed definitions
and provenance metadata. JOSH rejects catalogs marked incomplete, freezes the
definitions, and returns the metadata, digest, count, and tool summaries used
as the program input.

Without host injection, the packaged MCP bridge proves only its one-tool
integration catalog and cannot enumerate the current Codex built-ins, apps,
collaboration tools, or tools from other MCP servers. A native host adapter can
inject an exact projection/catalog/authorization snapshot through
`JOSH_ALLEN_HOST_PROJECTION_PATH`; JOSH validates its tools metadata and count,
then the bridge grants only tool names returned from the verified artifact and
authorized by that snapshot. Do not reconstruct registries from memory, infer
grants from source text, claim authenticated identity, or mark an inventory
complete without a host contract that guarantees it. Injection changes the
frozen contract, not the manual `next_action` routing mode.
