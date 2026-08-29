# FUN-TRAILING-CALLBACK: trailing callback block

Status: Implemented

Depends on: `FUN-LAMBDA-SHORT`

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FUN-TRAILING-CALLBACK](../../../docs/plans/allen-language-features-proposal.md#fun-trailing-callback-trailing-callback-block).

## Decision summary

Permit one lambda after a call's closing parenthesis. The lambda becomes the
call's final argument.

The final parameter must have one exact function type. The compiler evaluates
ordinary arguments before it constructs the callback. It then lowers the form
to a normal call.

Both explicit typed closures and `FUN-LAMBDA-SHORT` concise lambdas can use the
trailing position.

## Compiler work

1. Add one optional trailing lambda to call syntax and parser recovery.
2. Pass the final parameter's function type as the lambda's expected type.
3. Insert the checked closure as the final call argument in HIR.
4. Retain separate source spans for the call and callback.
5. Reject the form before lowering when no final function parameter exists.

## Acceptance contract

- Ordinary argument evaluation order remains unchanged.
- Capture, effect, and affine ownership checks match a parenthesized callback.
- Labeled calls resolve before the compiler inserts the trailing callback.
- Diagnostics identify the final parameter and its expected function type.
- Tests compare typed, concise, generic, effectful, imported, and invalid
  trailing callbacks with ordinary calls.

## Out of scope

The first version does not accept multiple trailing callbacks or a trailing
block that is not a lambda.
