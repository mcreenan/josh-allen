# Things that have been decided but not yet implemented

## P0

- [ ] Headless tool provider for josh run (PD-10 or variant) — route tool/invoke behind explicit grants instead of rejecting all provider requests (`crates/josh/src/runner.rs:383-402`).
- [ ] Whitelisted CLI exec capability — `exec.run(argv, stdin?)` → `Result<{status, stdout, stderr}, ExecError>`; manifest requests named commands, host grants per binary/arg-prefix (`--grant-exec "aws cloudwatch *"`); argv-only (no shell), explicit env allowlist, output/wall limits, record/replay via argv + stdout digest.
- [ ] `decode<T>(Bytes) → Result<T, DecodeError>` — in-program JSON decoding, reusing the entry-boundary lowering/validation.

## P1

- [ ] Clockless time module — `format_utc` / `parse_utc` / bucketing on epoch ints; no clock, time only arrives as data.
- [ ] `to_int(String) → Result<Int, ParseError>`
- [ ] Fixed-decimal float formatting — `float.format(x, decimals);`
- [ ] Typed template resources — external template file with a declared hole signature, verified by allen check (every hole filled, no stray placeholders, no ${/backtick escaping). Replaces the earlier string.replace-workaround and raw/heredoc items; still add string.replace as a general string op.
- [ ] Record invariants / entry validators — where clauses checked at the boundary (e.g. all five series same length); kills the misaligned-series bug class at load time.
- [ ] Lockstep iteration — `for (h, c, a) in zip(...)` instead of index loops with trapping `l[i]`.
- [ ] List stats helpers — min/max/sum/fold.
- [ ] `map.insert` / `map.remove` / `map.keys` — maps are literal-only; sparse series can't be grouped without them.

## P2

- [ ] Newtypes — distinct nominal aliases (newtype EpochSeconds = Int) so epoch seconds, hour indexes, and counts can't interchange.
- [ ] fail(reason) — a failed outcome distinct from stop's clean termination, for data-invariant violations.
- [ ] Option coalescing — postfix `unwrap_or` / `??` for gappy datapoints and the partial last hour.
- [ ] Top-level const — shared thresholds (97.0, 256, sweep start index) without threading through signatures.
- [ ] Source-level test blocks — test "..." { } run by allen test, on top of the existing testkit record/replay.
- [ ] Numeric literal separators — 485_273.
- [ ] Docs/examples — a minimal "typed JSON in → typed JSON out via --input" example (also preempts the parenthesized-if, capabilities: [], and export-entry stumbles); document the 1 MiB entry-output cap and its knob.
- [ ] PD-1/PD-2 durability — optional; cron/Orca covers scheduling.
