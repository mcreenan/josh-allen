# ALLEN language specification

Status: Early alpha; evolving version 0.1 source grammar and implemented profile

**ALLEN** means **Agent-Level Language, Embedded Natively**. This document defines the high-level specification for ALLEN as a standalone, agent-native programming language.

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** state requirement levels as defined by RFC 2119 and RFC 8174.

## 0. Language status

ALLEN is an early alpha under active development. The `0.1` label and the
`*-0.1` conformance profile names identify the one current language and
implementation profile. Source syntax, semantics, standard operations,
manifests, artifacts, provider contracts, and implementation interfaces MAY
change together while the language is in alpha.

The repository MUST describe one internally consistent current language. A
breaking change MUST update the normative specification, agent reference,
implementation, tests, conformance data, editor support, and examples as
applicable. The compiler, artifact decoder, runtime, and JOSH host support only
their current contracts; inputs produced for earlier repository states are
unsupported.

## 1. Goals

ALLEN has these goals:

- It MUST work without a specific agent harness.
- It MUST support unattended and standalone execution.
- It MUST make agent, model, tool, and permission effects visible in source code.
- It MUST use strict static types and runtime validation at external boundaries.
- It MUST make safe behavior the default.
- It SHOULD have a small core language.
- It SHOULD be portable across conforming hosts.
- It SHOULD remain useful when agent hosts and policies change.
- It SHOULD support skill packages that combine deterministic code with typed prompts.

## 2. Non-goals

ALLEN does not emulate JavaScript. It does not define one model provider, one agent harness, or one tool protocol. It does not expose hidden model reasoning. It does not make every external operation deterministic. It does not replace operating system security.

### 2.1 Execution actors

The runtime parses, checks, and executes a program. The host embeds or starts the runtime. The host supplies capabilities and external service providers.

An agent harness is one type of host. The language and runtime MUST NOT require a specific harness. The host MAY attach one invoking agent session to an execution. The invoking agent identity MUST remain stable for that execution.

A standalone host has no invoking agent session. It MAY still provide a user interface, a model provider, a sub-agent provider, and tools. Each provider is independent. The absence of one provider MUST NOT disable an unrelated provider.

**JOSH** means **JSON-Oriented Session Host**. JOSH is the optional reference host for attached ALLEN executions. It binds the invoking agent, negotiates capabilities and limits, projects the filtered transcript and frozen tool catalog, routes external effects, and returns the terminal execution outcome. ALLEN MUST support standalone and unattended execution without JOSH. Another host MAY implement the same provider contracts or the `josh/1` protocol.

## 3. Source and type system

The syntax SHOULD be familiar to TypeScript users. A conforming implementation MUST NOT add JavaScript coercion, hoisting, prototype inheritance, `undefined`, or automatic semicolon insertion.

The baseline type system MUST include:

- `Bool`, `Int`, `Float`, `String`, `Bytes`, `Void`, and `Never`.
- Lists, maps, tuples, and structural records.
- Nominal enums.
- Tagged unions and exhaustive pattern matching.
- Generic types and generic functions with constraints.
- `Option<T>` for an optional value.
- `Result<T, E>` for an operation that can fail.
- `Future<T>` for one lazy asynchronous computation.
- `Task<T>` for one started asynchronous computation.
- `Workspace` for one opaque execution-scoped working-directory capability.
- `ExternalFsAccess` for one external filesystem request mode.
- `unknown` for data that has not been validated.

The language MUST NOT provide `any`. A program MUST validate or narrow an `unknown` value before it uses operations of a concrete type. `null` MUST NOT be a member of another type. A nullable value MUST use `Option<T>` or an explicit union if the language later defines a distinct `Null` type.

A generic constraint MUST state the operations or structural contract that a type parameter requires. The compiler MUST check the constraint at each instantiation. The exact declaration syntax for reusable constraints remains open.

Variables and fields MUST be immutable by default. Source code MUST mark mutable state with `mut`. The compiler MUST reject an assignment to immutable state.

The compiler MUST support local type inference. Public function parameters, public return values, exported values, manifest inputs, manifest outputs, prompt responses, and tool interfaces MUST have declared types. An implementation MAY infer more types when this does not weaken interface checks.

The language MUST NOT perform implicit coercion. A conversion MUST use an explicit conversion function. A conversion that can fail MUST return `Result`.

```allen
record User {
  id: String
  email: Option<String>
}

enum Delivery {
  Email { address: String }
  Queue { name: String }
  Skip
}

fn route(delivery: Delivery) returns Result<String, RouteError> {
  match delivery {
    Email { address } => Ok(to_string(address))
    Queue { name } => Ok(to_string(name))
    Skip => Err(RouteError.Skipped)
  }
}

let count = 3
mut retries = 0
```

A `match` over a closed type MUST be exhaustive. The compiler MUST report unreachable cases.

`Never` is the type of an expression that cannot return normally. `Never` has no values. The compiler MUST accept a `Never` expression where another expression type is required. This rule is type compatibility. It is not a runtime coercion.

### 3.1 Version 0.1 core value profile

This section fixes the core value rules that the reference implementation uses.

`Int` is a signed 64-bit integer. Integer addition, subtraction,
multiplication, division, remainder, and negation are checked terminal traps. Overflow has
the stable code `arithmetic.overflow`. Integer division truncates toward zero.
Division by zero has the stable code
`arithmetic.division_by_zero`. For a nonzero divisor, `left % right` is
`left - (left / right) * right`, using that truncating division; it therefore
has the sign of `left` or is zero. Remainder by zero traps with
`arithmetic.division_by_zero`. `Int::MIN % -1` is zero and MUST NOT overflow,
even though `Int::MIN / -1` is unrepresentable.

`Float` is an IEEE 754 binary64 value. An implementation MUST replace every
NaN payload with the quiet NaN bit pattern `0x7ff8000000000000` when it creates
a language value. Float equality follows IEEE equality after this replacement:
NaN is not equal to any value, and positive zero is equal to negative zero.
Ordered comparison with NaN is false. Float division follows IEEE 754, including
division by zero. Float text MUST use the shortest locale-independent decimal
form that reads back to the same binary64 value. It MUST use `NaN`, `Infinity`,
and `-Infinity` for non-finite values. It MUST preserve the negative zero text
as `-0.0` and MUST include `.0` for a finite integral value when shortest
round-trip formatting permits it.

Source has no implicit numeric or text conversion. Version 0.1 initially
provides these total conversions:

- `to_float(Int) returns Float` uses IEEE round-to-nearest, ties-to-even.
- `to_string(Bool|Int|Float|String) returns String` uses the canonical scalar text.
- `to_bytes(String) returns Bytes` returns the UTF-8 bytes.

Other fallible scalar conversions are unsupported. `string.from_utf8` below
uses an explicit `Option<String>` result.

Version 0.1 also provides these pure collection and String operations:

- `length(Bytes) returns Int` returns the exact byte count.
- `length(List<T>) returns Int` returns the exact element count.
- `length(Map<K, V>) returns Int` returns the exact entry count.
- `length(String) returns Int` returns the Unicode scalar count.
- `list.append(values: List<T>, value: T) returns List<T>` returns a new list with
  `value` after every original element.
- `list.set(values: List<T>, index: Int, value: T) returns List<T>` returns a new
  list with exactly the indexed element replaced.
- `list.get<T>(values: List<T>, index: Int) returns Option<T>` and
  `bytes.get(values: Bytes, index: Int) returns Option<Int>` return `None` for a
  negative or out-of-range index.
- `list.try_set<T>(values: List<T>, index: Int, value: T) returns Option<List<T>>`
  returns `None` for an invalid index and otherwise a new list.
- `map.get<K, V>(values: Map<K, V>, key: K) returns Option<V>` returns `None` for
  an absent key.
- `int.checked_add`, `int.checked_sub`, `int.checked_mul`, `int.checked_div`,
  `int.checked_rem`, and `int.checked_neg` return `Option<Int>` instead of
  trapping. They return `None` for overflow or zero division; in particular
  `int.checked_rem(Int::MIN, -1)` is `Some(0)`.

The String namespace provides these exact pure operations:

