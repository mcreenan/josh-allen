# LIT-RAW-STRING: raw string literals

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [LIT-RAW-STRING](../../../docs/plans/allen-language-features-proposal.md#lit-raw-string-raw-string-literals).

## Decision summary

Add raw string literals with `r"..."` and hash-counted delimiters such as
`r#"..."#`. Raw strings perform no escape decoding and no interpolation.

Permit zero through 16 delimiter hashes. The opening and closing hash counts
must match. A raw string can contain line breaks and preserves its content
after the source loader normalizes line endings.

## Compiler work

1. Add raw-string token recognition before identifier and ordinary-string
   recognition.
2. Scan for the exact quote and hash closing delimiter without recursion.
3. Store the decoded value as the enclosed source scalars without another
   escape or template pass.
4. Point an unterminated-literal diagnostic at the opening token and include
   the required closing delimiter.
5. Add editor grammar support for every accepted delimiter count.

## Acceptance contract

- `${name}`, backslashes, and quotes remain literal content.
- A mismatched hash count does not close the literal.
- The lexer enforces the source-size limit while it scans.
- Canonical string encoding remains unchanged after lexing.
- Tests cover every delimiter boundary, multiline content, invalid UTF-8
  source handling, recovery, and adjacent tokens.

## Out of scope

This feature does not trim indentation or change ordinary string and template
escape rules. `LIT-MULTILINE` owns indentation trimming.
