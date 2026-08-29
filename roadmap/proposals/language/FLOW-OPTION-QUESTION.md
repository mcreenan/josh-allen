# FLOW-OPTION-QUESTION: postfix question operator for Option

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FLOW-OPTION-QUESTION](../../../docs/plans/allen-language-features-proposal.md#flow-option-question-postfix-question-operator-for-option).

## Decision summary

Extend postfix `?` to `Option<T>` inside a function that returns `Option<U>`.
`Some(value)?` produces `value`. `None?` returns `None` from the current
function.

Do not convert between `Option` and `Result`. Existing `Result<T, E>`
propagation keeps its exact error-type rule.

## Compiler work

1. Extend postfix propagation type checking with the `Option` case.
2. Reuse the current early-return HIR with an `Option` return constructor.
3. Retain postfix precedence and the operand source span.
4. Reject `Option` propagation in non-`Option` functions and closures.
5. Keep ownership joins identical to explicit `match` lowering.

## Acceptance contract

- The compiler evaluates the operand once.
- `None` skips all remaining expressions in the current function.
- An async function still returns its declared `Future<Option<U>>` result.
- Diagnostics distinguish an invalid container from an invalid function
  return type.
- Tests compare the feature with an explicit `match` for values, effects,
  ownership, bytecode, limits, and replay.

## Out of scope

This feature does not add forced unwrap, automatic `Option` to `Result`
conversion, or propagation through an enclosing callback.