```allen
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

Except for `byte_length`, every String index and length counts Unicode scalar
values, not UTF-8 bytes or grapheme clusters. `get` returns one scalar encoded
as a String. `slice` uses a scalar-indexed half-open interval. Both return
`None` for a negative or invalid bound and never split UTF-8. `find` returns
the first matching scalar index and returns `Some(0)` for an empty needle;
matching is exact, without normalization or case folding. `contains`,
`starts_with`, and `ends_with` use the same exact matching.

`split` returns `None` for an empty separator. Otherwise it preserves empty
leading, trailing, and adjacent fields and returns `Some([value])` when the
separator is absent. `join` preserves list order and returns `""` for an empty
list. `trim_ascii` removes only ASCII space, tab, LF, CR, form feed, and
vertical tab from both ends. `from_utf8` returns `Some` exactly when all input
bytes form valid UTF-8; `to_bytes` is its inverse for every String.

`length` accepts exactly one argument. Each String operation has exactly the
arity and operand types shown. `list.append` accepts exactly two, and
`list.set` accepts exactly three. The compiler MUST reject every other arity
and MUST require the appended or replacement value to have exactly the list's
element type. These operations are generic for every valid list element type,
including scalar, record, enum, and list types. They are local pure operations:
they add no effect and require no manifest capability. `list.append` and
`list.set` do not mutate or otherwise change `values`, including when another
binding aliases the same list. A negative index or an index greater than or
equal to the list length traps with `index.out_of_bounds` before producing a
result list. The safe alternatives never mutate an aliased aggregate and return
`None` before allocating a replacement aggregate on invalid data.

`String` contains valid UTF-8. String operations use locale-independent exact
Unicode-scalar or UTF-8-byte rules and add no effect or capability. `Bytes`
contains arbitrary bytes. A list has one
element type. A map has one key type and one value type. A tuple has a fixed
length and one type for each position. `Void` is a singleton type: `()` is its
only value and is also the empty tuple. Unlike TypeScript `void`, ALLEN `Void`
does not permit implicit value discarding or type compatibility. `(value,)` is
a one-element tuple.

Map keys in this profile are `Bool`, `Int`, `String`, or `Bytes`. A map stores
keys in ascending language order. Boolean order is `false`, then `true`.
Integer order is signed numeric order. String order is lexicographic UTF-8 byte
order. Bytes order is lexicographic unsigned-byte order. Duplicate keys trap with
`map.duplicate_key`. Map construction order does not affect equality,
iteration, display, or serialization.

Equality is available for all inhabited core values of the same type. It is
recursive for collections and uses the Float rule above. Ordered operators are
available for `Int`, `Float`, `String`, and `Bytes`. `+`, `-`, `*`, and `/`
require two values of the same numeric type; `%` requires two `Int` values.
`!`, `&&`, and `||` require `Bool`. Boolean binary operators evaluate their
left operand first. `&&` evaluates its right operand only when left is true;
`||` evaluates its right operand only when left is false.

List and bytes indexes are `Int`. A list index returns its element. A bytes
index returns an `Int` from 0 through 255. A map index has the map key type and
returns the value type. A tuple index MUST be a nonnegative integer literal so
the compiler can select its result type. A missing map key traps with
`map.key_not_found`. Any invalid sequence index traps with `index.out_of_bounds`.
String indexing is not part of this profile. String scalar access uses
`string.get`, and `for scalar in value` iterates one evaluated immutable String
snapshot in Unicode scalar order.

Local bindings use `let name = expression;` or `mut name = expression;`. A type
annotation MAY follow the name. Assignment is a statement and uses
`name = expression;`, `name += expression;`, `name -= expression;`,
`name *= expression;`, `name /= expression;`, or `name %= expression;`. A
semicolon is mandatory after a local declaration or assignment. The compiler
rejects a duplicate local name, use before declaration, assignment with a
different type, and assignment to `let`. Aggregate values are immutable. `mut`
permits binding reassignment; it does not permit element mutation.

A compound assignment targets exactly one declared `mut` local. It reads the
old value once, evaluates the right operand once, applies the corresponding
checked operator, and replaces the binding only after that operation succeeds.
`+=`, `-=`, `*=`, and `/=` require matching `Int` operands or matching `Float`
operands; `%=` requires matching `Int` operands. A failed checked operation or
right operand leaves the target unchanged. Compound assignment is not an
expression and cannot target a field, index, loop binding, undeclared name, or
affine value.

List literals use `[a, b]`. Map literals use `map { key: value }`. Tuple literals
use `(a, b)` or `(a,)`. A trailing comma is allowed. An empty list or map needs
an expected type from a type annotation or the function return type.

The canonical value encoding is a binary, self-delimiting format. Lengths and
counts are unsigned 32-bit big-endian values. Integers use eight-byte
two's-complement big-endian form. Floats use their canonical eight-byte binary64
bit pattern in big-endian form. Strings use their UTF-8 bytes. The tags are:

| Tag | Value | Payload |
|---|---|---|
| `00` | `Void` | None |
| `01` | `false` | None |
| `02` | `true` | None |
| `03` | `Int` | Eight bytes |
| `04` | `Float` | Eight bytes |
| `05` | `String` | Byte length, then bytes |
| `06` | `Bytes` | Byte length, then bytes |
| `07` | List | Element count, then encoded elements |
| `08` | Map | Entry count, then encoded keys and values in map order |
| `09` | Tuple | Element count, then encoded elements |

The encoder MUST reject a length above `u32::MAX`. `Never` has no tag. Future
value kinds MUST use new tags and MUST NOT change these encodings.

The first VM accounting hooks use deterministic logical allocation sizes. A
frame costs 32 bytes plus 16 bytes for each register. A string or bytes payload
costs 8 bytes plus its byte length. A list or tuple costs 8 bytes plus 16 bytes
for each element. A map costs 8 bytes plus 32 bytes for each entry. The VM MUST
charge the complete logical size before it creates the value or frame. It MUST
charge one instruction before it executes each instruction. The entry frame has
call depth 1, and the VM MUST check that depth before it allocates the frame.
Size arithmetic MUST be checked.

`length` creates no aggregate and adds no allocation charge. `list.append`
charges the complete logical size of the returned list, using its new element
count, before allocating or copying any element. `list.set` first validates the
index, then charges the complete logical size of the returned list before
allocating or copying any element. A refused charge traps with `resource.limit` and
leaves every input value unchanged. The normal instruction charge occurs
before each collection operation. None of these rules changes the canonical
encoding of bytes or lists.

Every successful String-producing operation charges a fresh logical String of
8 bytes plus the complete output UTF-8 byte length, even when an implementation
shares immutable storage. A successful `Some` result charges its enum wrapper
and the complete contained String or aggregate. `split` and
`capability.granted` preflight one checked total containing the list shell and
every element String before they allocate, copy, or install any part. A
`None` result consumes no incremental logical allocation. Refused or
overflowing charges construct no partial result and leave inputs unchanged.

Iteration over a map reads its already canonical entry sequence by a verified,
source-hidden integer index. One yielded map entry is a two-element tuple and
therefore costs 40 logical bytes. The VM validates the internal index, charges
that complete tuple size, and only then reads, clones, constructs, or installs
the yielded entry. This operation is not a source-visible map cursor or index
form.

### 3.2 Version 0.1 data-type profile

This section fixes the first record, enum, pattern, result, and `unknown` rules.

A source file can declare records and enums before its entry function. A record
declaration has this form:

```allen
record Point {
  x: Int
  y: Int
}
```

A record declaration introduces a type name and a constructor name. Record
types are structural. Two record declarations define the same type when they
have the same field names and field types. Declaration names do not affect this
test. A compiler MUST put record fields in ascending UTF-8 byte order before it
compares types, creates a runtime value, displays a value, or encodes a value.
Fields in a declaration, constructor, or pattern MUST be unique. A constructor
MUST contain every declared field and MUST NOT contain an extra field.

```allen
let point = Point { y: 2, x: 1 };
let x = point.x;
```

An enum declaration has payloadless, tuple, or record variants:

```allen
enum Reading {
  Empty
  Number(Int)
  Named { label: String, value: Int }
}
```

Enum types are nominal. Two declarations do not define the same type, even when
their names and variants have the same text. The compiler MUST resolve an enum
to a module type ID. The bytecode and runtime value MUST keep that ID. A source
name alone is not an enum identity. Stable IDs cover all modules in a source
bundle.

A user enum constructor is qualified, for example `Reading.Empty`,
`Reading.Number(3)`, or `Reading.Named { label: "cpu", value: 3 }`. Tuple payload
order is significant. Record-variant fields use ascending UTF-8 byte order after
the compiler checks the source fields.

A type alias is a compile-time transparent synonym for any version 0.1 type:

```allen
type Measurements = List<Int>
type PointView = Point
type LabeledPoint = { label: String, point: PointView }
```

The alias introduces no nominal identity, runtime wrapper, bytecode type, or
conversion. `Measurements` is exactly `List<Int>`, `PointView` retains the
structural identity of `Point`, and an alias of an enum retains that enum's
existing nominal module type ID. Version 0.1 aliases have no generic parameter
list; compose existing generic types on the right-hand side instead. The right-
hand side may refer to an alias declared later in the same module. The compiler
MUST resolve alias chains without source-order dependence, MUST reject an
unknown alias target even when the alias is unused, and MUST reject every
direct or indirect alias cycle deterministically before bytecode emission.

Alias, record, and enum names share one module type namespace and the same
duplicate-name rules. `export type PublicName = ...` is importable wherever an
exported record or enum is importable; a private alias is not. The imported
alias is resolved in its defining module, so its target does not become public
merely because the alias is exported. A type-alias declaration ends after its
syntactically complete right-hand-side type. It has no source terminator:
neither a newline nor a literal semicolon is required or accepted as part of
the declaration.

Version 0.1 requires at least one enum variant and at least one type in a tuple
variant. It does not support recursive enum payload types. It rejects an
expanded value-type shape that is deeper than 128 aggregate or enum-payload
steps. These checks happen before the compiler emits bytecode.

`Option<T>` and `Result<T, E>` are built-in tagged types. `Option<T>` has variant
0 `None` and variant 1 `Some(T)`. `Result<T, E>` has variant 0 `Ok(T)` and variant
1 `Err(E)`. Source uses the unqualified constructors `None`, `Some(value)`,
`Ok(value)`, and `Err(value)`. `Some(value)` can infer `T`. `None` needs an
expected `Option<T>`. `Ok(value)` and `Err(value)` need an expected `Result<T,
E>` for the type parameter that has no payload. The compiler MUST reject a
constructor when it cannot resolve every type parameter.

Field access uses `value.field`. Destructuring uses a match pattern. Version 0.1
has Boolean patterns, record patterns, enum-variant patterns, and the wildcard
`_`. A payload subpattern in this profile is a new local binding or `_`.
Patterns do not have guards, OR operators, or collection forms.

```allen
match reading {
  Reading.Empty => 0
  Reading.Number(value) => value
  Reading.Named { value, label: _ } => value
}
```

Commas between match arms are optional. A pattern binding exists only in its
arm. Match arms MUST have one exact result type. A `Never` arm is compatible
with that type under the existing `Never` rule. A match on `Bool`, a record, a
nominal enum, `Option`, or `Result` MUST be exhaustive. A wildcard covers each
case that an earlier pattern did not cover. The compiler MUST reject a duplicate
case and each case after a wildcard as unreachable. For this profile, a record
pattern or wildcard covers the one possible structural record shape.

Postfix `?` applies only to `Result<T, E>`. It is valid only in a function whose
return type is `Result<U, E>` with the exact same error type. `Ok(value)?`
produces `value`. `Err(error)?` returns the original `Err` value from the current
function. It does not convert or reconstruct that value.

The type `unknown` is inhabited, but no concrete operation accepts it. Local
code can create one only with the explicit total operation
`to_unknown(value) returns unknown`. The built-in operation
`narrow<T>(value: unknown) returns Option<T>` performs exact recursive runtime shape
validation. It returns `Some(value)` when the value has type `T` and `None`
otherwise. Empty collections can validate against a compatible collection type
because they contain no conflicting element. The target `T` MUST be a complete
concrete type. Version 0.1 does not provide another cast from `unknown`.

The source words `any`, `undefined`, and `null` are forbidden. A type suffix
such as `String?` is also forbidden. Optional data uses `Option<T>`.

Records, enums, and unknown values extend the canonical encoding from Section
3.1:

| Tag | Value | Payload |
|---|---|---|
| `0A` | Record | Field count, then each field name and value in field order |
| `0B` | Enum | Enum identity, variant ID, payload kind, field count, then fields |
| `0C` | Unknown | The encoded contained value |

A field name is a four-byte length followed by its UTF-8 bytes. It does not have
the String tag. An enum identity starts with one byte. `00` means a user enum
and is followed by its four-byte module type ID. `01` means `Option`. `02` means
`Result`. The variant ID is four bytes. Payload kind `00` is payloadless, `01`
is a tuple payload, and `02` is a record payload. A tuple payload encodes its
values in order. A record payload encodes each field name and value in field order.
All counts and IDs use unsigned big-endian form. Encoding an unknown value adds
tag `0C` before the normal canonical encoding of its contained value.

Record construction costs 8 bytes plus 16 bytes for each field. Enum
construction costs 8 bytes plus 16 bytes for each payload value. Creating an
unknown wrapper costs 16 bytes. These are logical allocation charges. The VM
MUST charge the complete amount before it creates the value.

### 3.3 Version 0.1 function, module, generic, and effect profile

A source bundle has one root source file and each source file that the root
imports. A source file is one module. The module name is its normalized UTF-8
path relative to the source-bundle root. A module path uses `/` separators,
ends in `.allen`, and has no empty, `.` or `..` component. Source cannot import
an absolute path or a path outside the bundle root.

A module uses a named relative import:

```allen
import { Reading, add } from "./support.allen";
```

An imported name can use a local alias:

```allen
import { Reading as LocalReading } from "./support.allen";
```

The compiler resolves the path relative to the importing module. An import can
name an exported function, record, enum, or type alias. An import does not re-export a
name. A private declaration cannot be imported. Version 0.1 rejects a
self-import and every cycle in the module import graph. It does not support a
wildcard import, namespace import, or re-export.

A package module can import through one dependency alias declared in its
manifest:

```allen
import { normalize } from "text_utils/src/text.allen";
```

The first component is the dependency alias. The remaining path is relative to
that dependency package root. A package-qualified import cannot use `.`, `..`,
an absolute path, or a path not present in the locked source package. A
relative import never crosses its current package root. The canonical module
identity is the package name, exact package version, and normalized module
path. Two packages cannot provide the same name and version in one resolved
graph.

Each resolved declaration has a bundle-local numeric ID. The compiler sorts
the normalized tuple `(module path, declaration kind, source name)` by UTF-8
bytes and assigns consecutive IDs. The result MUST NOT depend on source-bundle
enumeration order. A synthetic closure name contains its source byte offset and
uses the same ordering rule. A user enum identity is its resolved nominal type
ID. Two enum declarations in different modules remain different even when they
have the same source name and variants. These numeric IDs are compiler and
bytecode identities for one exact source bundle. Version 0.1 does not promise
that an ID remains unchanged after the bundle adds, removes, or renames a
declaration.

A module can contain private and exported functions:

```allen
fn add(left: Int, right: Int) returns Int {
  left + right
}

