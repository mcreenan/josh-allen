# Current parser seeds

These tracked inputs make the frozen comment-fuzz coverage reproducible. Run the
short smoke, when `cargo-fuzz` is installed, from the repository root:

```sh
cargo fuzz run parser fuzz/seeds/parser -- -runs=1000
```

The byte-oriented loading adapter invokes the compiler only for strictly valid
UTF-8 and explicitly validates the rejection boundary for invalid byte input.
The `invalid-utf8` seed keeps that branch in the starting corpus; the CLI
integration test separately proves filesystem source loading rejects it. The
control-flow seeds cover comments between `else` and `if`, nested conditionals
and bare return, and a truncated else-if condition. The loop seeds cover valid
and malformed `while`, `loop`, and `for` forms, tuple and wildcard bindings,
comments around range punctuation, nested `break` and `continue`, and truncated
range/control headers.
The operator seeds cover remainder and every numeric compound-assignment form,
adjacent precedence tiers through `&&` and `||`, and malformed or truncated
binary and compound operators.
