# PD-3: information-flow types

Status: Proposed

Depends on: None

Back to the [roadmap](../../ROADMAP.md#proposed).

## Decision summary

Add labels for confidentiality, integrity, and data origin. Carry these labels
through values, prompts, storage, tools, models, and output boundaries.

Require explicit authority to remove or improve a label.

## Problem

The current effect system controls which operations a function can call. It
does not control which data enters those operations.

A function can have file-read and network authority. The function can then send
file content to a remote host. The effect declarations show both authorities,
but they do not identify the data flow.

Typed prompt fields keep instructions and data separate. This separation does
not make tool, model, transcript, or network content trustworthy.

## Scope

This proposal adds three label groups:

| Group | Example labels | Purpose |
|---|---|---|
| Confidentiality | `Public`, `Private`, `Secret` | Control where data can go. |
| Integrity | `Untrusted`, `Validated`, `Approved` | Record which checks support data use. |
| Origin | file, user, model, tool, network, memory | Record where data came from. |

The final names need a separate language decision. The rules in this proposal
do not depend on those names.

## Terms

A label is type information that describes a value.

A source creates labeled data.

A sink sends data to an external boundary or final output.

Declassification lowers a confidentiality restriction.

Endorsement increases an integrity claim.

Validation checks structure. Validation does not prove that content is true.

## Type behavior

Labels compose with existing types.

Illustrative future types follow:

```allen
Secret<String>
Untrusted<HttpResponse>
Origin<tools.github.get_issue, Issue>
Approved<ReleasePlan>
```

The compiler calculates labels for each expression. It uses a conservative
join when an expression combines values.

For confidentiality, the result keeps the most restrictive input label.

For integrity, the result keeps the least trusted input label.

For origin, the result records each applicable source. The implementation can
use a bounded origin set or a canonical derived-origin digest.

## Example

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
let notes: Secret<String> = await fs.read_text(workspace, "notes.txt")?;
let page: Untrusted<Bytes> = (await http.get(url))?.body;

// The compiler rejects this call without a matching release policy.
await model.request(prompt {
  system: "Summarize the notes.",
  data: { notes },
  output: Summary,
});
```

The model sink declares which confidentiality labels it accepts. The selected
provider policy can make that sink more restrictive.

## Sources

Each external boundary assigns initial labels.

- File reads use the label of the workspace grant and file policy.
- Network responses are untrusted and record the final origin.
- Tool results use the tool identity, schema, and declared integrity policy.
- Model results are untrusted unless a later operation endorses them.
- User input records the authenticated user when available.
- Transcript parts retain their source and redaction state.
- Persistent memory restores the labels stored with each value.

A strict schema changes `unknown` data to a typed shape. It does not remove the
`Untrusted` label.

## Sinks

Each sink declares a label policy.

Applicable sinks include:

- Model prompts.
- Agent and user messages.
- Tool inputs.
- Network requests.
- File writes.
- Persistent memory writes.
- Workflow outputs.
- Logs, events, and receipts.

The compiler checks static label rules. The runtime checks labels that depend
on selected providers, paths, origins, or principals.

## Declassification

Declassification changes confidentiality. It requires a named effect and a
manifest request.

The operation states:

- The input label.
- The output label.
- The subject digest.
- The reason.
- The policy identity.
- The approving principal when policy requires approval.

The runtime records a receipt for the operation.

Redaction is not automatic declassification. Redaction creates a new derived
value. The new value needs a policy check before release.

## Endorsement

Endorsement changes integrity. It requires evidence.

Examples include:

- A schema validator confirms data shape.
- A signature check confirms data origin.
- A test run confirms one build result.
- A user approves one exact plan.

Each operation adds a specific claim. It must not change unrelated claims.

For example, signature verification confirms origin and integrity of bytes. It
does not confirm that the signed statement is true.

## Generic functions

Generic code must not remove labels.

A function that accepts `T` and returns `T` keeps all labels. A function that
creates a derived value calculates labels from its inputs.

Future work can add label polymorphism. The first version should support a
small fixed set of compiler-known label operations.

## Serialization and storage

Canonical encoding includes the semantic labels or a bound label descriptor.
Decoding restores those labels.

`unknown` cannot erase a label. A map, list, record, enum, prompt, stream, or
memory record keeps the labels of contained values.

The runtime rejects a boundary format that cannot represent required labels.
It must not silently convert labeled data to an unlabeled value.

## Recording and replay

Recording stores source labels, sink policy digests, and label-change receipts.

Replay recalculates each label transition. Replay rejects a changed source,
sink policy, declassification, or endorsement.

Redacted replay exports can remove content. They must keep enough label and
digest data to verify the control flow.

## Security rules

- Source cannot construct `Approved<T>` without an endorsement operation.
- Source cannot remove `Secret<T>` with a conversion or generic function.
- A host can make a sink policy more restrictive.
- A host cannot silently make a sink policy less restrictive.
- Labels survive prompts, streams, checkpoints, memory, and receipts.
- Error messages must not disclose labeled content.
- A digest of secret low-entropy data is also sensitive.

## Failure cases

The compiler or runtime must reject these conditions:

- Secret data enters an unapproved model or network sink.
- Untrusted data enters a sink that requires endorsed data.
- Serialization removes a required label.
- A generic function returns a value with weaker labels.
- A replay uses a different sink policy.
- A receipt publishes a sensitive plain digest.
- A child agent receives data outside its projection policy.

## Implementation work

1. Define the initial label lattice.
2. Add labels to semantic types and HIR.
3. Add label joins to expressions and control flow.
4. Add source and sink policies to provider contracts.
5. Add declassification and endorsement effects.
6. Add label data to canonical encoding and bytecode checks.
7. Extend recording, replay, events, and redaction.
8. Add compile-fail tests for prohibited flows.

## Acceptance tests

- Reject secret file content in an unapproved model prompt.
- Permit an approved redacted value and record its derivation.
- Keep `Untrusted` after exact JSON schema validation.
- Add an origin claim after signature verification.
- Reject a generic function that removes a label.
- Store and retrieve a labeled memory value without label loss.
- Reject a replay after a sink policy changes.

## Open decisions

1. Are labels part of types or a separate compiler analysis?
2. Which confidentiality and integrity labels belong in the first profile?
3. How many origin entries can one value retain?
4. Which labels must appear in boundary schemas?
5. Can one policy approve a class of values, or only one subject digest?
