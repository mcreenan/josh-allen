# ALLEN language reference for AI agents

Status: Early-alpha agent reference for the evolving version 0.1 source profile
Normative source: [`docs/language-spec.md`](../../language-spec.md)

Entry point: [`docs/agents/README.md`](../README.md). Load only the section
routed by that file unless a full language review is required.

**ALLEN** means **Agent-Level Language, Embedded Natively**. This document is
organized for an LLM or AI agent that must write, review, or repair ALLEN
source. It restates source-relevant rules from the human language specification
and deliberately omits most bytecode and implementation rationale.

If this document disagrees with `docs/language-spec.md`, the human language
specification is authoritative. Do not resolve a disagreement by inventing a
third interpretation.

## 0. Language status

ALLEN is an early alpha under active development. Version `0.1` and the
`*-0.1` conformance profile names identify the one current implementation.
Breaking changes are acceptable, but source, semantics, standard operations,
manifests, artifacts, providers, tests, conformance data, editor support,
examples, and both specifications must remain internally consistent. Inputs
produced for earlier repository states are unsupported.

## 1. Rules for agents

When producing ALLEN source:

1. Use only syntax, types, operations, effects, and manifest fields documented
   here.
2. Do not infer behavior from JavaScript or TypeScript. ALLEN only borrows some
   familiar syntax.
3. Do not invent a built-in operation, method, implicit conversion, exception
   form, capability, tool, prompt field, or provider fallback.
4. Declare exact parameter, return, entry-boundary, prompt-response, and tool
   types.
5. Declare the maximum effect set of every effectful function. An omitted
   clause means the exact empty effect set. An effect is not a capability grant.
6. Treat omitted and unsupported features as unavailable.
7. Keep provider identities distinct. The invoking agent, a sub-agent, a user,
   a model, and a tool are different targets.
8. Use the complete lexical grammar and operator-precedence table in Section
   3.7; prefer parentheses when they make an expression easier to review.
9. Use `Option<T>` instead of `null`, `Result<T, E>` for expected failures, and
   explicit conversions instead of coercion.
10. Await or transfer every live `Task<T>`. Never detach or discard it.

### 1.1 Non-JavaScript rules

ALLEN has:

- no implicit coercion;
- no `any`, `undefined`, or `null` source value;
- no nullable suffix such as `String?`;
- no hoisting or prototype inheritance;
- no automatic semicolon insertion;
- no implicit string or numeric conversion;
- short-circuit Boolean operators: `&&` evaluates its right operand only when
  its left operand is true, and `||` only when its left operand is false;
- immutable aggregate values, even when their binding uses `mut`;
- checked signed-integer arithmetic;
- exhaustive matching over closed types;
- explicit effect contracts; and
- lazy futures and affine task ownership.

### 1.2 Execution actors

| Term | Exact role |
|---|---|
| program | Untrusted typed ALLEN code. |
| runtime | Parses, checks, and executes the program. |
| host | Embeds or starts the runtime and supplies capabilities and providers. |
| invoking agent | The one stable agent session attached to this execution, if any. |
| sub-agent | A fresh agent reached through an execution-scoped `SubAgent` handle. |
| user-interaction provider | Supplies typed replies for `user.ask`; it is independent of the invoking agent. |
| model provider | Supplies typed replies for `model.request`; it is independent of the invoking agent. |
| tool provider | Dispatches members of the frozen typed tool catalog. |
| JOSH | The optional JSON-Oriented Session Host reference host for the `josh/1` attached-execution contract. |
| effect | A source-level declaration of possible external interaction or local task/debug operation. |
| capability | Unforgeable runtime authority to perform an operation. |
| grant | A capability issued after policy approval; it is no wider than the request. |

JOSH is optional. It binds the invoking agent, negotiates capabilities and
limits, projects the filtered transcript and frozen tool catalog, routes
external effects, and returns the terminal outcome. Another host may implement
the same provider contracts or the `josh/1` protocol. Standalone and unattended
executions are valid. Missing one provider must not disable an unrelated
provider. An operation fails for a missing provider only when the program
evaluates that operation, except that a missing required tool fails program
loading.

To launch JOSH, bind an invoking session, load a program, and service the
bidirectional protocol, use the [JOSH protocol reference](josh-protocol.md).

## 2. Minimal programs

### 2.1 Pure loose source

```allen
export fn main() returns Int {
  40 + 2
}
```

Check or run it:

```sh
cargo run --bin allen -- check examples/answer.allen
cargo run --bin allen -- run examples/answer.allen
```

The compiler synthesizes a capability-free manifest for loose core source.
That manifest grants no effect.

### 2.2 Effectful inline source

```allen
manifest {
  language: "0.1"
  entry: main
  capabilities: [fs.read(workdir)]
}

export async fn main() returns Result<String, FileError> effects [fs.read] {
  let workspace = fs.workspace();
  await fs.read_text(workspace, "message.txt")
}
```

The source effect, manifest request, host policy, provider availability, and
runtime checks are separate. Declaring `effects [fs.read]` does not grant read
authority.

## 3. Source files and declarations

### 3.1 Files and modules

- Source files use `.allen`.
- One source file is one module.
- A source bundle is one root source file plus all files it imports.
- A normalized module path is UTF-8, uses `/`, ends in `.allen`, and contains
  no empty, `.` or `..` component.
- Source cannot import an absolute path or escape its bundle or package root.

Named relative import:

```allen
import { Reading, add } from "./support.allen";
```

Import with a local alias:

```allen
import { Reading as LocalReading } from "./support.allen";
```

Package-qualified import through a dependency alias:

```allen
import { normalize } from "text_utils/src/text.allen";
```

An import may name an exported function, record, enum, or type alias. Version 0.1 has no
private import, wildcard import, namespace import, implicit re-export, explicit
re-export, self-import, or cyclic import graph.

A package module's canonical identity is its package name, exact package
version, and normalized module path. Two packages cannot provide the same name
and version in one resolved graph. Declaration and nominal-type IDs are
deterministic for one exact bundle but may change when that bundle's declaration
graph changes; do not persist or guess them.

### 3.2 Records

```allen
record Point {
  x: Int
  y: Int
}

let point = Point { y: 2, x: 1 };
let x = point.x;
```

- A record declaration creates a structural type and a constructor.
- Record identity depends on exact field names and field types, not the
  declaration name.
- Fields are canonicalized in ascending UTF-8 byte order.
- Declarations, constructors, and patterns reject duplicate fields.
- Construction requires every field and rejects extra fields.

### 3.3 Enums

```allen
enum Reading {
  Empty
  Number(Int)
  Named { label: String, value: Int }
}

let empty = Reading.Empty;
let number = Reading.Number(3);
let named = Reading.Named { label: "cpu", value: 3 };
```

- User enums are nominal. Identically shaped declarations are different types.
- Constructors are qualified with the enum name.
- An enum has at least one variant.
- A tuple variant has at least one payload type.
- Recursive enum payload types are unavailable in version 0.1.
- Expanded value-type depth is limited to 128 aggregate or enum-payload steps.

### 3.4 Type aliases

```allen
type Measurements = List<Int>
type PointView = Point
type LabeledPoint = { label: String, point: PointView }
```

- An alias is a compile-time transparent synonym. It creates no wrapper,
  conversion, runtime identity, or bytecode identity.
- An alias of a record keeps that record's structural type. An alias of an enum
  keeps the enum's existing nominal module type ID.
- Version 0.1 has no generic alias parameters. Put existing generic types on
  the right-hand side, as in `type Measurements = List<Int>`.
- Alias chains may refer forward without source-order dependence. Unknown
  targets and every direct or indirect alias cycle are rejected even when the
  alias is unused.
- Aliases, records, and enums share one module type namespace and duplicate
  rules. `export type` is importable; a private alias is not.
- The declaration ends with its complete right-hand-side type. It has no source
  terminator; do not add a semicolon.

### 3.5 Functions and exports

```allen
fn add(left: Int, right: Int) returns Int {
  left + right
}

export fn main() returns Int {
  return add(40, 2);
}
```

- Named functions and closures require declared parameter and return types.
- An omitted function effect clause is equivalent to an explicit empty clause.
- A body may use a tail expression, `return expression;`, a bare `return;` in
  a `Void` function, or both on different control-flow paths.
- A reachable path cannot fall through a non-`Void` function.
- A function containing `await`, including an `await { ... }` block, must be
  declared `async fn`.
- Calling an `async fn` returns `Future<T>`, where `T` is the declared result.

### 3.6 Closures and callbacks

```allen
let offset = 1;
let add_offset = fn(value: Int) returns Int { value + offset };

fn apply(callback: fn(Int) returns Int, value: Int) returns Int {
  callback(value)
}
```

- A closure captures an immutable local by value.
- Mutable captures, recursive closures, and cyclic closure environments are
  rejected.
- Callback parameter types, return type, and closed effect set match exactly;
  omission denotes the exact empty effect set.
- Version 0.1 has no effect variance or effect polymorphism.
- Function values are not comparable, encodable, map keys, `unknown` contents,
  or `narrow` targets.

### 3.7 Complete version 0.1 lexical and syntactic grammar

This is the complete source grammar for version 0.1. It has the same meaning
as Section 3.4 of the human language specification. The notation uses `[]` for
an optional production, `{}` for repetition, quoted text for literal tokens,
and `one of` for a choice. Lexical whitespace may appear between tokens. A
trailing comma is accepted in every comma-separated source list shown below.

#### Lexical rules

Source is valid UTF-8. Lexical whitespace consists of space, tab, carriage
return, line feed, and comments. It separates tokens and otherwise has no
meaning. A comment may appear anywhere whitespace is legal, including between
tokens on one line and inside an inline source manifest.

