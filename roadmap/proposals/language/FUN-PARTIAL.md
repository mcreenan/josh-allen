# FUN-PARTIAL: placeholder partial application

Status: Implemented

Depends on: `FUN-LAMBDA-SHORT`, `FUN-LABELED-ARGS`

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FUN-PARTIAL](../../../docs/plans/allen-language-features-proposal.md#fun-partial-placeholder-partial-application).

## Decision summary

Permit `_` in a direct call argument. Each placeholder creates one closure
parameter in source order.

Resolve the callee before the compiler creates the closure. Every placeholder
gets the exact corresponding parameter type. Reject the form when resolution
needs an overload or another expected-type guess.

Evaluate non-placeholder arguments once when the compiler creates the partial
closure.
Those evaluations contribute their effects to closure creation. Calling the
closure contributes the original callee's effects.

## Compiler work

1. Add placeholder call arguments without changing `_` pattern syntax.
2. Resolve positional or fully labeled arguments against one named callee.
3. Store non-placeholder results in source-order temporaries.
4. Synthesize one typed closure with placeholder parameters in source order.
5. Apply current capture and affine ownership checks to stored arguments.

## Acceptance contract

- Repeated placeholders create distinct parameters.
- Nested calls cannot contain holes for the outer partial call.
- The compiler evaluates each supplied argument once.
- Diagnostics show the original call and the unresolved placeholder position.
- Tests cover multiple holes, labels, generics, effects, capture timing,
  ownership, nested expressions, and invalid callee resolution.

## Out of scope

The first version does not permit a placeholder as the callee, inside a nested
argument expression, or outside a direct call argument.
