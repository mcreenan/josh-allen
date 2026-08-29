# COL-LAZY-SEQUENCE: lazy sequences or iterators

Status: Implemented

Depends on: `COL-COMBINATORS`

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [COL-LAZY-SEQUENCE](../../../docs/plans/allen-language-features-proposal.md#col-lazy-sequence-lazy-sequences-or-iterators).

## Decision summary

Add affine, single-pass `Sequence<T>` values. Sequence adapters are lazy.
Terminal operations consume the sequence.

The first version adds `seq.from_list`, `seq.map`, `seq.filter`, `seq.take`,
`seq.find`, `seq.any`, `seq.all`, `seq.fold`, and `seq.to_list`. Callbacks are
pure until the language can propagate a generic callback effect set.

A sequence cannot cross an entry, tool, prompt, package-data, canonical
encoding, or replay boundary. A program can move it but cannot copy or consume
it twice.

## Compiler and runtime work

1. Add `Sequence<T>` to the type system with affine ownership rules.
2. Represent adapters as bounded iterator state without materializing results.
3. Charge instructions when a terminal operation pulls each element.
4. Make every terminal consume the complete sequence handle.
5. Enforce list allocation limits during `to_list` before publication.

## Acceptance contract

- Adapter creation does not invoke a callback.
- `take` stops upstream pulls after its declared count.
- `find`, `any`, and `all` short-circuit and consume the handle.
- A program can drop an unconsumed affine sequence. The runtime runs bounded
  cleanup.
- Tests cover move errors, double consumption, laziness, order,
  short-circuiting, limits, callback traps, nested adapters, and boundary
  rejection.

## Out of scope

The first version does not add effectful callbacks, external streams,
multiconsumer sequences, replayable iterators, asynchronous pulls, or boundary
encoding.
