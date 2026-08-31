# Host projection implementation handoff

Date: 2026-08-30  
Branch: `main`  
Starting commit: `2ccc1a9`  
State: deliberately paused mid-change at the user's request; nothing has been committed

## Original objective

The user wanted to keep developing ALLEN as both a functional language and an
agent-native language, with special emphasis on non-functional properties,
agent interoperability, and concrete cases where an agent benefits from
running a bounded ALLEN workflow. The user accepted the proposed direction and
then made the central requirement explicit: the agent host should project its
available environment into ALLEN automatically. The requested action was to
document the accepted design and implement it.

The accepted design is recorded in
[`roadmap/proposals/PD-11.md`](roadmap/proposals/PD-11.md). Its principal
decisions are:

- Introduce one canonical, atomic host projection after `initialize` and
  before `catalog/set`.
- Keep contract discovery, context disclosure, per-execution authority, and
  runtime routing as separate layers. Discovery never grants authority.
- Use profile `josh.host-projection/0.1` with exactly ten ordered sections:
  tools, resources, attachments, transcript, models, user interaction, agents,
  roots, permissions, and telemetry.
- Give every section provenance, revision, observation time, freshness,
  completeness, and item count.
- Represent session binding honestly as `none`, `prompt_assisted`, or
  `authenticated`; the current MCP bridge is only prompt-assisted.
- Have JOSH compute the projection digest, bind the subsequent catalog's tool
  metadata/count to the projection, and report exact verified artifact tool
  requirements from `program/load`.
- Let a host inject a projection/catalog/authorization bundle into the MCP
  bridge. Grant only verified artifact requirements that the host authorized;
  never infer grants by searching source text.
- Preserve manual `next_action` callback forwarding as a compatibility mode,
  while treating native provider routing as the intended destination.
- Deliver later phases for native tool and callback dispatch, MCP 2026 MRTR and
  Tasks, typed resource/context/artifact projection, package-as-MCP export,
  A2A, AG-UI/OpenTelemetry, and authenticated receipts/mutation safety.

## Fully implemented in the current worktree

The following phase-one code exists and was exercised by focused tests:

- JOSH protocol version is bumped from `josh/1.5` to `josh/1.6`, with the
  `host-projection` feature string.
- `josh-protocol` defines the strict host projection profile, the ten canonical
  section kinds, session-binding levels, request/result payloads, validation,
  and the `projection.invalid` / `projection.mismatch` wire errors.
- Protocol state now requires `initialize -> host/project -> catalog/set`.
  `host/project` is host-to-runtime, single-assignment, and valid only in the
  initialized state.
- `josh-host::Session` freezes the initialized host identity and execution
  mode, validates projection/session consistency, hashes the normalized typed
  projection with SHA-256, and refuses a catalog whose tools metadata or count
  differs from the frozen tools section.
- `program/load` now returns sorted exact `required_tools` derived from the
  verified artifact manifest.
- The stdio server accepts `host/project` and advances protocol state only
  after a successful projection freeze.
- `josh run` automatically synthesizes and sends a complete ten-section
  projection before its catalog. Its non-tool sections currently declare an
  empty inventory.
- All raw Rust test clients touched by the new required lifecycle now send a
  projection: stdio, tool, model/user, and sub-agent round trips.
- The packaged Python MCP bridge sends `host/project` before `catalog/set`.
  Its default projection describes its built-in catalog and empty non-tool
  sections.
- A host can set `JOSH_ALLEN_HOST_PROJECTION_PATH` to an exact JSON bundle with
  keys `projection`, `catalog`, and `granted_tools`. The reader bounds the
  file, rejects duplicate JSON keys, requires the exact top-level shape, and
  requires a sorted unique grant list.
- The bridge checks `program/load.required_tools` against that host-owned
  authorization list and starts execution with exactly the verified required
  tools. The previous source-substring grant inference has been removed.
