# Advent of Code 2025

The 2025 event has 12 days. Each `dayNN` directory contains:

- `example.txt`, a runnable example;
- `input.txt`, an empty slot for the account-specific puzzle input; and
- `run.allen`, which reads the filename supplied as its entry input and returns both answers.

Run one day from the repository root:

```sh
target/debug/josh run examples/aoc/2025/day01/run.allen \
  --input '"example.txt"' \
  --workdir examples/aoc/2025/day01 \
  --grant fs.read
```

Replace `example.txt` with `input.txt` after copying in your own input.

Day 8's example starts with `pairs=10`, the connection count used by the puzzle's sample. The solver defaults to the part 1 input rule of 1000 pairs when that line is absent.

Day 12 has one coding puzzle. Its second star is the event-completion action, so `part2` reports that fact as text. The example contains the two feasible regions from the puzzle statement. The solver uses exact packing for small ambiguous regions, safe area and bounding-box proofs where they decide the result, and an exact list-based fallback for larger ambiguous regions. General polyomino packing is NP-hard, so adversarial ambiguous inputs can take a long time.

The programs use the current language only. Across the set they exercise inline manifests, filesystem effects, async functions, `Result` propagation, records, a type alias, lists, canonical maps, options, matches, loops, recursion, immutable collection updates, range iteration, tuple destructuring, and checked integer arithmetic. Planned syntax is excluded because this repository's language policy does not allow unsupported forms in runnable examples.
