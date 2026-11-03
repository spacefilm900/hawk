# hawk design notes

hawk is a Rust port of pyawk. The **semantic decisions** are pyawk's and are
not repeated here. They are in
[docs/pyawk-v1.0-DESIGN.md](docs/pyawk-v1.0-DESIGN.md), a copy of pyawk's
DESIGN.md at tag `v1.0`. It covers values and strnums, the lexer rules,
undefined escapes, parser precedence, ranges, user-defined functions,
`substr` clamping, and the golden-corpus overrides. Where they leave a
question open, the project's tie-break rule applies:

> follow POSIX where it is clear; otherwise follow the majority of
> onetrue-awk, `gawk --posix` and mawk.

This file covers what is specific to the Rust implementation: the
architecture, how values and bytes are represented, and short notes on the
Rust idioms the code relies on. The notes are for readers who know
programming well but not Rust. Decisions that hawk takes differently from
pyawk v1.0 are listed at the end.

**v1.1 extras (branch `enhancements`).** hawk is being brought up to pyawk
v1.1: the POSIX parse-time errors, `nextfile`, the I/O and `getline` audit
with `ENVIRON`, and UTF-8 locales. Their decisions are pyawk v1.1's, in
[docs/pyawk-v1.1-DESIGN.md](docs/pyawk-v1.1-DESIGN.md) (a copy of pyawk's
DESIGN.md at tag `v1.1`); "Porting pyawk v1.1" near the end of this file
records how hawk implements them and any question pyawk left open.

## Pipeline

```text
source bytes --lexer.rs--> Vec<Token> --parser.rs--> ast.rs --interp.rs--> output
                                       value.rs, format.rs, array.rs, input.rs, io.rs, rand.rs, regex/
```

| Module | Responsibility | Phase |
|---|---|---|
| `main.rs` | executable; reads the command line as bytes, calls `cli::main` | 0 |
| `cli.rs` | options `-F -v -f --`, `--version`, `-h`; runs the interpreter on a big-stack thread; exit status | 0 / 4 |
| `error.rs` | `Error` type: syntax errors (with source line), runtime errors, broken pipe | 1 |
| `lexer.rs` | tokens, newline rules, regex-vs-division, string escapes, numbers | 1 |
| `ast.rs` | expression and statement enums | 2 |
| `parser.rs` | recursive descent, one function per precedence level | 2 |
| `value.rs` | `Value` (number / string / strnum / uninit), conversions, comparison | 3 |
| `format.rs` | `printf` formatting; checked against the C library (a 17,939-case table) | 3 / 5 |
| `array.rs` | AWK arrays: insertion-ordered hash maps | 3 |
| `interp.rs` | the tree-walking interpreter: variables, expressions, statements, functions, main loop, records and fields, builtins | 3–7 |
| `input.rs` | record reading for every kind of `RS` | 4 |
| `io.rs` | output redirections, pipes, `getline` sources, `close`, `system` | 3 / 7 |
| `rand.rs` | `rand`/`srand`: CPython's Mersenne Twister, so numbers match pyawk | 7 |
| `regex/` | ERE parser, then NFA, then lazy DFA, with leftmost-longest matching | 6 |

The crate is a library (`src/lib.rs`) plus a thin binary (`src/main.rs`).
Each module keeps its unit tests at the bottom, in `#[cfg(test)] mod tests`.
That section is compiled only for `cargo test`. `tests/run.rs` runs the `hawk` binary end to end, and the external corpus
runners check the rest (see README).

## Bytes everywhere

AWK has to carry input that is not valid UTF-8 through unchanged. Rust's
`String` and `&str` types *guarantee* valid UTF-8, so hawk uses them only
for things that really are text: identifiers, which are ASCII, and error
messages. All AWK data uses `Vec<u8>` (an owned, growable byte buffer) or
`&[u8]` (a borrowed view of bytes). That includes program text, string
literals, regex sources, records, fields and values. Lowercasing,
"is a digit" and so on are ASCII operations on bytes, as in the C locale.
A UTF-8 mode, as in pyawk v1.1, can later sit on top of this as a
character-counting layer.

