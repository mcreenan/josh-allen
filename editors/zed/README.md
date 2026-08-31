# ALLEN support for Zed

This extension adds ALLEN 0.1 language detection, Tree-sitter syntax
highlighting, comment toggling, bracket matching, indentation, and outline
items for `.allen` source files.

The grammar lives in `tree-sitter-allen` and follows the current grammar in
`docs/language-spec.md`. Run the validation script from the repository root:

```sh
./editors/zed/scripts/test.sh
```

For local development, open Zed's command palette, run **zed: install dev
extension**, and choose `editors/zed`. The grammar revision in
`extension.toml` must refer to a commit that contains the grammar sources.