export fn main() returns Int {
  return add(40, 2);
}
```

A parameter and return value use exact declared types. Version 0.1 requires
these annotations on every named function and closure. Every named function
MUST declare every parameter type and its return type; an omitted effect clause
means the exact empty effect set. A function body has a tail result, an explicit `return expression;`,
a bare `return;` in a `Void` function, or both on separate control-flow paths.
Every reachable return MUST have the declared return type. Control MUST NOT reach the end of a
non-`Void` function without a value.

A generic function declares one or more type parameters. Version 0.1 provides
one reusable constraint, `Eq`:

```allen
fn same<T: Eq>(left: T, right: T) returns Bool {
  left == right
}
```

`Eq` permits `==` and `!=`. A type satisfies `Eq` when equality is available
for that complete concrete type under Sections 3.1 and 3.2. `Never`, `unknown`,
a function type, and a type that contains `unknown` or a function value do not
satisfy `Eq`. Generic type arguments are inferred from value arguments. Every
use of one type parameter MUST infer the same exact type. Version 0.1 does not
provide explicit type arguments, a reusable user-declared constraint, a
constraint other than `Eq`, higher-kinded types, specialization, or generic
recursion. The compiler monomorphizes each used concrete instantiation and MUST
bound the number and expanded type depth of instantiations.

A closure uses function syntax in an expression:

```allen
let offset = 1;
let add_offset = fn(value: Int) returns Int { value + offset };
```

A closure captures an immutable local by value. Version 0.1 rejects mutable
captures, recursive closures, and cyclic closure environments. A closure is not
equal to another value. It cannot be canonically encoded, used as a map key,
wrapped by `to_unknown`, or used as a `narrow` target.

A callback type includes its exact closed effect set:

```allen
fn apply(callback: fn(Int) returns Int, value: Int) returns Int {
  callback(value)
}
```

Callback parameter types, return type, and effect set MUST match exactly in
version 0.1. An omitted callback-type effect clause is the exact empty set.
Effect variance and effect polymorphism are unsupported.

An effect ID uses ASCII lower-case segments separated by `.`. Each segment
starts with `a` through `z` and continues with `a` through `z`, `0` through `9`,
or `_`. A tool or other versioned effect MAY end with `@` and a positive decimal
major version without a leading zero. Examples are `fs.read`, `agent.ask`, and
`tool.github.create_issue@2`. An effect set contains canonical IDs once and in
ascending UTF-8 byte order.

Every named function and closure MAY omit its effect clause. Omission is
exactly equivalent to an explicit empty clause and declares pure code. An explicit effect
clause is a maximum effect contract. A declared superset is valid. A function
cannot use an effect that is absent from its contract. Pure code cannot call or
capture an effectful callable.

A named function MAY declare a nonempty maximum set while its body uses only
local computation. A call to that function requires its declared effects even
when the call does not perform an external operation. Standard operations
introduce and execute their documented effects.

The standard filesystem namespace provides these version 0.1 operations:

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
record SearchMatch { column: Int line: Int path: String text: String }
async fn fs.search(workspace: Workspace, path: String, query: String)
  returns Result<List<SearchMatch>, FileError> effects [fs.read]
```

`ExternalFsAccess` has the exact variants `Read`, `Write`, and `ReadWrite`.
The standard permission namespace provides these request operations:

```allen
record ExternalFileRequest {
  access: ExternalFsAccess
  path: String
  reason: String
}
record ExternalDirectoryRequest {
  access: ExternalFsAccess
  path: String
  reason: String
  recursive: Bool
}
async fn permission.request_file(request: ExternalFileRequest)
  returns Result<Workspace, PermissionError>
  effects [permission.request_external_fs]

async fn permission.request_directory(request: ExternalDirectoryRequest)
  returns Result<Workspace, PermissionError>
  effects [permission.request_external_fs]
```

An external file grant uses `.` as its only file path. A directory grant uses
the normal normalized relative path rules. A nonrecursive directory grant
permits the root and direct-child files. A recursive grant permits descendants.
The request result can be passed to the existing `fs` operations. A file grant
cannot be listed or widened to its parent.

The standard network namespace provides this version 0.1 operation:

```allen
async fn http.get(url: String) returns Result<{
  body: Bytes
  final_url: String
  headers: Map<String, List<String>>
  status: Int
}, NetworkError> effects [net.http_get]
```

`NetworkError` is the exact structural record `{ code: String, message: String
}`. The URL is parsed and validated when the lazy operation starts. A non-2xx
status is not a transport error.

`FileError`, `NetworkError`, `AgentError`, `UserError`, `SubAgentError`,
`ModelError`, and `PermissionError` are each the exact structural record
`{ code: String, message: String }`. Their code is the only programmatic
discriminator; messages are bounded, safe, and nonsecret.
Its code and safe message do not expose an ambient path. A filesystem call is
lazy and starts only when its future is awaited or spawned. `fs.list` returns
UTF-8 entry names in ascending byte order. `fs.search` recursively scans regular
UTF-8 files for a literal, case-sensitive query. It returns at most one match
per line with the first matching one-based UTF-8 byte column. Matches are
ordered by normalized path and then one-based line number. Search includes
hidden files and does not apply ignore files. It skips symbolic links, special
files, and files with non-UTF-8 contents; multiply-linked regular files remain
denied. An empty query matches every non-final line, including empty lines. The
filesystem entry limit applies to each searched directory and to the total
match count. A nonrecursive external directory grant searches only its
direct-child files. The exact path `.` selects the workspace root. No other
`.` component is valid.

`Workspace` is opaque. Source cannot construct, compare, encode, narrow, or
widen it. A reference can be copied and passed within one execution, but each
copy refers to the same capability-table entry and rights. The reference
expires with that execution and is invalid in another execution. It cannot
cross an entry input or output boundary.

