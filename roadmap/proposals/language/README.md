# ALLEN language feature proposals

Status: Implemented in ALLEN 0.1.1

Editorial target: ASD-STE100 Issue 9 descriptive writing. Source code,
identifiers, file names, and feature names are project technical terms.

Back to the [roadmap](../../../ROADMAP.md#selected-language-features).

The files in this directory define the implementation contract for each
language feature. Stable feature IDs connect these proposals to the
[design inventory](../../../docs/plans/allen-language-features-proposal.md),
the roadmap, and `TODO.md`.

## Status policy

`Selected` records a product choice. It does not make the syntax current.

Change a proposal to `Accepted` only after it has no open semantic decisions.
Change it to `Implemented` only when the complete current-language change
lands.

## Shared completion gate

Every feature change must update these parts together:

1. Lexer, grammar, CST, parser recovery, and generated syntax data.
2. Name resolution, type checking, ownership checks, and HIR lowering.
3. Bytecode, runtime, artifacts, canonical values, and replay when affected.
4. Positive, negative, limit, diagnostic, and regression tests.
5. Conformance data, editor grammar, examples, and package fixtures.
6. The human language specification and both agent language references.

The compiler must retain the source span through any sugar expansion.
Diagnostics must describe the source form, not only the lowered form.

## Delivery batches

Batch L1 adds foundations with no selected-feature dependencies. Batch L2
uses those foundations for call and data sugar. Batch L3 adds new values,
pattern rules, local compiler identities, and affine sequences.

The [roadmap table](../../../ROADMAP.md#selected-language-features) is the
authoritative dependency list.
