# Rosetta Code examples

This directory contains 40 standalone ALLEN programs. The programs use tasks
from the [Rosetta Code task index](https://rosettacode.org/wiki/Category:Programming_Tasks).
The selection favors tasks with many language implementations and tasks that
fit the current ALLEN language profile.

Each program has an exported `main` function. The function returns the result.
The programs do not write to standard output. Helper functions are pure and use
an omitted effect clause, which denotes the empty effect set.

## Run an example

From the repository root, run these commands:

```sh
cargo run --bin allen -- check examples/rosetta-code/factorial.allen
cargo run --bin allen -- run examples/rosetta-code/factorial.allen
```

## Task index

| ALLEN file | Rosetta Code task | Result |
| --- | --- | --- |
| [`hello-world.allen`](hello-world.allen) | [Hello world/Text](https://rosettacode.org/wiki/Hello_world/Text) | `"Hello, world!"` |
| [`hundred-doors.allen`](hundred-doors.allen) | [100 doors](https://rosettacode.org/wiki/100_doors) | `(true, false, true, false, true, true)` |
| [`fibonacci-sequence.allen`](fibonacci-sequence.allen) | [Fibonacci sequence](https://rosettacode.org/wiki/Fibonacci_sequence) | `(0, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55)` |
| [`factorial.allen`](factorial.allen) | [Factorial](https://rosettacode.org/wiki/Factorial) | `(1, 1, 120, 3628800)` |
| [`fizzbuzz.allen`](fizzbuzz.allen) | [FizzBuzz](https://rosettacode.org/wiki/FizzBuzz) | `("Number", "Fizz", "Buzz", "FizzBuzz", "Number")` |
| [`a-plus-b.allen`](a-plus-b.allen) | [A+B](https://rosettacode.org/wiki/A%2BB) | `(3, 42, 0)` |
| [`ackermann-function.allen`](ackermann-function.allen) | [Ackermann function](https://rosettacode.org/wiki/Ackermann_function) | `29` |
| [`reverse-bytes.allen`](reverse-bytes.allen) | [Reverse a string](https://rosettacode.org/wiki/Reverse_a_string) | `(110, 101, 108, 108, 65)` |
| [`integer-arithmetic.allen`](integer-arithmetic.allen) | [Arithmetic/Integer](https://rosettacode.org/wiki/Arithmetic/Integer) | `(22, 12, 85, 3, 2)` |
| [`greatest-list-element.allen`](greatest-list-element.allen) | [Greatest element of a list](https://rosettacode.org/wiki/Greatest_element_of_a_list) | `17` |
| [`arithmetic-mean.allen`](arithmetic-mean.allen) | [Averages/Arithmetic mean](https://rosettacode.org/wiki/Averages/Arithmetic_mean) | `18` |
| [`integer-comparison.allen`](integer-comparison.allen) | [Integer comparison](https://rosettacode.org/wiki/Integer_comparison) | `(-1, 0, 1)` |
| [`greatest-common-divisor.allen`](greatest-common-divisor.allen) | [Greatest common divisor](https://rosettacode.org/wiki/Greatest_common_divisor) | `21` |
| [`array-sum-and-product.allen`](array-sum-and-product.allen) | [Sum and product of an array](https://rosettacode.org/wiki/Sum_and_product_of_an_array) | `(15, 120)` |
| [`even-or-odd.allen`](even-or-odd.allen) | [Even or odd](https://rosettacode.org/wiki/Even_or_odd) | `(true, false)` |
| [`palindrome-detection.allen`](palindrome-detection.allen) | [Palindrome detection](https://rosettacode.org/wiki/Palindrome_detection) | `(true, false)` |
| [`sum-of-a-series.allen`](sum-of-a-series.allen) | [Sum of a series](https://rosettacode.org/wiki/Sum_of_a_series) | `100` |
| [`sum-of-squares.allen`](sum-of-squares.allen) | [Sum of squares](https://rosettacode.org/wiki/Sum_of_squares) | `385` |
| [`leap-year.allen`](leap-year.allen) | [Leap year](https://rosettacode.org/wiki/Leap_year) | `(true, false, true, false)` |
| [`towers-of-hanoi.allen`](towers-of-hanoi.allen) | [Towers of Hanoi](https://rosettacode.org/wiki/Towers_of_Hanoi) | `(0, 1, 7, 1023)` |
| [`hailstone-sequence.allen`](hailstone-sequence.allen) | [Hailstone sequence](https://rosettacode.org/wiki/Hailstone_sequence) | `{length: 10, maximum: 40}` |
| [`dot-product.allen`](dot-product.allen) | [Dot product](https://rosettacode.org/wiki/Dot_product) | `3` |
| [`primality-by-trial-division.allen`](primality-by-trial-division.allen) | [Primality by trial division](https://rosettacode.org/wiki/Primality_by_trial_division) | `(false, true, true, false, true)` |
| [`binary-search.allen`](binary-search.allen) | [Binary search](https://rosettacode.org/wiki/Binary_search) | `(0, 4, -1)` |
| [`least-common-multiple.allen`](least-common-multiple.allen) | [Least common multiple](https://rosettacode.org/wiki/Least_common_multiple) | `(36, 42, 0)` |
| [`exponentiation-by-squaring.allen`](exponentiation-by-squaring.allen) | [Exponentiation by squaring](https://rosettacode.org/wiki/Exponentiation_by_squaring) | `(1024, 2187, 1)` |
| [`extended-euclidean-algorithm.allen`](extended-euclidean-algorithm.allen) | [Extended Euclidean algorithm](https://rosettacode.org/wiki/Extended_Euclidean_algorithm) | `{gcd: 2, x: -9, y: 47}` |
| [`modular-inverse.allen`](modular-inverse.allen) | [Modular inverse](https://rosettacode.org/wiki/Modular_inverse) | `(4, 2753, 19)` |
| [`chinese-remainder-theorem.allen`](chinese-remainder-theorem.allen) | [Chinese remainder theorem](https://rosettacode.org/wiki/Chinese_remainder_theorem) | `(23, 39)` |
| [`modular-exponentiation.allen`](modular-exponentiation.allen) | [Modular exponentiation](https://rosettacode.org/wiki/Modular_exponentiation) | `(445, 1, 16)` |
| [`miller-rabin-primality-test.allen`](miller-rabin-primality-test.allen) | [Miller-Rabin primality test](https://rosettacode.org/wiki/Miller%E2%80%93Rabin_primality_test) | `(true, true, false, false, true)` |
| [`lucas-lehmer-test.allen`](lucas-lehmer-test.allen) | [Lucas-Lehmer test](https://rosettacode.org/wiki/Lucas-Lehmer_test) | `(true, true, true, true, false, true)` |
| [`integer-square-root.allen`](integer-square-root.allen) | [Integer square root](https://rosettacode.org/wiki/Integer_square_root) | `(0, 1, 1, 3, 4, 1414)` |
| [`fast-doubling-fibonacci.allen`](fast-doubling-fibonacci.allen) | [Fibonacci sequence](https://rosettacode.org/wiki/Fibonacci_sequence) | `(0, 1, 55, 6765, 102334155)` |
| [`karatsuba-multiplication.allen`](karatsuba-multiplication.allen) | [Karatsuba multiplication](https://rosettacode.org/wiki/Karatsuba_multiplication) | `(408, 7006652, 83810205)` |
| [`euler-totient-function.allen`](euler-totient-function.allen) | [Euler's totient function](https://rosettacode.org/wiki/Euler%27s_totient_function) | `(1, 6, 4, 12, 96)` |
| [`happy-numbers.allen`](happy-numbers.allen) | [Happy numbers](https://rosettacode.org/wiki/Happy_numbers) | `(true, true, true, false, true)` |
| [`luhn-algorithm.allen`](luhn-algorithm.allen) | [Luhn test of credit card numbers](https://rosettacode.org/wiki/Luhn_test_of_credit_card_numbers) | `(true, false, true)` |
| [`josephus-problem.allen`](josephus-problem.allen) | [Josephus problem](https://rosettacode.org/wiki/Josephus_problem) | `(1, 3, 4, 31)` |
| [`n-queens.allen`](n-queens.allen) | [N-queens problem](https://rosettacode.org/wiki/N-queens_problem) | `(1, 2, 10, 92)` |

## Modern language profile

The examples use direct control flow, loops, dynamic lists, `%`, compound
assignment, and short-circuit Boolean operators where those features fit the
task. Recursive implementations remain where recursion is central to the
algorithm, including Ackermann, fast-doubling Fibonacci, Karatsuba
multiplication, N-Queens backtracking, and Towers of Hanoi.

The following examples retain a deliberately bounded result shape:

- `hundred-doors.allen` tests representative door numbers. A door is open when
  its number is a perfect square.
- `fizzbuzz.allen` classifies five representative values.
- `reverse-bytes.allen` reverses a dynamic list containing the five ASCII byte
  values in `ALLEN`, then returns the documented tuple.
- `greatest-list-element.allen` and `arithmetic-mean.allen` traverse dynamic
  lists but return one scalar result.
- `array-sum-and-product.allen` traverses the list `[1, 2, 3, 4, 5]`.
- `palindrome-detection.allen` checks decimal integer digits.
- `towers-of-hanoi.allen` calculates the number of moves. It does not build a
  move list.
- `hailstone-sequence.allen` returns the length and maximum value for 13. This
  input stays below the default call-depth limit.
- `dot-product.allen` traverses two three-element lists.
- `binary-search.allen` searches the sorted list
  `[1, 4, 7, 9, 12, 18, 25]` and returns `-1` when a value is absent.
- `modular-inverse.allen` uses inputs that have an inverse. It returns the
  normalized inverse directly.
- `chinese-remainder-theorem.allen` traverses a list of three pairwise-coprime
  congruences.
- `miller-rabin-primality-test.allen` uses the fixed bases 2, 3, and 5 with
  small candidate values.
- `lucas-lehmer-test.allen` uses small prime exponents to keep all values in
  the ALLEN `Int` range.
- `karatsuba-multiplication.allen` uses positive decimal integers.
- `luhn-algorithm.allen` processes a complete number as decimal digits.
- `n-queens.allen` stores prior columns in a dynamic list and calculates the
  standard result of 92 solutions for a board size of 8.