A denied required filesystem capability fails before execution. A denied or
unavailable optional filesystem capability does not make the workspace
reference invalid. If the program evaluates that optional operation, it
receives `FileError { code: "fs.permission_denied", ... }`. The runtime records
an optional grant only when policy, provider, work-directory root, physical
right, and selected-entry effect all permit it.

The standard effect `task.spawn` identifies local task creation. It is not a
host capability and does not grant external authority. A function that uses
`spawn` MUST include `task.spawn` in its declared effect set. An
async function call also contributes the async function's declared effects to
the caller, even though the returned future remains lazy. This conservative
rule prevents an async computation from hiding authority in a future value.

The synchronous capability-inspection namespace provides:

```allen
fn capability.is_granted(name: String) returns Bool
  effects [capability.inspect]
fn capability.granted() returns List<String>
  effects [capability.inspect]
```

`capability.inspect` is a local observation effect. A function that directly
uses either inspection operation MUST have an explicit effect clause containing
it; an omitted clause is an empty contract. A manifest cannot and
need not request it. The inspected set is
frozen before execution. It contains sorted unique canonical manifest
capability names that were both requested and effectively granted for this
execution. In this profile those inspectable names are `fs.read`, `fs.write`,
`net.http_get`, and `permission.request_external_fs`. It contains no local
effect, tool, provider, network origin, filesystem path, opaque handle,
credential, dynamic external grant, or undeclared authority. `is_granted`
returns false for every malformed,
undeclared, unknown, or excluded name. Inspection grants no authority and
cannot make an operation bypass its ordinary effect, manifest, provider, or
runtime-policy checks.

### 3.4 Complete version 0.1 lexical and syntactic grammar

This section is the complete source grammar for version 0.1. It is normative
over examples elsewhere in this document. The notation uses `[]` for an
optional production, `{}` for repetition, quoted text for literal tokens, and
`one of` for a choice. Lexical whitespace may appear between tokens. A trailing
comma is accepted in every comma-separated source list shown below.

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
non-ASCII byte. In particular, `\\xHH` is not a String escape.

A `template-literal` begins and ends with a backtick. A literal segment accepts
the ordinary String escapes plus these added exact sequences:

```text
\`
\${
```

`\\` remains the one backslash escape. An unescaped backtick ends the template, and unescaped `${`
begins interpolation. Any other escape, unescaped line break, or unescaped
control scalar is rejected. Comment delimiters and braces in a literal segment
are text. Inside `${ expression }`, the normal lexer and grammar apply:
comments are comments, braces nest, and a nested template has its own literal
and interpolation modes. Interpolation expressions evaluate once from left to
right and must have type `String`. A template is an ordinary String; it does
not merge or reinterpret prompt segments.

The lexical terminal `template-text-scalar` below means one permitted literal
segment scalar: any non-control Unicode scalar other than backtick or
backslash, except that `$` is permitted only when it is not immediately
followed by `{`. The lexical terminal `template-escape` means exactly one of:

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
one branch evaluates. Both reachable branches must have one exact result type.
A branch whose result is `Never`, including a `return` or `stop`, is compatible
with the other branch under the ordinary `Never` rule. `else if (...) { ... }`
is right-associated shorthand for an `else` branch containing another
conditional. Because comments are lexical whitespace, whitespace or comments
between `else` and `if` do not change that association.

An `if` without `else` has type `Void`; its true body must produce `Void` or
`Never`, and its absent false body is exactly `()`. Any conditional whose
result type is `Void`, whether it has an `else` or not, may be used as a
statement before a later declaration, assignment, or return and needs no
semicolon. A value-producing conditional cannot be silently discarded; this
does not introduce arbitrary expression statements or implicit value
discarding. Bindings introduced by a branch are local to that branch and do not
escape it. An outer `mut` binding may be reassigned in either or both branches
only under the ordinary exact same-type assignment rule. At each continuing
branch join, affine `Future`/`Task` and must-consume state must agree, and a
`SubAgent` binding must have the same availability and lexical scope. A `Never`
branch does not contribute a join state. Leaving an `await` scope through either
return form still performs its normal structured cleanup, cancellation,
joining, and stopped-outcome handling.

`return expression;` retains its existing meaning. `return;` returns `()` and
is valid only in a function declared to return `Void`; it is valid in both
synchronous and asynchronous `Void` functions. A return inside a conditional
returns from the enclosing function, not merely from the branch. Every
continuing branch remains subject to the function's normal tail and return
rules.

#### Loops and iteration

`while (condition) { body }` evaluates its `Bool` condition before every
iteration and may execute its body zero times. `loop { body }` repeats until a
reachable `break;`, function return, `stop`, runtime failure, cancellation,
timeout, or budget exhaustion. A loop statement has type `Void`; a loop with
no reachable exit still makes following source unreachable.

Every syntactically continuing loop body must have type `Void`. A body whose
tail has type `Never`, including a `return` or `stop`, is accepted because it
does not continue to the loop transfer. A value-producing body tail is not
implicitly discarded, even though the enclosing loop itself has type `Void`.

`for item in values { body }` evaluates `values` exactly once and retains that
immutable snapshot for the loop. A `List<T>` yields `T` in list order, `Bytes`
yields `Int` values from 0 through 255 in byte order, `String` yields one
single-scalar String in Unicode scalar order, and `Map<K, V>` yields `(K, V)`
in canonical sorted key order. String iteration produces exactly the same
sequence as successful `string.get` calls at indexes zero through
`length(value) - 1`. Reassigning a mutable outer binding that supplied
`values` does not change the snapshot.

`for index in start..end { body }` evaluates `start` and then `end` exactly
once as `Int`. It yields the ascending half-open range from `start` through
`end - 1`; it is empty when `start >= end`. The last step is guarded before
addition, so endpoints at the `Int` boundaries neither overflow nor create an
extra iteration. `..` exists only in this clause and is not an expression
operator.

A loop binding is an identifier, `_`, or a one-level tuple of identifiers and
wildcards. It is immutable and scoped to one iteration. Tuple arity and element
types must exactly match a tuple yielded by the iterable; names in one binding
must be unique. `break;` exits the innermost loop and `continue;` starts its
next condition or iteration step. Neither has a value or label, neither is
valid outside a loop, and a closure cannot target a loop outside that closure.
An outer `mut` binding may be reassigned with its exact type.

Static loop effects are the sorted union of the condition or bounds, iterable,
and body even when runtime iteration is empty. Runtime evaluates only the
selected operations. No affine `Future`, `Task`, `SubAgent`, or must-consume
obligation may cross a loop back edge or `continue`. Every iteration must
discharge or transfer such an obligation; all `break` paths agree with the
state after the loop, including the zero-iteration path of `while` and `for`.
A return may transfer ownership normally. Exiting nested `await` scopes by
`break`, `continue`, return, stop, cancellation, timeout, or failure preserves
their structured join or cancel-and-join cleanup rules.

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

## 4. Core semantic model

A program consists of typed declarations and one or more entry points. An entry point receives typed input and returns a typed result. Evaluation is strict unless a construct states otherwise.

### 4.1 Effect system

An effect identifies an interaction with the runtime or the host. An effect also identifies the authority that a function needs. Agent calls, model calls, tool calls, file access, network access, permission requests, and task creation are effects.

An effect does not state that a result is nondeterministic. A file read has an effect because it uses file authority. The file can contain the same data on each read. A model request has an effect because it uses a model service. A recorded model response can have the same result during replay. Both operations still have effects.

A pure function has no effects. It can use only its arguments and local immutable data. A pure function MUST NOT call an effectful function.

An effectful function has an effect set. The effect set states the maximum authority that the function can use. The function does not have to use each effect on each call. An effect declaration does not grant authority. The manifest and the host grant authority.

The compiler MUST include the effects of called functions when checking them against the caller's effect contract. An omitted function, closure, or callback-type effect clause MUST be treated as the exact empty effect set.

A conditional's static effect set is the union of the condition and both
branches, even though execution evaluates only the condition and one selected
branch. A skipped branch performs no allocation, arithmetic trap, provider,
filesystem, network, tool, task-spawn, or `stop` operation.

A Boolean `&&` or `||` expression has the static union of both operands'
effects, even when short-circuiting skips its right operand at runtime. A
skipped right operand performs no allocation, arithmetic trap, provider,
filesystem, network, tool, task-spawn, or `stop` operation.

A loop's static effect set includes its condition or bounds, iterable, and
body. This remains true for an empty range, collection, or initially false
`while` condition. Runtime effects, allocation, traps, task creation, and
`stop` occur only for expressions that are actually evaluated.

The effect set gives useful checks. The compiler can reject a hidden network call in a pure function. The loader can compare required effects with the manifest. The host can compare required effects with granted capabilities. A reviewer can see which host interactions a function can make.

The effect system does not prove that an external operation is safe. It does not prove that an external result is repeatable. The sandbox and host policy MUST enforce authority at runtime.

`stop(reason)` is a built-in control terminator. Its type is `fn stop(reason: String) returns Never`. It is not an authority effect. The compiler MUST NOT add an effect when a function calls `stop`. The manifest MUST NOT require a stop capability.

`stop(reason)` permanently ends the current language execution. The runtime returns a terminal `Stopped` outcome to the host or standalone caller. The program does not receive this outcome. The operation MUST NOT terminate an invoking agent session. It MUST NOT terminate the host. No exception handler, recovery handler, retry policy, or host callback MAY resume the stopped program instance. The runtime SHOULD flush committed audit records before it returns the outcome.

For example, a reachable `stop("Approval was not granted.")` expression
terminates the current execution. A conditional can select whether to evaluate
`stop`, but a branch that is not selected performs no stop operation.

```allen
async fn load_plan(workspace: Workspace, path: String) returns Result<String, FileError>
  effects [fs.read] {
  (await fs.read_text(workspace, path))?
}
```

In this example, `fs.read` states an authority requirement. It does not state
that `load_plan` has a random result. The runtime permits the read only when
the selected execution has the matching workspace right.