Octal escapes give a byte: `"\101"` is `A`. Values above `\377` wrap to
8 bits, as in the C implementations.

## Errors

A function that can fail returns `Result<T, Error>`. That is an enum
holding either `Ok(value)` or `Err(error)`. Rust has no exceptions; the `?`
operator after a call means "if this is an `Err`, return it from the
current function right away; otherwise unwrap the `Ok` value". `cli::main`
is the one place that turns an `Error` into `hawk: ...` on stderr and exit
status 2. The message wording follows pyawk: `syntax error at source line
N: ...`.

AWK's own control flow (`next`, `return`, `break`, `continue`) is *not*
an error. pyawk used Python exceptions for it. hawk's statement executor
returns a small enum (`Flow::Normal`, `Flow::Break`, `Flow::Next`,
`Flow::Return`, ...) that each loop, rule and call site inspects. That is
explicit and costs nothing when nothing happens.

`exit` is different: it can happen inside an expression (a function called
from an expression may `exit`), so it travels on the error side of
`Result`, together with runtime errors, as `Unwind::Exit` /
`Unwind::Err(e)`. `?` carries it up to `Interp::run`, which runs the END
actions after an `exit` in BEGIN or a rule and stops at once for an `exit`
in END.

## Values (phase 3)

```rust
enum Value { Num(f64), Str(Rc<[u8]>), StrNum(Rc<[u8]>, f64), Uninit }
```

An `enum` in Rust is a *tagged union*: each variant can carry its own data,
and a `match` must handle every variant, or the program does not compile.
`Rc<[u8]>` is a reference-counted, immutable byte string. Copying a value
then copies a pointer and bumps a counter instead of duplicating the bytes.
That matters because AWK copies values constantly (assignments, function
arguments, fields). `Rc` is single-threaded, and hawk has one thread.

Arrays (`array.rs`) are hash maps that remember insertion order, because
pyawk's `for (k in a)` visits keys in insertion order (Python dictionaries
keep it) and hawk prints the same. Arrays are passed to functions by
reference, and a variable can turn out to be an array only when a function
uses it as one. So an array slot is `Rc<RefCell<Array>>`: shared
ownership (`Rc`) of a value that may be changed through any of its owners
(`RefCell` checks the borrowing rules at run time instead of compile time).
Array keys come from the input, so the key index uses the standard
library's randomly seeded hasher: input crafted so that every key lands in
the same hash bucket (which made inserts quadratic with the unseeded FxHash
used before) cannot be prepared in advance. FxHash is kept for global
variable names (chosen by the program text) and the regex cache (at most
512 entries, so collisions there cannot cost much).

## Execution model (phase 3)

pyawk compiles the AST into Python closures before running it, because
walking a tree in Python is slow. hawk instead walks the tree directly
(`interp.rs`): `exec(&Stmt)` runs a statement and `eval(&Expr)` computes a
value, each a `match` over the node kinds. In Rust a tree walk is already
fast, it is far simpler to read and to check against pyawk's semantics,
and it keeps the borrow checker out of the way (closures that capture a
mutable interpreter are awkward in Rust). Correctness comes first;
performance is measured in phase 8, and the hot spots (global variable
lookup by name, string constants allocated on each evaluation) can then be
resolved to slots or cached at load time without changing the interface.