`//` starts a line comment. The comment ends immediately before LF, CRLF, a
lone CR, or end of file. A line terminator is not part of the comment; it
remains whitespace and participates in source-location accounting. `/*` starts
a block comment and `*/` ends it. Block comments nest in last-opened,
first-closed order. The lexer MUST scan nested block comments iteratively and
MUST accept at most 128 simultaneously open block comments. Existing
source-size limits include all comment bytes and bound total comment text.

Comment text may contain any valid UTF-8. Comment delimiters have no meaning
inside String, Bytes, or template literal segments. Within a line comment, all
delimiter-like text is inert until its line terminator or end of file. Literal
delimiters, `//`, quotes, backslashes, and braces have no meaning inside a
block comment; only `/*` and `*/` change its nesting depth. An unterminated
block comment produces exactly one stable diagnostic whose primary span is the
two-byte `/*` of the most recently opened block comment still unclosed at end
of file. Attempting to open a 129th block comment produces exactly one stable
diagnostic spanning that two-byte `/*`. All source spans are half-open UTF-8
byte spans into the original, unmodified source.

Comments create no source value and their text is not retained in bytecode,
canonical values, entry boundaries, effects, or audit data. `///`, `//!`,
`/**`, and `/*!` are ordinary comments: version 0.1 gives them no documentation
semantics and does not retain them as documentation.

An `identifier` is an ASCII letter or `_`, followed by zero or more ASCII
letters, decimal digits, or `_`. Identifiers are case-sensitive.

The reserved words are `as`, `async`, `await`, `break`, `continue`, `effects`,
`else`, `enum`, `export`, `false`, `fn`, `for`, `from`, `if`, `import`, `in`,
`let`, `loop`, `manifest`, `map`, `match`, `mut`, `prompt`, `record`, `return`,
`returns`, `spawn`, `true`, `type`, and `while`. `None`,
`Some`, `Ok`, `Err`, the
built-in type names, and standard-library names are not general declarations
when their specified meaning is required. A program cannot use `any`,
`undefined`, or `null` as a value or type.

An `int-literal` is one or more decimal digits and denotes an `Int` in range;
its sign is supplied by unary `-`. A `float-literal` is decimal digits, `.`,
decimal digits, and an optional exponent `e` or `E`, optional `+` or `-`, and
one or more decimal digits. `NaN`, `Infinity`, and `-Infinity` are display
spellings, not source literals.

A `string-literal` is `"` followed by zero or more non-control Unicode scalar
values other than `"` and `\\`, followed by `"`. Its only escapes are
`\\"`, `\\\\`, `\\n`, `\\r`, `\\t`, `\\0`, `\\b`, and `\\f`. It cannot
contain an unescaped line break or control character. A `bytes-literal` starts
with `b"`, ends with `"`, and contains only printable ASCII other than `"` and
`\\`, or one of those same escapes plus `\\xHH`, where each `H` is an ASCII
hexadecimal digit. Bytes literals cannot contain an unescaped line break or
non-ASCII byte. In particular, `\\xHH` is not a String escape, and backtick
strings use the template rules below.

A template begins and ends with a backtick. Literal segments accept the
ordinary String escapes plus these added exact sequences:

```text
\`
\${
```

`\\` is the escaped backslash. An unescaped backtick ends the template and an unescaped `${` opens
an interpolation. Other escapes, unescaped line breaks, and unescaped control
scalars are invalid. Comment delimiters and braces in a literal segment are
text. Inside interpolation, normal comments, nested braces, and nested
templates apply. Interpolations evaluate once from left to right and each must
have type `String`.

The lexical terminal `template-text-scalar` below means any permitted
non-control Unicode scalar other than backtick or backslash, with `$` permitted
only when it is not immediately followed by `{`. The lexical terminal
`template-escape` is this exact closed set:

```text
\"
\\
\n
\r
\t
\0
\b
\f
\`
\${
```

An `effect-id` has one or more lowercase ASCII segments separated by `.`. A
segment starts with `a` through `z` and continues with lowercase ASCII letters,
digits, or `_`. An effect ID may end in `@` and a positive decimal major
version without a leading zero. `fs.read` and `tool.github.create_issue@2` are
examples.

#### Grammar

```text
source             = [ inline-manifest ] { import-declaration }
                     { declaration } EOF ;
inline-manifest    = "manifest" "{" manifest-field { [ "," ] manifest-field }
                     [ "," ] "}" ;
manifest-field     = "language" ":" string-literal
                   | "entry" ":" identifier
                   | "capabilities" ":" "[" [ capability { "," capability } [ "," ] ] "]"
                   | "http_origins" ":" "[" [ string-literal { "," string-literal } [ "," ] ] "]"
                   | "tools" ":" "{" "required" ":" "["
                     [ tool-requirement { "," tool-requirement } [ "," ] ] "]" [ "," ] "}" ;
capability         = effect-id [ "(" identifier ")" ] ;
tool-requirement   = "{" "name" ":" string-literal ","
                     "version" ":" string-literal [ "," ] "}" ;
import-declaration = "import" "{" import-name { "," import-name } [ "," ] "}"
                     "from" string-literal ";" ;
import-name        = identifier [ "as" identifier ] ;
declaration        = record-declaration | enum-declaration | type-alias-declaration
                   | function-declaration ;
record-declaration = [ "export" ] "record" identifier "{"
                     [ record-field { [ "," ] record-field } [ "," ] ] "}" ;
record-field       = identifier ":" type ;
enum-declaration   = [ "export" ] "enum" identifier "{"
                     enum-variant { [ "," ] enum-variant } [ "," ] "}" ;
enum-variant       = identifier
                   | identifier "(" type { "," type } [ "," ] ")"
                   | identifier "{" [ record-field { [ "," ] record-field } [ "," ] ] "}" ;
type-alias-declaration = [ "export" ] "type" identifier "=" type ;
function-declaration = [ "export" ] [ "async" ] "fn" identifier [ generic-parameters ]
                     "(" [ parameter { "," parameter } [ "," ] ] ")" "returns" type
                     [ effect-clause ] body ;
generic-parameters = "<" generic-parameter { "," generic-parameter } [ "," ] ">" ;
generic-parameter  = identifier ":" "Eq" ;
parameter          = identifier ":" type ;
effect-clause      = "effects" "[" [ effect-id { "," effect-id } [ "," ] ] "]" ;
body               = "{" { statement } [ expression ] "}" ;
statement          = ( "let" | "mut" ) identifier [ ":" type ] "=" expression ";"
                   | identifier ( "=" | "+=" | "-=" | "*=" | "/=" | "%=" ) expression ";"
                   | "return" [ expression ] ";"
                   | conditional-expression
                   | while-statement | loop-statement | for-statement
                   | "break" ";" | "continue" ";" ;
while-statement    = "while" "(" expression ")" body ;
loop-statement     = "loop" body ;
for-statement      = "for" loop-binding "in" expression [ ".." expression ] body ;
loop-binding       = identifier | "_" | "(" loop-binding-item ","
                     [ loop-binding-item { "," loop-binding-item } [ "," ] ] ")" ;
loop-binding-item  = identifier | "_" ;
type               = named-type | generic-type | tuple-type | record-type | function-type ;
named-type         = identifier { "." identifier } ;
generic-type       = ( "List" | "Option" | "Future" | "Task" | "Prompt" )
                     "<" type ">"
                   | ( "Map" | "Result" ) "<" type "," type ">" ;
tuple-type         = "(" [ type "," [ type { "," type } [ "," ] ] ] ")" ;
record-type        = "{" [ record-field { [ "," ] record-field } [ "," ] ] "}" ;
function-type      = "fn" "(" [ type { "," type } [ "," ] ] ")" "returns" type
                     [ effect-clause ] ;
expression         = disjunction ;
disjunction        = conjunction { "||" conjunction } ;
conjunction        = equality { "&&" equality } ;
equality           = comparison { ( "==" | "!=" ) comparison } ;
comparison         = addition { ( "<" | "<=" | ">" | ">=" ) addition } ;
addition           = multiplication { ( "+" | "-" ) multiplication } ;
multiplication     = unary { ( "*" | "/" | "%" ) unary } ;
unary              = ( "!" | "-" | "await" | "spawn" ) unary | postfix ;
postfix            = primary { "[" expression "]" | "." identifier
                     | [ type-argument ] "(" [ expression { "," expression } [ "," ] ] ")"
                     | "?" } ;
type-argument      = "<" type ">" ;
primary            = literal | template-literal | identifier | "map" | "Some" | "Ok" | "Err"
                   | enum-record-constructor | qualified-enum
                   | record-constructor | anonymous-record | list-literal | map-literal | tuple-or-group | match-expression
                   | conditional-expression | closure | prompt-expression | await-block ;
literal            = int-literal | float-literal | string-literal | bytes-literal
                   | "true" | "false" | "None" | "(" ")" ;
template-literal   = "`" { template-segment | template-interpolation } "`" ;
template-segment   = template-text-or-escape { template-text-or-escape } ;
template-text-or-escape = template-text-scalar | template-escape ;
template-interpolation = "${" expression "}" ;
qualified-enum     = identifier "." identifier ;
enum-record-constructor = identifier "." identifier "{" [ record-value-field
                     { [ "," ] record-value-field } [ "," ] ] "}" ;
record-constructor = identifier "{" [ record-value-field
                     { [ "," ] record-value-field } [ "," ] ] "}" ;
anonymous-record   = "{" [ record-value-field
                     { [ "," ] record-value-field } [ "," ] ] "}" ;
record-value-field = identifier [ ":" expression ] ;
list-literal       = "[" [ expression { "," expression } [ "," ] ] "]" ;
map-literal        = "map" "{" [ expression ":" expression
                     { [ "," ] expression ":" expression } [ "," ] ] "}" ;
tuple-or-group     = "(" expression [ "," [ expression { "," expression } [ "," ] ] ] ")" ;
match-expression   = "match" expression "{" match-arm { [ "," ] match-arm } [ "," ] "}" ;
conditional-expression = "if" "(" expression ")" body
                         [ "else" ( conditional-expression | body ) ] ;
match-arm          = pattern "=>" expression ;
pattern            = "_" | "true" | "false" | record-pattern | enum-pattern
                   | "None" | ( "Some" | "Ok" | "Err" ) "(" ( identifier | "_" ) ")" ;
record-pattern     = identifier "{" [ pattern-field { [ "," ] pattern-field } [ "," ] ] "}" ;
enum-pattern       = identifier "." identifier
                   [ "(" ( identifier | "_" ) { "," ( identifier | "_" ) } [ "," ] ")"
                   | "{" [ pattern-field { [ "," ] pattern-field } [ "," ] ] "}" ] ;
pattern-field      = identifier [ ":" ( identifier | "_" ) ] ;
closure            = "fn" "(" [ parameter { "," parameter } [ "," ] ] ")" "returns" type
                     [ effect-clause ] body ;
prompt-expression  = "prompt" "{" prompt-field { [ "," ] prompt-field } [ "," ] "}" ;
prompt-field       = "system" ":" expression | "context" ":" expression
                   | "data" ":" expression | "output" ":" type
                   | "policy" ":" "{" "max_attempts" ":" int-literal [ "," ] "}" ;
await-block        = "await" body ;
```

