# COL-SLICES: bracket slicing for lists, bytes, and strings

Status: Implemented

Depends on: `COL-RANGE-VALUES`

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [COL-SLICES](../../../docs/plans/allen-language-features-proposal.md#col-slices-bracket-slicing-for-lists-bytes-and-strings).

## Decision summary

Permit a half-open `Range<Int>` inside index brackets. A slice returns
`Option<List<T>>`, `Option<Bytes>`, or `Option<String>` for the corresponding
receiver.

Return `None` when a bound is negative, the start exceeds the end, or the end
exceeds the receiver length. Empty valid ranges return `Some` with an empty
value.

String bounds count Unicode scalar values. This matches the current
`string.slice` operation.

## Compiler and runtime work

1. Distinguish ordinary index and range slice expressions in postfix syntax.
2. Require a half-open `Range<Int>` slice operand.
3. Evaluate the receiver before both range bounds and evaluate each once.
4. Add safe list and bytes slice operations and reuse string slice semantics.
5. Charge the fresh result allocation before copying any content.

## Acceptance contract

- Slicing never traps for an invalid bound.
- The result is a fresh immutable value, not a borrowed view.
- Inclusive ranges produce a type or slice-form diagnostic.
- The existing `string.slice` function stays available and equivalent.
- Tests cover every valid boundary, invalid bounds, empty values, Unicode,
  generic lists, allocation limits, precedence, and diagnostics.

## Out of scope

The first version does not add borrowed views, assignment through slices,
open-ended slices, negative-from-end indexes, or stepped slices.
