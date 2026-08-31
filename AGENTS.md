# Agent instructions

## Current language only

JOSH/ALLEN is an early-alpha project. The repository supports one current
ALLEN source language, artifact format, runtime contract, replay format, and
JOSH protocol. Older inputs are unsupported.

The [language status policy](docs/language-spec.md#0-language-status) is
authoritative. A breaking language change must update the implementation,
specifications, tests, conformance data, editor support, examples, and agent
reference in the same change.

Language changes must keep these files in sync:

- `docs/language-spec.md`
- `docs/agents/reference/allen-language.md`
- `plugins/josh-allen/skills/josh-allen/references/allen-language.md`

JOSH protocol changes must keep these files in sync:

- `docs/implementation-spec.md`
- `docs/agents/reference/josh-protocol.md`

The human specification wins if a paired agent reference disagrees.

## Run JOSH/ALLEN workflows

Load the `josh-allen` skill when the user asks to run an ALLEN program. Choose
the runner from the program's effects.

Use `josh run` for filesystem and network capabilities. Use the `josh_allen`
MCP server for callbacks through the current task, including agent, model,
user, tool, and child-agent effects. Use two ALLEN stages when a workflow needs
both kinds of access.

Never run callback effects through unattended `josh run`. It is not bound to
the current task. The MCP bridge is prompt-assisted and does not prove caller
identity, discover a complete tool registry by default, issue signed receipts,
or isolate child-agent authority. A native host may inject a frozen projection,
catalog, and authorization snapshot, but that does not authenticate the bridge
or add native callback routing.

Use JOSH/ALLEN for a natural work request when all of these are true:

- the input is a bounded sample, fixture, proposal, or batch;
- deterministic rules do useful grouping, filtering, matching, aggregation,
  or hard gating;
- ambiguous judgment or final approval stays with a typed callback; and
- the result is a report, dry run, or plan, not an authorized external change.

The skill owns program authoring, file placement, capability limits,
execution, and cleanup. Do not use ALLEN when a direct workflow is clearer.

## Agent reference

Start with [the agent entry point](docs/agents/README.md) when writing ALLEN or
implementing the JOSH protocol. Load only the reference section required for
the current task.
