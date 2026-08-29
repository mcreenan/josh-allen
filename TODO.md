# Language feature implementation status

## Selected language features

The stable IDs link to focused proposals. Implement the batches in order.
Items in one batch can proceed independently unless the item names a
dependency.

### Batch L1: syntax and library foundations

- [x] [`LIT-RAW-STRING`](roadmap/proposals/language/LIT-RAW-STRING.md) - Add raw string literals with hash-counted delimiters.
- [x] [`LIT-MULTILINE`](roadmap/proposals/language/LIT-MULTILINE.md) - Add indentation-trimmed multiline strings.
- [x] [`FUN-LABELED-ARGS`](roadmap/proposals/language/FUN-LABELED-ARGS.md) - Add labels to declaration and call contracts.
- [x] [`FUN-LAMBDA-SHORT`](roadmap/proposals/language/FUN-LAMBDA-SHORT.md) - Infer concise lambda types from one exact expected function type.
- [x] [`COL-COMBINATORS`](roadmap/proposals/language/COL-COMBINATORS.md) - Add the selected eager list transform and predicate operations.
- [x] [`FLOW-OPTION-QUESTION`](roadmap/proposals/language/FLOW-OPTION-QUESTION.md) - Extend postfix question propagation to `Option`.

### Batch L2: call and data sugar

- [x] [`FUN-DEFAULT-ARGS`](roadmap/proposals/language/FUN-DEFAULT-ARGS.md) - Add declaration-owned default arguments after labeled arguments.
- [x] [`FUN-TRAILING-CALLBACK`](roadmap/proposals/language/FUN-TRAILING-CALLBACK.md) - Move one final callback outside call parentheses.
- [x] [`FUN-PARTIAL`](roadmap/proposals/language/FUN-PARTIAL.md) - Lower direct-call placeholders to typed closures.
- [x] [`FUN-COMPOSE`](roadmap/proposals/language/FUN-COMPOSE.md) - Lower function composition to one typed closure.
- [x] [`FUN-PIPE`](roadmap/proposals/language/FUN-PIPE.md) - Add the forward pipe after partial application and eager combinators.
- [x] [`FUN-EXTENSION-CALL`](roadmap/proposals/language/FUN-EXTENSION-CALL.md) - Add namespace-owned and explicitly imported extension-call resolution.
- [x] [`COL-RECORD-UPDATE`](roadmap/proposals/language/COL-RECORD-UPDATE.md) - Add immutable record update syntax.
- [x] [`COL-LITERAL-SPREAD`](roadmap/proposals/language/COL-LITERAL-SPREAD.md) - Add list and map spread items.
- [x] [`FLOW-OPTION-CHAIN`](roadmap/proposals/language/FLOW-OPTION-CHAIN.md) - Add optional field and call chaining after `Option` propagation.

### Batch L3: new values and pattern work

- [x] [`COL-RANGE-VALUES`](roadmap/proposals/language/COL-RANGE-VALUES.md) - Add half-open and inclusive `Range<Int>` values.
- [x] [`COL-SLICES`](roadmap/proposals/language/COL-SLICES.md) - Add safe bracket slices after range values.
- [x] [`PAT-RANGE`](roadmap/proposals/language/PAT-RANGE.md) - Add ordered range patterns after range syntax is fixed.
- [x] [`PAT-OR`](roadmap/proposals/language/PAT-OR.md) - Add OR patterns with exact binding agreement.
- [x] [`FUN-LOCAL`](roadmap/proposals/language/FUN-LOCAL.md) - Add noncapturing, nongeneric local named functions.
- [x] [`COL-LAZY-SEQUENCE`](roadmap/proposals/language/COL-LAZY-SEQUENCE.md) - Add affine, single-pass lazy sequences after eager combinators.
