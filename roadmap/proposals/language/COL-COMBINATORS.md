# COL-COMBINATORS: eager list transform and predicate combinators

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [COL-COMBINATORS](../../../docs/plans/allen-language-features-proposal.md#col-combinators-eager-list-transform-and-predicate-combinators).

## Decision summary

Add `list.map`, `list.filter`, `list.flat_map`, `list.filter_map`, `list.find`,
`list.any`, `list.all`, `list.partition`, and `list.scan`. Keep `list.fold`
unchanged.

The first version accepts pure callbacks only. The current type system cannot
propagate an arbitrary callback effect set through a generic library function.

Every operation visits input from left to right. `find` and `any` stop at the
first success. `all` stops at the first failure. `scan` returns each
post-callback accumulator and excludes the initial value.

## Result contracts

- `map` and `flat_map` return a new list in input order.
- `filter` and `filter_map` retain input order.
- `find` returns the first match as `Option<T>`.
- `any` and `all` return `Bool` and short-circuit.
- `partition` returns `{ matched: List<T>, rest: List<T> }`.
- `scan` returns `List<A>` for an initial `A` and callback `(A, T) -> A`.

## Compiler and runtime work

1. Register exact generic signatures for all nine operations.
2. Type-check each callback against the operation's exact pure function type.
3. Add bytecode or intrinsic support with deterministic instruction charging.
4. Charge complete output allocation before publishing a result.
5. Preserve current list element and affine-value restrictions.

## Acceptance contract

Tests cover empty lists, order, short-circuiting, callback traps, generic
instantiation, allocation limits, instruction limits, NaN values, and nested
lists. Reference and conformance data must list every new operation.

## Out of scope

The first version does not add effectful callbacks, lazy evaluation, parallel
collection operations, or changes to `list.fold`.