`Void`, `Bool`, `Int`, `Float`, `String`, `Bytes`, `Never`, `List`, `Map`,
`Option`, `Result`, `Future`, `Task`, `Prompt`, `Workspace`,
`ExternalFsAccess`, `SubAgent`, `ExternalFileRequest`,
`ExternalDirectoryRequest`, `HttpResponse`, `FileError`, `NetworkError`,
`TranscriptPart`, `TranscriptMessage`, `TranscriptSnapshot`, and `unknown` are
the built-in named types. `List<T>`,
`Map<K, V>`, `Option<T>`, `Result<T, E>`, `Future<T>`, `Task<T>`, and
`Prompt<T>` have the arities shown; a user type or generic parameter is a
single-segment `named-type` with its declared arity. Generated tool schema
types use their complete `tools.`-qualified `named-type` and cannot be shortened
or imported. An empty list or map requires an expected
type. A tuple type or value with one member requires its trailing comma.

`mut name = expression;` and `mut name: Type = expression;` are the only
mutable-local declarations. `let mut` is not grammar and is rejected.
Assignment is only `name = expression;` or a numeric compound form
`name op= expression;`; it cannot target a field or index. Compound assignment
requires an existing `mut` local and is never an expression.
Every named function, closure, and callback function type may omit
`effects [...]`; omission is the exact empty effect set. A nonempty callback
effect set must remain explicit.

`Type.Variant`, including `Reading.Empty`, `Reading.Number(value)`, and
`Reading.Named { label: "cpu", value: 3 }`, is the only user-enum spelling.
`Type::Variant` is not ALLEN syntax. `None`, `Some`, `Ok`, and `Err` are the
built-in `Option` and `Result` constructors. A `?` postfix applies only to a
compatible `Result`; its current function must return the same error type.
`await` and `spawn` require their specified `Future`/`Task` operands, and an
`await` block uses the separate `await body` production.

#### Conditional control flow

`if (condition) { when_true } else { when_false }` is an expression. Its
parenthesized condition has type `Bool`; it evaluates exactly once, and exactly
one branch evaluates. Both reachable branches have one exact result type. A
`Never` branch, including `return` or `stop`, is compatible with the other
branch. `else if (...) { ... }` is right-associated shorthand for an `else`
branch containing another conditional. Comments are lexical whitespace, so
comments or formatting between `else` and `if` do not change the parse.

An `if` without `else` has type `Void`; its true body must produce `Void` or
`Never`, and its absent false body is exactly `()`. Any `Void`-valued
conditional, with or without `else`, may be used as a statement before later
declarations, assignments, or returns without a semicolon. A value-producing
conditional cannot be discarded; this does not permit arbitrary expression
statements or implicit value discarding. Branch bindings do not escape. An
outer `mut` binding may be reassigned in either or both branches only with its
exact type.

`return;` returns `()` and is valid only in synchronous or asynchronous
functions declared `returns Void`; `return expression;` is unchanged. A return in a
conditional returns from the enclosing function. Conditional effects are the
conservative union of the condition and both branches, while the skipped branch
does no provider, capability, task, allocation, trap, or `stop` work.

#### Loops and iteration

`while (condition) { body }` reevaluates its `Bool` condition before every
iteration. `loop { body }` repeats until control leaves it. Both forms, and
every `for`, have source type `Void`; a loop with no reachable exit makes later
code unreachable.

Every syntactically continuing loop body must itself have type `Void`. A body
tail of type `Never`, such as `return` or `stop`, is accepted because it does
not continue to the loop transfer. A value-producing body tail is not
implicitly discarded merely because the enclosing loop has type `Void`.

`for binding in iterable { body }` evaluates `iterable` exactly once and keeps
that immutable snapshot. A `List<T>` yields its elements in ascending index
order, `Bytes` yields `Int` values from 0 through 255 in ascending index order,
`String` yields one-scalar Strings in Unicode scalar order, and `Map<K, V>`
yields `(K, V)` entries in canonical key order. String iteration matches
successful `string.get` calls from zero through `length(value) - 1`. A range
loop, `for binding in start..end { body }`, evaluates `start` and then `end`
exactly once and yields the ascending half-open `Int` range without overflowing
at integer boundaries. `..` is available only in a `for` clause.

A loop binding is an identifier, `_`, or a one-level tuple of identifiers and
wildcards. It is immutable and scoped to one iteration. Tuple arity must match
the iterated element type and duplicate identifiers are rejected. `break;` and
`continue;` target only the innermost lexical loop; they are invalid outside a
loop, do not cross a function or closure boundary, and have no labels or
values. An outer `mut` local may be reassigned by a loop body.

Loop effects conservatively include the condition or iterable and bounds plus
the body. Runtime effects still occur only along executed iterations. An
affine `Future`, `Task`, `SubAgent`, or other must-consume value cannot remain
live across a back edge or `continue`; every continuing and breaking path must
agree on ownership state. `return`, `break`, and `continue` from inside an
`await` scope retain the normal cancellation, join, cleanup, and stopped-outcome
rules for scopes they leave.

#### Operator precedence and associativity

| Precedence, tight to loose | Operators or form | Associativity |
|---|---|---|
| 1 | call, index `[]`, field `.`, postfix `?` | left |
| 2 | prefix `!`, numeric `-`, `await`, `spawn` | right |
| 3 | `*`, `/`, `%` | left |
| 4 | `+`, `-` | left |
| 5 | `<`, `<=`, `>`, `>=` | left |
| 6 | `==`, `!=` | left |
| 7 | `&&` | left |
| 8 | `||` | left |
| statement only | `=`, `+=`, `-=`, `*=`, `/=`, `%=` on a mutable local | not an expression |

Calls, indexing, field access, and `?` chain left to right. Type arguments are
part of a call only; they do not make `<` or `>` expression operators. Only
`narrow<T>` and typed response operations accept explicit type arguments;
ordinary generic calls infer them. ALLEN evaluates the left operand of `&&`
and `||` first. It evaluates the right operand of `&&` only when left is true,
and the right operand of `||` only when left is false.

#### Reserved syntax

The following forms are deliberately unavailable and MUST NOT be treated as
extensions of this grammar: `try`, `catch`, `finally`, `throw`, or
general exception handling. Pattern guards, OR patterns, collection patterns,
`let mut`, `Type::Variant`, and implicit conversions are also not version 0.1
syntax.

## 4. Statements, expressions, and operators

### 4.1 Bindings and assignment

```allen
let count = 3;
let total: Int = count + 1;
mut retries = 0;
retries = retries + 1;
```

- `let` creates an immutable binding.
- `mut` permits reassignment of the binding only.
- Assignment syntax is `name = expression;` or a numeric compound assignment
  `name op= expression;`.
- Local declarations and assignments require semicolons.
- Duplicate locals, use before declaration, assignment to `let`, assignment
  with a different type, and a compound target that is not a `mut` local are
  rejected.
- A compound assignment reads the old local once, evaluates its right operand
  once, applies the checked operation, and writes only on success. `+=`, `-=`,
  `*=`, and `/=` accept matching `Int` or `Float`; `%=` accepts `Int` only.
- Lists, maps, tuples, records, and enums remain immutable values. `mut` does
  not permit element or field mutation.

### 4.2 Literals and indexing

| Value | Syntax |
|---|---|
| Void / empty tuple | `()` |
| list | `[a, b]` |
| map | `map { key: value }` |
| tuple | `(a, b)` |
| one-element tuple | `(a,)` |
| template String | `` `status: ${status}` `` |

A trailing comma is allowed. An empty list or map needs an expected type from a
type annotation or function return type.

Indexing rules:

- `List<T>[Int] returns T`;
- `Bytes[Int] returns Int` in the range 0 through 255;
- `Map<K, V>[K] returns V`;
- a tuple index is a nonnegative integer literal; and
- `String` indexing is unavailable; use `string.get` for safe scalar access.

Any invalid sequence index is a terminal `index.out_of_bounds` trap. A missing
map key is a terminal `map.key_not_found` trap.

`length(String)`, `length(Bytes)`, `length(List<T>)`, and
`length(Map<K, V>)` return `Int`; String length counts Unicode scalar values.
Map traversal is available through `for`; there is no source-visible map cursor
or mutable/unordered indexed-access API.

#### Safe data operations

The language provides these pure, capability-free alternatives to the trapping
collection and integer forms:

