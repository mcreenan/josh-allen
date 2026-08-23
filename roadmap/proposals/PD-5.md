# PD-5: typed capability and tool negotiation

Status: Proposed

Depends on: [PD-3](PD-3.md), [PD-4](PD-4.md)

Back to the [roadmap](../../ROADMAP.md#proposed).

## Decision summary

Let a program declare typed tool requirements. Match those requirements against
the frozen host catalog before entry execution.

Source receives a typed selected tool or a typed unavailable result. The
language does not add an untyped call by name.

## Problem

ALLEN `0.1` requires each selected tool in the manifest. A missing tool prevents
program load. This rule gives strong static types, but it reduces portability.

Two hosts can offer compatible issue trackers under different tool names. One
host can also omit an optional reporting tool. A program cannot adapt without a
different manifest or source package.

Dynamic JSON calls would avoid this load failure. They would also remove the
schema and effect guarantees that ALLEN is designed to keep.

## Scope

This proposal adds:

- Named typed tool interfaces.
- Required and optional interface requirements.
- Exact compatibility checks during preflight.
- Version and identity policy.
- Typed selection and unavailability results.
- A frozen selected catalog for each execution.
- Deterministic selection when more than one tool matches.

This proposal does not add runtime discovery after the entry starts. It does
not add `call(name, json)`.

## Terms

A tool interface defines operations, schemas, effects, and semantics.

A tool implementation is one catalog tool that claims an interface.

A requirement states which interface and policy a program accepts.

Negotiation matches requirements to the host catalog.

Selection is the deterministic result of negotiation.

## Concrete example: GitHub Issues or Jira Cloud

This example runs one incident workflow on two hosts. One host has GitHub
Issues. The other host has Jira Cloud.

Both adapters implement the proposed `IssueTracker@1` interface. Slack is an
optional notification interface.

| Requirement | Host A implementation | Host B implementation |
|---|---|---|
| `IssueTracker@1` | `tools.github.issues@2` | `tools.jira_cloud.issues@1` |
| `ObjectStore@1` | `tools.aws_s3.objects@1` | `tools.google_cloud_storage.objects@1` |
| `Notifier@1` | `tools.slack.chat@1` | Unavailable |

The GitHub adapter maps `project` to a repository. The Jira Cloud adapter maps
`project` to a Jira project key. Both adapters return the same exact interface
type.

The object-store interface writes one incident report. Host A selects Amazon
S3. Host B selects Google Cloud Storage.

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
let tracker = tools.select_required<IssueTracker>();
let objects = tools.select_required<ObjectStore>();
let notifier = tools.select_optional<Notifier>();

let issue = await tracker.create({
  project: input.project,
  summary: input.summary,
  body: input.report,
})?;

let stored = await objects.put({
  key: "incidents/" + issue.value.id + ".json",
  value: encode(input.report),
})?;

match notifier {
  Available(slack) => await slack.post({
    channel: "incident-response",
    text: issue.value.url,
  }),
  Unavailable(_) => Ok(Void),
}
```

On Host A, the issue URL points to GitHub Issues. On Host B, the URL points to
Jira Cloud. Source uses the same `IssueTracker@1` result type.

Host B continues when Slack is unavailable. The optional result does not give
source an untyped Slack fallback.

If the Jira adapter omits one required output field, preflight rejects it. The
runtime does not discover the mismatch after issue creation.

Events can use the selected implementation identity. For example, Host A can
receive `github.issue.closed@1`. Host B can receive `jira.issue.updated@1` with
status `Done`. Both adapters lower the event to `IssueClosed@1`.

## Interface contract

An interface includes:

- A stable interface name.
- An interface version.
- Operation names.
- Exact input, output, and error schemas.
- Effect classes from PD-2 when applicable.
- Data-flow policies from PD-3.
- Required receipt level from PD-4.
- Semantic claims that have machine-readable IDs.

The interface digest covers all these items.

Illustrative interface syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
tool interface IssueTracker@1 {
  async fn create(input: CreateIssue)
    returns Result<CreateIssueResult, CreateIssueError>
    effects [issue.create]

  async fn get(input: GetIssue)
    returns Result<Issue, GetIssueError>
    effects [issue.read]
}
```

## Manifest contract

A manifest can declare a required or optional interface.

```toml
[[tool_interfaces.optional]]
name = "issue-tracker"
version = ">=1.2.0, <2.0.0"
receipt_level = "provider"
```

The requirement can also restrict accepted principals, providers, effect
classes, privacy policies, and resource limits.

The host can deny a matching tool. The host cannot select a tool that fails a
source requirement.

## Compatibility

The first version should use exact operation schemas. Exact schemas avoid
hidden coercion and structural subtyping errors.

An implementation matches when:

- The interface name and version match.
- Every required operation exists.
- Each schema digest is exact.
- Each declared effect is equal to or narrower than the requirement.
- Each effect class is equal to the required class.
- The principal and receipt policies match.
- The data-flow policy is equal to or more restrictive than required.

Future versions can add explicit adapters. An adapter must be a checked package.
Negotiation must not invent an adapter.

## Selection

Preflight sorts candidates by canonical principal ID, tool name, and version.
The manifest can state a deterministic preference list.

If one candidate has the highest accepted preference, preflight selects it.
If candidates remain equal, preflight returns an ambiguity diagnostic.

The host must not select a candidate based on hidden enumeration order.

## Source use

Illustrative future syntax follows:

```allen
match tools.select<IssueTracker>() {
  Available(tracker) => await tracker.create(issue),
  Unavailable(reason) => Ok(Plan.Manual { reason }),
}
```

`tracker` is an opaque typed handle. Its methods have the schemas and effects
from the selected interface.

An optional selection must use exhaustive matching. Source cannot assume that
the tool exists.

## Capability negotiation

The same model can apply to optional standard capabilities. The language keeps
standard capability types separate from tool interfaces.

A capability requirement can state a minimum contract. Preflight returns a
typed availability value. It does not grant the capability.

The launch still intersects the request with host policy. The selected result
contains only effective capability information that policy permits source to
inspect.

## Freeze rule

Negotiation completes before entry execution. The runtime then freezes:

- Selected tool principals.
- Exact tool versions.
- Interface and schema digests.
- Effect and data-flow policies.
- Receipt requirements.
- Effective tool grants.

The runtime does not add or replace a tool during execution.

A durable workflow can select a new tool only through an explicit migration.
The migration records the old and new selections.

## Recording and replay

The replay contract includes every requirement and selection digest.

Replay uses the recorded typed selection. It does not query a live catalog.
Replay rejects a different tool principal, version, schema, or policy.

An unavailable result also enters the replay contract.

## Security rules

- Negotiation never returns an untyped tool handle.
- Optional status does not permit undeclared effects.
- The host cannot weaken privacy or receipt requirements.
- A selected principal must verify under PD-4.
- Tool output retains PD-3 labels.
- Source cannot enumerate hidden tools outside declared requirements.
- Catalog freeze occurs before source execution.

## Failure cases

Preflight returns a typed unavailable result or a diagnostic for these cases:

- No tool matches a required interface.
- A schema digest differs.
- A candidate has wider effects than the requirement permits.
- A principal or receipt policy fails.
- Two candidates remain equal after deterministic selection.
- A catalog changes after freeze.
- An adapter package is missing or unverified.

## Implementation work

1. Define interface declarations and canonical digests.
2. Add manifest requirement forms.
3. Add preflight matching and deterministic selection.
4. Generate opaque typed selected handles.
5. Add optional capability availability types.
6. Bind selections to artifacts and replay.
7. Add selection data to JOSH initialization and load results.
8. Add compatibility and hostile-catalog tests.

## Acceptance tests

- Select `tools.github.issues@2` as `IssueTracker@1` on Host A.
- Select `tools.jira_cloud.issues@1` as `IssueTracker@1` on Host B.
- Select Amazon S3 on Host A and Google Cloud Storage on Host B.
- Continue without Slack on Host B.
- Lower GitHub and Jira close events to one typed interface event.
- Reject a Jira adapter that omits one required result field.
- Select one of two compatible tools by declared preference.
- Return typed unavailability for a missing optional tool.
- Fail load for a missing required interface.
- Reject a candidate with one changed schema field.
- Reject a candidate with wider effects.
- Replay the recorded selection without a live catalog.
- Reject a catalog change after freeze.
- Prevent source from listing undeclared hidden tools.

## Open decisions

1. Can an interface contain optional operations?
2. How should checked adapters declare semantic changes?
3. Which preference fields are portable across hosts?
4. Can one requirement select more than one implementation for a quorum?
5. How does a durable workflow migrate a selected tool safely?
