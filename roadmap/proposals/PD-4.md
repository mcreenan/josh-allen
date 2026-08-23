# PD-4: authenticated actors and verifiable receipts

Status: Proposed

Depends on: None

Back to the [roadmap](../../ROADMAP.md#proposed).

## Decision summary

Add typed authenticated actors to ALLEN. Add signed receipts for approvals,
provider effects, delegation, and runtime validation.

A receipt binds one actor to one exact action. A verifier checks the receipt
against an explicit trust policy.

## Problem

The host supplies provider identity and audit metadata today. Source cannot
carry portable proof that a user, agent, model, tool, or host produced a result.

A stable session ID identifies one local session. It does not prove identity to
another host. A text name is weaker because source can construct that text.

Long workflows need evidence that remains valid after the first execution ends.
They also need exact approval scope and delegation limits.

## Scope

This proposal adds:

- Authenticated principals for users, agents, models, tools, and hosts.
- Approval receipts for exact typed subjects.
- Effect receipts for provider operations.
- Delegation certificates for restricted child authority.
- Runtime validation receipts.
- Explicit trust-store capabilities.
- Canonical signed receipt encodings.
- Receipt verification after the source execution ends.

This proposal does not expose hidden reasoning. It does not prove that model or
tool output is true.

## Terms

A principal is an authenticated actor.

Authentication proves the actor identity under one trust policy.

Authorization decides which actions that actor can perform.

A receipt is a signed record for one event.

An attestation is a signed claim from an issuer.

A trust store contains accepted trust roots and verification policy.

A subject is the exact typed value that an actor approves or acts on.

## Concrete example: GitHub release identity chain

This example starts in GitHub Actions and publishes a package to npm. It uses
AWS STS for restricted build authority and Sigstore for artifact verification.

The tool and event names are proposed ALLEN adapters.

| Proposal concept | Named service, tool, or event | Concrete use |
|---|---|---|
| Agent principal | `github_actions.oidc.subject@1` | Identify one GitHub Actions workflow run. |
| Restricted delegation | `tools.aws_sts.assume_role_with_web_identity@1` | Give the run one short AWS role session. |
| Validation receipt | `tools.sigstore.cosign.verify@1` | Verify the signed release artifact. |
| User approval | `github.deployment_review.submitted@1` | Approve one production deployment plan. |
| Host effect receipt | `tools.npm.publish@1` | Bind the host-observed npm response to the package and digest. |
| Provider receipt | `tools.sigstore.rekor.verify_inclusion@1` | Verify a signed Sigstore Rekor inclusion proof. |
| Audit event | `npm.package.visible@1` | Confirm the published package and artifact digest. |

The GitHub OIDC adapter creates `Principal<Agent>` after token validation. The
principal identifies the repository, workflow, commit, and run subject.

The AWS STS adapter returns a delegation receipt. The receipt limits the role
session to one S3 artifact prefix and one expiration time.

Sigstore verification returns `ValidationReceipt<Artifact>`. The receipt binds
the artifact digest, accepted signer, and verification policy.

The GitHub deployment review approves one exact release-plan digest. A review
for commit `4fd28aa` cannot approve commit `983bc10`.

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
let run: Principal<Agent> =
  await github_actions.authenticate_oidc(event.oidc_token)?;

let aws = await tools.aws_sts.assume_role_with_web_identity.call({
  principal: run,
  role: "arn:aws:iam::123456789012:role/release-build",
  scope: "s3://release-artifacts/josh-allen/0.2.0/",
})?;

let artifact = await tools.sigstore.cosign.verify.call({
  digest: build.artifact_digest,
  trust_policy: "josh-allen-release-v1",
})?;

let approval = await event.wait<GitHubDeploymentReview>({
  name: "github.deployment_review.submitted@1",
  subject_digest: digest(release_plan),
})?;

let published = await tools.npm.publish.call({
  approval: approval.receipt,
  artifact: artifact.verified,
  package: "josh-allen",
  version: "0.2.0",
})?;
```

The npm adapter returns a host-level effect receipt in this example. It proves
what the JOSH host observed. It does not claim that npm signed the response or
that every npm mirror already has the package.

The later `npm.package.visible@1` event supplies that separate observation. The
workflow checks its package version and artifact digest.

This chain produces three evidence levels. Sigstore Rekor supplies signed
provider evidence for its transparency-log entry. The JOSH host issues a host
receipt for the npm HTTP response that it observed. The ALLEN runtime issues a
runtime receipt after it validates `PublishResult`. These receipts make
different claims.

## Principal types

Source uses specific principal types:

```allen
Principal<User>
Principal<Agent>
Principal<Model>
Principal<Tool>
Principal<Host>
```

Illustrative future syntax in this document is not valid ALLEN `0.1`.

A principal contains a stable subject ID, issuer ID, principal kind, and key
reference. A display name is optional metadata. The display name is not the
principal identity.

Source cannot construct, change, encode as an unauthenticated value, or widen a
principal.

## Receipt types

The first profile defines these receipt classes:

| Receipt | Claim |
|---|---|
| `ApprovalReceipt<T>` | One principal approved one exact subject of type `T`. |
| `EffectReceipt<T>` | One provider performed one exact operation with result type `T`. |
| `DelegationReceipt` | One principal gave restricted authority to another principal. |
| `ValidationReceipt<T>` | One runtime applied one named validator to one exact value. |

Each receipt has a common signed envelope:

```allen
record ReceiptEnvelope {
  algorithm_profile: String
  artifact_digest: Digest
  audience: Option<PrincipalId>
  expires_at: Option<Timestamp>
  issued_at: Timestamp
  issuer: PrincipalId
  language_profile: String
  nonce: Bytes
  receipt_id: ReceiptId
  receipt_schema_digest: Digest
  signature: Signature
  subject_digest: Digest
  workflow: Option<WorkflowPosition>
}
```

The receipt payload supplies the claims for its receipt class. The signature
binds the envelope and payload.

## Approval example

Assume that a program prepares this release plan:

```text
Repository: josh-allen
Commit: 4fd28aa
Environment: production
Operation: publish version 0.2.0
```

The program calculates the canonical subject digest. It asks an authenticated
release manager to approve that subject.

```allen
record ReleaseApproval {
  artifact_digest: Digest
  commit: String
  environment: String
  operation: String
  plan_digest: Digest
}

let approval: ApprovalReceipt<ReleaseApproval> =
  await user.approve(subject)?;
```

The receipt approves that exact commit, environment, operation, and plan. A
changed commit has a different subject digest. The old receipt then fails
verification.

A plain `Bool` or a text value of `"yes"` cannot provide this guarantee.

## Effect example

Assume that a GitHub tool creates issue `418`. The tool returns the output and
an effect receipt.

```allen
record EffectOutcome<T> {
  receipt: EffectReceipt<T>
  value: T
}

let outcome: EffectOutcome<tools.github.create_issue.Output> =
  await tools.github.create_issue.call(input)?;
```

The receipt binds:

- The tool principal.
- The operation and selected version.
- The input and output schema digests.
- The request and result digests.
- The effective authority.
- The program artifact.
- The workflow position.
- The provider completion state.

PD-2 uses this receipt to prevent a duplicate mutation after a lost response.

## Delegation example

A parent agent can give a child agent restricted authority. The delegation
receipt identifies both principals.

```allen
record DelegationClaims {
  child: Principal<Agent>
  expires_at: Timestamp
  issuer: Principal<Agent>
  limits: Map<String, Int>
  permitted_effects: List<EffectId>
  permitted_resources: List<ResourceScope>
  workflow: WorkflowId
}
```

The runtime checks that the delegated authority is no greater than the parent
authority. The runtime also checks each child operation against the receipt.

For example, the GitHub Actions principal can delegate one short AWS STS role
session to a GitHub Copilot coding agent. The delegation permits reads from one
Amazon S3 log prefix and writes to branch `repair/0.2.0-build`. It does not
permit `tools.npm.publish@1`.

## Receipt issuers

The proposal defines three evidence levels:

1. A provider receipt means that the provider signed its action.
2. A host receipt means that the host observed and signed an action.
3. A runtime receipt means that the ALLEN runtime validated and recorded data.

These levels are not equal.

A JOSH host can prove that it received an Amazon Bedrock response. Only an
accepted Amazon Bedrock issuer can attest that its service produced the
response. The ALLEN runtime can prove schema validation. It cannot prove that
the response is correct.

The receipt type and issuer claims must identify the evidence level.

## Verification

Verification uses an explicit trust-store capability. A manifest request does
not grant access to a trust store.

```allen
let verified: Verified<EffectReceipt<IssueOutput>> =
  await trust.verify(receipt, policy)?;
```

Source cannot construct `Verified<T>`.

The verifier performs these steps:

1. Parse the receipt with strict size limits.
2. Validate the receipt schema.
3. Calculate its canonical digest.
4. Verify the signature.
5. Resolve the issuer under the selected trust policy.
6. Check expiration and revocation.
7. Check the subject, schema, and artifact digests.
8. Check the audience and workflow position.
9. Check the authority claims.
10. Return an opaque verified value.

## What a receipt proves

A valid receipt proves that the accepted issuer signed the recorded claims. It
also proves that the signed fields did not change.

A receipt does not prove these conditions:

- The result is factually correct.
- The provider implementation has no defect.
- The user understood the approval request.
- The signing key was never stolen.
- The provider completed internal work that the receipt does not describe.
- The receipt has a specific legal status.

The trust policy defines which issuer claims the verifier accepts.

## Privacy

A plain hash can disclose a low-entropy secret. An attacker can guess the value
and compare hashes.

Sensitive receipts use a hiding commitment. The algorithm profile defines the
commitment. The runtime applies the labels from PD-3 to receipt fields.

A redacted receipt export can remove protected content. It must preserve the
signed structure and verification status where policy permits this.

## Recording and replay

Recording stores the receipt bytes, verification policy digest, verification
result, and applicable revocation state.

Replay verifies the recorded receipt again under its replay contract. It must
not replace a provider receipt with a host receipt.

The workflow history binds receipts in order. A missing or reordered receipt
causes replay divergence.

## Security rules

- Source cannot construct an authenticated principal or signature.
- Authentication does not grant authority.
- Approval binds one exact subject and scope.
- Delegation cannot increase authority or limits.
- Verification names its trust policy and algorithm profile.
- Receipt schemas and effect schemas enter the signed claims.
- Expired or revoked credentials fail according to policy.
- Error messages do not contain signed secret content.

## Failure cases

The verifier must reject these conditions:

- The signature is invalid.
- The issuer is not trusted for the claimed actor kind.
- The receipt is expired or revoked.
- The subject digest does not match.
- The receipt names a different schema or artifact.
- The workflow position or audience does not match.
- A delegation exceeds parent authority.
- The evidence level is weaker than policy requires.
- The algorithm profile is unknown or denied.

## Implementation work

1. Define principal and receipt schemas.
2. Define canonical signed encodings.
3. Add a host-neutral trust-store provider.
4. Add opaque `Verified<T>` values to the type system and VM.
5. Add approval, effect, delegation, and validation receipts.
6. Add receipt data to JOSH provider messages.
7. Extend recording, replay, events, and redaction.
8. Add key rotation and revocation test profiles.

## Acceptance tests

- Authenticate one GitHub Actions OIDC subject as an agent principal.
- Restrict one AWS STS session to the declared S3 prefix.
- Verify one artifact through the Sigstore Cosign adapter.
- Bind a GitHub deployment review to one release-plan digest.
- Bind an npm host receipt to one package version and artifact.
- Reject an npm visibility event with a different digest.
- Reject a source-constructed principal.
- Verify a signed Sigstore Rekor provider receipt on a second host.
- Reject an approval after one subject field changes.
- Reject a child operation outside delegated authority.
- Distinguish provider, host, and runtime receipts.
- Reject an expired or revoked signing key.
- Verify a redacted receipt without exposing its secret subject.
- Reject a replay with a missing receipt.

## Open decisions

1. Which signature and commitment profiles belong in the first version?
2. Which receipt fields must source code read?
3. Which revocation state must replay preserve?
4. Can a standalone host authenticate a local user without a network issuer?
5. Which providers must sign receipts directly?
