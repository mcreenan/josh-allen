# FUN-DEFAULT-ARGS: default arguments

Status: Implemented

Depends on: `FUN-LABELED-ARGS`

Back to the [selected language features](../../../ROADMAP.md#selected-language-features).

Design source: [FUN-DEFAULT-ARGS](../../../docs/plans/allen-language-features-proposal.md#fun-default-args-default-arguments).

## Decision summary

Permit a function declaration to assign pure default expressions to a suffix
of its parameters. Positional calls can omit only that suffix. Fully labeled
calls can omit any parameter that has a default.

A default can refer to constants and earlier parameters. It cannot refer to a
later parameter, local state, a capability, or an effectful operation.

The compiler evaluates supplied arguments once in source order. It then
evaluates missing defaults in declaration order and emits a complete positional
call.

## Compiler work

1. Add optional parameter defaults to declarations and the CST.
2. Type-check each default in declaration scope against its parameter type.
3. Reject a required parameter after the first defaulted parameter.
4. Expand missing arguments during direct-call resolution.
5. Record exported defaults and their canonical source digest in artifacts.

## Acceptance contract

- Calls through function values supply every argument.
- The compiler copies default semantics and does not reparse source at each
  call.
- Changing a public default changes the artifact and package contract digest.
- Imported declarations apply the same expansion as local declarations.
- Tests cover labeled omission, positional suffix omission, generic defaults,
  source order, invalid references, artifacts, and diagnostics.

## Out of scope

The first version does not add variadic parameters, overloads, runtime default
maps, or defaults on external host and tool operations.
