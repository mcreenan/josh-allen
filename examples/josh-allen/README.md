# JOSH/ALLEN agent examples

These manifest-first programs each exercise one MCP callback route. Ask Codex
or Claude Code to execute a file from a task rooted at this repository. The
`josh-allen` skill handles the bridge loop.

| Feature | Program | Prompt |
| --- | --- | --- |
| Send a status message | `agent-message.allen` | `Execute examples/josh-allen/agent-message.allen.` |
| Ask the invoking agent for typed data | `agent-ask.allen` | `Execute examples/josh-allen/agent-ask.allen.` |
| Request a typed model response | `model-request.allen` | `Execute examples/josh-allen/model-request.allen.` |
| Pause for a user decision | `user-ask.allen` | `Execute examples/josh-allen/user-ask.allen.` |
| Echo a supplied tool catalog | `tool-echo.allen` | `Execute examples/josh-allen/tool-echo.allen.` |
| Call the frozen MCP echo tool | `tool-call.allen` | `Execute examples/josh-allen/tool-call.allen.` |
| Run a child agent | `subagent-run.allen` | `Execute examples/josh-allen/subagent-run.allen.` |

`user-ask.allen` pauses until the user answers `true` or `false`.

`tool-echo.allen` does not discover tools. The invoking host supplies its
catalog as typed input. Codex can build that input from `ALL_TOOLS`. A host
without a complete catalog export must state the limit instead of presenting a
partial list as complete.

The packaged bridge exposes only `allen_integration_echo` in JOSH's frozen
tool catalog. `tool-call.allen` calls that tool and relays its typed result.
