# ALLEN high-level language features proposal

Status: 21 features selected for implementation; 26 features remain candidates

Editorial target: ASD-STE100 Issue 9 descriptive writing. Source code,
identifiers, file names, language names, and feature names are project technical
terms.

Companion artifacts:

- [Research inventory](../research/functional-language-features.md)
- [Interactive feature picker](allen-language-feature-picker.html)
- [Selected implementation proposals](../../roadmap/proposals/language/README.md)

## Decision summary

This proposal defines 47 additions to ALLEN. Every feature has a stable ID
shared with the interactive picker. The roadmap selects 21 features for
implementation. The other 26 remain candidates.

Selection does not add syntax to the current language. Each selected feature
has a focused proposal in `roadmap/proposals/language`. A feature becomes
current only after its complete implementation and documentation change lands.

Add syntax only when the compiler can use one predictable expansion. The
syntax must preserve evaluation order, types, effects, ownership, limits,
canonical values, and replay behavior.

A feature that cannot obey these rules needs a separate semantic proposal. Do
not present that feature as syntax sugar.

## Selected implementation set

The roadmap divides the selected features into three dependency-ordered
batches. The [roadmap table](../../ROADMAP.md#selected-language-features) links
every stable ID to its focused proposal.

- Batch L1: `LIT-RAW-STRING`, `LIT-MULTILINE`, `FUN-LABELED-ARGS`,
  `FUN-LAMBDA-SHORT`, `COL-COMBINATORS`, and `FLOW-OPTION-QUESTION`.
- Batch L2: `FUN-DEFAULT-ARGS`, `FUN-TRAILING-CALLBACK`, `FUN-PARTIAL`,
  `FUN-COMPOSE`, `FUN-PIPE`, `FUN-EXTENSION-CALL`, `COL-RECORD-UPDATE`,
  `COL-LITERAL-SPREAD`, and `FLOW-OPTION-CHAIN`.
- Batch L3: `COL-RANGE-VALUES`, `COL-SLICES`, `PAT-RANGE`, `PAT-OR`,
  `FUN-LOCAL`, and `COL-LAZY-SEQUENCE`.

## Current baseline

The current language has typed closures, generic functions with the built-in
`Eq` constraint, immutable aggregate values, `Option<T>`, `Result<T, E>`, and
exhaustive `match`. It has postfix `?` for `Result`, `??` for `Option`,
`list.fold`, expression-valued `if`, and numeric separators. It also has
explicit effect sets, lazy futures, and structured task scopes. The features
below extend that baseline. They do not rename current behavior.

## Current gap verification

This audit compares every candidate with the current worktree on 2026-08-29.
It checks the grammar, compiler name resolution, built-in operations, language
specification, and unsupported feature list.

The audit found one duplicate. Current ALLEN already supports numeric
separators. The research inventory now proposes raw strings instead. The audit
also narrowed three proposals that have nearby current features.

- `COL-COMBINATORS` adds only list operations that do not exist today.
- `PAT-LITERAL` adds non-Boolean literals. Current patterns already support
  `true` and `false`.
- `COL-SLICES` adds bracket slices, list slices, and bytes slices. Current
  ALLEN already has `string.slice`.

All 47 rows below describe a missing feature. The current overlap column names
the nearest implemented feature. It does not mark the proposal as implemented.

| ID | Current overlap | Missing feature |
|---|---|---|
| `FUN-PIPE` | Ordinary calls | No `|>` token, grammar, or expansion exists. |
| `FUN-COMPOSE` | Typed closures | No composition operator exists. |
| `FUN-PARTIAL` | `_` in loop and match patterns | `_` cannot occur as a call argument. |
| `FUN-LAMBDA-SHORT` | Fully typed closures | A closure requires typed parameters, `returns`, and a body. |
| `FUN-TRAILING-CALLBACK` | Function arguments inside parentheses | No trailing callback form exists. |
| `FUN-LABELED-ARGS` | Positional call arguments | Call labels do not exist. |
| `FUN-DEFAULT-ARGS` | Required parameters | Default parameter expressions do not exist. |
| `FUN-EXPR-BODY` | Function body blocks | A function declaration requires braces. |
| `FUN-LOCAL` | Top-level functions | A body cannot contain a function declaration. |
| `FUN-EXTENSION-CALL` | Namespace functions and field access | No extension-call resolution exists. |
| `COL-COMBINATORS` | `list.fold`, `list.append`, and other current operations | The proposed transform and predicate operations do not exist. |
| `COL-LAZY-SEQUENCE` | Eager `List<T>` operations | No sequence type or lazy collection operation exists. |
| `COL-COMPREHENSION` | `for` statements | No list comprehension or `yield` expression exists. |
| `COL-RECORD-UPDATE` | Complete record constructors | No record update form exists. |
| `COL-LITERAL-SPREAD` | List and map literals | No spread item exists. |
| `COL-RANGE-VALUES` | `start..end` in a `for` clause | A range cannot occur as a value. Inclusive ranges do not exist. |
| `COL-SLICES` | Index syntax and `string.slice` | Bracket slices, list slices, and bytes slices do not exist. |
| `PAT-DESTRUCTURE-LET` | Identifier local bindings | Local declarations cannot contain patterns. |
| `PAT-DESTRUCTURE-PARAM` | Identifier parameters | Parameters cannot contain patterns. |
| `PAT-LITERAL` | `true` and `false` patterns | `Int`, `String`, and `Bytes` patterns do not exist. |
| `PAT-RANGE` | Boolean and enum patterns | Range patterns do not exist. |
| `PAT-NESTED` | One payload binder or `_` | Pattern payloads cannot contain nested patterns. |
| `PAT-GUARD` | Match arms | Guards are explicitly unsupported. |
| `PAT-OR` | One pattern per arm | OR patterns are explicitly unsupported. |
| `PAT-AS` | Pattern binders | A pattern cannot also bind its complete value. |
| `PAT-COLLECTION` | List and map values | Collection patterns are explicitly unsupported. |
| `PAT-IF-LET` | `if` and `match` expressions | `if let` does not exist. |
| `PAT-LET-ELSE` | Local bindings and `Never` | `let else` does not exist. |
| `PAT-IS` | Full `match` expressions | No Boolean pattern test exists. |
| `PAT-ACTIVE` | Functions and enum patterns | A user function cannot act as a pattern. |
| `FLOW-OPTION-QUESTION` | `?` for `Result<T, E>` | `?` rejects `Option<T>`. |
| `FLOW-OPTION-CHAIN` | Field access and `??` | The `?.` operator does not exist. |
| `FLOW-BIND` | Closures and direct calls | No callback bind syntax exists. |
| `FLOW-PARALLEL-BIND` | `spawn` and `await` scopes | No applicative bind syntax exists. |
| `FLOW-ASYNC-CLOSURE` | Named `async fn` declarations | Closures cannot have `async`. |
| `FLOW-TAP` | User functions | The standard library has no `tap` operation. |
| `FLOW-BUILDER-BLOCK` | Ordinary calls and body blocks | No builder protocol or builder block exists. |
| `TYPE-CONSTRAINT` | Built-in `Eq` constraint | Users cannot declare constraints or implementations. |
| `TYPE-DERIVE` | Intrinsic `Eq` and canonical codecs | No explicit derive syntax exists for new constraints. |
| `TYPE-EFFECT-POLY` | Closed effect sets | Effect polymorphism is explicitly unsupported. |
| `TYPE-HKT` | First-order generic parameters | Higher-kinded types are explicitly unsupported. |
| `TYPE-UNION` | Nominal enums | Anonymous union types do not exist. |
| `TYPE-OPEN-VARIANT` | Closed nominal enums | Open variants do not exist. |
| `TYPE-RECURSIVE-ADT` | Nonrecursive records and enums | Recursive enum payloads are explicitly unsupported. |
| `TYPE-EFFECT-HANDLER` | Fixed host effects | User-defined effects and handlers do not exist. |
| `LIT-MULTILINE` | Single-line strings and templates | Source strings cannot contain an unescaped line break. |
| `LIT-RAW-STRING` | Escaped strings and templates | Raw string delimiters do not exist. |

The audit uses these implementation sources:

- [`allen-0.1.ungram`](../../crates/allen-syntax/grammar/allen-0.1.ungram)
  contains the complete current grammar.
- [`resolution.rs`](../../crates/allen-compiler/src/frontend/resolution.rs)
  contains the current built-in operation registry.
- [ALLEN language reference section 11](../agents/reference/allen-language.md#11-unsupported-syntax-and-operations)
  lists the features that the current language rejects explicitly.

## Shared implementation contract

An accepted feature MUST have a feature decision that fixes:

1. Its exact tokens, grammar productions, precedence, associativity, and
   reserved-word changes.
2. A typed expansion into existing HIR when the feature is sugar. The compiler
   MUST retain the original source span for diagnostics.
3. Left-to-right evaluation order, single-evaluation guarantees, skipped
   expressions, and control-flow exits.
4. Type inference boundaries. Public declarations and external boundaries keep
   their current explicit-type requirements.
5. The exact effect set. Sugar MUST NOT hide, erase, or widen callback or host
   effects.
6. Ownership behavior for `Future`, `Task`, `SubAgent`, capabilities, and any
   later affine type.
7. Exhaustiveness, reachability, depth, instruction, allocation, and collection
   limits.
8. Artifact, canonical encoding, replay, conformance, editor, example, human
   specification, and agent-reference updates required by the current-language
   policy.

Feature IDs are permanent. If a proposal changes, keep the ID and record the
change. If the project rejects a proposal, keep the ID reserved.

## Delivery slices

The slices describe dependency order, not commitments.

- Slice A, mechanical syntax: expression bodies, concise lambdas, pipes,
  labeled calls, record updates, `Option` propagation, multiline strings, and
  raw strings.
- Slice B, pattern language: destructuring, nested patterns, literals, ranges,
  guards, OR patterns, as-patterns, conditional bindings, and collection
  patterns.
- Slice C, collection vocabulary: eager combinators first, then
  comprehensions, range values, spreads, slicing, and only then lazy sequences.
- Slice D, callable and flow abstractions: partial application, composition,
  trailing callbacks, local functions, extension calls, async closures, bind
  forms, and builder blocks.
- Slice E, type-system work: user constraints, derivation, effect polymorphism,
  recursive data, higher-kinded types, union and open variants, and effect
  handlers. Each item in this slice needs a separate design decision.

## Function values and call syntax

### `FUN-PIPE` Forward pipe

Proposed form: `users |> list.filter(is_active) |> list.map(to_name)`.

Insert the left value as the first argument of the right call. If the right
side contains one `_` placeholder, insert at that position instead. Evaluate
each stage once from left to right. The pipe has lower precedence than calls
and arithmetic and higher precedence than `??`. Lower directly to nested calls.

### `FUN-COMPOSE` Function composition

Proposed form: `let normalize = string.trim_ascii >> string.to_lower;`.

`f >> g` creates the exact closure `fn(value) { g(f(value)) }`. The result type
of `f` must exactly match the input type of `g`. The composed function carries
the union of both effect sets and cannot capture affine values.

### `FUN-PARTIAL` Placeholder partial application

Proposed form: `let warn = log.write(level: "warn", message: _);`.

Each `_` creates one closure parameter in source order. The first version
permits placeholders only in direct call arguments. Reject a capture when the
compiler cannot infer the expected parameter type exactly.

### `FUN-LAMBDA-SHORT` Concise inferred lambda

Proposed form: `list.map(values, fn(x) => x + 1)`.

Infer parameter and result types only from one exact expected function type.
Require annotations when no such type exists or when resolution needs an
overload. Lower to the existing closure representation.

### `FUN-TRAILING-CALLBACK` Trailing callback block

Proposed form: `list.fold(values, 0) fn(total, item) { total + item }`.

Permit this form only when the final parameter has a function type. Evaluate all
ordinary arguments before constructing the callback. Lower to a normal final
call argument without changing capture or effect rules.

### `FUN-LABELED-ARGS` Labeled arguments

Proposed form:
`string.replace(value: text, needle: old, replacement: next)`.

Labels belong to a declaration's call contract but not its runtime value. The
compiler reorders labeled arguments into declaration order after rejecting
unknown, duplicate, and missing labels. Evaluate supplied arguments in source
order into temporaries before reordering.

### `FUN-DEFAULT-ARGS` Default arguments

Proposed form: `fn retry(count: Int = 3, delay: Int = 1) returns Int`.

The compiler expands declaration-owned defaults at the call site in parameter
order. A default can refer only to earlier parameters and pure
constants. Exported and boundary functions must include defaults in their
artifact contract and digest.

### `FUN-EXPR-BODY` Expression-bodied function

Proposed form: `fn double(x: Int) returns Int => x * 2;`.

This is exact sugar for `{ x * 2 }`. It does not relax annotations, add an
implicit return statement, or change semicolon rules elsewhere.

### `FUN-LOCAL` Local named functions

Proposed form: `fn visit(node: Node) returns Int { ... }` inside a body.

Start with local functions that do not capture values. Declare each function
before its first use. Give each function a stable nested compiler identity.
Do not include recursion, captures, or generic local functions in the first
version.

### `FUN-EXTENSION-CALL` Uniform extension-call sugar

Proposed form: `values.map(fn(x) => x + 1)` for
`list.map(values, fn(x) => x + 1)`.

Resolve only functions owned by the receiver type's namespace or explicitly
imported as extensions. Reject ambiguity. Do not add method dispatch, implicit
instances, receiver mutation, or conversions.

## Collections and immutable data

### `COL-COMBINATORS` Eager list transform and predicate combinators

Add the missing eager `map`, `filter`, `flat_map`, `filter_map`, `find`, `any`,
`all`, `partition`, and `scan` operations. Example:
`values |> list.filter(valid) |> list.map(render)`. Each operation visits input
left to right. Current `list.fold` stays unchanged. Define empty results,
short-circuit points, callback effects, and complete allocation charging
separately for every new operation.

### `COL-LAZY-SEQUENCE` Lazy sequences or iterators

Proposed form:
`values |> seq.from_list |> seq.map(expensive) |> seq.take(10) |> seq.to_list`.

Use affine, single-pass `Sequence<T>` values in the first design. Thus, a
program cannot replay effectful steps by accident. A sequence cannot cross an entry boundary
or enter canonical encoding. Every terminal operation needs a consumption and
resource-limit rule.

### `COL-COMPREHENSION` List comprehension

Proposed form:
`[for user in users if user.active yield user.name]`.

Start with one or more `List` generators and Boolean guards. Evaluate clauses
left to right. Lower to nested loops that append into one compiler-owned result
builder with deterministic allocation charging.

### `COL-RECORD-UPDATE` Immutable record update

Proposed form: `let enabled = User { ..user, active: true };`.

Evaluate `user` once, require its exact record type, then evaluate explicit
fields left to right. Reject unknown or repeated fields. Construct a fresh
value with the same nominal or structural identity.

### `COL-LITERAL-SPREAD` List and map spread

Proposed forms: `[..prefix, item, ..suffix]` and
`map { ..defaults, "mode": selected }`.

Evaluate parts left to right. List spread accepts only `List<T>`. Map spread
accepts the same exact key and value types. Later keys replace earlier keys.
This behavior matches repeated `map.insert` calls. Charge allocation before
construction.

### `COL-RANGE-VALUES` First-class ranges

Proposed forms: `0..length(values)` and `1..=10`.

Introduce immutable `Range<Int>` with half-open and inclusive constructors.
Iteration must terminate at integer boundaries without overflow. Range values
are canonical but stay unavailable at entry boundaries until their JSON shape
is explicitly specified.

### `COL-SLICES` Bracket slicing for lists, bytes, and strings

Proposed form: `values[2..5]`.

For `List<T>` and `Bytes`, return `Option<List<T>>` and `Option<Bytes>` so an
invalid bound does not trap. String slice behavior remains Unicode-scalar based and
returns `Option<String>`. The existing `string.slice` function remains the
explicit spelling. A later borrowed-view design is a different feature.

## Pattern syntax

### `PAT-DESTRUCTURE-LET` Irrefutable destructuring bindings

Proposed forms: `let (name, score) = entry;` and
`let Point { x, y } = point;`.

Accept only patterns proven to match every value of the scrutinee type. Evaluate
the scrutinee once, bind all names immutably, and reject duplicate names.

### `PAT-DESTRUCTURE-PARAM` Destructured parameters

Proposed form:
`fn sum((left, right): (Int, Int)) returns Int { left + right }`.

Named functions keep one explicit parameter type. Lambdas use the exact
expected callback type. The call ABI remains one aggregate parameter.

### `PAT-LITERAL` Non-Boolean scalar literal patterns

Proposed form: `match status { 200 => "ok" _ => "other" }`.

Keep the existing `true` and `false` patterns. Add `Int`, `String`, and `Bytes`
constants with deterministic equality. Do not allow `Float` literal patterns.
Duplicate literals are unreachable.

### `PAT-RANGE` Range patterns

Proposed form: `match code { 200..=299 => "ok" _ => "other" }`.

Require compile-time `Int`, `String`, or `Bytes` endpoints with ordered
comparison. Diagnose empty, overlapping, and unreachable ranges.

### `PAT-NESTED` Recursive nested patterns

Proposed form: `Some(User { name, role: Role.Admin }) => name`.

Permit patterns recursively inside tuple, record, enum, `Option`, and `Result`
payloads. Apply the existing type-depth limit and preserve exhaustiveness over
the outer and inner closed types.

### `PAT-GUARD` Match guards

Proposed form: `User { age, name } if age >= 18 => name`.

Run a guard only after its structural pattern matches, with bindings in scope.
A guard must be `Bool`. Guarded arms do not satisfy exhaustiveness because the
compiler cannot generally prove that a guard is true.

### `PAT-OR` OR patterns

Proposed form: `Reading.Empty | Reading.Skipped => 0`.

Every alternative must bind the same names with exact matching types and
ownership states. Alternatives test left to right without evaluating the
scrutinee again.

### `PAT-AS` As patterns

Proposed form: `whole @ User { id, .. } => audit(whole, id)`.

Bind the complete matched value and selected parts. For affine values, a part
can use a borrow only if ALLEN later adds borrows. Until then, reject a form
that duplicates or partially moves an affine value.

### `PAT-COLLECTION` List and map patterns with rest

Proposed forms: `[head, ..tail]` and `map { "id": id, .. }`.

List rest constructs a new `List<T>`. Map patterns evaluate constant keys in
canonical order. The pattern is open only when `..` appears. Define allocation charges
and a maximum pattern width.

### `PAT-IF-LET` Conditional pattern binding

Proposed form:
`if let Some(user) = lookup(id) { user.name } else { "unknown" }`.

Evaluate the scrutinee once. Bind names only in the true branch. Branch type,
effect, and ownership joins use the existing `if` rules.

### `PAT-LET-ELSE` Guarding pattern binding

Proposed form:
`let Some(user) = lookup(id) else { return None; };`.

The `else` body must have type `Never`. On success, bindings enter the outer
scope. On failure, no partial binding becomes visible.

### `PAT-IS` Boolean pattern test

Proposed form: `matches(value, Some(_))`.

Use a predicate-only form first. It returns `Bool`, binds no names, and tests
the scrutinee once. The project can consider binding-aware `is` conditions
after it adds a flow-sensitive type system.

### `PAT-ACTIVE` User-defined active patterns

Proposed form: `ParseInt(value) => value` where `ParseInt` is a declared pure
view from `String` to `Option<Int>`.

Resolve active patterns explicitly, run them once, and prohibit effects. This
feature needs a declaration syntax and must not make ordinary function calls
look like enum constructors without clear name resolution.

## Option, result, callback, and task flow

### `FLOW-OPTION-QUESTION` Postfix question operator for Option

Proposed form: `let user = map.get(users, id)?;` inside a function returning
`Option<U>`.

`Some(value)?` produces `value`. `None?` returns `None` from the current
function. Do not convert between `Option` and `Result`. Reuse the current
postfix precedence and early-return lowering.

### `FLOW-OPTION-CHAIN` Optional member and call chaining

Proposed form: `user?.address?.city`.

Evaluate each receiver once and stop the remaining chain on `None`. A successful
field or call result becomes `Some(result)` unless it already returns `Option`,
in which case the chain flattens one layer. Do not add forced unwrap.

### `FLOW-BIND` Typed callback bind sugar

Proposed form:
`use user <- result.try(load_user(id)); render(user)`.

Treat `use` as a source transform controlled by the called function's exact
final callback parameter. It introduces no universal monad interface and no
hidden effect. Diagnostics show both the source form and expanded call.

### `FLOW-PARALLEL-BIND` Independent applicative binds

Proposed form: `let! a = fetch_a(); and! b = fetch_b();`.

Lower siblings into one `await` scope with explicit task creation. Fix start
order, cancellation, join order, and multiple-error selection. This feature
depends on a separate structured-concurrency decision and is not parser sugar.

### `FLOW-ASYNC-CLOSURE` Async closures and callback types

Proposed form:
`async fn(url: String) returns Result<String, NetworkError> effects [net.http_get] { ... }`.

A call to the closure returns `Future<T>`. The call contributes its declared
effects under the rules for a named `async fn`. Keep current affine capture limits.

### `FLOW-TAP` Value-preserving observation combinator

Proposed form:
`plan |> tap(fn(value) effects [debug] { debug(value) }) |> execute`.

Implement this as a generic library function that calls the callback once and
returns its original value. Its type carries the callback's exact effect set,
so it depends on `TYPE-EFFECT-POLY` for effectful callbacks.

### `FLOW-BUILDER-BLOCK` Typed builder or computation block

Proposed form: `report { section("Summary") table(rows) }`.

Expand through one declared builder protocol with named operations. The first
version supports pure deterministic builders only. Do not add token
macros, AST rewriting APIs, or runtime reflection.

## Type-level abstractions

### `TYPE-CONSTRAINT` User-defined traits or constraints

Proposed form:
`constraint Render<T> { fn render(value: T) returns String }`.

Use explicit imports and coherent module-scoped implementations. Resolution
must select at most one implementation without conversions. Artifacts record
the selected implementation for each monomorphized call.

### `TYPE-DERIVE` Compiler-derived constraint implementations

Proposed form: `record User derives [Ord, Hash] { ... }`.

Current ALLEN already determines `Eq` structurally. It also owns canonical
encode and decode behavior. Explicit derivation must not duplicate those behaviors. Start
with a closed list of future compiler-owned constraints that are not intrinsic.
Derivation rejects a field that lacks the capability and contributes
deterministic generated functions and contract digests to the artifact.

### `TYPE-EFFECT-POLY` Effect-polymorphic callbacks

Proposed form:
`fn map<A, B, effects E>(f: fn(A) returns B effects E, values: List<A>) returns List<B> effects E`.

An effect variable ranges over one closed set. Source can only propagate or
bound the set. Source cannot inspect it at runtime. Instantiation records the exact set in
the artifact. Public APIs must expose every effect variable and bound.

### `TYPE-HKT` Higher-kinded type parameters

Proposed form: `fn traverse<F<_>, A, B>(...) returns F<List<B>>`.

Add explicit kinds and a kind check before inference. Monomorphization must
remain finite and bounded. Implement this feature after ordinary user
constraints and effect polymorphism prove their design.

### `TYPE-UNION` Anonymous union types

Proposed form: `Int | ParseError`.

Define an explicit tagged runtime representation, deterministic member order,
narrowing, exhaustive matching, equality, canonical encoding, and schema
projection. Do not use an untagged best-fit decoder at external boundaries.

### `TYPE-OPEN-VARIANT` Open or polymorphic variants

Proposed form: `[Reading.Empty | Reading.Number(Int) | ..]`.

Constructor labels carry stable identities independent of one closed enum.
Open matches are never exhaustive without `_`. Encoding must include the full
constructor identity. This competes with nominal enums and needs a separate
decision.

### `TYPE-RECURSIVE-ADT` Recursive enums and records

Proposed form: `enum Tree<T> { Leaf(T) Branch(List<Tree<T>>) }`.

Use indirect heap representation at recursive edges. Define cycle-free values,
depth limits, equality, encoding, decoding, allocation charging, and bounded
monomorphization. Recursive values remain immutable.

### `TYPE-EFFECT-HANDLER` User-defined typed effects and handlers

Proposed form: `handle state_program() with map_state(initial)`.

Keep locally handled computational effects distinct from host authority
effects. A handler cannot intercept or forge filesystem, tool, agent, model,
permission, or other authority. Continuations, replay, cancellation, and task
ownership require a dedicated runtime design.

## Literal and lexical sugar

### `LIT-MULTILINE` Indentation-trimmed multiline strings

Proposed form: triple-quoted strings with existing `${expression}`
interpolation.

Normalize source newlines to LF. Remove the longest common indentation shared
by nonblank lines after the opening line. Preserve interpolation and escapes.
A later raw delimiter can disable both. That delimiter is outside this feature.

### `LIT-RAW-STRING` Raw string literals

Proposed form: `r#"${name}\\d+"#`.

Preserve every enclosed scalar without escape decoding or interpolation.
Hash-counted delimiters permit quote characters in the value. Set a small
maximum delimiter count and diagnose an unmatched closing delimiter at its
opening token.

## Implementation checklist for one selected ID

Before you implement an item from the picker:

1. Create one focused decision document named with the stable feature ID.
2. Resolve every open semantic point in that feature's section and the shared
   implementation contract.
3. Update the grammar and CST first. Also update recovery fixtures and editor
   grammar.
4. Add type checks and ownership tests before bytecode expansion.
5. Prefer HIR expansion for sugar. Add bytecode only when no exact expansion
   exists.
6. Add positive, negative, limit, replay, canonicalization, and boundary tests
   in proportion to the feature's effects.
7. Update the human specification and both agent references in the same change.
8. Run the complete conformance and editor checks before changing the proposal
   status.

## Resolved selection decisions

The focused proposals resolve the three choices from the candidate review:

- `FUN-PIPE` inserts the first argument by default and accepts one explicit
  `_` placement.
- `FUN-LABELED-ARGS` keeps labels optional. A call is fully positional or fully
  labeled.
- `COL-RANGE-VALUES` adds a first-class internal value. Range values cannot
  cross a data boundary or become artifact constants until a later proposal
  fixes their encoding.