The first version SHOULD use closed effect sets. A later version MAY add effect polymorphism. Effect polymorphism MUST NOT let a function hide or widen an effect.

`?` MUST return the `Err` value from the current function. It MUST apply only to compatible `Result` values.

### 4.2 Suspension and concurrency

An `async fn` call returns `Future<T>`, where `T` is the declared return type.
The call captures its arguments but does not start the function body. A future
starts only when `await` or `spawn` consumes it. A future is affine: source code
cannot copy it or consume it more than once. Source code MAY discard a future
that has not started and does not own started work. Discarding it performs no
work and no effect. If a task moves into the arguments of a lazy async call,
the returned future owns that task. The program MUST consume or transfer that
future. It cannot discard the future while the task is live.

A future created directly by a standard provider operation or generated tool
call is also a must-consume value. The program MUST await it, spawn it, or
transfer it. This rule rejects accidentally forgotten provider operations but
does not make them eager: no provider request starts until `await` or `spawn`
consumes the future. An ordinary async-call future that owns no started task
remains discardable.

The runtime MUST NOT automatically flatten nested asynchronous types. For
example, calling an `async fn` that declares `Task<T>` as its return type
produces `Future<Task<T>>`. An `await` removes exactly one `Future` or `Task`
layer.

`spawn future` consumes one `Future<T>`, schedules it, and returns one owned
`Task<T>` handle. It has the effect `task.spawn`. A task is affine and live
until `await` consumes it or ownership moves to another function. A move can
use a local binding, a function argument, or a function return. The compiler
MUST reject a copied task, a discarded live task, use after move, and each
control-flow path that loses ownership. The first version MUST NOT put a
future or task in an aggregate, mutable binding, closure capture, canonical
value encoding, or `unknown` value. It MUST NOT provide detached tasks. An entry
point MUST NOT return a live future or task.

An affine local reference is a move, not an alias that creates another handle.
After a move or consuming `await`, every use of the previous binding is invalid.
Continuing branches MUST agree on the one ownership state, no live affine value
may cross a loop back edge, and closures cannot capture futures or tasks. These
rules make a second reachable await of the same future or task invalid even
through a moved local name or a loop. Mutually exclusive branches MAY each
await the same incoming handle only when every continuing branch leaves the
same consumed state; one execution still evaluates exactly one such await.

Only an explicit `await` can suspend the current task. `await value` consumes
one `Future<T>` or `Task<T>` and produces `T`. Awaiting a task waits for the
task that already started. Awaiting a future runs that lazy computation in the
current task. A function that contains `await`, including an `await` block,
MUST be declared `async fn`. The runtime MUST schedule externally visible
progress at explicit `await` points. It MAY rotate ready tasks at a documented
instruction checkpoint. It MUST NOT add another hidden suspension point that
changes program behavior.

An `await value` expression is valid anywhere an expression is valid inside an
async function, both outside and inside an `await { ... }` block. The enclosing
block changes task ownership and cleanup, not the meaning or result type of the
await expression.

`await { ... }` creates a structured-concurrency boundary and is an
expression. The block owns each task created while that boundary is current.
Ownership of such a task MUST NOT leave the block. A tail value or an explicit
return keeps its normal meaning, but control does not leave until the scope is
clean. Before normal exit, the runtime joins every unfinished owned task in
ascending task-ID order. If a joined task fails, the lowest task ID selects
the reported error and the exit becomes exceptional. A task that produces a
future or task MUST be awaited explicitly. A scope cannot discard that affine
result during an implicit join.

Await blocks MAY nest. A task created in the nested block belongs to the
innermost current block; a task created before entry remains owned by its outer
block. The inner block completes its cleanup before evaluation continues in the
outer block. A live owned `Task<T>` with a non-affine `T` need not be explicitly
awaited before normal block exit: the scope consumes its ownership through the
implicit join and discards the ordinary result. Outside its owning await block,
a live task MUST instead be awaited or transferred.

Normal block exit, an explicit return, `?`, `break`, and `continue` join owned
tasks without cancelling them. An ordinary `Result::Err`, including one
propagated by `?`, is a value and does not cancel siblings. Failure of an
explicitly awaited task propagates through the awaiting task. Failure found by
an implicit join makes that block exit exceptional, using the lowest failed
task ID as described above.

On a runtime error, timeout, cancellation, or `stop`, the runtime MUST prevent
new work in the scope, cancel every unfinished owned task, and join all owned
tasks within a separate finite cleanup budget. A late result from cancelled
work MUST be discarded. Control MUST NOT leave the block before cleanup ends.
If cleanup exceeds its budget during `stop`, the terminal result remains
`Stopped`; the runtime records the cleanup failure as safe internal diagnostic
metadata. For another exit, cleanup-budget exhaustion reports a stable
resource failure. Multiple ready tasks run by ascending task ID with
round-robin rotation.

```allen
await {
  let a = spawn summarize(left);
  let b = spawn summarize(right);
  let left_part = await a;
  let right_part = await b;
  return combine((left_part, right_part));
}
```

Task ownership can instead move to a caller:

```allen
fn startSummary(input: Text) returns Task<Summary> {
  spawn summarize(input)
}
```

The caller of `startSummary` becomes the owner. Passing that handle to another
function moves ownership to the callee. The callee MUST await it or return one
owned task to its caller.

The reserved diagnostic builtin
`allen.internal.task_snapshot(task)` observes one live `Task<T>` without
consuming it. This operation is the only version 0.1 source operation that can
read a task handle without moving it. It returns this exact structural type:

```allen
{
  function: String,
  id: Int,
  location: Option<String>,
  owner_id: Int,
  state: String,
}
```

`id` and `owner_id` are deterministic, execution-local scheduler IDs. `state`
is `ready`, `waiting`, `completed`, or `failed`. Cancelled tasks have no live
source handle and appear only in the host lifecycle trace. `location` uses a
canonical module path and UTF-8 byte span when debug information exists. The
builtin has the local effect `debug.inspect`. This effect is diagnostic; it is
not host authority and needs no manifest capability. The builtin MUST NOT
expose an operating-system process or thread ID, memory address, raw scheduler
scope, task result, error detail, stop reason, captured value, or host handle.
Programs MUST NOT use diagnostic fields as persistent identity or portable
business data.

## 5. Closed error model

Every outcome belongs to exactly one channel: (1) compile/load diagnostics,
(2) expected operation `Result` failures, (3) terminal runtime traps, or (4)
the distinct terminal `Stopped` outcome. There is no source `throw`, `try`, or
`catch`; `?` propagates only an exact compatible `Err` and never intercepts a
trap or stop. The canonical machine-readable registry is
[`errors-0.1.json`](conformance/errors-0.1.json).

| Exact operation | Result error | Registered codes | Retryability | Channel and cleanup |
|---|---|---|---|---|
| parse, manifest, lockfile, artifact, package, schema, entry, capability, tool-catalog, input, workspace, input-limit, and replay-contract validation | diagnostic before execution | compiler: `E0002`, `E0003`, `E0004`, `E0005`, `E2003`, `E2009`, `E2011`, `E2012`, `E2015`, `E2016`, `E2017`, `E2018`, `E2019`, `E2020`, `E2403`, `E3002`, `E3003`, `E3005`, `E3007`, `E3008`, `E3010`, `E3011`; artifact: the 19 `ARTIFACT_*` registry codes; package/load: the listed `package.*`, `manifest.*`, `lock.*`, `runtime.entry_not_found`, `runtime.manifest_invalid`, `runtime.capability_denied`, `tool.catalog_mismatch`, `runtime.invalid_input`, `runtime.workspace_unavailable`, `resource.input_bytes`, and `replay.diverged`; schema: the listed `schema.*` codes | never | no task exists |
| `async fn fs.read_text(Workspace, String) returns Result<String, FileError> effects [fs.read]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.is_directory`, `fs.not_directory`, `fs.not_found`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `fs.invalid_utf8`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn fs.read_bytes(Workspace, String) returns Result<Bytes, FileError> effects [fs.read]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.is_directory`, `fs.not_directory`, `fs.not_found`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn fs.write_text(Workspace, String, String) returns Result<Void, FileError> effects [fs.write]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.not_directory`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn fs.write_bytes(Workspace, String, Bytes) returns Result<Void, FileError> effects [fs.write]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.not_directory`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn fs.list(Workspace, String) returns Result<List<String>, FileError> effects [fs.read]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.not_directory`, `fs.not_found`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `fs.invalid_utf8`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
| `async fn fs.search(Workspace, String, String) returns Result<List<SearchMatch>, FileError> effects [fs.read]` | `FileError` | `fs.permission_denied`, `fs.unavailable`, `fs.hard_link_denied`, `fs.invalid_path`, `fs.io`, `fs.is_directory`, `fs.not_directory`, `fs.not_found`, `fs.special_file_denied`, `fs.symlink_denied`, `fs.target_changed`, `fs.unsupported_platform`, `fs.invalid_utf8`, `resource.limit` | caller for `fs.unavailable`, `fs.io`, `fs.target_changed`; never otherwise | `Err`; `resource.limit` is a terminal trap and joins |
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

Filesystem, HTTP, and output resource exhaustion terminalizes as the single
public code `resource.limit`. The registry contains only codes reachable in
the current implementation and assigns each code to exactly one current
channel.

`runtime.panic` is a bounded safe terminal host error for an implementation
invariant breach; it is never a Rust panic escaping the supervisor. Provider
detail, credentials, paths, prompt data, transcript content, and reasoning are
not language-visible error data. A late or duplicate response is a terminal
`protocol.violation` and cannot resume execution.
Pre-execution replay binding/contract rejection is the `replay.diverged`
diagnostic. Request, schema, order, exhaustion, or final-channel drift found
after execution begins is the distinct `replay.runtime_diverged` terminal trap.