- Rust tests cover canonical section presence/order/completeness, session and
  host binding, stable digests, catalog projection mismatches, and migrated
  wire transcripts. Python tests cover malformed/partial/oversized/duplicate
  projection bundles and verified-requirement authorization.
- `ROADMAP.md` now lists PD-11 as accepted, and the full accepted proposal is
  present as a new untracked file.

## Partial, interrupted, or uncertain work

The documentation synchronization was interrupted. Version strings were
changed to `josh/1.6` in the README, naming document, implementation spec,
agent entry point/reference, conformance host fixture, and PD-10. A search
finds no remaining stale `josh/1.5` uses outside historical/search references
in this handoff and ignored build/VCS data.
However:

- `docs/implementation-spec.md` does not yet specify `host/project`, its exact
  payload, digest semantics, lifecycle, catalog binding, or `required_tools`.
- `docs/agents/reference/josh-protocol.md` only has version replacements. Its
  feature list omits `host-projection`; lifecycle and checklist still jump
  directly from initialization to catalog freezing.
- `docs/install.md` does not document
  `JOSH_ALLEN_HOST_PROJECTION_PATH` or the bundle schema.
- `plugins/josh-allen/skills/josh-allen/SKILL.md` still says the bridge cannot
  expose a complete tool registry and only has the built-in one-tool catalog.
  That remains true by default, but it needs wording for host injection and
  its prompt-assisted security boundary.
- The conformance JSON only had its protocol version changed; it does not yet
  contain a host projection fixture or expected transition.
- PD-11 and `ROADMAP.md` still say “phase 1 in implementation.” Do not mark it
  implemented until documentation, conformance, and full verification finish.

The implementation is intentionally only phase one of PD-11:

- The projection currently carries section manifests/counts, not typed item
  payloads for resources, attachments, transcript, models, agents, roots,
  permissions, or telemetry. Those are not yet values available to ALLEN.
- The CLI and default bridge correctly report those non-tool sections as
  empty. There is no automatic T3/Codex host registry discovery in this repo.
- The injected file is a host adapter boundary, not native dispatch. The
  bridge still returns `next_action` for agent/model/user/tool/sub-agent
  callbacks.
- There is no MCP `2026-07-28` MRTR/Tasks adapter, package-as-MCP server, A2A
  adapter, AG-UI event adapter, OpenTelemetry export, authenticated identity
  receipt, or mutation-safe retry implementation.
- The projection digest is computed after strict deserialization by serializing
  the Rust struct with `serde_json`. This is deterministic for the current
  fixed struct/order, but cross-implementation canonicalization should be
  explicitly reviewed and documented.
- The successful projection digest is not yet bound into execution accepted
  events, artifacts, or replay records. PD-11 should state whether phase one
  deliberately excludes that binding or the implementation should add it.
- The injected bridge validates bundle framing/top-level shape itself, while
  JOSH validates the detailed projection/catalog. Review whether the bridge
  should reject more malformed data before spawning JOSH.
- `HostProjectionSetParams::section` indexes using the enum discriminant. It is
  safe with the current fieldless ordered enum and prior validation, but an
  explicit lookup would be more robust against future enum edits.

## Files changed

Modified:

- `README.md`
- `ROADMAP.md`
- `crates/josh-host/src/server.rs`
- `crates/josh-host/src/session.rs`
- `crates/josh-host/tests/common/mod.rs`
- `crates/josh-host/tests/protocol_state.rs`
- `crates/josh-protocol/src/handshake.rs`
- `crates/josh-protocol/src/message.rs`
- `crates/josh-protocol/src/payload.rs`
- `crates/josh-protocol/src/state.rs`
- `crates/josh-protocol/src/version.rs`
- `crates/josh-protocol/tests/payload.rs`
- `crates/josh-protocol/tests/state.rs`
- `crates/josh/src/runner.rs`
- `crates/josh/tests/stdio.rs`
- `crates/josh/tests/sub_agent_roundtrip.rs`
- `crates/josh/tests/tool_roundtrip.rs`
- `docs/agents/README.md`
- `docs/agents/reference/josh-protocol.md`
- `docs/conformance/host-0.1.json`
- `docs/implementation-spec.md`
- `docs/naming.md`
- `plugins/josh-allen/mcp/server.py`
- `plugins/josh-allen/tests/test_server.py`
- `roadmap/proposals/PD-10.md`

