# ALLEN editor support

This directory contains a VS Code-compatible language extension for current
`.allen` source. It provides TextMate syntax highlighting and editor behavior;
the compiler remains authoritative for parsing, diagnostics, and execution.

## Current language support

The normative grammar and operator table are in
[`docs/language-spec.md`](../../docs/language-spec.md). Loose source, module
bundles, inline manifests, and packages all use that grammar.

Highlighting covers:

- imports, records, enums, transparent type aliases, patterns, functions, closures, generics, and exact
  effect clauses;
- mutable declarations, assignment, literals, escapes, tuples, lists, maps,
  records, prompts, templates, and generated tool names;
- `if`/`else`, `match`, `while`, `loop`, `for`, ranges, `break`, `continue`,
  `return`, `async`, `spawn`, and `await`;
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
agent/tool forms, inline manifests, incomplete source, comments, templates,
conditionals, loops, and iteration. The `allen-syntax` integration suite
independently runs the canonical lexer and parser over the same complete fixture
inventory, including recovery cases; TextMate highlighting is never a compiler
parser.
