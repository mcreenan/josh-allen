# PAT-RANGE: range patterns

Status: Implemented

Depends on: `COL-RANGE-VALUES`

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [PAT-RANGE](../../../docs/plans/allen-language-features-proposal.md#pat-range-range-patterns).

## Decision summary

Add half-open and inclusive range patterns with compile-time endpoints. Permit
`Int`, `String`, and `Bytes` endpoints.

Compare strings by Unicode scalar sequence and bytes lexicographically. Both
orders are total and independent of locale. A range pattern binds no names.

Reject empty ranges. Diagnose an arm when an earlier unguarded arm covers its
complete range. Include range coverage in overlap and reachability checks.

## Compiler work

1. Add `literal..literal` and `literal..=literal` pattern nodes.
2. Type-check both endpoints against the scrutinee's exact scalar type.
3. Evaluate and normalize endpoints at compile time.
4. Add interval coverage to the pattern usefulness checker.
5. Lower a valid range to deterministic ordered comparisons.

## Acceptance contract

- Endpoint expressions cannot call functions or read constants with runtime
  initialization.
- `Float` range patterns remain invalid.
- The compiler evaluates the scrutinee once.
- Exhaustiveness still requires complete finite coverage or a catch-all arm.
- Tests cover boundaries, overlap, unreachable arms, half-open and inclusive
  forms, scalar ordering, invalid types, and diagnostics.

## Out of scope

The first version does not add standalone non-Boolean literal patterns,
descending ranges, custom ordered types, or binding inside a range pattern.