```allen
list.get<T>(values: List<T>, index: Int) returns Option<T>
list.try_set<T>(values: List<T>, index: Int, value: T) returns Option<List<T>>
bytes.get(values: Bytes, index: Int) returns Option<Int>
map.get<K, V>(values: Map<K, V>, key: K) returns Option<V>
int.checked_add(left: Int, right: Int) returns Option<Int>
int.checked_sub(left: Int, right: Int) returns Option<Int>
int.checked_mul(left: Int, right: Int) returns Option<Int>
int.checked_neg(value: Int) returns Option<Int>
int.checked_div(left: Int, right: Int) returns Option<Int>
int.checked_rem(left: Int, right: Int) returns Option<Int>
```

`list.get` and `bytes.get` return `None` for a negative or out-of-range index.
`list.try_set` returns `None` for the same invalid indexes; otherwise it returns
`Some` containing a new list with exactly that element replaced. `map.get`
returns `None` when the key is absent. The checked integer operations return
`None` for overflow, and division and remainder also return `None` for a zero
right operand. The one special remainder case is
`int.checked_rem(Int::MIN, -1) == Some(0)`.

These calls do not mutate or consume their inputs. Existing aliases continue
to observe the original immutable aggregates. An invalid safe operation creates
no aggregate result and incurs no aggregate-allocation charge. A successful
`list.try_set` constructs and charges a fresh complete list before publishing
`Some`; other successful calls construct their `Some` result. Allocation or
execution-budget exhaustion remains the uncatchable terminal
`resource.limit`, so a safe operation cannot promise a value after exhaustion.

The corresponding trapping alternatives remain available: `values[index]` traps with
`index.out_of_bounds`, missing `map[key]` traps with `map.key_not_found`, an
invalid `list.set` index traps with `index.out_of_bounds`, and ordinary checked
integer operators trap with `arithmetic.overflow` or
`arithmetic.division_by_zero`. Safe operations return `Option`; they never
catch, convert, or resume one of those traps.

### 4.3 Operators

| Operation | Allowed types | Rule |
|---|---|---|
| `+`, `-`, `*`, `/` | two values of the same numeric type | No mixed numeric arithmetic or coercion. |
| `%` | two `Int` values | Truncating-division remainder; no Float remainder. |
| unary `-` | numeric value | `Int` negation is checked. |
| `==`, `!=` | equal concrete types with equality | Equality is recursive for aggregates. |
| ordered comparison | `Int`, `Float`, `String`, `Bytes` | Both operands have the same type. |
| `!`, `&&`, `||` | `Bool` | The left operand evaluates first. `&&` skips right when left is false; `||` skips right when left is true. Static effects still include both operands. |
| postfix `?` | `Result<T, E>` | Propagates the exact `Err(E)` from the current function. |

Section 3.7 is the complete lexical grammar and operator-precedence table.
Do not infer additional token, escape, or comment rules from JavaScript.
Templates are ordinary Strings, require String interpolations, and do not add
String `+`, `+=`, indexing, implicit conversion, or formatting operators.

### 4.4 Match expressions

```allen
match reading {
  Reading.Empty => 0
  Reading.Number(value) => value
  Reading.Named { value, label: _ } => value
}
```

Version 0.1 patterns include Boolean patterns, record patterns, enum-variant
patterns, `Option` and `Result` variants, and wildcard `_`. A payload subpattern
is a new local binding or `_`.

- Match arms have one exact result type; a `Never` arm is compatible.
- A match over a closed type is exhaustive.
- Duplicate cases, unreachable cases, and cases after `_` are rejected.
- Arm bindings exist only in that arm.
- Commas between arms are optional.
- Guards, OR patterns, and collection patterns are unavailable.

### 4.5 Conditional expressions

```allen
fn choose(ready: Bool) returns Int {
  if (ready) { 1 } else if /* still whitespace */ (false) { 2 } else { 3 }
}
```

- The condition is parenthesized, has type `Bool`, and evaluates once.
- Exactly one branch evaluates; both continuing branches have the same exact
  type, except that `Never` is compatible.
- An else-less conditional is `Void` when its true body is `Void` or `Never`.
  Any conditional may stand before later statements only when its result is
  `Void`.
- Branch-local names do not escape. Each continuing branch must agree on affine
  `Future`/`Task` and must-consume state plus `SubAgent` binding availability
  and lexical scope; a `Never` branch contributes no join state.
- Leaving an `await` block through either return form retains normal structured
  cleanup, cancellation, joining, and stopped-outcome behavior.

## 5. Types and values

### 5.1 Type inventory

| Type | Meaning and important restrictions |
|---|---|
| `Bool` | `true` or `false`. |
| `Int` | Signed 64-bit checked integer. |
| `Float` | IEEE 754 binary64 with canonical NaN handling. |
| `String` | Valid UTF-8 text. |
| `Bytes` | Arbitrary bytes. |
| `Void` | The single value `()`. |
| `Never` | No values; an expression that cannot return normally. |
| `List<T>` | Homogeneous immutable list. |
| `Map<K, V>` | Immutable canonical-order map; `K` is `Bool`, `Int`, `String`, or `Bytes`. |
| tuples | Fixed length with one type per position. |
| records | Structural field types. |
| user enums | Nominal tagged values. |
| `Option<T>` | `None` or `Some(T)`; never nullable shorthand. |
| `Result<T, E>` | `Ok(T)` or `Err(E)`. |
| `unknown` | A value that cannot be used concretely until narrowed. |
| `Future<T>` | One lazy affine asynchronous computation. |
| `Task<T>` | One started affine asynchronous computation. |
| `Workspace` | Opaque execution-scoped filesystem capability. |
| `ExternalFsAccess` | Exactly `Read`, `Write`, or `ReadWrite`. |
| `Prompt<T>` | Structured request whose expected response type is `T`. |
| `SubAgent` | Opaque execution-scoped sub-agent handle. |

`any`, `undefined`, `null`, and `String?`-style nullable types are forbidden.
`Never` is type-compatible wherever another expression type is required. This
is static compatibility, not a runtime coercion.

### 5.2 Integers and floats

`Int` addition, subtraction, multiplication, division, remainder, and negation
are checked. Overflow is terminal `arithmetic.overflow`. Division or remainder by
zero is terminal `arithmetic.division_by_zero`. Integer division truncates toward
zero. For nonzero `right`, `left % right` is `left - (left / right) * right`,
has the sign of `left` or is zero, and `Int::MIN % -1` is zero without
overflow. Use the exact `int.checked_*` operations in Section 4.2 when invalid
data should produce `None` instead of a terminal trap.

`Float` rules:

- values use IEEE 754 binary64;
- NaN payloads become quiet NaN `0x7ff8000000000000`;
- NaN is unequal to every value;
- positive zero equals negative zero;
- ordered comparison with NaN is false;
- division follows IEEE 754, including division by zero; and
- canonical text uses shortest locale-independent round-trip form, `NaN`,
  `Infinity`, `-Infinity`, preserved `-0.0`, and `.0` for finite integral
  values when permitted by shortest-round-trip formatting.

### 5.3 Maps and deterministic ordering

Map keys are ordered as follows:

- `Bool`: `false`, then `true`;
- `Int`: signed numeric order;
- `String`: lexicographic UTF-8 byte order; and
- `Bytes`: lexicographic unsigned-byte order.

Duplicate keys terminally trap with `map.duplicate_key`. Construction order does not change
map equality, iteration, display, or serialization.

### 5.4 `Option`, `Result`, and `?`

```allen
let present: Option<Int> = Some(3);
let absent: Option<Int> = None;
let success: Result<Int, String> = Ok(3);
let failure: Result<Int, String> = Err("failed");
```

`Some(value)` usually infers `T`. `None` needs an expected `Option<T>`.
`Ok(value)` and `Err(value)` need an expected `Result<T, E>` for the type
parameter not supplied by the payload.

Postfix `?` is valid only on `Result<T, E>` inside a function returning
`Result<U, E>` with the exact same error type. It unwraps `Ok` or returns the
original `Err` unchanged. It performs no error conversion.

### 5.5 `unknown`

```allen
let wrapped = to_unknown(3);
let value: Option<Int> = narrow<Int>(wrapped);
```

- No concrete operation accepts `unknown` directly.
- `to_unknown(value) returns unknown` is the explicit total wrapper.
- `narrow<T>(value: unknown) returns Option<T>` performs exact recursive runtime
  shape validation.
- `T` must be complete and concrete.
- Empty collections can validate when they contain no conflicting element.
- There is no other cast from `unknown`.

### 5.6 Explicit conversions

| Operation | Result |
|---|---|
| `to_float(Int)` | `Float`, round-to-nearest ties-to-even. |
| `to_string(Bool\|Int\|Float\|String)` | Canonical scalar `String`. |
| `to_bytes(String)` | UTF-8 `Bytes`. |

There is no implicit numeric, text, byte, or collection conversion.

### 5.7 String operations

String indexes and lengths count Unicode scalar values unless the operation
says `byte`; they never count grapheme clusters. These operations are exact,
locale-independent, and pure:

```allen
length(value: String) returns Int
string.byte_length(value: String) returns Int
string.concat(left: String, right: String) returns String
string.get(value: String, index: Int) returns Option<String>
string.slice(value: String, start: Int, end: Int) returns Option<String>
string.find(value: String, needle: String) returns Option<Int>
string.contains(value: String, needle: String) returns Bool
string.starts_with(value: String, prefix: String) returns Bool
string.ends_with(value: String, suffix: String) returns Bool
string.split(value: String, separator: String) returns Option<List<String>>
string.join(values: List<String>, separator: String) returns String
string.trim_ascii(value: String) returns String
string.from_utf8(value: Bytes) returns Option<String>
```