* **Variables.** Globals live in a hash map from name to
  `Var::Scalar(Value)` or `Var::Array(ArrayRef)`; a missing name is still
  untyped. Function parameters live in the current `Frame` as `Cell`s; an
  untyped parameter remembers (`Link`) which caller variable it came from,
  so that if the function uses it as an array the caller's variable becomes
  that same array (pyawk's binder rules, ported one-to-one).
* **Evaluation order** follows pyawk: an assignment resolves its target
  (array and subscript, or field number) before evaluating the right-hand
  side, and a getline target is resolved before reading.
* **Fields** are split lazily, the first time a field or NF is used, with
  the FS that was current when the record was read.
* **Output** to stdout goes through a 64 KiB buffer, flushed before running
  a command, at exit, and after every write when stdout is a terminal.
  Output files are buffered too; if writing out the rest of a file's
  buffer fails when it is closed (by `close()` or at exit), that is a
  fatal "write error on NAME (reason)", exit status 2, since the data is
  lost. For an output pipe, a command that stopped reading is not an error:
  `close()` returns the command's exit status, as in pyawk, gawk and
  onetrue-awk (mawk dies of SIGPIPE).
* **Recursion.** The interpreter recurses on the native stack, so `cli.rs`
  runs it on a thread with a 1 GiB stack (reserved address space, not
  memory); if that much cannot be reserved (`ulimit -v`), it tries 256, 64
  and 16 MiB, then the main thread. Each user function call compares the
  stack position with the start of the run and stops with pyawk's
  "function call nesting too deep (runaway recursion?)" before the stack
  runs out (about 300 000 levels in a release build with 1 GiB), instead
  of crashing. On a stack under 8 MiB the function-call budget is half
  the stack (the usual 4 MiB margin would leave nothing).
  Evaluating an expression, a condition or a statement also checks the
  stack (one comparison; no measurable cost in `tools/bench.py`), at a
  limit halfway between that budget and the end of the stack, and stops
  with "expression nesting too deep". Each tree level costs the
  interpreter about 1 KiB in a release build and several in a debug
  build, more than parsing it did, so the parser's limits alone cannot
  keep a small stack safe. On hawk's own 1 GiB stack this never fires for
  a program the parser accepts.
* **Nesting in the program text.** The parser recurses too. Every
  recursive parsing function counts its depth and checks the stack it has
  used (at most half the thread's stack); past 20,100 levels (5,000
  nested parentheses, as many as gawk takes) or the stack budget it stops
  with "syntax error ...: nesting too deep" instead of overflowing.
  Operator chains such as `1+1+...+1` are parsed by loops, but build a
  tree as deep as the chain is long, which the interpreter (and freeing
  the tree) recurse over; so each link counts as a level too, and
  reserves 512 bytes of the parser's stack budget for freeing and running
  the tree later. Links keep counting until the statement or pattern they
  are in is finished, not just until their parentheses close: a chain
  inside parentheses can start another chain, as in `((1+1)+1)+1`, and
  the depths add up. (Separate chains in one statement are counted
  together too, which is more than needed.) A chain of 4
  million terms used to overflow the 1 GiB stack; now chains past about
  20,000 terms are a syntax error (about 2,000 on a 2 MiB stack).
* **Unknown stack size.** `cli.rs` tells the parser and the interpreter
  how big their stack is. Library callers of `parser::parse` (or an
  interpreter `Config` with `stack_size` 0) do not, so both then assume
  2 MiB, what Rust gives the threads it spawns, rather than the main
  thread's usual 8 MiB: limits are then hit sooner. These checks only
  hold if the size is right: a caller that runs them on a thread with
  less than 2 MiB of stack left (a smaller thread, or deep in its own
  recursion) must pass the real size to `parse_with_stack` and
  `Config::stack_size`, or deep input may still overflow its stack.

## Regex (phase 6)

hawk cannot use the `regex` crate (no dependencies), and that crate does not
implement POSIX leftmost-longest matching anyway. hawk has its own engine in
`src/regex/`, modelled on pyawk v1.1's `regex.py` (semantics, error messages
and limits):

* `parse.rs`: ERE text to an AST (`Node`). Bracket expressions are resolved
  at parse time into a `ByteSet` (256 bits), so `[[:alpha:]_-]`, ranges,
  negation, `[.x.]` and `[=x=]` all become one set. Intervals `{m,n}` are
  checked against `DUP_MAX` = 32767 and expanded by `repeat()`. Errors are
  pyawk's: `bad regular expression /src/: why`. The tree's depth (nested
  groups and stacked repeats such as `a**`) is capped at `MAX_DEPTH` =
  2,000, so `((((...))))` a million deep is "too deeply nested" rather
  than a stack overflow; mawk and pyawk stop at 500 groups.