## 6. Agent effects

Agent effects communicate with the agent session that invoked the program. Each effect has a typed input. Each effect has a typed output or a typed error.

Version 0.1 initially provides these lazy operations:

```allen
async fn agent.message(message: String) returns Result<Void, AgentError>
  effects [agent.message]
async fn agent.ask<T>(request: Prompt<T>) returns Result<T, AgentError>
  effects [agent.ask]
async fn agent.transcript(query: { limit: Int }) returns Result<TranscriptSnapshot, AgentError>
  effects [agent.transcript]
async fn model.request<T>(request: Prompt<T>) returns Result<T, ModelError>
  effects [model.request]
async fn user.ask<T>(request: Prompt<T>) returns Result<T, UserError>
  effects [user.ask]

record Projection {
  capabilities: List<String>
  limits: Map<String, Int>
  tools: List<String>
}
```

`agent.message` sends information to the invoking agent. It MUST NOT target a different agent. The operation MAY wait for delivery acceptance. It MUST NOT wait for a content reply.

The version 0.1 reference profile waits for delivery acceptance. Completion
means only that the bound invoking-agent provider accepted delivery.

`agent.ask` asks the invoking agent for a typed reply. It MUST NOT create or select another agent. It waits at an explicit `await` point. The host gives control to the same invoking agent so that the agent can produce the reply. The host then gives control back to the program.

`user.ask<T>(request: Prompt<T>) returns Result<T, UserError>` is separate from
`agent.ask`. The host controls the user interface. It MAY work without an
invoking agent when the runtime has a user-interaction provider. If none exists,
it returns `Err` with a registered `user.unavailable` code.

`sub_agent.create` creates a fresh sub-agent. It returns a typed sub-agent handle. This operation MAY run without an invoking agent if the runtime can create the sub-agent.

`sub_agent.run` creates a fresh sub-agent and waits for its typed result. It is a convenience operation. It MAY run without an invoking agent if the runtime can create the sub-agent. If no sub-agent provider exists, `sub_agent.create` and `sub_agent.run` return `Err(SubAgentError)`.

`sub_agent.message` and `sub_agent.ask` target the selected sub-agent. They MUST NOT target the invoking agent by default.

Version 0.1 provides these source declarations:

```allen
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

`Prompt.context` is the only context projected to a new sub-agent. The
projection record is the complete requested authority projection. Its
capabilities and tools are sorted unique names, and every requested limit is a
positive implemented execution limit. The runtime MUST reject a projection
that is not a subset of the parent's effective capabilities and tools or that
raises an effective parent limit. An omitted prompt context, empty list, or
empty map grants nothing of that kind. A `SubAgent` is an opaque,
execution-scoped, non-serializable handle. Source cannot construct, compare,
store inside another value, return from an entry point, or reuse it in another
execution.

`agent.transcript` returns visible session context as structured data. The default view SHOULD include user-visible user messages, assistant messages, tool calls, tool results, and attachment references. It MUST identify message roles and content kinds. The default view MUST exclude hidden system instructions, hidden developer instructions, hidden reasoning, credentials, and secrets that policy marks as hidden.

The host MUST be able to filter, redact, or omit transcript content. The result SHOULD identify its transcript policy version. It MUST represent redaction or omission when host policy permits that fact to be visible. Programs MUST NOT depend on a complete transcript. The transcript policy is extensible. A later policy version MAY expose or hide more content. No policy version may expose hidden reasoning.

The version 0.1 transcript query has only `limit`. It MUST be from 1 through
100. Returned messages use oldest-first order. `TranscriptSnapshot` has the
fields `snapshot_id: String`, `session_id: String`, `policy_version: String`,
`captured_at: String`, `truncated: Bool`, and
`messages: List<TranscriptMessage>`. A timestamp is canonical RFC 3339 UTC
text with a `Z` suffix.

`TranscriptMessage` has `id: Option<String>`, `role: String`,
`time: Option<String>`, and `content: List<TranscriptPart>`. The role is
exactly `user`, `assistant`, `system_visible`, or `tool`.

`TranscriptPart` is one exact tagged record with `kind` equal to `text`,
`json`, `tool_call`, `tool_result`, `attachment`, `redacted`, or `omitted`.
Each kind contains only its specified payload fields. Attachment content is an
opaque reference, not inline bytes. A redacted part contains only a safe
reason code. An omitted part contains only the omitted content kind and a
positive count. A host MUST NOT put removed content in a marker. The runtime
validates the complete snapshot and its effective byte limit after host
projection. The returned `session_id` MUST equal the bound invoking session.

```allen
record Approval {
  approved: Bool
  note: Option<String>
}

async fn confirm(plan: Plan) returns Result<Approval, AgentError>
  effects [agent.ask] {
  await agent.ask<Approval>(prompt {
    system: "Review the plan. Return an approval decision."
    data: { plan }
  })
}
```

`agent.ask` and all other typed reply operations MUST use the validation rules in Section 8. A validation failure MUST NOT make an invalid value available to the program.

A retry of `agent.ask` MUST ask the same invoking agent. It MUST NOT change the target to a sub-agent or a new session.

`agent.message`, `agent.ask`, and `agent.transcript` return documented
`Err(AgentError)` values when no invoking-agent session exists. The runtime
MUST detect this condition before it attempts the operation. This rule does not
apply to `user.ask`, `sub_agent.create`, or `sub_agent.run`.

The absence of an invoking agent MUST NOT make program loading fail. The runtime
returns `Err` only when the program evaluates an operation that requires the
invoking agent. A branch that does not evaluate such an operation can complete normally.

## 7. Prompt construct

`prompt` is a first-class language construct. It is not an untyped string alias. A prompt value MUST preserve instruction, context, data, and output-contract boundaries. These boundaries support validation and audit. They do not guarantee that a model will ignore instructions that occur in untrusted data.

The exact prompt syntax and component model are preliminary. The first version SHOULD start with these fields:

- `system` contains stable task instructions.
- `context` contains visible supporting content.
- `data` contains typed values.
- `output` names the requested response type when type inference cannot determine it.
- `policy` contains optional execution preferences. A host MAY reject an unsupported preference. Provider-specific preferences MUST use an extension namespace.

`system` is a language-level segment name. It does not grant instruction priority. A host adapter MUST NOT place this segment above the host's own system, developer, safety, permission, or user policy. The host MAY map it to a lower-priority provider role.

String interpolation inside a prompt MUST preserve the value boundary. The runtime MUST encode an interpolated non-text value as data. It MUST NOT silently join the value to instruction text. A deliberate text conversion MUST be explicit.

```allen
record Summary { text: String }

record SummaryError {
  code: String
  message: String
}

