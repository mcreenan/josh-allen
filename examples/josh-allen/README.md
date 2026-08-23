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
| Echo the host's frozen tool catalog | `tool-echo.allen` | `Execute examples/josh-allen/tool-echo.allen.` |
| Call the frozen MCP echo tool | `tool-call.allen` | `Execute examples/josh-allen/tool-call.allen.` |
| Run a child agent | `subagent-run.allen` | `Execute examples/josh-allen/subagent-run.allen.` |

`user-ask.allen` pauses until the user answers `true` or `false`.

`tool-echo.allen` does not discover tools. The invoking host freezes a catalog
with source, revision, observation time, freshness, and completeness metadata.
The MCP bridge passes the runtime-confirmed `catalog/set` result as entry input
when `allen_session_start` uses `catalog_input: true`. Unattended JOSH accepts
the same flow through `josh run --catalog catalog.json --catalog-input`.

The packaged bridge catalog contains only `allen_integration_echo`. It does not
claim to enumerate every Codex tool. A host without a complete registry export
must not set `complete: true` or present a partial list as complete.

`tool-call.allen` calls the packaged bridge tool and relays its typed result.
