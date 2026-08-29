# FUN-PIPE: forward pipe

Status: Implemented

Depends on: `FUN-PARTIAL`, `COL-COMBINATORS`

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FUN-PIPE](../../../docs/plans/allen-language-features-proposal.md#fun-pipe-forward-pipe).

## Decision summary

Add left-associative `|>` pipelines. The operator binds below calls,
arithmetic, and `>>`. It binds above `??`.

Without a placeholder, insert the left value as the first argument of the
right direct call. With exactly one `_` call argument, insert the value at that
position. Reject more than one pipe placeholder.

Evaluate the initial value and each pipeline stage once from left to right.
Lower the complete pipeline to nested calls with source-order temporaries.

## Compiler work

1. Add the pipeline precedence level, CST node, and editor token.
2. Distinguish one pipe placeholder from ordinary partial application.
3. Resolve each expanded direct call before lowering the next stage.
4. Preserve a source span for every operator and stage.
5. Show both the stage source and expanded call in type diagnostics.

## Acceptance contract

- A stage cannot duplicate or skip the piped value.
- Label resolution and placeholder insertion produce one exact call.
- Stage effects and early exits occur in source order.
- Tests fix precedence with nested `??`, composition, arithmetic, and calls.
- HIR and bytecode match equivalent manually nested calls.

## Out of scope

The first version does not pipe into a bare function value, property write,
assignment, or expression with no direct call.
