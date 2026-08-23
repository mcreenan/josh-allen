---
name: josh-allen
description: Author and run typed ALLEN programs through JOSH when a bounded workflow needs deterministic rules, narrow capabilities, or agent, model, user, tool, and subagent callbacks.
---

# JOSH/ALLEN

JOSH/ALLEN has two parts. ALLEN is the language and runtime. JOSH is the host
that runs ALLEN programs and connects them to an agent session.

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
signed receipts, expose a complete tool registry, or isolate child-agent
authority.

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
  with `{"outcome":"ok","value":<tool output>}`. The packaged bridge exposes
  only `allen_integration_echo` in its frozen JOSH catalog.
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

`examples/josh-allen/tool-echo.allen` takes a `ToolCatalog` supplied by the
invoking agent. The program does not discover tools.

In Codex, read every enabled nested tool's exact `name` and `description` from
`ALL_TOOLS` inside `functions.exec`. Add `functions.exec` and directly exposed
tools that are absent from `ALL_TOOLS`. Deduplicate exact names, sort by name,
and pass the complete list to the program. Print every returned pair.

On another host, use its supported tool metadata only when that metadata can
be enumerated. If the host has no complete export, state that limit. Do not ask
the user to invent a catalog or claim that a partial list is complete.