`get` returns one scalar String; invalid indexes return `None`. `slice` accepts
exactly `0 <= start <= end <= length(value)`. `find` returns the first scalar
index and returns `Some(0)` for an empty needle. `split` returns `None` only for
an empty separator and otherwise preserves empty fields. `join` preserves list
order. `trim_ascii` removes only ASCII space, tab, LF, CR, form feed, and
vertical tab. `from_utf8` succeeds only for wholly valid UTF-8. Matching does
not normalize, fold case, or use locale rules.

### 5.8 Source-boundary JSON

Entry input and output use exact JSON validation:

| ALLEN value | JSON form |
|---|---|
| `Void` | `null` |
| finite scalar | Exact JSON scalar of the declared type. |
| non-finite `Float` | Canonical string such as `"NaN"`. |
| `Bytes` | `{ "$bytes": "<canonical-base64>" }` |
| list or tuple | JSON array. |
| map | Sorted array of `[key, value]` pairs. |
| record | Exact JSON object. |
| `Option`, `Result`, user enum | `{ "tag": String, "value": ... }`; omit `value` only for a payloadless variant. |

Unknown fields, missing fields, duplicate or unsorted map keys, implicit
coercion, and out-of-limit values are invalid. Callable, `Future`, `Task`,
`Workspace`, `SubAgent`, and `Never` values cannot cross an entry boundary.

Canonical binary value tags and exact VM allocation charges are portability and
implementation contracts. They do not change source authoring and are not
duplicated here; consult Sections 3.1 and 3.2 of the human specification when
working on artifacts or runtimes.

## 6. Generics and effects

### 6.1 Generics

```allen
fn same<T: Eq>(left: T, right: T) returns Bool {
  left == right
}
```

- Version 0.1 has one reusable constraint: `Eq`.
- `Eq` permits `==` and `!=`.
- A complete concrete type satisfies `Eq` when equality exists for it.
- `Never`, `unknown`, a function type, and a type containing `unknown` or a
  function do not satisfy `Eq`.
- Type arguments for ordinary generic calls are inferred from value arguments.
- Every use of the same type parameter infers the same exact type.
- The compiler monomorphizes used concrete instantiations under bounded count
  and type-depth limits.

Version 0.1 has no explicit type arguments for ordinary generic calls,
user-declared constraints, higher-kinded types, specialization, or generic
recursion. The `<T>` syntax used by special built-ins such as `narrow<T>` and
typed response operations does not imply general explicit generic calls.

### 6.2 Effect syntax and propagation

```allen
export async fn load(workspace: Workspace) returns Result<String, FileError>
  effects [fs.read] {
  await fs.read_text(workspace, "input.txt")
}
```

An effect ID has lower-case ASCII dot-separated segments. Each segment starts
with `a` through `z` and continues with `a` through `z`, `0` through `9`, or
`_`. A versioned effect may end with `@` and a positive decimal major version
without a leading zero. Effect sets are sorted unique canonical IDs.

- An omitted or explicit empty effect clause declares pure code.
- Every named function and closure may omit its effect clause; omission is
  equivalent to an explicit empty clause.
- Every explicit effect clause is a maximum contract. A declared superset is
  valid.
- A function cannot use an effect outside its contract.
- A pure function cannot call or capture an effectful callable.
- Callback types include their exact effect sets; omission means the empty set.
- Calling an async function contributes its declared effects immediately even
  though its returned future is lazy.
- A loop's static effect set is the union of its condition or iterable and
  bounds with its body, regardless of whether the body executes at runtime.
- `task.spawn`, `debug.inspect`, and `capability.inspect` are local effects,
  not host capabilities.
- `stop` has no effect.

An effect declaration is neither permission nor proof of nondeterminism. The
manifest requests authority, and the host/runtime compute an equal-or-narrower
effective capability set.

## 7. Async execution and task ownership

### 7.1 Futures

- Calling `async fn f() returns T` returns `Future<T>`.
- The call captures its arguments but does not start the body.
- `await future` consumes it, starts it in the current task, and removes exactly
  one `Future` layer.
- `spawn future` consumes it, starts it as a task, and returns `Task<T>`.
- A future is affine: it cannot be copied or consumed twice.
- An unstarted future may be discarded only when it owns no started task.
- A future that captures ownership of a live task must be consumed or
  transferred.
- A future created directly by a provider operation or generated tool call is
  must-consume but remains lazy; it must be awaited, spawned, or transferred.
- Async types do not flatten automatically. An async function declared to
  return `Task<T>` is called as `Future<Task<T>>`.

### 7.2 Tasks

```allen
async fn left() returns Int { 40 }
async fn right() returns Int { 2 }

export async fn main() returns Int effects [task.spawn] {
  await {
    let left_task = spawn left();
    let right_task = spawn right();
    let left_value = await left_task;
    let right_value = await right_task;
    return left_value + right_value;
  }
}
```

- `spawn` has effect `task.spawn`.
- `Task<T>` is affine and live until `await` consumes it or ownership moves.
- Ownership can move through a local binding, function argument, or function
  return.
- Copy, discard, use after move, or ownership loss on any control-flow path is
  rejected.
- A local reference moves a future or task. A previous binding cannot be used
  after the move, and a consuming await invalidates its one live binding.
- Continuing branches must agree on ownership, live affine values cannot cross
  loop back edges, and closures cannot capture futures or tasks. A second
  reachable await is therefore rejected through moved local names and loops as
  well as in straight-line code. Mutually exclusive branches may each await the
  incoming handle when all continuing branches leave it consumed.
- Futures and tasks cannot appear in aggregates, mutable bindings, closure
  captures, canonical encoding, or `unknown`.
- An entry point cannot return a future or task.
- There are no detached tasks.
- `await task` consumes the already-started handle and removes exactly one
  `Task` layer.
- `await value` has the same meaning inside or outside a structured await
  block; the block changes ownership and cleanup, not the await's result type.
- Only explicit `await` suspends the current task. The runtime cannot add a
  hidden suspension point that changes program behavior.

### 7.3 Structured `await` blocks

`await { ... }` is an expression and a structured-concurrency boundary.

- It owns tasks created while the block is current.
- Blocks may nest. New tasks belong to the innermost current block, and inner
  cleanup finishes before evaluation resumes in the outer block.
- Owned tasks cannot escape the block.
- Normal exit joins unfinished tasks in ascending task-ID order before leaving.
- Normal exit, explicit return, `?`, `break`, and `continue` join without
  cancelling. A live task with a non-affine result may be left for this implicit
  join; outside its owning block it must be awaited or transferred.
- The lowest failed task ID selects the reported implicit-join error.
- A task whose result is a future or task must be awaited explicitly; an
  implicit join cannot discard the affine result.
- An ordinary `Result::Err`, including one propagated by `?`, does not cancel
  siblings. An explicitly awaited task failure propagates through its awaiter.
- Runtime error, timeout, cancellation, or `stop` prevents new work, cancels
  every unfinished task, and joins all owned tasks within a finite cleanup
  budget. Control cannot leave before cleanup completes.
- Late cancelled results are discarded.
- Cleanup-budget exhaustion during `stop` leaves the outcome `Stopped`; on
  another exit it reports a stable resource failure.
- Ready tasks progress deterministically by ascending task ID with round-robin
  rotation.

### 7.4 Task diagnostics

`allen.internal.task_snapshot(task)` observes a live task without consuming it
and has effect `debug.inspect`. Its exact result is:

```allen
{
  function: String,
  id: Int,
  location: Option<String>,
  owner_id: Int,
  state: String,
}
```

`state` is `ready`, `waiting`, `completed`, or `failed`. IDs are deterministic
only within an execution. `location`, when debug information exists, is a
canonical module path plus a UTF-8 byte span. Cancelled tasks have no live
source handle and appear only in the host lifecycle trace. Do not use diagnostic
fields as persistent identity. The operation does not expose process/thread
IDs, addresses, raw scheduler scopes, task results, error details, stop reasons,
captured values, or host handles.

## 8. Standard operations and providers

An `async fn` declaration below means calling the operation returns a lazy
`Future<DeclaredResult>`. The effect is required at the call site even before
the future starts.

### 8.1 Filesystem

```allen
fn fs.workspace() returns Workspace

async fn fs.read_text(workspace: Workspace, path: String)
  returns Result<String, FileError> effects [fs.read]
async fn fs.read_bytes(workspace: Workspace, path: String)
  returns Result<Bytes, FileError> effects [fs.read]
async fn fs.write_text(workspace: Workspace, path: String, value: String)
  returns Result<Void, FileError> effects [fs.write]
async fn fs.write_bytes(workspace: Workspace, path: String, value: Bytes)
  returns Result<Void, FileError> effects [fs.write]
async fn fs.list(workspace: Workspace, path: String)
  returns Result<List<String>, FileError> effects [fs.read]
```

`FileError` is exactly `{ code: String, message: String }`. `Workspace` is an
opaque execution-scoped capability. Source cannot construct, compare, encode,
narrow, widen, or cross an entry boundary with it. Copies refer to the same
capability-table entry.

The host selects one working directory before execution. The default sandbox
grants declared `fs.read(workdir)` and `fs.write(workdir)` only for that
directory and its descendants; host policy may remove or narrow either grant.
Paths are normalized relative language paths. Exact `.` selects the workspace
root; no other `.` component is valid. `fs.list` returns UTF-8 names in
ascending byte order. The runtime prevents `..` traversal, link escape,
equivalent path aliases, and check/open races. A required capability denial
fails before execution. An evaluated optional denial returns `FileError` with
code `fs.permission_denied`.

### 8.2 External filesystem requests

