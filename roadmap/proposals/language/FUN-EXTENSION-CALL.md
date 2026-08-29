# FUN-EXTENSION-CALL: uniform extension-call sugar

Status: Implemented

Depends on: `COL-COMBINATORS`

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FUN-EXTENSION-CALL](../../../docs/plans/allen-language-features-proposal.md#fun-extension-call-uniform-extension-call-sugar).

## Decision summary

Permit `receiver.operation(arguments)` to call a namespace function whose
first parameter has the receiver's exact static type. The compiler inserts the
receiver as the first positional argument.

Compiler-owned namespaces include `List<T>` to `list`, `Map<K, V>` to `map`,
`String` to `string`, and `Bytes` to `bytes`. This makes `values.map(f)` exact
sugar for `list.map(values, f)`.

An `import extension { render } from "package";` declaration adds an imported
function to extension lookup. The importer's local alias sets the member name.
The exported function itself remains an ordinary function.

Field access wins when the receiver type has that field. Reject every other
ambiguity. Do not search unrelated imports or apply conversions.

## Compiler work

1. Add explicit extension imports to import grammar and package resolution.
2. Delay field-call resolution until the receiver has an exact type.
3. Collect compiler-owned and explicitly imported candidates by member name.
4. Require one candidate with an exact first parameter type.
5. Lower the form to a direct function call with the receiver evaluated once.
6. Preserve field-access diagnostics when no extension candidate exists.

## Acceptance contract

- Extension syntax does not add dynamic method dispatch.
- An ordinary import or local function does not enter extension lookup.
- A field wins over a compiler-owned or imported extension with the same name.
- Generic receiver types must instantiate exactly before resolution.
- Effects, labels, defaults, and callback rules match the expanded call.
- Tests cover every namespace, field precedence, ambiguity, generics,
  ownership, source order, and diagnostics.

## Out of scope

The first version does not add extension declarations, implicit extension
imports, dynamic dispatch, receiver conversion, mutation, or global instance
search.
