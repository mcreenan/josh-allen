# FUN-COMPOSE: function composition

Status: Implemented

Depends on: `FUN-LAMBDA-SHORT`

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FUN-COMPOSE](../../../docs/plans/allen-language-features-proposal.md#fun-compose-function-composition).

## Decision summary

Add left-associative `>>` composition for unary function values. The operator
binds below calls and arithmetic and above `|>` and `??`.

For `f >> g`, the result type of `f` must exactly match the input type of `g`.
The result is the typed closure `fn(value) { g(f(value)) }`.

The composed closure carries the union of both declared effect sets. It cannot
capture an affine function value.

## Compiler work

1. Add the composition precedence level, CST node, and editor token.
2. Require exact unary function types on both operands.
3. Construct typed closure HIR with source spans for both calls.
4. Calculate the closed union of both effect sets.
5. Reject affine captures and nonconcrete intermediate types.

## Acceptance contract

- The compiler evaluates each operand once when it creates the closure.
- A composed call evaluates `f` before `g`.
- Left association has the same value result as explicit nested composition.
- Diagnostics name the incompatible intermediate types.
- Tests cover pure and effectful functions, generics after instantiation,
  association, precedence, ownership, limits, and source spans.

## Out of scope

The first version does not add reverse composition, automatic tuple spreading,
overload selection, or implicit conversions.