async fn summarize(records: List<String>) returns Result<Summary, SummaryError>
  effects [agent.transcript, model.request] {
  let transcript = (await agent.transcript({ limit: 20 }))?;
  let request: Prompt<Summary> = prompt {
    system: "Summarize the records. State only supported facts."
    context: transcript
    data: { records }
    output: Summary
    policy: { max_attempts: 2 }
  };
  await model.request<Summary>(request)
}
```

A prompt type is `Prompt<T>`, where `T` is the response type. The version 0.1
component contract contains required `system` and `output`, optional `context`
and `data`, and optional `policy`. The only version 0.1 policy field is
`max_attempts`, from 1 through 3 including the initial attempt. Prompt
composition MUST preserve field boundaries and source labels. Structured
providers receive separate segments. A text-only provider receives the
canonical `Allen-PROMPT/1` rendering: labeled, length-prefixed canonical JSON
segments followed by an `END` marker. Segment values therefore cannot create
or terminate another segment. Libraries MAY provide prompt templates with
typed parameters. Reusable-template versioning and multimodal content are
unsupported.

## 8. Typed responses

This section applies to `model.request`, `agent.ask`, `user.ask`, `sub_agent.ask`, and `sub_agent.run` when they request a typed result.

The runtime MUST lower the response type to a strict response schema. The schema MUST reject unknown fields at every record boundary. The schema MUST reject implicit coercion. Required fields MUST remain required. Tagged unions MUST keep their discriminators.

`Option<T>` has the exact JSON forms `{"tag":"None"}` and
`{"tag":"Some","value":value}`. It is never represented by `null` or an
omitted record field. The same exact tagged representation rule applies
recursively.

The runtime MUST validate each response before it returns the value to the program. On validation failure, the runtime MAY retry when the active policy permits a retry. `max_attempts` and the `response_attempts` manifest and host limits count the initial response. The effective total is their minimum and the runtime hard maximum of 3. The runtime gives only bounded JSON Pointer and stable-code validation issues to the response producer for the next attempt. If all attempts fail, the operation MUST return a typed validation or request error. It MUST NOT return partially valid data as the requested type.

For a model-produced response, the host SHOULD record the model identity, schema version, attempt count, and validation result in execution metadata. It MUST NOT record protected prompt data when policy forbids it. A retry MUST remain within the effective resource limits.

## 9. Tool model

At program load, the host supplies the runtime with a frozen tool catalog. In an agent-hosted execution, this catalog MUST contain every tool and schema that the invoking session can use for that execution. The program manifest selects the tools that the program can invoke. A host policy MAY deny invocation, but it MUST NOT silently change a declared schema. The host supplies each tool definition as typed schemas. A tool definition MUST include a stable name, input schema, output schema, declared effects, and error schema. The compiler MAY generate language types from the schemas.

Version 0.1 uses required tools only. A required-tool manifest entry contains
one canonical tool name and one bounded semantic-version range. A canonical
tool name is valid UTF-8 of 1 through 255 bytes. It contains one or more
nonempty segments separated by the ASCII byte `.`. Each segment is at most 63
UTF-8 bytes and contains no control or whitespace scalar. A manifest MUST
reject a duplicate name and an empty segment. It MUST compare the exact UTF-8
bytes and MUST NOT normalize case or Unicode. A bounded range has the exact
form `>=M.m.p, <M.m.p`. Versions contain decimal integers without a leading
zero, a pre-release part, or build metadata. The lower bound MUST be less than
the upper bound. The catalog contains at most one exact version for each name.
That version MUST be inside the requested range.

A package manifest uses this form:

```toml
[[tools.required]]
name = "github.create_issue"
version = ">=2.0.0, <3.0.0"
```

The equivalent inline-manifest field is a `tools` record with a `required`
list of `{ name, version }` records. Version 0.1 has no optional tool, dynamic
tool name, or manifest-provided schema.

The program MUST receive a typed tool namespace. It MUST NOT receive an untyped tool invocation escape hatch. A program can invoke a tool only when its manifest declares that tool and the effective capability set permits it.

The runtime MUST validate tool input before invocation. It MUST validate tool output before it returns control to the program. It MUST reject unknown fields and implicit coercion at both boundaries. A runtime input, output, or error-schema failure MUST return the generated `Error.Schema { code: "tool.schema", message: ... }` variant.

```allen
async fn publish(report: Report)
  returns Result<tools.report_publish.Output, tools.report_publish.Error>
  effects [tool.report_publish@1] {
  await tools.report_publish.call({ report })
}
```

Tool names and schemas form part of the program capability contract. The tool catalog MUST be frozen for an execution before program loading completes. A missing required tool MUST fail during program loading. The runtime MUST NOT add an untyped or dynamic lookup path. If a declared tool cannot be dispatched after loading, the call MUST return the generated `Error.Unavailable { code: "tool.unavailable", message: ... }` variant.

The generated namespace retains the catalog error as `DeclaredError` and defines
`enum Error { Declared(DeclaredError), Unavailable { code: String, message: String }, Schema { code: String, message: String } }`.
The generated operation returns `Result<Output, Error>`. A validated declared
error produces `Declared`; dispatch absence produces `Unavailable`; and invalid
host input, output, or error values produce `Schema`. The runtime MUST discard
an outcome that arrives after cancellation.

At the VM boundary, a structurally valid `Result::Err` is rejected with
`protocol.violation` unless its standard error code is registered for that
exact operation and its message is protocol-bounded. Generated tool
`Unavailable` and `Schema` variants must carry their exact operational codes.
Replay also revalidates tool output and declared-error payloads against the
frozen strict catalog schemas.

The compiler applies the tool-segment mangling rule in the implementation
specification to every canonical-name segment. It creates one virtual leaf
namespace for each complete tool name. A tool name can also be a prefix of
another name. Each leaf has the fixed members `Input`, `Output`,
`DeclaredError`, `Error`, and `call`, its generated tagged-union declarations,
and any child tool namespace. `Input`, `Output`, and `DeclaredError` lower the
catalog schemas; `Error` is the closed wrapper defined above.
`call(input: Input) returns Result<Output, Error>` is the only operation. For
example, `github.create_issue` is called as
`tools.github.create_issue.call(input)`. Effect-name mangling is separate from
source-name mangling. In an effect segment, the compiler preserves lower-case
ASCII letters, digits, and `_`; encodes every other UTF-8 byte as `_xhh_` with
lower-case hex; and prefixes `_n_` when the first preserved character is a
digit. It rejects effect-name collisions. The generated effect is `tool.` plus
the dot-separated effect-mangled name plus `@` and the selected version major.
Thus `release-tools.create-issue` has effect
`tool.release_x2d_tools.create_x2d_issue@2`.

A tagged union gets one generated nominal enum in the same leaf namespace. Its
name is `Input_union_`, `Output_union_`, or `Error_union_` plus the first 16
lower-case hex digits of the SHA-256 digest of its expanded-schema JSON Pointer.
Each variant name is `_tag_` plus the tag string after the same UTF-8 byte
mangling. The compiler retains the full pointer and rejects a truncated-digest
collision. It also rejects a collision between any generated namespace,
member, enum, or variant after mangling. These generated declarations are
read-only and cannot be imported without the complete `tools.` qualification.

### 9.1 Version 0.1 tool-schema profile

The profile ID is `allen.tool-schema/0.1`. The input document uses JSON Schema
2020-12 and has the exact dialect URI
`https://json-schema.org/draft/2020-12/schema`. Schema objects reject duplicate
keys and every keyword that this section does not list.

The profile supports these forms:

- `{"type":"null"}` lowers to `Void`.
- `{"type":"boolean"}` lowers to `Bool`.
- An integer schema has `type`, `minimum`, and `maximum`. Both bounds are JSON
  integers in the `Int` range. It lowers to `Int` and keeps both bounds for
  boundary validation.
- A number schema has `type` and MAY have `minimum` and `maximum`. Each bound
  is finite. It lowers to `Float`. The wire value MUST be finite.
- A string schema has `type` and MAY have `minLength`, `maxLength`, or `enum`.
  Length counts Unicode scalar values. An enum is a nonempty sorted unique list
  of strings. It lowers to `String` and keeps the limits for validation.
- A list schema has `type: "array"`, one `items` schema, `minItems`, and
  `maxItems`. It lowers to `List<T>`.
- A tuple schema has `type: "array"`, a nonempty `prefixItems` list,
  `items: false`, and equal `minItems` and `maxItems` values. The values equal
  the tuple length. It lowers to a tuple.
- An exact record has `type: "object"`, `properties`, `required`, and
  `additionalProperties: false`. `required` contains every property name once
  in ascending UTF-8 order. It lowers to a structural record with fields in
  the same order.
- A string-key map has `type: "object"`, an empty `properties` object,
  `required: []`, and one schema in `additionalProperties`. It lowers to
  `Map<String, T>`.
- A tagged union has `oneOf` with 2 through 64 exact-record branches. Each
  branch has a required `tag` property whose schema is one distinct
  single-value string `enum`. Every branch uses the same discriminator name
  `tag`. It lowers to a generated nominal enum. Other branch fields form one
  record payload. A branch with only `tag` is a payloadless variant.
- A root schema MAY have `$defs`. A schema position MAY contain only a local
  `$ref` of the form `#/$defs/<token>`. The token uses JSON Pointer escaping.
  A reference object has no sibling keyword. Definitions are unique, used,
  acyclic, and in ascending UTF-8 name order.

The metadata keywords `title` and `description` are allowed on a schema that
also has one supported form. They are UTF-8 strings and do not affect lowering
or a schema digest. No other annotation affects the contract. In particular,
the profile rejects `default`, `examples`, `format`, `pattern`, `const`,
`nullable`, type arrays, remote references, recursive references, open
objects, optional object properties, overlapping union tags, and implicit
coercion.

The schema limits apply before and during lowering. One schema is at most
262,144 UTF-8 JSON bytes, 4,096 schema nodes, depth 32 after reference
expansion, 256 object properties, 256 definitions, 256 enum strings, and 64
union branches. A property, definition, or tag is at most 255 UTF-8 bytes. A
string or collection bound cannot exceed 1,048,576. The catalog is at most 256
tools and 3 MiB of decoded schema text. A host can lower these limits during
initialization.

The runtime computes a schema digest from the lowered descriptor, not from the
input JSON text. It removes `title` and `description`, expands local
references, sorts record fields by UTF-8 bytes, and writes the supported form
as canonical JSON. Canonical JSON uses UTF-8, sorted object keys by UTF-8 bytes,
no insignificant whitespace, lower-case literals, decimal integers without a
leading zero, and the shortest finite binary64 number that reads back to the
same value. It escapes only `"`, `\\`, and control characters, using lower-case
hex for a required `\\u00xx` escape. The digest text is lower-case
`sha256:` plus the SHA-256 hex digest of those bytes. Two schemas have the same
digest exactly when their lowered canonical descriptors are byte-identical.

The lowered descriptor uses only these exact canonical JSON forms:

- Void and Boolean are `{"kind":"void"}` and `{"kind":"bool"}`.
- Integer is `{"kind":"int","max":I,"min":I}`.
- Number is `{"kind":"float","max":N-or-null,"min":N-or-null}`.
- String is
  `{"enum":[...],"kind":"string","max":I-or-null,"min":I-or-null}`.
- List is `{"items":D,"kind":"list","max":I,"min":I}`.
- Tuple is `{"items":[D...],"kind":"tuple"}`.
- Record is `{"fields":[{"name":S,"schema":D}...],"kind":"record"}`.
- String-key map is `{"kind":"string_map","values":D}`.
- Tagged union is
  `{"kind":"tagged_union","variants":[{"fields":[...],"tag":S}...]}`.

`D` is another descriptor. Record fields are in ascending UTF-8 name order.
Union variants are in ascending UTF-8 tag order, and their fields use the
record order. An absent optional bound is JSON `null`. An unconstrained string
enum is the empty list. No source annotation, definition name, or reference
path enters this descriptor.

Generated nested-union names use a pointer in the fully expanded canonical
descriptor, rooted at `/input`, `/output`, or `/error`. A record field appends
`/fields/<name>`, a list appends `/items`, a tuple item appends
`/items/<index>`, a map appends `/values`, and a union variant field appends
`/variants/<index>/fields/<name>`, where the index refers to the canonical
tag-sorted variant array. Names use JSON Pointer escaping.
Expansion happens at each reference use, so two use sites have distinct
pointers even when they refer to one `$defs` entry. The SHA-256 name suffix is
computed from the pointer's UTF-8 bytes.

## 10. Security and capability model

Every program MUST have a manifest model. A package uses `allen.toml`. One
standalone source file MAY use one inline manifest. The compiler synthesizes a
capability-free manifest for a loose core source file. The
manifest MUST declare entry points, requested capabilities, required tools,
and language version. The runtime MUST reject undeclared effects.

```allen
manifest {
  language: "0.1"
  entry: main
  capabilities: [fs.read(workdir), fs.write(workdir)]
}
```