```allen
record ExternalFileRequest { access: ExternalFsAccess path: String reason: String }
record ExternalDirectoryRequest { access: ExternalFsAccess path: String reason: String recursive: Bool }
async fn permission.request_file(request: ExternalFileRequest)
  returns Result<Workspace, PermissionError>
  effects [permission.request_external_fs]

async fn permission.request_directory(request: ExternalDirectoryRequest)
  returns Result<Workspace, PermissionError>
  effects [permission.request_external_fs]
```

`ExternalFsAccess` variants are exactly `Read`, `Write`, and `ReadWrite`. The
request effect grants authority to ask, not authority to access. The request
routes through the invoking agent; user approval may be a prerequisite but
does not itself create a capability. If no invoking agent exists, evaluation
returns `Err(PermissionError { code: "permission.unavailable", ... })`.

An issued grant is unforgeable, no wider than requested, valid only for this
execution, and never persisted. A file grant exposes the file only as `.`. A
nonrecursive directory grant exposes its root and direct-child files; a
recursive grant exposes descendants. A file grant cannot be listed or widened
to its parent.

The request path is an absolute native path, and effective limits bound the
maximum byte scope. A directory request names an existing directory. Before
asking for approval, the runtime resolves and retains an existing target's
descriptor and identity. For a missing write-capable file target, it retains
the parent descriptor, final component, and expected absence. Source cannot use
path replacement or a time-of-check/time-of-use race to redirect the grant.

Policy denial returns `Err(PermissionError { code: "permission.denied", ... })`.

### 8.3 HTTP GET

```allen
async fn http.get(url: String) returns Result<{
  body: Bytes
  final_url: String
  headers: Map<String, List<String>>
  status: Int
}, NetworkError> effects [net.http_get]
```

`NetworkError` is exactly `{ code: String, message: String }`. A non-2xx status
is not a transport error.

Version 0.1 accepts absolute HTTPS URLs only. It provides no browser, DOM, raw
socket, request body, source-provided header, ambient cookie, authorization
header, client certificate, or host credential. It rejects credentials in
URLs. The runtime sends only its fixed `User-Agent` and
`Accept-Encoding: gzip`. It accepts identity or gzip response bodies and
rejects unknown, nested, or multiple content encodings. Redirects are
revalidated and remain GET. After name resolution, destination policy denies
loopback, link-local, private, multicast, and host-metadata addresses and
protects against DNS rebinding.

The host enforces limits for connection time, time to first byte, idle time,
total time, redirect count, DNS results, header count and size, compressed and
decoded body sizes, and decompression ratio. Do not assume a request can exceed
those limits merely because its source types are valid.

A package requesting `net.http_get` declares canonical HTTPS origins. A
standalone launch separately allows each origin. Both sets must include the
normalized destination. Other raw network operations are denied; use a typed
tool for other network behavior.

### 8.4 Invoking-agent operations

```allen
async fn agent.message(message: String) returns Result<Void, AgentError>
  effects [agent.message]
async fn agent.ask<T>(request: Prompt<T>) returns Result<T, AgentError> effects [agent.ask]
async fn agent.transcript(query: { limit: Int }) returns Result<TranscriptSnapshot, AgentError>
  effects [agent.transcript]
```

- `agent.message` targets only the invoking agent and waits for delivery
  acceptance, not a content reply.
- `agent.ask` targets and retries with the same invoking agent.
- `agent.ask` is used as `agent.ask<T>(request: Prompt<T>)`.
- All three operations return `Err(AgentError { code: "agent.unavailable", ... })`
  when evaluated without an invoking session. They do not fall back to a model or sub-agent.
- Missing an invoking agent does not fail program loading.

`agent.transcript` accepts only `limit`, from 1 through 100, and returns oldest
messages first. `TranscriptSnapshot` fields are:

```allen
{
  snapshot_id: String,
  session_id: String,
  policy_version: String,
  captured_at: String,
  truncated: Bool,
  messages: List<TranscriptMessage>,
}
```

`TranscriptMessage` has `id: Option<String>`, `role: String`,
`time: Option<String>`, and `content: List<TranscriptPart>`. Roles are exactly
`user`, `assistant`, `system_visible`, or `tool`. Part kinds are exactly `text`,
`json`, `tool_call`, `tool_result`, `attachment`, `redacted`, or `omitted`.
Every part is an exact tagged record and contains only the fields specified for
its kind. Attachments are opaque references. A `redacted` part contains only a
safe reason code. An `omitted` part contains only the omitted content kind and
a positive count. No marker contains removed content. Timestamps are canonical
RFC 3339 UTC with `Z`.

The default view should include visible user and assistant messages, tool calls,
tool results, and attachment references. The host may filter, redact, or omit
content. Programs cannot assume a complete transcript. Hidden system/developer
instructions, hidden reasoning, credentials, and policy-hidden secrets are
never exposed. The runtime validates the complete projected snapshot, its byte
limit, and that the returned `session_id` matches the invoking session before
the program receives it.

### 8.5 Prompts, models, and users

`prompt` is a first-class `Prompt<T>`, not an untyped string alias.

```allen
record Review {
  approved: Bool
  reasons: List<String>
}

let request: Prompt<Review> = prompt {
  system: "Review using only supplied evidence."
  context: "release-candidate"
  data: { diff: "safe", tests: 12 }
  output: Review
  policy: { max_attempts: 3 }
};

let model_reply = (await model.request(request))?;
```

Version 0.1 prompt components are required `system` and `output`, optional
`context` and `data`, and optional `policy`. The only version 0.1 policy field
is `max_attempts`, from 1 through 3 including the initial attempt.

The `system` name is a language segment, not host-level instruction priority.
Host safety, permission, system, developer, and user policy remain above it.
Interpolated non-text values remain structured data unless explicitly converted
to text. Structured providers receive separate segments. Text-only providers
receive canonical `Allen-PROMPT/1` rendering with labeled, length-prefixed
canonical JSON segments followed by an `END` marker. A value cannot create or
terminate a segment because the adapter consumes its declared byte length.

A host may reject an unsupported prompt-policy preference. Provider-specific
preferences must use an extension namespace; do not treat an arbitrary policy
field as portable version 0.1 syntax.

Typed response rules apply to `model.request`, typed `agent.ask`, `user.ask`,
`sub_agent.ask`, and `sub_agent.run`:

- objects reject unknown fields recursively;
- required fields cannot be missing;
- no value is implicitly coerced;
- tagged unions preserve their discriminator;
- `Option<T>` is exactly `{ "tag": "None" }` or
  `{ "tag": "Some", "value": value }`, never `null` or omission;
- each response is validated before the program receives it;
- partial values are never exposed as `T`; and
- a repair receives only bounded JSON Pointer paths and stable-code validation
  issues; and
- repair attempts are bounded by the minimum of prompt `max_attempts`, manifest
  `response_attempts`, the host limit, and a hard maximum of three total
  attempts, including the initial response.

If every attempt fails, the operation returns its typed validation or request
error. It never returns a partially valid value as `T`.

`user.ask` uses an independent user-interaction provider and may work without an
invoking agent. Missing that provider returns `Err(UserError)` with
`user.unavailable`. A model call depends on a model provider, not an invoking agent.

`model.request<T>(request: Prompt<T>) returns Result<T, ModelError>` and
`user.ask<T>(request: Prompt<T>) returns Result<T, UserError>` are the exact typed
forms. No provider fallback or error conversion exists.

### 8.6 Sub-agents

```allen
record Projection {
  capabilities: List<String>
  limits: Map<String, Int>
  tools: List<String>
}

async fn sub_agent.create(
  initial: Prompt<Void>,
  projection: Projection
) returns Result<SubAgent, SubAgentError> effects [sub_agent.create]

async fn sub_agent.run<T>(
  request: Prompt<T>,
  projection: Projection
) returns Result<T, SubAgentError> effects [sub_agent.run]

async fn sub_agent.message(target: SubAgent, message: String) returns Result<Void, SubAgentError>
  effects [sub_agent.message]
async fn sub_agent.ask<T>(target: SubAgent, request: Prompt<T>) returns Result<T, SubAgentError>
  effects [sub_agent.ask]
```

- `create` and `run` create a fresh sub-agent.
- They may work without an invoking agent when a sub-agent provider exists.
- Missing that provider returns `Err(SubAgentError)` with `sub_agent.unavailable`.
- `message` and `ask` target the selected sub-agent, not the invoking agent.
- `Prompt.context` is the only projected context.
- The projection record is the complete authority request.
- Capability and tool names are sorted and unique; limits are positive
  implemented execution limits.
- Requested capabilities and tools must be subsets of the parent's effective
  sets. A requested limit cannot exceed the parent's effective limit.
- Empty or omitted context, lists, or maps grant nothing of that kind.
- `SubAgent` is opaque, nonconstructible, noncomparable, nonserializable,
  execution-scoped, invalid in aggregates and entry values, and unusable in
  another execution.

### 8.7 Tools

The host freezes a typed tool catalog before loading the program. In an
agent-hosted execution, it contains every tool and schema the invoking session
may use for this execution. Each definition has a stable name, input schema,
output schema, declared effects, and error schema. Host policy may deny a call
but cannot silently change a declared schema.

Version 0.1 has required tools only. A canonical tool name is 1 through 255
UTF-8 bytes and consists of nonempty dot-separated segments. Each segment is at
most 63 UTF-8 bytes and contains no whitespace or control scalar. Names compare
as exact UTF-8 bytes without case or Unicode normalization. Empty segments and
duplicate names are invalid.

A required range has exactly `>=M.m.p, <M.m.p`. Version integers have no leading
zero, prerelease, or build metadata, and the lower bound is less than the upper
bound. The catalog contains at most one selected exact version for a name, and
that version must be inside the requested range.

Tool calls have generated namespaces:

```allen
let result = await tools.github.create_issue.call(input);
```

Each complete tool name has read-only generated members `Input`, `Output`,
`DeclaredError`, `Error`, and `call`. The generated effect operation is exactly:

```allen
extern async fn call(input: Input) returns Result<Output, Error>
  effects [tool.<canonical-effect-name>@<selected-major>]
```

Calling it creates the ordinary lazy future for that generated async effect;
`await` or `spawn` starts dispatch. There is no synchronous provider escape
hatch. The catalog-selected tool name and major determine the concrete effect
name; source does not write the angle-bracket placeholders above.

For source namespaces, split the canonical name at `.`. Within each UTF-8
segment, preserve ASCII letters, digits, and `_`; encode every other byte as
`_xHH_` with upper-case hex; prefix `_n_` when the first preserved character is
a digit; and prefix `_kw_` when the result is a reserved word. Do not normalize
Unicode or case. The compiler rejects collisions. Thus
`release-tools.create-issue` maps to
`tools.release_x2D_tools.create_x2D_issue`.

Its effect is `tool.` plus the canonical effect-mangled tool name plus `@` and
the selected version major. Effect mangling preserves lower-case ASCII letters,
digits, and `_`; encodes every other UTF-8 byte as `_xhh_` with lower-case hex;
and prefixes `_n_` when the first preserved character is a digit. The compiler
rejects collisions after mangling. For example, `release-tools.create-issue`
becomes `tool.release_x2d_tools.create_x2d_issue@2`.

There is no dynamic name lookup or untyped invocation escape hatch. A tool must
be declared by the manifest and allowed by effective authority. Missing a
required tool fails loading. A validated tool-declared error returns `Err`.
Input is validated before dispatch. Output and declared error values are
validated before control returns to the program. Both boundaries reject unknown
fields and implicit coercion. Dispatch or transport failure returns
`Err(Error.Unavailable { ... })`; invalid host output or declared error returns
`Err(Error.Schema { ... })`. Late outcomes after cancellation are terminal
protocol violations and cannot resume execution.

#### Tool schema profile

The profile ID is `allen.tool-schema/0.1`. Its exact JSON Schema 2020-12 dialect
URI is `https://json-schema.org/draft/2020-12/schema`. Schema objects reject
duplicate JSON keys and every keyword not explicitly listed by the profile. It
supports these forms with the stated required keywords:

- `{"type":"null"}` as `Void`;
- `{"type":"boolean"}` as `Bool`;
- integer with required `minimum` and `maximum` JSON integers in the `Int`
  range, lowered to `Int`;
- number with optional finite `minimum` and `maximum`, lowered to `Float`; every
  wire value must be finite;
- string with optional `minLength`, `maxLength`, or nonempty sorted unique
  string `enum`, lowered to `String`;
- homogeneous array with `items`, `minItems`, and `maxItems`, lowered to
  `List<T>`;
- tuple with nonempty `prefixItems`, `items: false`, and equal `minItems` and
  `maxItems` values equal to the tuple length;
- exact record with `properties`, every property once in UTF-8-sorted
  `required`, and `additionalProperties: false`;
- string-key map with empty `properties`, `required: []`, and one schema in
  `additionalProperties`;
- tagged union with `oneOf` containing 2 through 64 exact-record branches,
  each with one distinct required single-value string-enum `tag`; and
- root `$defs` with used, unique, acyclic definitions in UTF-8 order, referenced
  only by a sibling-free local `$ref` of the form `#/$defs/<token>` using JSON
  Pointer escaping.

It rejects unsupported keywords and forms, including `default`, `examples`,
`format`, `pattern`, `const`, `nullable`, type arrays, remote or recursive
references, open objects, optional properties, overlapping union tags, and
implicit coercion. `title` and `description` are permitted metadata only.

Limits are 262,144 UTF-8 JSON bytes and 4,096 nodes per schema, expanded depth
32, 256 object properties, 256 definitions, 256 enum strings, 64 union
branches, 255 UTF-8 bytes per property/definition/tag, collection or string
bounds no larger than 1,048,576, at most 256 tools, and at most 3 MiB decoded
schema text. A host may lower limits.

Schema digests come from lowered canonical descriptors, not input JSON text.
References expand, annotations disappear, records and variants sort by UTF-8
bytes, and the digest is lower-case `sha256:` plus the SHA-256 hex digest.

A generated tagged union is named `Input_union_`, `Output_union_`, or
`Error_union_` plus the first 16 lower-case hex digits of the SHA-256 digest of
its expanded-schema JSON Pointer. Each variant is `_tag_` plus the tag after the
same UTF-8 byte mangling used for source identifiers. The compiler retains the
full pointer and rejects a truncated-digest or namespace/member/variant
collision. Generated declarations are read-only and require the complete
`tools.` qualification. Do not guess a generated name when compiler tooling can
report it.

### 8.8 Capability inspection

```allen
fn capability.is_granted(name: String) returns Bool
  effects [capability.inspect]
fn capability.granted() returns List<String>
  effects [capability.inspect]
```

The runtime freezes the sorted unique intersection of requested manifest
capabilities and grants actually effective for the launch before entry
execution. In this profile its possible names are `fs.read`, `fs.write`,
`net.http_get`, and `permission.request_external_fs`. `is_granted` returns
false for malformed, undeclared, denied, local-effect, tool, provider, dynamic
external-grant, and unknown names.

`capability.inspect` is a synchronous local observation effect. A function
that directly calls either operation must have an explicit effect clause
containing it; an omitted clause is an empty contract. No
manifest entry grants it. Results expose no origin, path, handle, credential,
provider, or tool state. Inspection cannot create or widen authority and cannot
bypass any operation's normal checks.

## 9. Packages, manifests, and capabilities

### 9.1 Package manifest

A package uses `allen.toml`. The high-level human specification fixes the
manifest concepts but does not reproduce the complete package-file grammar.
The current package syntax below is defined by the
[`docs/implementation-spec.md`](../../implementation-spec.md) and repository
examples:

```toml
[package]
name = "filesystem-example"
version = "0.1.0"
language = "^0.1"

[[entry]]
name = "main"
function = "src/main.allen::main"
input = "String"
output = "Result<String, FileError>"

[capabilities]
required = ["fs.read"]

[[tools.required]]
name = "github.create_issue"
version = ">=2.0.0, <3.0.0"
```

In the current package TOML, `capabilities.required` contains source effect IDs
such as `fs.read`; the selected work-directory scope is supplied by the launch
contract. The inline language form spells the scoped request as
`fs.read(workdir)`. Do not silently substitute one manifest syntax for the
other.

The inline equivalent of `[[tools.required]]` is a `tools` record whose
`required` field is a list of exact `{ name, version }` records. Version 0.1 has
no optional tool, dynamic tool name, or inline manifest-provided schema.

Every program has a manifest model. A package uses `allen.toml`; one standalone
file may contain one inline manifest; loose core source may receive only a
capability-free synthesized manifest.

The manifest declares language selection, entries, requested capabilities, and
required tools. Unknown fields are rejected unless they use an approved
extension namespace. The current `0.1` selector names the evolving early-alpha
profile. The runtime accepts only the current profile contract.

### 9.2 Entry points

An entry names one exported function and exact input/output types. The function
has zero or one parameter; zero parameters mean `Void` input. Entry values use
the exact JSON forms in Section 5.8.

### 9.3 Capabilities and grants

- A source effect states maximum possible authority.
- A manifest capability is a request, not a grant.
- Host policy, user policy, agent policy, and runtime limits compute an
  equal-or-narrower effective capability set.
- Provider availability is separate from effect declaration and capability
  possession.
- Capabilities are unforgeable runtime values.
- Source cannot create, widen, or persist a capability.
- Capability inspection reads only the immutable requested-and-effective
  manifest set frozen before entry execution; it exposes no narrower authority
  details or ambient host state.
- An external grant expires when its execution ends.
- Sub-agent authority is explicitly projected and attenuated.
- `task.spawn`, `debug.inspect`, `capability.inspect`, and `stop` require no
  host capability.
- The default sandbox denies subprocess creation. A subprocess is available
  only through a declared host tool.

### 9.4 Packages and lockfiles

The canonical lockfile is `allen.lock`. Version 0.1 resolves local source
dependencies only. Each dependency has a source alias, exact selected version,
normalized root-relative source path, language selection, content hash, and
sorted dependency list. A stale or noncanonical required lockfile fails before
compilation. Registries and network dependency fetching are unavailable.

## 10. Errors, outcomes, and lifecycle

### 10.1 Closed error model

ALLEN has exactly four disjoint channels: compile/load diagnostics;
`Result<T, E>` expected-operation failures; uncatchable terminal runtime traps;
and the distinct terminal `Stopped` outcome. There is no `throw`, `try`, or
`catch` syntax. `?` propagates only the exact compatible `Err` and never
converts a trap or stop. The canonical registry is
[`errors-0.1.json`](../../conformance/errors-0.1.json).

Every standard expected error is exactly `{ code: String, message: String }`.
Code is the discriminator; messages are bounded, safe, and nonsecret. The named
records are `FileError`, `NetworkError`, `AgentError`, `UserError`,
`SubAgentError`, `ModelError`, and `PermissionError`. Do not add causes,
metadata, subtyping, or provider detail to source values.

