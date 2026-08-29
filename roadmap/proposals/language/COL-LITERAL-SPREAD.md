# COL-LITERAL-SPREAD: list and map spread

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [COL-LITERAL-SPREAD](../../../docs/plans/allen-language-features-proposal.md#col-literal-spread-list-and-map-spread).

## Decision summary

Permit spread items anywhere in list and map literals. A list spread accepts
only `List<T>`. A map spread accepts only the literal's exact key and value
types.

Evaluate every ordinary and spread item once from left to right. Later map
entries replace earlier entries with the same key. This rule differs from a
duplicate key inside one ordinary map literal, which remains an error.

## Compiler and runtime work

1. Add spread items to list and map literal grammar and CST nodes.
2. Infer one exact element type or key and value type across all items.
3. Lower list spreads to a compiler-owned bounded builder.
4. Lower map spreads to source-order insertion with last-write-wins behavior.
5. Calculate and charge the complete result allocation before publication.

## Acceptance contract

- Empty spreads have no effect.
- Source evaluation order does not depend on canonical map key order.
- A failed item does not publish a partial collection.
- Equality and canonical encoding use the resulting ordinary list or map.
- Tests cover multiple spreads, replacement order, generics, empty values,
  traps, size limits, allocation limits, and type errors.

## Out of scope

This feature does not add record spread, iterable conversion, lazy spread, or
spread in function calls.
