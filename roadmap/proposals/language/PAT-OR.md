# PAT-OR: OR patterns

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [PAT-OR](../../../docs/plans/allen-language-features-proposal.md#pat-or-or-patterns).

## Decision summary

Add `left | right` alternatives inside one match arm. The operator applies
only in pattern grammar.

Every alternative must bind the same names with the same exact types and
ownership states. Alternatives test from left to right without evaluating the
scrutinee again.

The usefulness checker treats the alternatives as one arm for exhaustiveness
and treats each alternative separately for overlap and reachability.

## Compiler work

1. Add a low-precedence OR node to pattern grammar and CST data.
2. Calculate and compare the binding environment of every alternative.
3. Merge compatible bindings into one arm environment.
4. Extend usefulness and unreachable-pattern analysis across alternatives.
5. Lower alternatives to branches that share one arm expression.

## Acceptance contract

- An alternative cannot duplicate or partially move an affine value.
- Nested OR patterns obey the same binding agreement rule.
- A wildcard alternative makes later alternatives in that OR unreachable.
- Diagnostics identify the first mismatched name, type, or ownership state.
- Tests cover enums, records, Boolean patterns, nested alternatives, range
  patterns, exhaustiveness, reachability, ownership, and parser recovery.

## Out of scope

The first version does not add pattern guards, as-patterns, collection
patterns, or a general Boolean algebra for patterns.