| Exact operation | Result error | Registered codes | Retryability | Channel and cleanup |
|---|---|---|---|---|
| parse, manifest, lockfile, artifact, package, schema, entry, capability, tool-catalog, input, workspace, input-limit, and replay-contract validation | diagnostic before execution | compiler: `E0002`, `E0003`, `E0004`, `E0005`, `E2003`, `E2009`, `E2011`, `E2012`, `E2015`, `E2016`, `E2017`, `E2018`, `E2019`, `E2020`, `E2403`, `E3002`, `E3003`, `E3005`, `E3007`, `E3008`, `E3010`, `E3011`; artifact: the 19 `ARTIFACT_*` registry codes; package/load: the listed `package.*`, `manifest.*`, `lock.*`, `runtime.entry_not_found`, `runtime.manifest_invalid`, `runtime.capability_denied`, `tool.catalog_mismatch`, `runtime.invalid_input`, `runtime.workspace_unavailable`, `resource.input_bytes`, and `replay.diverged`; schema: the listed `schema.*` codes | never | no task exists |
| `async fn fs.read_text(Workspace, String) returns Result<String, FileError> effects [fs.read]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.is_directory`, `fs.not_directory`, `fs.not_found`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `fs.invalid_utf8`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn fs.read_bytes(Workspace, String) returns Result<Bytes, FileError> effects [fs.read]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.is_directory`, `fs.not_directory`, `fs.not_found`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn fs.write_text(Workspace, String, String) returns Result<Void, FileError> effects [fs.write]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.not_directory`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn fs.write_bytes(Workspace, String, Bytes) returns Result<Void, FileError> effects [fs.write]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.not_directory`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn fs.list(Workspace, String) returns Result<List<String>, FileError> effects [fs.read]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.not_directory`, `fs.not_found`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `fs.invalid_utf8`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn http.get(String) returns Result<HttpResponse, NetworkError> effects [net.http_get]` | `NetworkError` | `net.permission_denied`, `network.unavailable`, `net.invalid_limits`, `net.invalid_url`, `net.origin_denied`, `net.destination_denied`, `net.dns`, `net.dns_timeout`, `net.peer_mismatch`, `net.connect_timeout`, `net.first_byte_timeout`, `net.idle_timeout`, `net.total_timeout`, `net.redirect_invalid`, `net.protocol`, `net.unsupported_encoding`, `net.tls`, `net.io`, `resource.limit` | caller for unavailable, DNS, timeout, TLS, and I/O; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn agent.message(String) returns Result<Void, AgentError> effects [agent.message]` | `AgentError` | `agent.unavailable`, `agent.denied` | caller / never denied | ordinary `Err` |
| `async fn agent.ask<T>(Prompt<T>) returns Result<T, AgentError> effects [agent.ask]` | `AgentError` | `agent.unavailable`, `agent.denied`, `agent.validation_failed` | caller / never denied | ordinary `Err` |
| `async fn agent.transcript({limit:Int}) returns Result<TranscriptSnapshot, AgentError> effects [agent.transcript]` | `AgentError` | `agent.unavailable`, `agent.denied` | caller / never denied | ordinary `Err` |
| `async fn model.request<T>(Prompt<T>) returns Result<T, ModelError> effects [model.request]` | `ModelError` | `model.unavailable`, `model.denied`, `model.validation_failed` | caller / never denied | ordinary `Err` |
| `async fn user.ask<T>(Prompt<T>) returns Result<T, UserError> effects [user.ask]` | `UserError` | `user.unavailable`, `user.denied`, `user.validation_failed` | caller / never denied | ordinary `Err` |
| `async fn sub_agent.create(Prompt<Void>, Projection) returns Result<SubAgent, SubAgentError> effects [sub_agent.create]` | `SubAgentError` | `sub_agent.unavailable`, `sub_agent.denied` | caller / never denied | ordinary `Err` |
| `async fn sub_agent.run<T>(Prompt<T>, Projection) returns Result<T, SubAgentError> effects [sub_agent.run]` | `SubAgentError` | `sub_agent.unavailable`, `sub_agent.denied`, `sub_agent.validation_failed` | caller / never denied | ordinary `Err` |
| `async fn sub_agent.message(SubAgent, String) returns Result<Void, SubAgentError> effects [sub_agent.message]` | `SubAgentError` | `sub_agent.unavailable`, `sub_agent.denied` | caller / never denied | ordinary `Err` |
| `async fn sub_agent.ask<T>(SubAgent, Prompt<T>) returns Result<T, SubAgentError> effects [sub_agent.ask]` | `SubAgentError` | `sub_agent.unavailable`, `sub_agent.denied`, `sub_agent.validation_failed` | caller / never denied | ordinary `Err` |
| `async fn permission.request_file(ExternalFileRequest) returns Result<Workspace, PermissionError> effects [permission.request_external_fs]` | `PermissionError` | `permission.denied`, `permission.unavailable` | caller / never denied | ordinary `Err` |
| `async fn permission.request_directory(ExternalDirectoryRequest) returns Result<Workspace, PermissionError> effects [permission.request_external_fs]` | `PermissionError` | `permission.denied`, `permission.unavailable` | caller / never denied | ordinary `Err` |
| `async fn tools.*.call(Input) returns Result<Output, Error> effects [tool.*@major]` | generated closed `Error` | `tool.unavailable`, `tool.denied`, `tool.schema` | caller / never denied or schema | ordinary `Err` |
| checked arithmetic; trapping index/set/map operations | terminal trap | `arithmetic.`, `index.`, `map.` | never | cancel and join owned tasks |
| limits, cancellation, timeout, invariant/protocol violation, or replay drift detected after execution begins | terminal trap | `resource.`, `runtime.`, `protocol.`, `replay.runtime_diverged` | never | cancel and join owned tasks |
| `stop(reason)` | `Stopped` | `stopped` is not an error code | never | cancel and join owned tasks |

Every filesystem, HTTP, or output resource exhaustion terminalizes as the
single public code `resource.limit`. The registry contains only current codes
and assigns each code to exactly one current channel.

Generated tool `Error` is exactly `Declared(DeclaredError)`,
`Unavailable { code: String, message: String }`, or
`Schema { code: String, message: String }`. A declared tool error can only use
`Declared`; malformed host data can only use `Schema`.

At the VM boundary, a structurally valid `Result::Err` is rejected with
`protocol.violation` unless its standard error code is registered for that
exact operation and its message is protocol-bounded. Generated tool
`Unavailable` and `Schema` variants must carry their exact operational codes.
Replay also revalidates tool output and declared-error payloads against the
frozen strict catalog schemas.

The operation table is the current `FileError` and `NetworkError` inventory. In
particular, every filesystem or HTTP resource exhaustion is the terminal
`resource.limit`; it is not a `FileError` or `NetworkError` value.

Source-visible stable codes explicitly fixed by the human specification include
`arithmetic.overflow`, `arithmetic.division_by_zero`, `map.duplicate_key`,
`map.key_not_found`, `index.out_of_bounds`, and `fs.permission_denied`.

`Stopped` is a terminal execution outcome, not an error or an entry return
value. A program cannot catch or inspect it. Traps, cancellation, timeout,
resource exhaustion, and runtime invariant/protocol violations cancel and join
owned tasks; ordinary `Err` does not cancel siblings.

### 10.2 `stop`

```allen
fn stop(reason: String) returns Never
```

`stop` has no effect and needs no manifest capability. It permanently ends only
the current ALLEN execution. It does not end the host or invoking-agent session
and cannot be caught, retried, resumed, or recovered. The runtime cleans up
owned tasks, should flush committed audit records, skips entry-result
validation, and returns `Stopped`. The reason is untrusted program output.

### 10.3 Runtime lifecycle

The runtime proceeds in this order:

1. parse source and manifest;
2. resolve modules, types, effects, tools, and capabilities;
3. reject invalid source or unsatisfied static contracts;
4. create the sandbox and freeze the effective manifest capability set;
5. validate entry input;
6. execute the entry;
7. close or cancel owned tasks and resources; and
8. validate and return a normal entry result.

`stop` performs cleanup but skips step 8.

### 10.4 Determinism and limits

Pure evaluation is deterministic for the same language version, program, and
input. Integer arithmetic, text, collection order, serialization, matching, and
the ready-task policy have specified behavior.

Time, randomness, files, networks, tools, agents, models, scheduling inputs, and
permission decisions are outside the deterministic boundary and require
declared effects. Recording or replay preserves validated boundary values; it
does not mean the external system ran again. Replay binds the frozen sorted
effective manifest capability set in its versioned contract, reproduces it for
playback, and rejects a mismatch before entry execution.

The host defines limits for time, memory, effect count, and concurrent tasks.
The manifest may request limits; the host may lower them. Exhaustion is the
terminal `resource.limit` trap, and an execution deadline is terminal
`runtime.timeout`. Response attempts, transcript size, HTTP activity, filesystem
activity, schemas, aggregate depth, generic expansion, allocations, and cleanup
are also bounded.

## 11. Unsupported syntax and operations

Do not generate any of the following as usable version 0.1 behavior:

- JavaScript coercion, hoisting, prototypes, `any`, `undefined`, `null`, or
  automatic semicolon insertion;
- string indexing;
- mutable aggregate elements or fields;
- recursive enum payload types;
- collection patterns, pattern guards, or OR patterns;
- explicit ordinary generic-call type arguments, user-defined constraints,
  higher-kinded types, specialization, or generic recursion;
- effect variance, effect polymorphism, or implicit capability delegation;
- detached tasks or task/future storage in aggregates, mutable bindings,
  closures, `unknown`, or entry values;
- general exception-catching syntax;
- optional tools, dynamic tool lookup, manifest-provided tool schemas, or an
  untyped tool call;
- raw sockets, raw subprocesses, plain HTTP, arbitrary HTTP methods,
  source-provided HTTP headers, ambient credentials, or a browser DOM;
- package registries, remote dependency fetching, signatures, or publishing;
- durable sub-agent handles;
- reusable prompt-template versioning or multimodal prompt content; or
- hidden model reasoning or hidden host instructions through transcripts.

`decode<T>(bytes)` is not a source operation.

ALLEN has no general exception syntax. Invoking-agent operations return their
documented `Result` errors. `AgentError` is a standard error record. Names such
as `Text`, `Summary`, `Plan`, `Dataset`, or `ExternalReadError` in examples are
user-defined placeholders unless their declaration appears in the program.
