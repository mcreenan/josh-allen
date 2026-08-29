# FUN-LAMBDA-SHORT: concise inferred lambda

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FUN-LAMBDA-SHORT](../../../docs/plans/allen-language-features-proposal.md#fun-lambda-short-concise-inferred-lambda).

## Decision summary

Add `fn(x) => expression` for a lambda with one expression body. Infer its
parameter types, result type, and effect set from one exact expected function
type.

Reject the concise form when the surrounding expression has no exact expected
function type. Do not use argument names, overload search, or return-flow
analysis to guess a type.

Lower the form to the current typed closure representation before ordinary
closure checking.

## Compiler work

1. Add untyped lambda parameters and an arrow body to the grammar and CST.
2. Pass an expected function type into lambda type checking.
3. Synthesize the current explicit closure HIR with the original source spans.
4. Apply the existing capture, ownership, and effect rules to the result.
5. Report the missing or ambiguous expected type at the concise lambda.

## Acceptance contract

- Every parameter and the result receive one exact type before HIR lowering.
- The feature does not infer public function declarations.
- An effectful body must match the expected callback effect set exactly.
- Affine captures obey the current closure rules.
- Tests cover nested lambdas, generic calls, missing context, effect mismatch,
  ownership errors, recovery, and diagnostics.

## Out of scope

The first version does not add implicit single-parameter names, tuple shorthand,
pattern parameters, or multi-statement concise bodies.