Untracked:

- `roadmap/proposals/PD-11.md`
- `HANDOFF.md` (this file)

Before this handoff was added, `git diff --stat` reported 25 tracked files,
1,007 insertions, and 113 deletions. No implementation file was changed after
the user requested the pause.

## Tests already run

Passed during this work:

- `cargo check --workspace`
- `cargo test --workspace --no-run`
- Full `cargo test -p josh-protocol` at the then-current state: 44 integration
  tests passed. The subsequently added focused host-projection payload test
  also passed.
- Full `cargo test -p josh-host` at the then-current state: 22 unit tests, 9
  program-load tests, and 5 protocol-state tests passed. The two subsequently
  added focused projection/catalog tests also passed.
- `cargo test -p josh --test stdio --test sub_agent_roundtrip --test tool_roundtrip`
  after raw-client migration: stdio 3/3, sub-agent 1/1, and tool roundtrip
  10/10 passed (the tool transcript was rerun after its expected lifecycle was
  corrected).
- `python3 -m unittest plugins/josh-allen/tests/test_server.py` after the final
  bridge tests: 26/26 passed.
- `cargo fmt --all` was run before the final focused test pass.
- `git diff --check` produced no errors during handoff inspection.

Not rerun after the latest test/document edits:

- The complete workspace test suite.
- The complete `josh-protocol` and `josh-host` suites as single final commands
  after adding the last focused tests (the new tests themselves passed).

One earlier broad `cargo test -p josh` run had one failure:
`executor_timeout_terminates_descendants_that_hold_output_pipes_open` in
`crates/josh/tests/runner_tool_provider.rs`. The other provider tests passed,
and this test is unrelated to host projection; it appeared timing/flaky, but
that has not been proven. Do not silently dismiss it—rerun and investigate if
it reproduces.

## Current git status

At handoff creation, the repository is on `main` at `2ccc1a9`. The status is:

```text
 M README.md
 M ROADMAP.md
 M crates/josh-host/src/server.rs
 M crates/josh-host/src/session.rs
 M crates/josh-host/tests/common/mod.rs
 M crates/josh-host/tests/protocol_state.rs
 M crates/josh-protocol/src/handshake.rs
 M crates/josh-protocol/src/message.rs
 M crates/josh-protocol/src/payload.rs
 M crates/josh-protocol/src/state.rs
 M crates/josh-protocol/src/version.rs
 M crates/josh-protocol/tests/payload.rs
 M crates/josh-protocol/tests/state.rs
 M crates/josh/src/runner.rs
 M crates/josh/tests/stdio.rs
 M crates/josh/tests/sub_agent_roundtrip.rs
 M crates/josh/tests/tool_roundtrip.rs
 M docs/agents/README.md
 M docs/agents/reference/josh-protocol.md
 M docs/conformance/host-0.1.json
 M docs/implementation-spec.md
 M docs/naming.md
 M plugins/josh-allen/mcp/server.py
 M plugins/josh-allen/tests/test_server.py
 M roadmap/proposals/PD-10.md
?? HANDOFF.md
?? roadmap/proposals/PD-11.md
```

Preserve all of these changes. Do not reset, discard, or rewrite unrelated
work.

## Known failures and risks

- Documentation and agent-reference synchronization is incomplete, which
  violates the repository rule for a current JOSH protocol change until fixed.
- The protocol is a hard break to `josh/1.6`; all clients must send
  `host/project`. Search for external/golden clients not covered by Rust/Python
  tests before considering it complete.