The host selects one working directory before execution. The default sandbox policy grants declared `fs.read(workdir)` and `fs.write(workdir)` capabilities. These capabilities apply only to that directory and its descendants. A host policy MAY remove or narrow either capability.

The canonical package lockfile is `allen.lock`. Version 0.1 resolves local
source dependencies only. Each dependency has one source alias, exact selected
version, normalized root-relative source path, language selection, content
hash, and sorted dependency list in the lockfile. A stale or non-canonical
required lockfile fails before compilation. Registries and network fetching are
not part of this package profile.

An entry names one exported function and exact input and output types. The
function has zero or one parameter; zero parameters mean `Void` input. Entry
input and output use exact JSON validation. Void is JSON `null`. Scalars use
their exact JSON scalar except non-finite floats use their canonical strings.
Bytes use `{ "$bytes": "<canonical-base64>" }`. Lists and tuples use arrays.
Maps use a sorted array of `[key, value]` pairs. Records use exact objects.
`Option`, `Result`, and user enums use `{ "tag": String, "value": ... }`, with
`value` absent for a payloadless variant. Unknown fields, missing fields,
duplicate map keys, unsorted map keys, implicit coercion, and values outside
declared limits are invalid. Callable, future, task, workspace, and `Never`
types cannot be entry types.

The runtime MUST deny file access outside the effective file scopes. It MUST prevent `..` traversal, symbolic-link escape, and equivalent path aliases. It MUST make the access decision on the object that it opens. A path check followed by an unsafe open is not sufficient. A conforming runtime SHOULD use operating system confinement in addition to language checks.

The default sandbox MUST deny subprocess creation. A program can use a subprocess only through a host-supplied tool with a declared schema and capability.

The default network interface permits only an HTTP `GET` operation. The
version 0.1 reference profile accepts absolute `https` URLs only. It rejects
plain HTTP and all other schemes. It returns response data. It does not render
the response, execute scripts or active content, or provide a browser object
model.

The default `GET` operation MUST NOT send ambient cookies, authorization
headers, client certificates, or host credentials. It MUST reject credentials
in a URL. The version 0.1 reference profile accepts no source-provided request
header. It sends only a fixed runtime `User-Agent` and `Accept-Encoding: gzip`.
It supports identity and gzip response bodies and rejects unknown, nested, or
multiple content encodings. It MUST enforce host limits for connection time,
first byte, idle time, total time, redirect count, DNS results, header count,
header size, compressed size, decoded response size, and decompression ratio.
It MUST validate each redirect as a new request. It MUST NOT change the method
on redirect.

The default destination policy MUST deny loopback, link-local, private, multicast, and host-metadata addresses. The runtime MUST apply the policy after name resolution. It MUST protect the connection against DNS rebinding. A host MAY grant narrow access to a denied destination through a separate capability or tool.

```allen
let response = (await http.get("https://example.test/data.json"))?
let data: Bytes = response.body
```

Version 0.1 does not define `decode<T>` or another JSON-decoding source
operation. A later language change must define its exact failure contract before
programs can decode a response body into a typed record.

An HTTP `GET` is not necessarily safe. A remote service can change state in response to `GET`. A URL can also disclose data. The runtime MUST treat `net.http_get` as an effect. The host SHOULD let policy restrict destinations. The host SHOULD treat combined file-read and network authority as a data-exfiltration risk.

All other raw network operations MUST be denied by default. Other network operations SHOULD use declared tools.

A manifest request is not a grant. The host MUST calculate the effective capability set from the manifest, user policy, agent policy, and runtime limits. The effective set MUST be equal to or narrower than the requested set. A package that requests `net.http_get` MUST declare canonical HTTPS origins in `[network.http_get]`. A standalone launch MUST separately allow each effective origin. The operation is permitted only when the normalized destination origin appears in both sets.

Before entry execution, the runtime MUST freeze the intersection of requested
manifest capabilities and grants that are actually effective for that launch.
That immutable set is the sole input to `capability.is_granted` and
`capability.granted`. It MUST NOT include the narrower path, origin, handle, or
provider state used to enforce an operation. Inspecting the set neither widens
it nor substitutes for the operation's ordinary authority checks.

`permission.request_file` and `permission.request_directory` have the explicit effect `permission.request_external_fs`. A function that calls either operation MUST declare this effect. The manifest MUST declare this effect before the program can make a request.

The `permission.request_external_fs` effect is authority to request access. It is not authority to access an external file or directory. A declaration of this effect MUST NOT grant external file access. The runtime issues a separate unforgeable capability only after it approves a request.

A running program MAY request file access outside the working directory. The
request names an `ExternalFsAccess` mode, absolute native path, reason, and a
directory recursion flag when applicable. Grant duration is the current
execution. Effective limits supply the maximum byte scope. The runtime MUST
resolve and retain the target before it presents the request. An existing
target retains a descriptor and identity. A missing write-capable file request
retains the parent descriptor, final component, and expected absence. A
directory request must name an existing directory. The request MUST go to the
invoking agent. The agent can ask the user for approval. User approval is a
prerequisite when host policy requires it. User approval does not create a
capability. The invoking agent and host still decide whether to issue the
requested capability. If no invoking agent exists, the operation returns
`Err(PermissionError { code: "permission.unavailable", ... })`.

An approved grant MUST be equal to or narrower than the requested access. It MUST apply only to the current execution. It MUST expire when that execution ends. The runtime MUST NOT persist the grant. A denial MUST return `Err(PermissionError { code: "permission.denied", ... })`.

```allen
record ExternalReadError {
  code: String
  message: String
}

async fn readExternalNotes() returns Result<String, ExternalReadError>
  effects [permission.request_external_fs, fs.read] {
  let grant = (await permission.request_directory({
    path: "/outside/path",
    access: ExternalFsAccess.Read,
    recursive: false,
    reason: "Read the selected source documents."
  }))?;

  await fs.read_text(grant, "notes.txt")
}
```

Capabilities MUST be unforgeable runtime values. Source code MUST NOT create or widen a capability token. A called function MUST receive only the capabilities permitted by its declared effect set.

## 11. Error taxonomy

The four disjoint channels and exact operation/code inventory are normative in
Section 5. Expected failures use the operation's `Result<T, E>` record;
compile/load rejection is diagnostic; arithmetic, invalid trapping collection
access, limits, timeout, cancellation, and runtime/protocol invariants are
terminal traps; and `Stopped` is separate from all three.

A source-visible expected error has exactly `{ code: String, message: String }`.
The code is the stable discriminator and the bounded message is safe and
nonsecret. Source-visible errors have no cause chain, structured metadata,
provider detail, or subtype. A host MAY retain richer private diagnostic data,
but it MUST NOT expose that data through the language-visible value.

`Stopped` is an execution outcome. It is not an error. It is not a value of the entry point return type. A program cannot catch or inspect it. The host MAY report the supplied stop reason. The host MUST treat that reason as untrusted program output and apply its output policy.

## 12. Deterministic boundary

Pure evaluation MUST be deterministic for the same language version, program, and input. Integer arithmetic, text operations, collection ordering, serialization, and pattern matching MUST have specified behavior.

Time, randomness, files, networks, tools, agents, models, scheduling, and permission decisions are outside the deterministic boundary. Programs MUST access these values through declared effects. A runtime SHOULD support effect recording and replay. A replay MUST preserve validated values and error results at effect boundaries. A replay MUST NOT claim that the external system ran again. The frozen effective manifest capability set is execution input rather than ambient host state. Record and replay MUST bind that sorted set to the replay contract, reproduce it during playback, and reject a mismatch before entry execution.

Map iteration order is the canonical sorted key order from Section 3.1.
Floating-point behavior MUST name the supported IEEE 754 profile.
Serialization used for schemas and audit records MUST have a canonical form.

## 13. Lifecycle

A runtime MUST process a program in these stages:

1. Parse the source and manifest.
2. Resolve modules, types, effects, tools, and capabilities.
3. Reject invalid source or an unsatisfied static contract.
4. Create the sandbox and freeze the effective manifest capability set.
5. Validate the entry input.
6. Run the entry point.
7. Close or cancel all owned tasks and resources.
8. If the entry point returns, validate and return its result.

If the program calls `stop(reason)`, the runtime MUST go to the resource-cleanup stage. It MUST skip entry-result validation. It MUST then return the terminal `Stopped` outcome and the reason to the host or standalone caller.

Standalone and unattended execution MUST be valid modes. In these modes, only operations that require an invoking agent MUST fail because that agent is absent. The runtime MUST continue to allow pure code and permitted local effects. It MUST allow model calls and tool calls when it has the required providers. It MUST allow `user.ask` when it has a user-interaction provider. It MUST allow `sub_agent.create` and `sub_agent.run` when it has a sub-agent provider.

The host MUST define limits for time, memory, effect count, and concurrent tasks. The manifest MAY request limits. The host MAY lower them. Resource exhaustion MUST terminate with `resource.limit`; an execution deadline MUST terminate with `runtime.timeout`.

## 14. Portability

A conforming program MUST have the same type and control-flow meaning on every conforming runtime. External results can differ across hosts.

Runtime-specific features MUST use namespaced capabilities or tools. A portable program SHOULD test optional capabilities before use or provide separate manifest profiles. The standard library MUST specify behavior independently from an operating system path format, locale, or shell.

The language MUST define a machine-readable conformance profile. A host SHOULD expose its language profile, standard capabilities, tool schemas, limits, and optional features before execution.

## 15. Profile selection

The `0.1` selector identifies the current evolving profile. A manifest MUST
select that profile or an accepted range containing it, and a runtime MUST NOT
silently select a different profile. Unknown manifest fields MUST be rejected
unless they use an approved extension namespace. Tool implementations and
model providers are independent host contracts and SHOULD carry their own
identities where required.
