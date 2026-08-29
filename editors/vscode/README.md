# ALLEN editor support

This directory contains a VS Code-compatible language extension for current
`.allen` source. It provides TextMate syntax highlighting and editor behavior;
the compiler remains authoritative for parsing, diagnostics, and execution.

## Current language support

The normative grammar and operator table are in
[`docs/language-spec.md`](../../docs/language-spec.md). Loose source, module
bundles, inline manifests, and packages all use that grammar.

Highlighting covers:

- ordinary and extension imports, records, immutable record updates, enums,
  transparent type aliases, top-level constants, patterns, functions,
  closures including concise and trailing callbacks, generic calls, labeled
  direct calls, default parameters, partial calls, and exact effect clauses;
- mutable declarations, assignment, literals including decimal separators, escapes, hash-delimited raw strings,
  indentation-trimmed multiline strings, tuples, lists and maps with spreads,
  records, prompts, templates, and generated tool names;
  package template-resource calls use the ordinary
  `templates.<name>.render({ fields })` member-call grammar;
- `if`/`else`, `match`, `while`, `loop`, `for`, ranges, `break`, `continue`,
  `return`, `async`, `spawn`, and `await`, plus postfix `?` for `Result` and
  `Option`, optional member and extension-call chaining, function composition,
  forward pipes, first-class half-open and inclusive ranges, safe bracket
  slices, range and OR patterns, local named functions, and lazy sequences;
- arithmetic, comparison, Boolean, remainder, compound-assignment, range,
  return-type, and match-arm operators; and
- line comments plus nested block comments.

The TextMate grammar cannot enforce types, effect membership, ownership,
exhaustiveness, or source-mode contracts. A highlighted file may still be
invalid ALLEN.

## Packaging

From this directory:

```sh
npx @vscode/vsce package
code --install-extension allen-language-support-0.1.0.vsix
```

## Validation

Install the pinned validation dependencies and run the focused grammar check:

```sh
npm ci
npm test
```

The validator parses every extension JSON file, checks the reserved-word and
built-in-type inventories directly against the current language spec, loads the
grammar through `vscode-textmate` and Oniguruma, and checks all tracked editor
fixtures for declarations, types, literals, escapes, operators, effects,
agent/tool forms, package template-resource calls, inline manifests, incomplete
source, comments, templates, conditionals, loops, iteration, labeled calls,
concise and trailing closures, defaults, partial calls, composition, pipes,
extension calls, record updates, collection spreads, raw strings, multiline
strings, Option propagation and chaining, first-class ranges, slices, range and
OR patterns, local functions, and `Sequence<T>`. The
`allen-syntax` integration suite
independently runs the canonical lexer and parser over the same complete fixture
inventory, including recovery cases; TextMate highlighting is never a compiler
parser.