- The required-test list in PD-11 is broader than the current focused tests.
  State rejection for skipped/repeated/post-catalog projections is enforced by
  code and partly exercised through migrated tests, but explicit named tests
  should be added. Section validation should be made table-driven across every
  section and invalid field class.
- `program/load.required_tools` is a new required response field. Check every
  fixture, consumer, conformance file, and third-party-facing example.
- The bridge injection environment variable is process-global and the bundle
  is loaded at `Bridge` construction. Document snapshot timing and file-owner
  expectations; decide whether symlink/permissions policy belongs here.
- The default MCP bridge still has a synthetic one-tool catalog. Calling the
  current phase “automatic full host projection” without that qualification
  would overstate the implementation.
- Full workspace verification has not been run at this exact stopping point.
- No commit exists, so the stopping point depends on preserving this dirty
  worktree.

## Exact next steps, in order

1. Read `AGENTS.md`, this handoff, and `roadmap/proposals/PD-11.md`; inspect
   `git status` and `git diff` before editing. Preserve the dirty worktree.
2. Finish the authoritative protocol documentation in
   `docs/implementation-spec.md`: add the required
   `initialize -> host/project -> catalog/set` lifecycle, exact projection
   profile/sections/session bindings, digest computation, catalog matching,
   new errors/feature string, and `program/load.required_tools` semantics.
3. Mirror that contract in `docs/agents/reference/josh-protocol.md`, including
   an exact request/result example, the `host-projection` feature, lifecycle,
   errors, and client checklist. The human implementation spec wins on any
   disagreement.
4. Document `JOSH_ALLEN_HOST_PROJECTION_PATH` in `docs/install.md`, including
   the exact three-key bundle, bounds, prompt-assisted identity limitation,
   authorization semantics, snapshot timing, and a minimal valid example.
5. Update `plugins/josh-allen/skills/josh-allen/SKILL.md` so it accurately
   distinguishes the default one-tool projection from a host-injected catalog
   and does not imply authenticated identity or native routing. Check any
   other agent-facing bridge docs/examples for the same claim.
6. Update conformance data/tests for `host/project`, not just the version
   string. Search all response fixtures for the newly required
   `required_tools` field and all raw clients for a missing projection.
7. Add explicit protocol-state tests for skipping projection, duplicate active
   or repeated projection, projection after catalog freeze, and wrong
   direction. Add table-driven payload tests for every missing, duplicate,
   reordered, incomplete, and invalid canonical section and all attached /
   unattended binding combinations.
8. Add or confirm a direct test that `ProgramLoadResult.required_tools` is
   sorted, exact, and derived from a verified artifact for both source and
   bytecode loads. Keep the existing Python authorization/source-mention test.
9. Decide and document whether phase one must bind `projection_digest` into
   accepted execution events/replay. If yes, implement and synchronize all
   payloads, events, fixtures, and docs; if no, state the deferral explicitly
   in PD-11.
10. Review projection digest canonicalization and the enum-indexed `section`
    helper. Make any robustness changes only with corresponding tests and
    protocol documentation.
11. Run `cargo fmt --all`, `git diff --check`,
    `cargo test --workspace --no-run`, full `cargo test -p josh-protocol`, full
    `cargo test -p josh-host`, the raw `josh` integration tests, and the Python
    bridge suite. Then run the complete workspace suite. Rerun the descendant
    timeout test separately if it fails and report whether it is reproducible.
12. Search for `josh/1.5`, stale lifecycle language, stale feature lists,
    one-tool-only claims, missing `required_tools`, and missing `host/project`.
    Only after docs and all required tests are synchronized should PD-11 and
    `ROADMAP.md` say phase one is implemented.
13. Review the final diff for scope and security boundaries, then commit only
    if the user asks. Do not begin PD-11 phases 2–9 as part of merely completing
    this phase-one change.
