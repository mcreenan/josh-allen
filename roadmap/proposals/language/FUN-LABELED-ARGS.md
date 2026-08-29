# FUN-LABELED-ARGS: labeled arguments

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FUN-LABELED-ARGS](../../../docs/plans/allen-language-features-proposal.md#fun-labeled-args-labeled-arguments).

## Decision summary

Permit a direct call to name arguments with the declaration's parameter names.
The first version accepts either a fully positional call or a fully labeled
call. It rejects mixed calls.

Labels are optional at a call site. This rule preserves current positional
source. Labels do not become part of a runtime function value or function
type. A call through a function value therefore remains positional.

The compiler evaluates supplied arguments once in source order. It stores the
results in temporaries, rejects invalid labels, and then reorders the values to
declaration order.

## Compiler work

1. Add labeled call arguments to the grammar and CST.
2. Preserve labels through syntax lowering until direct-call resolution.
3. Reject unknown, duplicate, missing, and mixed arguments.
4. Record parameter names in exported call contracts for direct named calls.
5. Keep the existing runtime ABI and positional function-value representation.

## Acceptance contract

- Reordering labels does not reorder evaluation.
- Renaming a public parameter changes its source call contract and artifact
  digest.
- Imported functions expose the same labels as local functions.
- Diagnostics name the declaration and the invalid label.
- Tests cover positional compatibility, reordering, errors, generics, effects,
  imports, tools, and external declarations.

## Out of scope

The first version has no custom external label, omitted label marker, label
alias, or mixed positional and labeled call.