* `nfa.rs`: the AST to a Thompson NFA (byte-set, split, `^`, `$`, match
  instructions). Its size is computed before it is built and capped at
  `NFA_LIMIT` = 1,000,000 instructions, as in pyawk, so `(a{1000}){1000}`
  is an error rather than a memory blow-up. The same AST also builds a
  *reversed* NFA (used to find match starts, below).
* `dfa.rs`: a lazy DFA. A state is the set of NFA instructions reachable
  without consuming a byte; states and transitions are built only when a
  byte is first seen in that state. Bytes the regex cannot tell apart share
  one transition column (byte classes). When a DFA grows past `DFA_LIMIT` =
  10,000 states, or when its states together hold more than `KERNEL_LIMIT` =
  2^20 NFA positions (4 MiB; a big regex can have states of thousands of
  positions each), its cache is flushed and rebuilt on demand, so memory
  stays bounded while the run time stays linear. Each state's position list
  is stored once, shared between the state table and the lookup map.
* `mod.rs`: `Regex` (`new`, `is_match`, `find_at`, `size`) and `Finder`,
  a search over one text that remembers work between calls (gsub and split
  call it repeatedly). `find_at` returns the *leftmost-longest* match: the
  forward DFA runs anchored from each candidate start, remembering the last
  accepting position. Candidates skip bytes that cannot begin a match. If
  this gets expensive (more than 2n+64 steps on a text of n bytes), one pass
  of the reversed DFA over the text marks every position where a match can
  start, and the search jumps straight to the next mark. Pure literals of up
  to 32 bytes skip the automata entirely.

Nothing backtracks, so no pattern can take exponential time; one match is
linear in the text. A loop of matches can still be quadratic for patterns
like `a*b|a` on `aaaa…`: finding the *longest* match at each start may have
to read to the end of the text before settling on a short one. That is
inherent to leftmost-longest (mawk and gawk share it).

The interpreter keeps compiled regexes in a cache keyed by source text, for
`/literals/` and dynamic (string) regexes alike, capped at 512 entries and
`NFA_LIMIT` NFA instructions in total; when either cap is hit the cache is
cleared, as pyawk does.

`src/subst.rs` holds what sits on top of matching: sub/gsub replacement
text (in the replacement's string value, `&` is the match, `\&` a literal
`&` and `\\` a backslash: mawk's and pyawk's rule for backslash runs; in
program text `"\\&"` is the literal `&`), the empty-match rules for gsub (an empty
match next to the previous match is skipped, so `gsub(/x*/, "-", "abc")` is
`-a-b-c-`), and splitting at a regex (at non-empty matches only).

