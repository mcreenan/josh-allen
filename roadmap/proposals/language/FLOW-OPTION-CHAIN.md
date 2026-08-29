# FLOW-OPTION-CHAIN: optional member and call chaining

Status: Implemented

Depends on: `FLOW-OPTION-QUESTION`

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FLOW-OPTION-CHAIN](../../../docs/plans/allen-language-features-proposal.md#flow-option-chain-optional-member-and-call-chaining).

## Decision summary

Add `?.` before a field access or extension-style call on an `Option<T>`
receiver. Evaluate the receiver once. Stop the remaining optional chain when
the receiver is `None`.

Wrap a successful non-`Option` result in `Some`. Keep an `Option<U>` result as
one layer instead of creating `Option<Option<U>>`.

Each later optional access must include its own `?.`. Ordinary `.` keeps its
current meaning and does not unwrap an `Option`.

## Compiler work

1. Add optional postfix field and call nodes without changing ordinary `.`.
2. Require an exact `Option<T>` receiver at each optional step.
3. Lower a chain to nested `match` expressions with one receiver temporary per
   step.
4. Flatten one layer when the selected member returns `Option<U>`.
5. Preserve member, call, and optional-operator spans in diagnostics.

## Acceptance contract

- The compiler skips later arguments and effects after `None`.
- Field and extension-call resolution uses the unwrapped static type.
- Ownership joins match an explicit nested `match`.
- Tests cover fields, calls, flattening, multiple steps, effects, affine
  rejection, precedence, recovery, and invalid receiver types.

## Out of scope

The first version does not add forced unwrap, optional assignment, automatic
chaining through ordinary `.`, or `Option` to `Result` conversion.
