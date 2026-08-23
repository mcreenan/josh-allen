# PD-8: capability-scoped persistent memory

Status: Proposed

Depends on: [PD-1](PD-1.md), [PD-2](PD-2.md), [PD-3](PD-3.md), [PD-4](PD-4.md)

Back to the [roadmap](../../ROADMAP.md#proposed).

## Decision summary

Add typed persistent memory through explicit capabilities. Each memory
namespace has an owner, schema, access rights, retention policy, and data-flow
policy.

The language defines memory behavior. It does not define one database or
semantic search model.

## Problem

`agent.transcript` returns a bounded session snapshot. It is not application
memory. It cannot store durable decisions, evidence, or workflow data.

A host can provide a memory tool today. Tool schemas can protect each call, but
the language has no common rules for ownership, retention, concurrency,
provenance, or use in prompts.

Ambient memory would be dangerous. One program must not read all records from
other programs, users, or tenants.

## Scope

This proposal adds:

- Opaque memory namespace capabilities.
- Exact typed record schemas.
- Read, append, update, and delete rights.
- Owner and tenant identity.
- Retention and expiration policy.
- Versioned compare-and-set updates.
- Mutation receipts.
- Exact-match and provider-declared indexed queries.
- Explicit context assembly with byte and token limits.
- Recording and replay rules.

This proposal does not require vector storage or one embedding provider.

## Terms

A namespace is one isolated memory collection.

A memory capability grants named rights for one namespace.

A record is one typed persistent value with metadata.

A version token identifies one committed record version.

Retention policy controls storage time and deletion behavior.

Context assembly selects memory records for one prompt or child request.

## Namespace contract

A namespace declaration includes:

- Namespace schema and version.
- Owner principal.
- Tenant principal when applicable.
- Allowed record type.
- Read and mutation rights.
- Retention class.
- Data-flow policy from PD-3.
- Receipt policy from PD-4.
- Query profile.
- Storage and query limits.

The host can deny or narrow this contract. It cannot merge the namespace with a
broader collection.

## Memory capability

`Memory<T, Rights>` is opaque. Source cannot construct, copy to another
execution, widen, or serialize the capability.

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
let memory: Memory<ReleaseDecision, ReadAppend> =
  memory.namespace<ReleaseDecision>("release-decisions");

let stored = await memory.append(ReleaseDecision {
  commit,
  decision,
  receipt,
})?;
```

The result includes a record ID, version token, and mutation receipt.

## Record metadata

Each record binds:

- Namespace identity.
- Record identity.
- Schema version.
- Record version.
- Creator principal.
- Creation and update workflow positions.
- Confidentiality, integrity, and origin labels.
- Retention and expiration data.
- Content commitment.
- Mutation receipt.

The runtime bounds metadata. The host does not expose hidden storage paths or
database identifiers.

## Read operations

The standard profile should support these read forms:

- Read one record by exact ID.
- Read one exact key when the namespace defines a key.
- List bounded metadata under an explicit index.
- Query a host-declared typed index.

Every query has a strict result limit and stable order. The provider returns a
continuation token for more results when policy permits it.

A continuation token is opaque. It binds the namespace, query digest, selected
order, and expiration.

## Mutation operations

Append creates a new record. Update and delete require the current version
token unless the namespace contract selects another explicit concurrency rule.

Compare-and-set has this behavior:

```text
expected version matches -> commit new version
expected version differs -> return conflict
```

PD-2 supplies idempotency and ambiguous-result rules. PD-4 supplies signed
mutation receipts.

The runtime must not repeat a memory mutation with a new idempotency key after
an ambiguous result.

## Deletion and expiration

Delete creates a tombstone receipt. The receipt proves the provider accepted
the delete request under its policy.

The receipt does not prove physical erasure on every storage copy unless the
provider contract makes and signs that claim.

Expiration prevents normal retrieval after the due policy event. Audit policy
defines which metadata remains visible.

Source must not use the word `deleted` for a record when only access expired.

## Query privacy

A denied query must not disclose hidden record counts, keys, or contents.

The provider contract defines observable ordering, timing class, and error
behavior. A portable profile should return the same safe error for absent and
inaccessible records when policy needs that protection.

Resource limits prevent broad scans. A namespace can deny list operations and
permit only exact-key reads.

## Semantic search

Semantic search is a typed optional provider interface. The interface binds:

- The embedding or retrieval profile ID.
- Input and result schemas.
- Index version.
- Data retention policy.
- Model principal when applicable.
- Data-flow policy.
- Score meaning and ordering.

The language does not define one universal similarity score. A program must not
compare scores from different profiles as if they have one scale.

## Context assembly

Context assembly converts selected memory records to a bounded prompt context.
The operation declares:

- Selection query.
- Maximum records.
- Maximum bytes and tokens.
- Required labels.
- Ordering rule.
- Redaction policy.
- Target provider policy.

The result keeps each record origin. A summary records the source record IDs and
the summarizer receipt.

## Durable workflows

PD-1 workflows can store memory record IDs and version tokens at checkpoints.
They cannot store the live memory capability unless its provider contract
defines a safe durable reference.

A resumed workflow must reacquire equal or narrower memory authority. It cannot
use an old checkpoint to restore revoked access.

## Recording and replay

Recording stores request digests, result digests, version tokens, receipts,
query order, and continuation-token commitments.

Replay does not query live mutable memory. It returns the recorded values and
verifies their schemas and receipts.

A separate audit mode can compare recorded results with current memory. That
mode is not deterministic replay.

## Security rules

- No memory namespace is ambient.
- The capability identifies exact rights and namespace.
- A resume cannot restore revoked memory access.
- Records retain PD-3 labels.
- Mutations use PD-2 and PD-4 receipts.
- Query errors follow the namespace disclosure policy.
- Prompt context checks target sink policy before disclosure.
- Provider credentials never enter source values.

## Failure cases

The runtime must reject these conditions:

- A capability names a different namespace or tenant.
- A record schema or version does not match.
- An update uses a stale version token.
- A continuation token has a different query digest.
- A query exceeds its result or work limit.
- Retrieved data loses a stored label.
- A resumed workflow requests wider memory authority.
- Replay queries live mutable memory.

## Implementation work

1. Define namespace manifests and opaque memory capabilities.
2. Define record metadata and version tokens.
3. Add standard read and mutation operations.
4. Apply PD-2 idempotency and PD-4 receipts.
5. Apply PD-3 labels to storage and retrieval.
6. Add query profiles and continuation tokens.
7. Add context assembly.
8. Extend recording, replay, redaction, and conformance tests.

## Acceptance tests

- Deny access without a namespace capability.
- Prevent one tenant from reading another tenant namespace.
- Reject an update with a stale version token.
- Retry one append without creating a duplicate record.
- Preserve labels after storage and retrieval.
- Expire access and retain the defined tombstone metadata.
- Build prompt context within exact byte and token limits.
- Replay a recorded query without live memory access.

## Open decisions

1. Which read and query operations belong in the standard profile?
2. Can a namespace capability cross a durable checkpoint?
3. Which deletion claims can the runtime verify?
4. How should a provider declare timing disclosure policy?
5. Which semantic search metadata is portable?