The FS and split rules: `" "` splits at runs of blanks, tabs and newlines;
any other single byte, regex-special or not (`|`, `.`, `\`), is a literal
separator; `""` gives one field per byte; anything longer is a regex. In
paragraph mode (`RS = ""`) newline is a separator as well (the regex becomes
`\n|(FS)`). A multi-byte RS is a regex: hawk reads the whole input first
(as pyawk does, so a match is never cut at a buffer boundary), and empty
matches do not end a record.

**The UTF-8 seam.** The engine works on bytes. Everything that knows about
characters is in `parse.rs`: `.`, bracket expressions and classes become
`ByteSet`s there. A UTF-8 mode would change only the parser, turning a
character set into an alternation of byte sequences (the usual way to run
UTF-8 through a byte automaton); the NFA, DFA and search would not change.
`RSTART`/`RLENGTH` and `substr` would then convert byte offsets to
character offsets at the interpreter boundary.

## printf and numbers (phase 5)

Rust's `format!` is not C's `printf`, but its float formatting with an
explicit precision (`{:.17e}`, `{:.60}`) is exact: it expands the double's
true binary value and rounds an exact tie to even, which is what glibc
does. So `format.rs` takes the digits from there and adds C's layout
itself: the exponent style (`e+05`, at least two digits), `%g`'s choice
between the `e` and `f` styles and its zero stripping, flags `-+ #0`,
width and precision (also from `*`), `inf`/`nan` spelled `+inf`/`-nan` as
gawk and pyawk do, and `%c` with a number or a string. Integers follow C
(`%.0d` of 0 prints nothing, `0` is ignored with a precision, negative
numbers are reinterpreted as unsigned 64-bit by `%u %o %x %X`); integers
too big for 128 bits are printed exactly with a small bignum, as pyawk does
with Python integers (C would overflow).

`tools/printf_table.py` compiles a C program that records the C library's
`snprintf` output for about 18,000 (format, value) pairs: 58 float formats
over 295 doubles (ties, powers of ten, subnormals, the largest double,
random bit patterns, up to `%.1000f`), 35 integer formats and 9 string
formats. The table is committed (`tests/data/printf_table.tsv`) and the
unit test `matches_the_c_library` checks every line, so the test needs no
C compiler. hawk agrees with glibc on all of them but one family (below).

Number parsing follows `strtod` for the AWK subset: leading blanks, sign,
digits, `.`, exponent, the longest valid prefix. Rust's
`str::parse::<f64>` is correctly rounded, so it does the final conversion
once hawk has found that prefix.

## Rust idioms used, in brief

* **Ownership and borrowing.** Every value has one owner and is freed when
  the owner goes away. `&x` lends it out read-only, and `&mut x` lends it
  out for writing (only one writer at a time). The compiler checks this,
  so there are no dangling pointers and no data races.
* **Enums with data and `match`.** These are used for tokens, AST nodes,
  values and control flow (see above).
* **`Option<T>`** is `Some(value)` or `None`. Rust's null is explicit.
* **`Result<T, E>` and `?`.** Error propagation (see "Errors").
* **Traits.** A trait is an interface (`Display` for "can be printed",
  `Fn` for "can be called"). `impl Trait for Type` implements it.
  `#[derive(Debug, PartialEq)]` asks the compiler to write standard
  implementations automatically.
* **`Rc` and `RefCell`.** Shared ownership, and mutation checked at run time
  (see "Values").
* **Lifetimes** (`'a`) name how long a borrow lasts. The lexer and parser
  avoid them by owning their data. Where they appear, they say "this
  borrowed view must not outlive the buffer it points into".

## Where hawk deliberately differs from pyawk v1.0

* **Regex alternation.** pyawk v1.0 translated EREs to Python's `re`, which
  picks the *first* matching alternative (`a|ab` on "ab" matched "a"; a
  documented known gap). hawk's own engine is leftmost-longest from the
  start, as POSIX requires and pyawk v1.1 does.
* **Bytes.** pyawk v1.0 decoded input as text. hawk works on bytes, so
  `length`, `substr` and `index` count bytes in v1.0 scope, which is the
  C-locale behaviour and what the corpus (run under `LC_ALL=C`) expects.
* **Lexer details.** Numeric literals are converted to `f64` in the lexer
  (pyawk kept the text). A non-ASCII byte in program text is reported as
  `unexpected character '\xc3'` rather than as a decoded character. A
  string literal continued with backslash-newline carries the line it
  *starts* on (pyawk used the line it ended on); this only affects which
  line a later error message names. A regex literal continued the same
  way also counts the continued line, which pyawk v1.0 forgot, so errors
  after it name the right line (as onetrue-awk does).
* **`cmd | getline` result in arithmetic.** `"cmd" | getline x + 1` is
  `("cmd" | getline x) + 1` in hawk, as in gawk and mawk; the same holds for
  `- * / % ^`. pyawk v1.0 rejects it as a syntax error, and onetrue-awk
  reads it as a concatenation with `+1`. hawk follows the reference
  majority (`gawk/getline.awk` in the corpus). pyawk v1.1 made the same
  change, so this is no longer a difference from the current pyawk.
* **ASCII-only names.** `-v name=value`, `name=value` operands and program
  identifiers accept only ASCII letters, digits and `_`, as POSIX
  specifies. pyawk v1.0 used Python's `str.isidentifier()`, which also
  accepts non-ASCII letters for `-v`.
* **`%#g` rounding up to a power of ten.** glibc prints
  `printf "%#g", 999999.5` as `1.e+06` (it keeps the precision of the `f`
  style it first chose); C and POSIX ask for `1.00000e+06`, which pyawk
  prints, and so does hawk (the user's decision: follow POSIX here). gawk,
  mawk and onetrue-awk show the glibc output since they call it.
* **`%c` of 128 to 255** writes that single raw byte, as gawk does in the
  C locale (the user's decision, in keeping with hawk working on bytes).
  pyawk v1.0 wrote the Unicode character, UTF-8 encoded (two bytes).
  Codes of 256 and up are still written as UTF-8, as pyawk does.
* **`%c` of a numeric string or an unset variable.** A field such as
  `65` is a numeric string, so `printf "%c", $1` writes `A`, and an unset
  variable is the number 0, so it writes a NUL byte. This is POSIX, and
  gawk, mawk and onetrue-awk all agree; pyawk v1.0 used the string (`6`,
  and nothing). A missing argument still writes nothing, as in pyawk.
* **Huge printf widths and precisions** are cut to 1,048,576 (2^20,
  `MAX_WIDTH` in `format.rs`) whether written in the format
  (`%99999999999d`) or given by `*` (the user's decision). pyawk v1.0
  tried to honour them and could run out of memory. Precisions past
  Rust's own formatting limit (65,535) still print exactly: the digits a
  double has are at most 1,074 decimals or 767 significant digits, and
  the rest are zeros.
* **Write errors on close.** pyawk v1.0 ignores a failed flush when a
  file is closed (`print > "/dev/full"` exits 0); hawk reports it and
  exits 2, as gawk, mawk and onetrue-awk do.
* **Deep nesting.** pyawk v1.0 fails (Python's recursion limit) before
  1,000 nested parentheses; hawk accepts up to 5,000 and reports anything
  deeper as a syntax error. Likewise for operator chains: pyawk v1.0
  fails before 1,000 terms of `1+1+...`, onetrue-awk's parser stack
  before 100,000; hawk takes about 20,000 (gawk and mawk take millions).
  In a regex, nesting is "too deeply nested" past 2,000 levels;
  pyawk v1.0 gives up at 500 nested groups.
* **Misplaced parenthesised lists.** A list `(a, b)` is valid only right
  before `in` or as print's whole argument list. pyawk v1.0 found other
  uses only when running them ("parenthesised expression list used as a
  value", and only if reached) and accepted `print ((a, b))`; hawk
  rejects them all as syntax errors while parsing, as gawk, mawk and
  onetrue-awk do.
* **`tolower`/`toupper`** change ASCII letters only (bytes, the C locale);
  pyawk v1.0 used Python's Unicode case mapping.

## Porting pyawk v1.1

### Test harness

* The golden corpus has pyawk v1.1's five new categories (`environ`,
  `io`, `nextfile`, `regex_posix`, `utf8`; 154 cases in all), its
  `OVERRIDES` and `DECISIONS` files, and the updated onetrue/gawk
  manifests (gawk's `close_status` and `regrange` now run).
* `tools/corpus.py` runs cases as pyawk v1.1's harness does: `utf8` under
  `LC_ALL=C.UTF-8` and the rest under `LC_ALL=C`; `NAME.environ` lines are
  added to the environment (inherited `PYAWK_T_*` variables are dropped
  first); golden cases run in a temporary copy of their directory.
* `tools/regex_check.py` also runs the UTF-8 record. The C table is now a
  hard gate (any disagreeing line fails the run); the UTF-8 table counts
  only regressions until hawk has a UTF-8 mode (`HARD_GATES`).
* `tools/ast_check.py` compares with pyawk **v1.1**'s parser. Programs it
  parses differently only because a stage is not done yet are listed in
  `PENDING` and reported, not failed. `tools/ast_dump.py` now decodes the
  program as Latin-1, as pyawk v1.1 does in the C locale, so a string
  escape such as `"\303"` dumps as the single byte it stands for.
