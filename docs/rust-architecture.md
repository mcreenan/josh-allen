# Rust architecture

This document defines the current organization rules for ALLEN and JOSH Rust
code. It is descriptive of this repository and prescriptive for new work.

## Influences

The organization follows three useful patterns from established Rust compiler
projects:

- [rustc](https://rustc-dev-guide.rust-lang.org/overview.html) separates the
  compilation pipeline into explicit stages with typed boundaries rather than
  one command-driven pass.
- [rust-analyzer](https://rust-analyzer.github.io/book/contributing/architecture.html)
  keeps syntax independent from higher semantic and protocol layers and uses
  small crates or modules at real dependency boundaries.
- [Gleam](https://github.com/gleam-lang/gleam/tree/main/compiler-core/src) uses
  a thin compiler-core facade over responsibility-named modules such as parse,
  AST, type analysis, diagnostics, metadata, and code generation.

ALLEN applies those ideas without creating a crate for every pass. A crate is a
trust, dependency, process, or reuse boundary; a module is an internal
responsibility boundary.

## Rules

1. `lib.rs` and binary `main.rs` files are facades. They declare modules,
   re-export the supported API, and perform minimal dispatch.
2. Modules are named for current responsibilities such as `parser`, `tool`,
   `handshake`, `events`, `broker`, `session`, or `runner`. Phase numbers and
   format-version numbers are not architectural boundaries.
3. Parsing, semantic resolution, IR, verification, execution, host adaptation,
   and transport remain separate layers. A lower layer does not import a
   higher one.
4. Wire and artifact identifiers are validated at their boundary, then current
   behavior is unconditional. Internal APIs do not carry a version argument
   merely to select semantics.
5. Host adapters translate strict boundary types into host-neutral provider
   traits. Provider routing does not leak into the compiler or bytecode model.
6. A module owns its private helpers and unit tests. Integration tests use only
   public crate APIs and live under `tests/`.
7. Extract a module when a file mixes independently testable responsibilities,
   has distinct dependency needs, or obscures the public facade. Do not split
   cohesive code solely to satisfy a line-count target.
8. Public types express invariants. Prefer verified, prepared, frozen, or
   opaque wrapper types over Boolean mode flags and partially valid structs.

## Workspace layers

```text
source/package/schema
        |
        v
lossless syntax -> compiler -> bytecode verifier -> VM
                              |
                              v
                         runtime brokers
                              |
                              v
                     JOSH host and protocol
```

The CLI depends on the language layers to expose user commands. The testkit
depends on current bytecode, VM, and runtime contracts to record and replay
effects. Filesystem and HTTP brokers remain isolated because they have distinct
security and platform dependencies.

## Review checklist

For a new module or refactor, check:

- Does the name describe a current responsibility?
- Is the dependency direction lower-to-higher and acyclic?
- Can invalid state be excluded by the type or constructor boundary?
- Does the public facade expose only what callers need?
- Are current semantics unconditional after boundary validation?
- Are tests colocated at the narrowest useful boundary?
- Did the change avoid a new compatibility flag, alias, fallback, or duplicate
  parser/decoder/provider path?
