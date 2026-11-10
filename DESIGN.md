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
UTF-8 mode (stage 4 of the v1.1 port, below) sits on top of this as a
character-counting layer: the data stays bytes.

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
in END. A `next` executed inside a function body has the same problem
(the call may be in the middle of an expression), so the call turns its
`Flow::Next` into `Unwind::Next`, which the record loop catches; from
BEGIN or END it becomes the run-time error "next called from a function
in BEGIN" (pyawk v1.1 and gawk).

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
Array keys come from the input, so each array's key index uses FxHash with
a random per-array seed: input crafted so that every key lands in the same
hash bucket (which made inserts quadratic with the unseeded FxHash used
before) cannot be prepared in advance. FxHash is not cryptographic: a
caller who can time lookups against a live process could still search for
a collision. It is several times faster than SipHash, and the seed
closes the attack the unseeded hash actually had. Global variable names
(chosen by the program text) and the regex cache (at most 512 entries)
keep the fixed-seed FxHash.

## Execution model (phase 3)

pyawk compiles the AST into Python closures before running it, because
walking a tree in Python is slow. hawk instead walks the tree directly
(`interp.rs`): `exec(&Stmt)` runs a statement and `eval(&Expr)` computes a
value, each a `match` over the node kinds. In Rust a tree walk is already
fast, it is far simpler to read and to check against pyawk's semantics,
and it keeps the borrow checker out of the way (closures that capture a
mutable interpreter are awkward in Rust). Correctness comes first.
Performance was measured after v1.1 (see "Performance" below). The hot
spots that section names (variable lookup, a fresh string per field) were
removed without changing what programs print. Compiling the tree to
closures or bytecode was not done; that section says what it would take.

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
  Output files are buffered too. When a flush fails, see "I/O (stage 3)"
  below for what happens.
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
  with "program is nested too deeply" (pyawk's wording; exit status 2)
  instead of overflowing.
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
  20,000 terms are "program is nested too deeply", exit status 2 (about
  2,000 on a 2 MiB stack).
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
`ByteSet`s there. UTF-8 mode changes only the parser, which turns a
character set into an alternation of byte sequences (the usual way to run
UTF-8 through a byte automaton); the NFA, DFA and search did not change.
`RSTART`/`RLENGTH` and `substr` convert byte offsets to character offsets
at the interpreter boundary. See "UTF-8 (stage 4)" below.

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
  Since stage 4 of the v1.1 port, C mode writes the low byte of any code
  (`%c` of 321 is `A`), as pyawk v1.1, gawk and mawk do; UTF-8 mode
  writes the character as UTF-8 (see "UTF-8 (stage 4)").
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
  deeper as "program is nested too deeply" (pyawk v1.1's wording; exit
  status 2). Likewise for operator chains: pyawk v1.0
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
  first; a line that is not `NAME=value` is an error, not an empty
  variable); golden cases run in a temporary copy of their directory.
  `tools/test_tools.py` unit-tests the harness and runs in
  `tools/regress.sh`.
* `tools/regex_check.py` also runs the UTF-8 record. The C table is now a
  hard gate (any disagreeing line fails the run). The UTF-8 table counted
  only regressions until stage 4; it is now a hard gate too (`HARD_GATES`).
* `tools/ast_check.py` compares with pyawk **v1.1**'s parser. Programs it
  parses differently only because a stage is not done yet are listed in
  `PENDING` and reported, not failed. `tools/ast_dump.py` now decodes the
  program as Latin-1, as pyawk v1.1 does in the C locale, so a string
  escape such as `"\303"` dumps as the single byte it stands for.

### Parse-time errors (stage 1)

These are rejected while parsing, so even code that never runs (`if (0)
...`) makes the program fail with exit status 2, as in mawk and gawk:

* `next` written in a BEGIN or END action ("next used in BEGIN action").
  `next` inside a function body is allowed (POSIX, gawk); if such a
  function is called from BEGIN or END the run stops with "next called
  from a function in BEGIN" (see "Errors" above for `Unwind::Next`).
