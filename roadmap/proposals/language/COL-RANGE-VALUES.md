# COL-RANGE-VALUES: first-class ranges

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [COL-RANGE-VALUES](../../../docs/plans/allen-language-features-proposal.md#col-range-values-first-class-ranges).

## Decision summary

Add immutable `Range<Int>` values. `start..end` creates a half-open range.
`start..=end` creates an inclusive range.

The first version supports ascending ranges only. A start greater than the end
creates an empty range. A half-open range with equal bounds is empty. An
inclusive range with equal bounds contains one value.

A range stores its start, end, and inclusive flag. Equality compares those
three fields. Range values cannot cross an entry, tool, prompt, or package
data boundary until a later proposal fixes their JSON schema.

## Compiler and runtime work

1. Move range parsing from the special `for` clause into expression grammar.
2. Add the `..=` token and a nonassociative range precedence level.
3. Add `Range<Int>` to semantic types, HIR, bytecode, and runtime values.
4. Make `for` consume a range value while preserving current half-open loops.
5. Stop inclusive iteration after it yields `Int::MAX` without incrementing.

## Acceptance contract

- The compiler evaluates each bound once from left to right.
- A range never wraps on an integer boundary.
- Nested or chained range operators require parentheses or produce an error.
- Artifacts can contain range constants only after a proposal fixes their
  encoding.
- Tests cover empty, singleton, inclusive, `Int::MIN`, `Int::MAX`, loops,
  equality, invalid types, precedence, limits, and boundary rejection.

## Out of scope

The first version does not add descending, stepped, open-ended, floating-point,
character, or generic ranges.
