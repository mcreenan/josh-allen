# PD-2: idempotent and transactional effects

Status: Proposed

Depends on: [PD-4](PD-4.md)

Back to the [roadmap](../../ROADMAP.md#proposed).

## Decision summary

Add mutation semantics to effect contracts. Add typed effect receipts and
stable idempotency keys. Define safe behavior for retries, transactions,
compensation, and ambiguous results.

## Problem

A provider can commit a mutation and lose its response. The caller sees no
result. An ordinary retry can then repeat the mutation.

This failure can create two issues, two payments, or two release records. The
runtime cannot solve the problem with local replay. The external provider has
already changed its state.

ALLEN must describe the retry behavior of each external effect. The runtime
must stop when it cannot select a safe action.

## Scope

This proposal adds five effect classes:

| Class | Meaning |
|---|---|
| `read_only` | The operation does not intentionally change external state. |
| `idempotent` | The provider accepts a stable key and returns the prior result for a duplicate request. |
| `transactional` | The provider supports prepare, commit, and abort. |
| `compensatable` | The provider defines a separate typed action that can offset a committed action. |
| `at_most_once` | The runtime must not retry after an ambiguous result. |

An operation can have more than one applicable property. For example, a
transaction commit can also be idempotent.

## Terms

An idempotency key identifies one intended mutation.

A request digest identifies the exact request data.

An effect receipt contains signed evidence for one provider operation.

An ambiguous result means that the caller cannot determine if the provider
committed the operation.

Compensation is a new effect. Compensation does not remove the first effect
from history.

## Concrete example: Stripe checkout

This example processes one Shopify order. It uses Stripe for payment and
PostgreSQL for the local order record.

The tool and event names are proposed ALLEN adapters. They do not claim that
the external services use these ALLEN names.

| Proposal concept | Named service, tool, or event | Concrete use |
|---|---|---|
| Read-only effect | `tools.stripe.payment_intents.retrieve@1` | Read the current Stripe payment state. |
| Idempotent effect | `tools.stripe.payment_intents.create@1` | Create one payment intent for one Shopify order. |
| Completion event | `stripe.payment_intent.succeeded@1` | Resume the workflow after Stripe confirms payment. |
| Transactional effect | `tools.postgresql.orders.commit@1` | Commit the order row and payment receipt together. |
| Compensatable effect | `tools.stripe.refunds.create@1` | Refund a captured payment after inventory failure. |
| At-most-once effect | `tools.sendgrid.mail.send@1` | Send one receipt when the adapter cannot prove deduplication. |
| Reconciliation | `tools.stripe.payment_intents.retrieve@1` | Query Stripe after a lost create response. |

The workflow receives `shopify.order.created@1` with order ID `S-1042`. It
derives `shopify:S-1042:payment` as the payment idempotency key.

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
let payment = await effect.idempotent(
  key: "shopify:S-1042:payment",
  call: tools.stripe.payment_intents.create.call({
    amount_minor: 12900,
    currency: "USD",
    order_id: "S-1042",
  }),
)?;

checkpoint {
  payment_id: payment.value.id,
  payment_receipt: payment.receipt,
};

let paid = await event.wait<StripePaymentIntentSucceeded>({
  correlation: payment.value.id,
  name: "stripe.payment_intent.succeeded@1",
})?;

await tools.postgresql.orders.commit.call({
  order_id: "S-1042",
  payment_receipt: paid.receipt,
})?;
```

If the create response does not arrive, the runtime keeps the same key. The
Stripe adapter returns the prior payment intent or queries it by stored data.

If inventory allocation later fails, source calls
`tools.stripe.refunds.create@1`. The refund receipt does not remove the payment
receipt. Workflow history contains both actions.

The SendGrid adapter uses `at_most_once` in this example. The adapter selected
this class because it cannot prove safe retry behavior for the configured send
operation. An ambiguous SendGrid result stops automatic retry.

The PostgreSQL adapter exposes a typed transaction operation. It commits the
order row and payment receipt in one database transaction. It does not claim a
distributed transaction with Stripe.

## Tool contract changes

Each state-changing tool operation declares its effect class. It also declares
its key format and reconciliation operation when applicable.

The frozen tool schema binds these declarations to the selected tool version.
The compiler includes the declaration digest in the artifact.

Illustrative tool metadata follows:

```json
{
  "effect_class": "idempotent",
  "idempotency_key": "required",
  "operation": "stripe.payment_intents.create",
  "reconcile": "stripe.payment_intents.retrieve",
  "version": "1.0.0"
}
```

The host cannot change this metadata after program load.

## Source contract

Illustrative future syntax follows. This syntax is not valid ALLEN `0.1`.

```allen
let outcome = await effect.idempotent(
  key: "shopify:S-1042:payment",
  call: tools.stripe.payment_intents.create.call({
    amount_minor: 12900,
    currency: "USD",
    order_id: "S-1042",
  }),
)?;

checkpoint {
  payment_receipt: outcome.receipt,
};
```

The runtime calculates the request digest before dispatch. It sends the key and
digest to the provider.

The provider must return the prior result for the same key and digest. It must
reject the same key with a different digest.

## Receipt data

PD-4 defines receipt verification. This proposal requires these effect claims:

- The selected operation and version.
- The effect class.
- The actor principal.
- The idempotency key digest.
- The request schema and request digest.
- The result schema and result digest.
- The completion state.
- The provider sequence or transaction ID.
- The workflow identity and position when available.

The receipt can contain a commitment instead of a plain digest for sensitive
input. PD-3 defines the related data labels.

## Retry rules

A retry policy lists exact error codes. The runtime rejects an unknown or
nonretryable code in that policy.

For a `read_only` operation, the runtime can retry after a documented temporary
error. The result can change because the external source can change.

For an `idempotent` operation, every retry uses the same key and request digest.

For a `transactional` operation, the runtime queries transaction state before
it repeats a prepare or commit request.

For a `compensatable` operation, the program selects compensation explicitly.
The runtime does not run compensation automatically after an unrelated error.

For an `at_most_once` operation, an absent final result creates an ambiguous
result. The runtime stops automatic progress and requests reconciliation.

## Transaction rules

A transaction has explicit states:

```text
new -> prepared -> committed
               -> aborted
```

The provider returns a signed receipt for each transition. A duplicate request
for the same transition returns the prior receipt.

The runtime does not provide a distributed transaction across unrelated tools.
Such a guarantee would be false for most providers.

A workflow can coordinate multiple tools with compensation. The workflow must
record each committed action before it starts the next dependent action.

## Compensation rules

The tool schema defines the compensation input and output. It also identifies
the original receipt type that it accepts.

Illustrative future syntax follows:

```allen
let payment = await tools.stripe.payment_intents.retrieve.call({
  payment_intent_id,
})?;

match await tools.stripe.refunds.create.call({
  amount_minor: payment.value.amount_received,
  original: payment_creation_receipt,
  payment_intent_id: payment.value.id,
}) {
  Ok(refund) => Ok(refund),
  Err(error) => Err(error),
}
```

Compensation can fail. A failed compensation remains visible in workflow
history. The program must not describe the original action as undone until the
provider confirms compensation.

## Ambiguous results

An ambiguous result needs a separate source-visible state. It is not an
ordinary provider `Err` because the mutation can exist.

The result must contain safe reconciliation data. It must not contain a claim
that the operation failed.

The program can do one of these actions:

- Call the declared reconciliation operation.
- Ask an authenticated actor to resolve the state.
- Stop the workflow for external repair.

The program must not retry with a new key unless policy approves a new intended
mutation.

## Recording and replay

Recording stores the effect class, key commitment, request digest, receipt, and
completion state.

Replay validates these values. Replay never dispatches the live mutation.

A replay report must distinguish a recorded provider action from a live
provider action. The report must also identify an ambiguous state.

## Security rules

- Source cannot alter a verified effect class.
- The runtime binds the key to the request digest.
- The provider identity enters the receipt.
- A retry cannot select a different provider without an explicit migration.
- A compensation call needs its own effect and authority.
- Sensitive keys and requests use commitments that follow PD-3.
- A host cannot convert `at_most_once` to `idempotent`.

## Failure cases

The runtime must reject these conditions:

- One key has two request digests.
- A receipt names a different operation or schema.
- A provider returns an impossible transaction transition.
- A retry changes the selected tool identity.
- A compensation receipt does not bind the original receipt.
- A replay omits an ambiguous result.

## Implementation work

1. Add effect classes to tool schemas and generated operations.
2. Add idempotency keys and commitments to provider requests.
3. Add typed effect outcomes and ambiguous results.
4. Add receipt checks from PD-4.
5. Add retry and reconciliation instructions.
6. Add transaction-state validation.
7. Extend recording and replay.
8. Add crash tests before and after provider commit.

## Acceptance tests

- Create one Stripe payment intent for Shopify order `S-1042`.
- Reuse the same Stripe key after a lost provider response.
- Resume from `stripe.payment_intent.succeeded@1` once.
- Commit one PostgreSQL order row with the payment receipt.
- Record a Stripe refund as a separate compensating effect.
- Stop after an ambiguous SendGrid send result.
- Lose the first provider response and retry with the same key.
- Confirm that the provider returns one mutation and one result.
- Reject the same key with changed input.
- Resume a transaction when the prepare response does not arrive.
- Record a failed compensation without hiding the first action.
- Stop automatic progress after an ambiguous `at_most_once` result.
- Reject a retry policy that names a nonretryable error.

## Open decisions

1. Does the language define effect classes, or does the tool schema define all
   classes?
2. Which commitment format protects low-entropy request data?
3. How long must a provider retain idempotency records?
4. Can a tool version change its effect class within one major version?
5. Which reconciliation results can restore automatic progress?
