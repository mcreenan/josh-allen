# LIT-MULTILINE: indentation-trimmed multiline strings

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [LIT-MULTILINE](../../../docs/plans/allen-language-features-proposal.md#lit-multiline-indentation-trimmed-multiline-strings).

## Decision summary

Add triple-quoted multiline strings. They use the current string escapes and
`${expression}` interpolation rules.

A line break must follow the opening delimiter. The closing delimiter must
start on its own line. Its indentation sets the maximum indentation that the
compiler can remove.

After line-ending normalization, remove the longest common indentation from
all nonblank content lines. Ignore blank lines when calculating that prefix.
Remove one initial and one final delimiter-adjacent line break.

## Compiler work

1. Add multiline string and template tokens with bounded iterative scanning.
2. Reuse the current escape and interpolation parser inside the new delimiter.
3. Calculate indentation from source text before interpolation lowering.
4. Preserve source offsets for each content and interpolation segment.
5. Add editor grammar and recovery for missing closing delimiters.

## Acceptance contract

- Tabs and spaces are distinct indentation scalars.
- A nonblank line with less closing indentation produces a diagnostic.
- Interpolation evaluation order matches the current template rules.
- The output contains LF line breaks after normalization.
- Tests cover blank lines, mixed indentation, escapes, interpolation,
  empty content, size limits, and malformed delimiters.

## Out of scope

This feature does not add raw multiline semantics. Use `LIT-RAW-STRING` when
content must disable both escapes and interpolation.
