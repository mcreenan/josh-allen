# PD-9: provider-independent model policy

Status: Proposed

Depends on: [PD-3](PD-3.md), [PD-4](PD-4.md), [PD-6](PD-6.md), [PD-7](PD-7.md)

Back to the [roadmap](../../ROADMAP.md#proposed).

## Decision summary

Let source declare model requirements for cost, time, privacy, content types,
context size, and review. Match those requirements before prompt disclosure.

Return verified model identity, usage, policy match, and validation data with
each model result.

## Problem

ALLEN `0.1` sends a typed prompt to a model provider. Prompt policy controls a
small response-attempt limit. The host selects the model.

Source cannot require a local provider, set a total cost, require image input,
or request independent review through one portable contract.

Provider-specific model names change often. Source code should state the work
requirements instead of one mutable provider product name.

## Scope

This proposal adds requirements for:

- Total and per-request cost.
- Input and output token limits.
- Per-attempt and total time.
- Privacy, retention, and training policy.
- Context capacity.
- Supported artifact kinds.
- Required response schema support.
- Provider and model identity policy.
- Fallback and independent review.
- Usage and validation receipts.

The proposal does not define model quality as one numeric value. It does not
treat model confidence as proof.

## Terms

A model requirement states conditions that a selected model must meet.

A model route is the selected provider and model under one requirement.

A fallback is a second route after a documented failure.

An evaluation plan checks a model result through declared review operations.

Usage records the resources that one request consumed.

## Concrete example: Zendesk support routing

This example drafts a response for one Zendesk ticket. The ticket can contain
private customer data and a screenshot.

The host offers Amazon Bedrock, Google Vertex AI, and a local Ollama service.
The exact model principals come from the frozen host catalog.

The tool, event, and route names are proposed ALLEN adapters.

| Proposal concept | Named service, tool, or event | Concrete use |
|---|---|---|
| Start event | `zendesk.ticket.created@1` | Start one response workflow. |
| Private route | `models.ollama.local@1` | Process restricted tickets on the local host. |
| Primary public route | `models.amazon_bedrock.anthropic@1` | Draft a public documentation response. |
| Fallback route | `models.google_vertex_ai.gemini@1` | Continue after one listed Bedrock error. |
| Image input | `tools.aws_s3.get_artifact_reference@1` | Supply the Zendesk screenshot by reference. |
| Independent review | Bedrock and Vertex AI principals | Require two separate typed review results. |
| Private approval | `zendesk.manager_approval.submitted@1` | Approve one local Ollama draft without remote disclosure. |
| Final sink | `tools.zendesk.tickets.add_comment@1` | Add the approved response to the ticket. |

The proposed adapter names do not fix a vendor model name in the language.
Preflight binds each route to one exact provider and model principal.

Tickets with `Private` data use only the local Ollama route. The host rejects
Bedrock and Vertex AI before it discloses the prompt.

Public documentation tickets can use Bedrock with Vertex AI as a fallback. The
fallback uses the remaining workflow cost and time budget.

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
let policy = match ticket.data_class {
  Private => ModelPolicy {
    accepted_routes: ["models.ollama.local@1"],
    retention: Retention.None,
    total_cost_microunits: 0,
  },
  Public => ModelPolicy {
    accepted_routes: [
      "models.amazon_bedrock.anthropic@1",
      "models.google_vertex_ai.gemini@1",
    ],
    retention: Retention.None,
    total_cost_microunits: 300000,
  },
};

let draft = await model.with_fallback(make_prompt(ticket), policy)?;

let approved = match ticket.data_class {
  Private => await event.wait<ZendeskManagerApproval>({
    name: "zendesk.manager_approval.submitted@1",
    subject_digest: digest(draft),
  })?,
  Public => await model.quorum({
    independence: PrincipalPolicy.SeparateProviders,
    required: 2,
    request: review_prompt(draft),
    routes: [
      "models.amazon_bedrock.anthropic@1",
      "models.google_vertex_ai.gemini@1",
    ],
  })?,
};

await tools.zendesk.tickets.add_comment.call({
  body: approved_response(draft, approved),
  ticket_id: ticket.id,
})?;
```

The screenshot stays in Amazon S3. The selected route receives a typed artifact
reference only when it accepts image input and the ticket labels.

If Bedrock returns a listed temporary error, the workflow can select Vertex AI.
It cannot use Vertex AI after a privacy-policy failure.

Public tickets get two review results with separate provider principals and
receipts. Private tickets wait for one Zendesk manager event. A model
confidence value remains untrusted and does not count as a review.

## Requirement type

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
let policy: ModelPolicy = {
  artifacts: [ArtifactKind.Image],
  context_tokens_min: 32000,
  cost_microunits_max: 250000,
  input_labels: [Private],
  latency_ms_max: 20000,
  retention: Retention.None,
  training_use: TrainingUse.Denied,
};

let result = await model.request(request, policy)?;
```

The language uses integer cost units. It does not use binary floating-point for
money.

The policy can name a currency and a price-schedule digest. The host reports an
unavailable result when it cannot prove the requested cost bound.

## Match before disclosure

The host matches policy before it sends any prompt part to a provider.

The match checks:

- Provider and model principal under PD-4.
- Privacy and retention policy.
- Training-use policy.
- Context and artifact support.
- Schema-output support.
- Cost and time bounds.
- Data-flow sink rules from PD-3.
- Required stream support from PD-7.

The host cannot send a test prompt to check compatibility. Such a prompt would
already disclose data.

## Identity and metadata

A model result includes:

- Provider principal.
- Model principal and version claim.
- Policy-match digest.
- Prompt schema and prompt commitment.
- Response schema and response digest.
- Attempt count.
- Input and output usage.
- Cost under the selected price schedule.
- Validation result.
- Provider or host receipt level.

The result can omit protected prompt data. It must keep the commitment needed
for verification.

## Cost policy

A cost policy sets a maximum for one request and for the parent workflow.

The runtime reserves the maximum cost before dispatch. It releases unused
reservation after a verified usage result.

A fallback attempt uses the remaining total budget. It does not receive a new
full budget.

If the provider cannot give a verifiable final cost, policy can require a host
upper bound or deny the route.

Price changes create a new price-schedule digest. Replay uses the recorded
schedule. It does not apply current prices to an old request.

## Privacy policy

The policy can require:

- Local processing.
- Accepted provider principals.
- No provider retention.
- No training use.
- A geographic or administrative trust domain.
- Accepted confidentiality labels.

These claims need provider or host attestations under PD-4. A source string that
says `no retention` is not sufficient proof.

PD-3 applies the strictest rule from the value label, source policy, host
policy, and selected provider.

## Fallback

A fallback plan lists requirement sets. It does not need to list provider
product names.

```allen
let result = await model.with_fallback(
  request,
  [primary_policy, reduced_policy],
)?;
```

The reduced policy cannot weaken a hard privacy or confidentiality rule. It can
reduce context size, artifact support, time, or cost when source declares that
change.

Source lists the exact error codes that permit fallback. Validation failure can
permit fallback. A policy denial does not permit silent fallback to a weaker
privacy rule.

## Independent review

An evaluation plan can request a second model, an agent, a tool, or a user to
review one result.

Each reviewer has a separate principal, prompt, effects, limits, and receipt.
The original model must not select its own reviewer when independence is
required.

The plan states a deterministic aggregation rule. Examples include unanimous
approval, majority over a fixed set, or one human approval after model review.

The aggregation rule cannot infer correctness from model confidence text.

## Quorum

PD-6 defines task quorum behavior. Model quorum adds these rules:

- Each model route meets the base privacy policy.
- Independence policy defines accepted principal relationships.
- Every response uses the same subject and output schema.
- The workflow retains every response and receipt.
- A pure deterministic function combines typed review decisions.

A quorum result records disagreement. It must not remove minority results from
the audit data.

## Streams and artifacts

PD-7 defines stream and artifact behavior. A model policy lists accepted input
and output artifact kinds.

A provider cannot convert an image to text without an explicit conversion
operation. The conversion has its own principal, policy, and receipt.

A streaming response remains partial until final schema validation completes.

## Recording and replay

Recording stores:

- The complete requirement digest.
- Selected provider and model principals.
- Price-schedule digest.
- Policy attestations.
- Prompt and response commitments.
- Usage and cost.
- Fallback decisions.
- Review and quorum receipts.
- Final validation result.

Replay returns recorded model results. It does not call a live model. Replay
verifies selection, policy, attempts, and aggregation.

## Security rules

- Policy matching occurs before prompt disclosure.
- The host cannot weaken a hard source bound.
- Fallback uses the remaining total budget.
- Model identity and policy claims use PD-4 receipts.
- Prompt and response values keep PD-3 labels.
- A reviewer has separate authority and identity.
- Model confidence remains untrusted data.
- Provider-specific fields use namespaced extensions.

## Failure cases

The runtime returns a typed unavailable result or rejects these conditions:

- No route satisfies all hard requirements.
- A provider cannot prove a required privacy claim.
- The prompt exceeds the selected context limit.
- The selected model cannot accept one artifact kind.
- Cost reservation exceeds the remaining workflow budget.
- A fallback weakens a hard policy.
- An independence rule selects related principals.
- Replay uses a different policy or price schedule.

## Implementation work

1. Define portable model requirement fields and canonical digests.
2. Add provider and model principal claims from PD-4.
3. Add pre-disclosure matching.
4. Add cost reservation and usage records.
5. Add privacy and data-flow checks from PD-3.
6. Add fallback through PD-6.
7. Add stream and artifact matching from PD-7.
8. Add independent review, recording, replay, and conformance tests.

## Acceptance tests

- Start one workflow from `zendesk.ticket.created@1`.
- Route a private ticket only to local Ollama.
- Reject Bedrock and Vertex AI before private prompt disclosure.
- Use Vertex AI after one listed Bedrock temporary error.
- Keep both routes inside one total cost budget.
- Require separate Bedrock and Vertex AI review principals.
- Send only the approved response to the Zendesk comment tool.
- Select a model that meets context, artifact, and privacy requirements.
- Reject all routes before prompt disclosure when privacy proof is absent.
- Keep fallback attempts within one total cost budget.
- Reject a fallback that weakens retention policy.
- Record exact provider and model principals.
- Run an independent review with a separate principal.
- Preserve all quorum results and receipts.
- Replay selection and aggregation without a live model.

## Open decisions

1. Which cost unit and price-schedule format are portable?
2. Which privacy claims require provider signatures?
3. How does policy express model-family independence?
4. Which requirement fields are hard bounds and which allow fallback changes?
5. Can hosts report model capability without disclosing hidden catalog data?