* A builtin called with the wrong number of arguments: "function substr
  called with 1 args (takes 2 to 3)". The counts live in
  `Builtin::arity` in `src/lexer.rs`; gawk's extensions (`match` with an
  array, `close` with "to"/"from") count as wrong.
* `split`'s second argument must be a plain name, and `sub`/`gsub`'s third
  must be a variable, element or field. `(x)` is rejected in both places,
  because all three reference awks treat a parenthesised name as a value.
  `x` and `(x)` make the same tree, so `parse_call_args` also records
  whether each argument starts with `(`.

### Other settled items (stage 1)

* `printf` skips C length modifiers (`h`, `l`, `ll`, `L`, `q`, `j`, `z`,
  `t`): `%ld` is `%d`, `%Lf` is `%f`, as in all three reference awks. With
  no letter after them (`%l|`) the last modifier is taken as an unknown
  conversion letter and the text is copied verbatim, as gawk does.
* Already agreeing with pyawk v1.1, now with tests: `-v NF=3` just sets NF
  (BEGIN sees 3, the first record recomputes it); very deep nesting stops
  with exit status 2 (since stage 3 with pyawk's message, "program is
  nested too deeply"); a repeat count above 32767, or a regex
  needing more than `NFA_LIMIT` automaton states, is a bad regex; `[z-a]`
  is invalid, `[a-c-e]` reads as mawk does (`a-c`, `-`, `e`) and `a+?` is
  `(a+)?`.

### `nextfile` (stage 2)

`nextfile` is a statement like `next` and travels the same way, as
`Flow::NextFile`, out of loops and blocks to the rule loop, which calls
`Interp::skip_file` and stops running rules on this record. It is a syntax
error in BEGIN and END ("nextfile used in BEGIN action", mawk and gawk)
and in a function ("nextfile is not allowed inside a function (f)"; POSIX
leaves it undefined there, mawk and onetrue-awk reject it), so unlike
`next` it never has to unwind through a function call.

`skip_file` drops the current file, so the next record comes from the
next operand and FNR starts again. Range state is not reset, so a range
left open continues into the next file. Standard input cannot be reopened,
so skipping it *uses it up* (`RecordReader::use_up`): a later `-` operand
or `getline < "-"` finds nothing (mawk, gawk; onetrue-awk carries on).
The rest is not read first, which could block on a terminal.

### I/O (stage 3)

The audit followed pyawk v1.1's I/O checklist and decision table item by
item (probes against pyawk v1.1 plus `tests/io.rs`, which runs the binary
with real pipes, `/dev/full` and signals). What changed:

* **getline.** `cmd | getline` leaves NR alone (all three references,
  against POSIX's text). `getline < dir` returns -1, and a directory
  operand stops the run with "cannot open DIR (Is a directory)", as for a
  missing file (pyawk, mawk, gawk; onetrue skips it).
* **Null names.** An empty file or command name in getline or a
  redirection is fatal: "null file name in print redirection" and the
  like (`IoManager::check_name`).
* **`-` for output** is a file called `-` (mawk, onetrue); for input it
  is standard input.
* **Special names.** `/dev/stdout`, `/dev/stderr`, `-` and `/dev/stdin`
  are remembered once used (`specials_used`). `close()` of one in use
  returns 0: for `/dev/stdout` and `/dev/stderr` it flushes and output
  carries on (gawk; onetrue and mawk lose stdout, which is never useful);
  for stdin it *uses it up*, so later reads find nothing (all agree).
  `close()` of a special never used returns -1, like any name not open
  (gawk). All names for standard input share one reader, so `getline <
  "-"`, `getline < "/dev/stdin"` and plain getline read consecutive lines
  (onetrue; the others lose lines to separate buffers).
* **close() and exit order.** `close()` of a command does not flush our
  stdout first; at exit, commands are closed before stdout is flushed
  (mawk, gawk). `close()` of a command returns its exit status, 256 + N
  for a signal (the owner's decision, recorded in pyawk's DECISIONS).
* **fflush.** `fflush()` and `fflush("")` flush everything; `fflush(name)`
  flushes only that output and returns -1 if it is not open for output.
  `system()` flushes everything before running.
* **Write errors.** A failed flush of a file or of `/dev/stderr` stops
  the run with "write error
  on NAME (reason)", exit status 2, wherever it happens (`close()`,
  `fflush()`, or exit): the data is lost. Standard output is the
  exception, following onetrue (the majority on each point): a failed
  flush of stdout makes `fflush()` return -1 and the program carries on;
  at exit hawk reports "write error on standard output (reason)" and
  exits 2, even after `exit 3`. When standard error itself cannot be
  written, hawk still exits 2 and drops the message: `eprintln!` would
  panic there, so `cli.rs` writes messages with a `report` function that
  ignores the failure.
* **Broken pipe.** When the reader of our stdout goes away (also at the
  final flush), or a command we print to exits while we are still writing
  to it (`print | "head -1"` in a loop), hawk dies of SIGPIPE: no
  message, and the shell sees status 141, as for any C program. Only at
  `close()` (or exit) is a command that stopped reading not an error:
  the rest of its buffer is dropped and `close()` returns its status. The Rust runtime ignores SIGPIPE at
  start-up, so the write fails with `BrokenPipe` instead; `cli.rs` then
  restores the default action and raises the signal with two libc
  functions, `signal` and `raise`, declared in an `unsafe extern "C"`
  block. This is hawk's only `unsafe` code; it is a call into the C
  library every Rust program already links, not a crate.
* **Ctrl-C** kills hawk by SIGINT, so the shell sees 130, as with the C
  awks. pyawk instead catches it and exits with code 130; a signal death
  is better for shells and scripts (a loop around hawk stops on Ctrl-C),
  so hawk keeps the default. **Flagged decision.**
* **ENVIRON** was already right: values are strnums, so ` 42 ` and `1e2`
  compare as numbers, but `0x1A` is a string (onetrue and mawk; gawk
  reads hex), values are not escape-processed, and child processes see
  the original environment even after ENVIRON is changed.

Unchanged and confirmed: getline's NR/FNR rules, `>` truncating once per
name, the redirection-target and `cmd | getline` precedence rules,
`"echo " "hi" | getline` running `echo hi`, and stdout not being flushed
before every getline (mawk, gawk).

### UTF-8 (stage 4)

hawk now has pyawk v1.1's two locale modes. The data stays bytes in both;
what changes is where the characters are.

* **Mode.** The first of `LC_ALL`, `LC_CTYPE` and `LANG` that is set and
  not empty decides: a name containing `UTF-8` or `utf8` (any case) is
  UTF-8 mode, anything else (and nothing) is C mode, where a byte is a
  character (`charset::locale_is_utf8`). Remember that the box and many
  CI images set `LC_ALL=C.UTF-8`; the end-to-end tests pin `LC_ALL=C`
  where they test bytes.
* **Characters.** A valid UTF-8 sequence is one character. Any other
  byte (stray continuation, truncated or overlong sequence, encoded
  surrogate) is an *invalid byte*: one character of its own, never
  changed, so any input is written back unchanged. `charset.rs` has the
  helpers (`next`, `count`, `char_offsets`, `round_up`, ...).
* **Builtins.** `length`, `substr`, `index`, `match`'s RSTART/RLENGTH,
  `split(s, a, "")` and `FS = ""` count characters; `toupper`/`tolower`
  map Unicode letters and leave invalid bytes alone; `printf` widths and
  precisions of `%s`/`%c` count characters. `%c` of a number writes that
  code point as UTF-8 (gawk's extended form above U+10FFFF, as pyawk);
  `%c` of a string writes its first character. Byte offsets are turned
  into character offsets only at the interpreter boundary.
* **Tables.** Classes (`[[:alpha:]]` and the rest) and case mappings come
  from gawk 5.4.1 under C.UTF-8, which takes them from glibc:
  `tools/gen_unicode_tables.py` asks gawk about every code point and
  writes `src/unicode_tables.rs` (sorted ranges); `--check` regenerates
  and compares, `--pyawk DIR` checks they equal pyawk v1.1's (they do).
  It is not part of `tools/regress.sh`, since another gawk or glibc may
  answer differently.
* **Regex.** The NFA, DFA and search are unchanged; only `parse.rs`
  knows about characters. The pattern is decoded into code points (an
  invalid byte becomes U+DC00 + byte; byte escapes such as `\303\251`
  that spell valid UTF-8 are joined into one character). Every set of
  characters (`.`, a bracket, a class, a negation) becomes a list of
  merged code-point ranges, which is split into UTF-8 byte-range
  sequences (the standard split by encoded length and lead byte) and
  built into a trie with shared prefixes; the ASCII part stays one
  `ByteSet`, so `[a-z]` costs what it did. `.` and negated sets leave out
  the escaped bytes, so they match characters only (pyawk, gawk).
  Matching starts only at character boundaries (`Finder::find` rounds
  up, looking back at most 3 bytes), and after an empty match `gsub`,
  and `split` step one character (an empty match never splits a field).
* **Patterns that match invalid bytes.** A pattern that names an escaped
  byte (`/\377/` when that is not valid UTF-8) is marked *raw*. Run
  against a text with invalid bytes, it searches a transcoded copy in
  which each invalid byte is written as the 3-byte encoding of its
  escape code point (ED B2/B3 xx), and the match positions are mapped
  back (`Transcoded`). Valid text, and every pattern that is not raw,
  runs on the bytes as they are, so the common case pays nothing.
  A raw regex `RS` would otherwise transcode the whole unread buffer
  once per record (quadratic: 20,000 records took 75 s), so the record
  reader keeps the transcoded copy of its buffer (`Transcoding`) and
  drops it only when the buffer changes (`fill`, `use_up`, `rest`).
  `Finder::cached` searches through that copy.
* **Size limit.** A character set is many byte states (`.` 35,
  `[[:alpha:]]` about 1,100) where pyawk counts one, so UTF-8 mode
  allows 4,000,000 states (`NFA_LIMIT_UTF8`) instead of 1,000,000:
  `.{32767}` compiles, as in C mode and pyawk. `[[:alpha:]]{4000}` is
  still "too big", which pyawk accepts. **Flagged decision.**
* **Gates.** The corpus `utf8` category passes 13/13 and the UTF-8 regex
  table (2,152 lines against pyawk v1.1) is a hard gate, as is the C
  table. `tools/bench.py --locale C.UTF-8` runs the benchmark in UTF-8
  mode (the default is `C`).

Decisions:

* **`%c` in C mode** is the low byte of the code (pyawk v1.1, gawk,
  mawk), replacing v1.0's "256 and up as UTF-8".
* **`%c` of a numeric string or unset variable** stays as in v1.0
  (POSIX: the number). pyawk v1.0 used the string; pyawk since 2026-11-03
  (commit 8e43a30) uses the number too, so this is no longer a difference.
* **String comparison** is by bytes in both modes. For valid UTF-8 that
  is code-point order. pyawk v1.1 first compared Python strings (invalid
  bytes at U+DC80..DCFF), so `"\200" < "é"` was 0 there and 1 in hawk,
  mawk and gawk. pyawk since 2026-11-03 (commit fcc39f5) also compares
  by bytes, so the two now agree.
* **Speed.** On the benchmark UTF-8 mode is within noise of C mode
  (the table in "Performance" below); on non-ASCII text `gsub(/./, ...)`
  and class matches cost the same as in C mode and `length` about 20%
  more (it counts characters).

## Performance

The interpreter still walks the syntax tree. What changed is everything
the walk was doing besides the arithmetic: hashing a name, allocating a
string, parsing that string back into a number, and throwing the string
away. Callgrind (instruction counts, not a sampling profiler) on an
8,000-line cut of `tools/bench.py`'s input said where. On the original
v1.1 binary, a field sum spent about 45% of its instructions inside
`get_field` and the allocator was the single largest callee in every
program; `bump("NR")` allocated a fresh map key on every record; `printf`
parsed its format string on every record.

Each change below kept the output identical to the previous binary (the
benchmark refuses a time when the awks disagree, and the corpus is the
real check). Times are the best of 3 on this machine, 500,000 lines,
`LC_ALL=C`, seconds. The "before" column is hawk at the v1.1 tag; mawk
and gawk are on the same box. Absolute times will differ elsewhere; the
ratios are the point.

| program | hawk before | hawk after | mawk | gawk |
|---|---:|---:|---:|---:|
| field sum | 0.26 | 0.07 | 0.05 | 0.07 |
| regex filter | 0.13 | 0.06 | 0.05 | 0.06 |
| gsub | 0.25 | 0.09 | 0.05 | 0.20 |
| regex FS + arrays | 0.29 | 0.08 | 0.08 | 0.12 |
| printf | 0.47 | 0.21 | 0.22 | 0.34 |

UTF-8 mode (`tools/bench.py --locale C.UTF-8`, same ASCII input) is the
same story. hawk after / mawk / gawk:

| program | hawk | mawk | gawk |
|---|---:|---:|---:|
| field sum | 0.07 | 0.05 | 0.07 |
| regex filter | 0.06 | 0.05 | 0.10 |
| gsub | 0.10 | 0.05 | 0.23 |
| regex FS + arrays | 0.08 | 0.08 | 0.12 |
| printf | 0.22 | 0.22 | 0.35 |

What each change did, and the hawk time before it and after it on the
field that moved (the others are in the commit message):

* **Special variables live in fields, not the hash map** (`FS`, `RS`,
  `NR`, `FNR`, `OFS`, `ORS`, `OFMT`, `CONVFMT`, `SUBSEP`). Bumping `NR`
  is an addition, not an allocation. A numeric `FS` is still formatted
  with the `CONVFMT` current at the read, not the one current when `FS`
  was assigned. Regex filter 0.13 to 0.07.
* **Fields are spans into the record.** Splitting records where each
  field sits. Arithmetic parses the number off those bytes and remembers
  it; the string value (a strnum when the whole field is numeric, so
  `print $2` still shows `10.0`) is built only when something needs the
  text. Plain integers skip the general float parser. Field sum 0.20 to
  0.09, regex FS 0.22 to 0.15, printf 0.41 to 0.34.
* **One copy of each record, and a word-at-a-time search for a one-byte
  RS.** The reader used to copy the line into a `Vec` and the interpreter
  copied it again into the `Rc` it stored. Regex filter 0.07 to 0.06.
* **A repeated byte set splits by scanning.** `[ ]+` and `:+` are a run
  of one byte. The DFA is still used for every other pattern, and in
  UTF-8 mode for a set that contains a non-ASCII byte. Regex FS 0.14 to
  0.13.
* **Array updates borrow the key.** `c[$1]++` copied the key into an
  `Rc` on every record, then hashed it with SipHash. The key is now
  copied into a reused buffer and looked up borrowed; only a new key is
  allocated. The hash is FxHash with a random per-array seed and a
  finalizer. Without the finalizer the low bits, which `HashMap` uses
  as the bucket index, were identical for keys that differ in the middle,
  and inserts were quadratic (the `colliding_keys_stay_fast` test).
  Regex FS 0.13 to 0.09.
* **`gsub(/a/, "A")` overwrites the record in place** when the pattern
  is a fixed string, the replacement is the same number of plain bytes,
  and a saved `$0` does not share the buffer. `length($0)` counts the
  buffer. Gsub 0.16 to 0.09 across this and the record-buffer change.
* **A constant `printf` format is parsed once.** `%d` and `%f` receive
  the number, so those fields are not turned into strings first. The
  line is built in a buffer that is recycled. Printf 0.33 to 0.21.
* **The last ordinary global stays in a field of the interpreter.**
  `s += $2` was two hash lookups. A different expression that spells
  `s` writes the value back to the map first, so `END` and a function
  argument still see it. Field sum 0.08 to 0.07, now level with gawk.
* **The current record is one reused `Vec`.** The next record is read
  into a spare buffer and the two are swapped only when the read
  succeeds, so end of input does not wipe `$0`. A value that escapes
  (`x = $0`) is copied out. `END { print NR }` on the benchmark file is
  0.022 s (mawk 0.007, gawk 0.037).
* **`panic = "abort"` in the release profile.** Times did not move. The
  binary went from about 1.1 MiB to 993,000 bytes.

### What is left, and what a compiler would buy

The programs that were allocation-bound are now close to mawk: regex FS
matches it, printf is level with it, the regex filter is 0.06 against
0.05, field sum matches gawk and is 1.4 times mawk. Gsub is 0.09 against
mawk's 0.05 and gawk's 0.20.

The gap that is still hawk's, rather than the work itself, is the walk.
`END { print NR }` does not look at a field and still takes 0.022 s
against mawk's 0.007: read the line, add one to `NR`, repeat. A sampling
of that run is the record loop and the `match` on the (empty) rule list.
Field sum adds a span scan and an integer parse on top of that, which is
why it is 0.07 rather than 0.022; mawk's field sum is 0.05, so most of
hawk's remaining distance to mawk on that program is the loop, not the
addition.

Closing that means compiling the tree, which this round does not do.

* **Slots for every variable**, not just the last one touched. A pass
  over the tree gives each name a number. `get` and `set` index a `Vec`.
  The one-name cache above is that idea for the single hot name. Doing
  it for all names removes the write-back when `END` uses a different
  copy of the same letters, and it is the storage a compiler wants
  anyway.
* **Closures or bytecode.** pyawk turns each expression into a Python
  function before running. The Rust equivalent is either a closure per
  node (awkward, because the closure has to hold a mutable interpreter)
  or a small bytecode the main loop switches on: load slot, add, store
  slot, next record. The `match` on `Expr` and the stack check on every
  node go away. Control flow (`next`, `break`, `exit`) is a jump or a
  status code, which hawk already uses a status code for.
* **What it would not speed up.** Printf's digits, the DFA scan in the
  regex filter (already 0.06 against 0.05), and the array update (already
  level with mawk) are the work, not the walk. A compiler does not make
  those much faster. Gsub of a one-byte literal is already an in-place
  scan; the remaining factor of about two against mawk is the loop
  around it plus `length`.
* **Expected gain.** On field sum and on `END { print NR }`, in the
  neighbourhood of the gap that is left: field sum from 0.07 toward
  mawk's 0.05, and the empty record loop from 0.022 toward something
  closer to 0.01, if the reader stays a copy out of a 64 KiB chunk. Not
  a further halving of printf or the regex filter. The cost is a second
  implementation of every expression, which has to stay in lockstep with
  the tree walk or replace it. That is the point at which this round
  stopped.

## Decisions approved by the owner

These choices were approved in discussion and were not yet written down in
this file (neighbouring sections already record the rest, including `%#g`,
the `%c` byte behaviour, the printf width cap, write errors, and the 4M
UTF-8 regex state cap). Dates are the day the behaviour landed, or the
day it was written into the notes.

* **`FILENAME` when reading standard input** (2026-11-07).
  If no file operand is processed (there are none, or only `var=value`
  assignments), hawk reads standard input and sets `FILENAME` to `"-"`,
  as POSIX.1-2024 requires: "awk shall set FILENAME to '-' and process
  standard input as if it was given as a file operand". mawk, gawk and
  pyawk do the same. `FILENAME` stays unset in `BEGIN` until input is
  opened.

* **Empty and missing fields, and Austin Group interpretation #1198**
  (noted 2026-11, during the POSIX challenge work).
  The Austin Group is the body that maintains the POSIX standard.
  Interpretation #1198 (2019) records that POSIX is unclear whether an
  empty or missing field has a string value. hawk follows the three
  reference awks and pyawk: a field past `NF`, and an existing empty
  field, is the empty string, not the uninitialized value, so `$5 == 0`
  is a *string* comparison and false. Those cases are *observational*:
  a difference is not treated as a POSIX failure.

* **`[[=é=]]` matches only itself** (2026-11-02, with pyawk's UTF-8
  choices). An equivalence class or collating symbol in a bracket
  expression matches that character only, in both the C locale and
  UTF-8 mode. It does not expand to glibc's class (`e`, `E`, `É`, ...).
  The same rule is in pyawk's "Locales and UTF-8".

* **`a+?` is `(a+)?`; POSIX 2024 shortest match is not implemented**
  (recorded with the regex decisions). A `?` after a repeat is read as
  "the repeat, made optional", as in mawk and onetrue-awk, not as POSIX
  2024's leftmost-shortest match. gawk (without `--posix`) does implement
  shortest match. This is unchanged on purpose.

