# FUN-LOCAL: local named functions

Status: Implemented

Depends on: None

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FUN-LOCAL](../../../docs/plans/allen-language-features-proposal.md#fun-local-local-named-functions).

## Decision summary

Permit a named function declaration inside a function body. The first version
supports noncapturing, nongeneric, synchronous local functions only.

A local function can reference its parameters, top-level constants, imported
items, and top-level functions. It cannot reference a value or mutable binding
from an enclosing body.

Declaration order controls visibility. A local function must appear before its
first use. Recursion and mutual recursion are invalid.

## Compiler work

1. Add local function declarations to body statement grammar and CST data.
2. Create a nested symbol scope without adding value capture.
3. Assign a stable identity from the containing function and lexical ordinal.
4. Reuse ordinary parameter, return-type, effect, and body checking.
5. Lower each local function to an internal function with no exported name.

## Acceptance contract

- Local names cannot escape as package exports.
- Artifact identities and call targets are deterministic.
- Shadowing follows the existing local-name rejection rules.
- A capture diagnostic names the enclosing value and local function.
- Tests cover declaration order, nested scopes, effects, imports, shadowing,
  forbidden captures, recursion, artifacts, and diagnostics.

## Out of scope

The first version does not add captures, generics, `async`, forward
declarations, recursion, mutual recursion, or local tests.
