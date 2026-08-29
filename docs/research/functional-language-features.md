# Functional language and syntax feature inventory

Status: research input for design proposals, not current ALLEN syntax

Editorial target: ASD-STE100 Issue 9 descriptive writing. Source code,
identifiers, file names, language names, feature names, and source titles are
project technical terms.

This inventory uses the current language in
[`docs/language-spec.md`](../language-spec.md) as its baseline. ALLEN has
immutable values, typed closures, exhaustive `match`, `Option<T>`,
`Result<T, E>`, and generic functions. It has postfix `?` for `Result`, `??`
for `Option`, `list.fold`, expression-valued conditionals, loops, and numeric
separators. It also has explicit effects, lazy futures, and structured task
scopes.
The examples show proposed syntax. They do not show current ALLEN syntax. Each
ID is the permanent key in the feature picker and design proposal.

Gleam and MoonBit provide references for compact typed syntax. Rust provides
references for patterns and failure flow. Scala 3 provides references for type
abstractions. Kotlin and Swift provide references for call syntax. Unison and
Koka provide references for typed effects.

## Function values and call syntax

| ID | Feature | Proposed ALLEN syntax | Design requirements | Primary sources |
|---|---|---|---|---|
| `FUN-PIPE` | Forward pipe | `users |> list.filter(is_active) |> list.map(_.name)` | Specify the argument position, evaluation order, precedence, and optional `_` position. First-argument insertion matches current ALLEN library signatures. | [Gleam pipelines](https://tour.gleam.run/functions/pipelines/), [MoonBit pipelines](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#pipelines), [F# pipelines](https://learn.microsoft.com/en-us/dotnet/fsharp/language-reference/functions#pipelines) |
| `FUN-COMPOSE` | Function composition | `let normalize = string.trim_ascii >> string.to_lower;` | Expand this form to a typed closure. Require exact intermediate types. Preserve both effect sets. | [F# function composition](https://learn.microsoft.com/en-us/dotnet/fsharp/language-reference/functions#function-composition) |
| `FUN-PARTIAL` | Placeholder partial application | `let warn = log.write(level: "warn", message: _);` | Each placeholder creates one closure parameter. Reject ambiguous repeated or nested holes unless their ordering is explicit. | [Gleam function captures](https://tour.gleam.run/functions/function-captures/), [Elixir capture operator](https://hexdocs.pm/elixir/main/Kernel.SpecialForms.html#&/1), [MoonBit pipelines](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#pipelines) |
| `FUN-LAMBDA-SHORT` | Concise inferred lambda | `list.map(values, fn(x) => x + 1)` | Keep boundary annotations strict. Infer callback parameters and results only from one exact expected function type. | [Kotlin lambdas](https://kotlinlang.org/docs/lambdas.html), [Swift closures](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/closures/), [Grain functions](https://grain-lang.org/docs/guide/functions) |
| `FUN-TRAILING-CALLBACK` | Trailing callback block | `list.fold(values, 0) fn(total, item) { total + item }` | Permit this form only for the final function argument. Preserve the evaluation order and exact effect check of the ordinary call. | [Kotlin trailing lambdas](https://kotlinlang.org/docs/lambdas.html#passing-trailing-lambdas), [Swift trailing closures](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/closures/#Trailing-Closures) |
| `FUN-LABELED-ARGS` | Labeled arguments | `string.replace(value: text, needle: old, replacement: next)` | Decide whether labels are required, optional, or part of the function type. The compiler lowers calls to positional arguments with no runtime map. | [Gleam labelled arguments](https://tour.gleam.run/functions/labelled-arguments/), [OCaml labelled arguments](https://ocaml.org/docs/labels), [Kotlin functions](https://kotlinlang.org/docs/functions.html) |
| `FUN-DEFAULT-ARGS` | Default arguments | `fn retry(count: Int = 3, delay: Int = 1) returns Int` | Evaluate defaults at the call site in declaration order. Forbid defaults on external boundaries unless the wire contract records the default. | [Kotlin default parameters](https://kotlinlang.org/docs/functions.html#parameters-with-default-values), [OCaml optional arguments](https://ocaml.org/docs/labels) |
| `FUN-EXPR-BODY` | Expression-bodied function | `fn double(x: Int) returns Int => x * 2;` | Pure syntax lowering to the current function body. Do not infer public parameter or return types. | [Kotlin expression bodies](https://kotlinlang.org/docs/functions.html#single-expression-functions), [Grain functions](https://grain-lang.org/docs/guide/functions) |
| `FUN-LOCAL` | Local named functions | `fn walk(...) returns Int { fn visit(...) returns Int { ... } visit(...) }` | Define capture, recursion, generic instantiation, declaration order, and effect rules. A conservative first version can allow only non-capturing local functions. | [Kotlin local functions](https://kotlinlang.org/docs/functions.html#local-functions), [F# function bodies](https://learn.microsoft.com/en-us/dotnet/fsharp/language-reference/functions#function-bodies) |
| `FUN-EXTENSION-CALL` | Uniform extension-call sugar | `values.map(fn(x) => x + 1)` as sugar for `list.map(values, ...)` | Resolve only imported or namespace-owned functions and report ambiguity. Avoid implicit conversion or hidden instance search. | [Scala 3 extension methods](https://docs.scala-lang.org/scala3/reference/contextual/extension-methods.html), [Kotlin extensions](https://kotlinlang.org/docs/extensions.html) |

## Collections and immutable data

| ID | Feature | Proposed ALLEN syntax | Design requirements | Primary sources |
|---|---|---|---|---|
| `COL-COMBINATORS` | Eager list transform and predicate combinators | `values |> list.filter(valid) |> list.map(render)` | Add `map`, `filter`, `flat_map`, `filter_map`, `find`, `any`, `all`, `partition`, and `scan`. Keep the current `list.fold`. Define order, allocation cost, callback effects, and empty results. | [Rust Iterator](https://doc.rust-lang.org/std/iter/trait.Iterator.html), [Kotlin collection operations](https://kotlinlang.org/docs/collection-operations.html), [Gleam list module](https://hexdocs.pm/gleam_stdlib/gleam/list.html) |
| `COL-LAZY-SEQUENCE` | Lazy sequences or iterators | `values |> seq.from_list |> seq.map(expensive) |> seq.take(10) |> seq.to_list` | Choose single-pass versus reusable values, ownership, effect replay, termination limits, and canonicalization. This is much larger than eager combinators. | [Rust Iterator](https://doc.rust-lang.org/std/iter/trait.Iterator.html), [Kotlin sequences](https://kotlinlang.org/docs/sequences.html) |
| `COL-COMPREHENSION` | List comprehension | `let names = [for user in users if user.active yield user.name];` | Give generators and guards left-to-right semantics. Start with `List` only and lower to ordinary loops or combinators. | [Elixir comprehensions](https://hexdocs.pm/elixir/main/Kernel.SpecialForms.html#for/1), [Scala for comprehensions](https://docs.scala-lang.org/tour/for-comprehensions.html), [F# computation expressions](https://learn.microsoft.com/en-us/dotnet/fsharp/language-reference/computation-expressions#yield) |
| `COL-RECORD-UPDATE` | Immutable record update | `let enabled = User { ..user, active: true };` | Evaluate the base once, preserve its structural or nominal type, reject duplicate or unknown fields, and construct a fresh value. | [Gleam record updates](https://tour.gleam.run/data-types/record-updates/), [Rust struct update](https://doc.rust-lang.org/reference/expressions/struct-expr.html#functional-update-syntax) |
| `COL-LITERAL-SPREAD` | List and map spread | `let all = [..prefix, item, ..suffix];` | Specify left-to-right evaluation, duplicate map keys, complete allocation cost, and exact accepted collection types. | [Dart collection operators](https://dart.dev/language/collections#spread-operators), [JavaScript spread syntax specification](https://tc39.es/ecma262/multipage/ecmascript-language-expressions.html#sec-array-initializer) |
| `COL-RANGE-VALUES` | First-class ranges | `let indexes = 0..length(values); let bounded = 1..=10;` | Decide whether ranges are values or compiler-only iterables, support half-open and inclusive forms, and retain checked boundary behavior. | [Rust range expressions](https://doc.rust-lang.org/reference/expressions/range-expr.html), [MoonBit loops and ranges](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#for-loop) |
| `COL-SLICES` | Bracket slicing for lists, bytes, and strings | `let middle = values[2..5];` | Current ALLEN has `string.slice`. It has no bracket slice, list slice, or bytes slice. Prefer a safe `Option` result. Define separate String index rules. | [Rust array and slice expressions](https://doc.rust-lang.org/reference/expressions/array-expr.html), [MoonBit array patterns and views](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#array-pattern) |

## Pattern syntax

| ID | Feature | Proposed ALLEN syntax | Design requirements | Primary sources |
|---|---|---|---|---|
| `PAT-DESTRUCTURE-LET` | Irrefutable destructuring bindings | `let (name, score) = entry; let Point { x, y } = point;` | Permit only patterns that the compiler proves irrefutable. Keep refutable forms in `match`, `if let`, or `let else`. | [Rust patterns](https://doc.rust-lang.org/reference/patterns.html), [Kotlin destructuring](https://kotlinlang.org/docs/destructuring-declarations.html) |
| `PAT-DESTRUCTURE-PARAM` | Destructured parameters | `fn sum((left, right): (Int, Int)) returns Int { left + right }` | Start with tuples and records. Require one explicit type for named function parameters and exact expected types for lambdas. | [Rust patterns](https://doc.rust-lang.org/reference/patterns.html), [Kotlin destructuring in lambdas](https://kotlinlang.org/docs/destructuring-declarations.html#destructuring-in-lambdas) |
| `PAT-LITERAL` | Non-Boolean scalar literal patterns | `match status { 200 => "ok" _ => "other" }` | Current ALLEN already supports `true` and `false` patterns. Add `Int`, `String`, and `Bytes` only where equality is total and deterministic. Keep float patterns out because NaN and signed zero cause unexpected results. | [Rust literal patterns](https://doc.rust-lang.org/reference/patterns.html#literal-patterns), [MoonBit pattern matching](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#pattern-matching) |
| `PAT-RANGE` | Range patterns | `match code { 200..=299 => "ok" _ => "other" }` | Restrict endpoints to compile-time scalar constants. Define overlap and unreachable-arm diagnostics. | [Rust range patterns](https://doc.rust-lang.org/reference/patterns.html#range-patterns) |
| `PAT-NESTED` | Recursive nested patterns | `Some(User { name, role: Role.Admin }) => name` | Remove the current payload restriction to binder or `_`. Define type-directed constructor resolution and depth limits. | [Rust patterns](https://doc.rust-lang.org/reference/patterns.html), [MoonBit pattern matching](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#pattern-matching) |
| `PAT-GUARD` | Match guards | `User { age, name } if age >= 18 => name` | Evaluate after structural matching with bindings in scope. Guards must not count toward exhaustiveness, as a true result is not generally provable. | [MoonBit guard conditions](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#guard-condition), [Rust match guards](https://doc.rust-lang.org/book/ch19-03-pattern-syntax.html#extra-conditionals-with-match-guards) |
| `PAT-OR` | OR patterns | `Reading.Empty | Reading.Skipped => 0` | Require every alternative to bind the same names with the same types. Preserve first-arm reachability checks. | [Rust OR patterns](https://doc.rust-lang.org/reference/patterns.html#or-patterns) |
| `PAT-AS` | As patterns | `whole @ User { id, .. } => audit(whole, id)` | Bind the whole value and its parts without copying affine values. The ownership rule matters if patterns later cover resources. | [Rust identifier patterns](https://doc.rust-lang.org/reference/patterns.html#identifier-patterns), [Unison as-patterns](https://www.unison-lang.org/docs/language-reference/match-expressions-and-pattern-matching/as-patterns/) |
| `PAT-COLLECTION` | List and map patterns with rest | `[head, ..tail] => ...` and `map { "id": id, .. } => ...` | List rest produces a new list unless ALLEN adds views. Map patterns need exact open-versus-closed rules and canonical key behavior. | [MoonBit array and map patterns](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#pattern-matching), [Rust slice patterns](https://doc.rust-lang.org/reference/patterns.html#slice-patterns) |
| `PAT-IF-LET` | Conditional pattern binding | `if let Some(user) = lookup(id) { user.name } else { "unknown" }` | The scrutinee evaluates once. Bindings exist only in the true branch. Keep expression type and effect joins identical to ordinary `if`. | [Rust `if let`](https://doc.rust-lang.org/rust-by-example/flow_control/if_let.html), [Swift optional binding](https://docs.swift.org/swift-book/LanguageGuide/TheBasics.html#ID333) |
| `PAT-LET-ELSE` | Guarding pattern binding | `let Some(user) = lookup(id) else { return None; };` | Require the `else` body to have type `Never`, so successful bindings remain in the surrounding scope. | [Rust `let else`](https://doc.rust-lang.org/rust-by-example/flow_control/let_else.html), [MoonBit guard statements](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#guard-statement) |
| `PAT-IS` | Boolean pattern test | `if value is Some(_) { ... }` or `matches(value, Some(_))` | Choose one syntax. If it binds names, limit their scope to the true side of short-circuit `&&`. A predicate-only form is simpler. | [MoonBit `is`](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#guard-statement-and-is-expression), [Rust `matches!`](https://doc.rust-lang.org/std/macro.matches.html) |
| `PAT-ACTIVE` | User-defined active patterns | `match text { ParseInt(value) => value _ => 0 }` | A partial active pattern is effectively a pure `T -> Option<U>` view. Restrict it to pure functions so match behavior stays reviewable. | [F# active patterns](https://learn.microsoft.com/en-us/dotnet/fsharp/language-reference/active-patterns) |

## Option, result, callback, and task flow

| ID | Feature | Proposed ALLEN syntax | Design requirements | Primary sources |
|---|---|---|---|---|
| `FLOW-OPTION-QUESTION` | Postfix question operator for Option | `let user = map.get(users, id)?;` in a function returning `Option<U>` | Use the current `Result` propagation model. `Some(value)?` yields `value`. `None?` returns `None` without conversion. | [Rust question-mark propagation](https://doc.rust-lang.org/reference/expressions/operator-expr.html#the-question-mark-operator) |
| `FLOW-OPTION-CHAIN` | Optional member and call chaining | `let city = user?.address?.city;` | Flatten nested optionality and skip the remaining postfix chain on `None`. Do not add forced unwrap syntax. | [Swift optional chaining](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/optionalchaining/), [Kotlin null-safe calls](https://kotlinlang.org/docs/null-safety.html#safe-call-operator) |
| `FLOW-BIND` | Typed callback bind sugar | `use user <- result.try(load_user(id)); render(user)` | Keep this as a syntactic callback transform rather than built-in monads. The called function's final callback type determines bindings and result. | [Gleam `use`](https://tour.gleam.run/advanced-features/use/), [OCaml binding operators](https://ocaml.org/manual/5.5/bindingops.html), [Elixir `with`](https://hexdocs.pm/elixir/main/Kernel.SpecialForms.html#with/1) |
| `FLOW-PARALLEL-BIND` | Independent applicative binds | `let! a = fetch_a(); and! b = fetch_b();` | The syntax must say that siblings are independent. Lower to ALLEN tasks inside an `await` scope and fix cancellation, join order, and error selection. | [F# applicative computation expressions](https://learn.microsoft.com/en-us/dotnet/fsharp/language-reference/computation-expressions#and) |
| `FLOW-ASYNC-CLOSURE` | Async closures and callback types | `let fetch = async fn(url: String) returns Result<String, NetworkError> effects [net] { ... };` | A call to the closure produces `Future<T>`, as a call to `async fn` does. Keep capture, affine ownership, and effect-set rules explicit. | [Rust async closures](https://doc.rust-lang.org/edition-guide/rust-2024/async-closures.html), [Kotlin suspending functions](https://kotlinlang.org/docs/coroutines-basics.html#extract-function-refactoring) |
| `FLOW-TAP` | Value-preserving observation combinator | `plan |> tap(fn(value) effects [debug] { debug(value) }) |> execute` | This can be a normal standard-library function, not syntax. It returns the original value after the callback and must expose callback effects. | [Kotlin scope functions `also`](https://kotlinlang.org/docs/scope-functions.html), [MoonBit cascade operator](https://docs.moonbitlang.com/en/stable/language/fundamentals.html#cascade-operator) |
| `FLOW-BUILDER-BLOCK` | Typed builder or computation block | `report { section("Summary") table(rows) }` | This is controlled desugaring into named builder functions. It needs a narrow protocol, deterministic expansion, strong diagnostics, and no general macros. | [Swift result builders](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/advancedoperators/#Result-Builders), [F# computation expressions](https://learn.microsoft.com/en-us/dotnet/fsharp/language-reference/computation-expressions) |

## Type-level abstractions

| ID | Feature | Proposed ALLEN syntax | Design requirements | Primary sources |
|---|---|---|---|---|
| `TYPE-CONSTRAINT` | User-defined traits or constraints | `constraint Render<T> { fn render(T) returns String }` | Prefer explicit dictionaries or narrowly scoped instance resolution. Avoid implicit conversions. This fills the syntax left open by the current ALLEN spec. | [Rust traits](https://doc.rust-lang.org/reference/items/traits.html), [Scala 3 contextual abstractions](https://docs.scala-lang.org/scala3/reference/contextual/) |
| `TYPE-DERIVE` | Compiler-derived constraint implementations | `record User derives [Ord, Hash] { ... }` | Current ALLEN determines `Eq` structurally and already owns canonical encoding and decoding. Do not repackage those behaviors as new features. Restrict explicit derivation to future compiler-owned deterministic constraints that are not already intrinsic, and include the derived contract in artifacts and schema hashes. | [Rust derive attributes](https://doc.rust-lang.org/reference/attributes/derive.html), [Scala 3 type-class derivation](https://docs.scala-lang.org/scala3/reference/contextual/derivation.html) |
| `TYPE-EFFECT-POLY` | Effect-polymorphic callbacks | `fn map<A, B, effects E>(f: fn(A) returns B effects E, values: List<A>) returns List<B> effects E` | Preserve closed effect sets at boundaries. Inference must never erase or widen an effect, and instantiated effects belong in artifacts. | [Unison abilities in function types](https://www.unison-lang.org/docs/language-reference/abilities-and-ability-handlers/#abilities-in-function-types), [Koka effect types](https://koka-lang.github.io/koka/doc/book.html) |
| `TYPE-HKT` | Higher-kinded type parameters | `fn traverse<F<_>, A, B>(...) returns F<List<B>>` | Needed for generic functor, applicative, and monadic libraries, but it expands kind checking, inference, and monomorphization substantially. | [Scala 3 type lambdas](https://docs.scala-lang.org/scala3/reference/new-types/type-lambdas.html), [Haskell kinds](https://www.haskell.org/onlinereport/decls.html#kind-inference) |
| `TYPE-UNION` | Anonymous union types | `fn parse(value: String) returns Int | ParseError` | Define representation, narrowing, exhaustiveness, canonical encoding, and boundary schemas. Prefer tagged values where wire identity matters. | [Scala 3 union types](https://docs.scala-lang.org/scala3/reference/new-types/union-types.html) |
| `TYPE-OPEN-VARIANT` | Open or polymorphic variants | `fn label<T: [Reading.Empty | Reading.Number(Int) | ..]>(value: T) returns String` | Useful for extensible errors and modular visitors, but weaker constructor ownership makes spelling mistakes harder to detect. | [OCaml polymorphic variants](https://ocaml.org/manual/5.5/polyvariant.html) |
| `TYPE-RECURSIVE-ADT` | Recursive enums and records | `enum Tree<T> { Leaf(T) Branch(List<Tree<T>>) }` | Add cycle-safe type layout, canonical encoding, equality, resource accounting, decoding depth limits, and monomorphization checks. | [Gleam recursive custom types](https://tour.gleam.run/data-types/recursive-custom-types/), [Rust recursive types](https://doc.rust-lang.org/book/ch15-01-box.html#enabling-recursive-types-with-boxes) |
| `TYPE-EFFECT-HANDLER` | User-defined typed effects and handlers | `handle state_program() with map_state(initial)` | Keep host authority effects distinct from locally handled computational effects. Handler continuations complicate replay, structured concurrency, and audit guarantees. | [Unison abilities and handlers](https://www.unison-lang.org/docs/language-reference/abilities-and-ability-handlers/), [Koka handlers](https://koka-lang.github.io/koka/doc/book.html#sec-handlers) |

## Literal and lexical sugar

| ID | Feature | Proposed ALLEN syntax | Design requirements | Primary sources |
|---|---|---|---|---|
| `LIT-MULTILINE` | Indentation-trimmed multiline strings | `let prompt = """\n  Summarize ${topic}.\n  Return JSON.\n  """;` | Normalize source newlines, define indentation stripping, and retain the current interpolation and escape rules. A separate raw delimiter can disable both. | [Swift string literals](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/lexicalstructure/#String-Literals) |
| `LIT-RAW-STRING` | Raw string literals | `let pattern = r#"${name}\\d+"#;` | Preserve every enclosed scalar without escapes or interpolation. A hash-counted delimiter permits quotes inside the value and must have one exact maximum nesting count. | [Rust raw string literals](https://doc.rust-lang.org/reference/tokens.html#raw-string-literals), [Swift extended string delimiters](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/stringsandcharacters/#Extended-String-Delimiters) |

## Cross-cutting design rules for any selected feature

Specify these items before you implement an accepted feature:

- Exact grammar, precedence, associativity, and reserved word changes.
- A mechanical expansion into the smallest possible core construct.
- Left-to-right evaluation and each skipped subexpression.
- Static and runtime effect behavior, with callback effects.
- Affine `Future`, `Task`, capability, and `SubAgent` ownership.
- Type inference boundaries and exact error cases.
- Exhaustiveness, unreachable code, and diagnostic behavior.
- Instruction, allocation, depth, and collection limits.
- Canonical encoding, artifacts, replay, and entry-boundary effects.
- Required updates to both language references, tests, conformance data, editor
  grammar, examples, and skill reference under the repository's current language
  policy.

The list has two implementation classes. The first class can expand into
constructs that ALLEN already has. This class includes most syntax features.

The second class changes the type system or runtime. It includes lazy
sequences, user constraints, effect polymorphism, higher-kinded types,
recursive data, builders, and effect handlers. Write a separate proposal for
each feature in this class.
