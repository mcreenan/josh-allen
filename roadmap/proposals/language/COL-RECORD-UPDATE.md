# COL-RECORD-UPDATE: immutable record update

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [COL-RECORD-UPDATE](../../../docs/plans/allen-language-features-proposal.md#col-record-update-immutable-record-update).

## Decision summary

Add one base spread at the start of a record constructor. Explicit fields that
follow the base replace fields with the same names.

Evaluate the base once. Then evaluate replacement fields once from left to
right. Require the base and result to have the same exact nominal or structural
record type.

Reject unknown fields, repeated replacement fields, a missing base, and more
than one base spread.

## Compiler work

1. Add a leading record-update base to named and anonymous record syntax.
2. Resolve the base type before checking replacement fields.
3. Lower to one base temporary and one fresh complete record value.
4. Reuse record invariant checks after construction.
5. Charge complete allocation before publishing the new value.

## Acceptance contract

- The source record remains unchanged.
- Replacement evaluation order matches its written order.
- A trap or failed invariant does not publish a partial record.
- Record identity, equality, encoding, and schema remain unchanged.
- Tests cover nominal and structural records, shorthand fields, invariants,
  evaluation order, allocation limits, and all rejected forms.

## Out of scope

This feature does not add nested update paths, mutation, optional fields, or a
record spread that changes the result type.
