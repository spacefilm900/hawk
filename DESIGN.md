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
| `format.rs` | `printf` formatting; an interim version now, made glibc-exact in phase 5 | 3 / 5 |
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
* **Recursion.** The interpreter recurses on the native stack, so `cli.rs`
  runs it on a thread with a 1 GiB stack (reserved address space, not
  memory). Each user function call compares the stack position with the
  start of the run and stops with pyawk's "function call nesting too deep
  (runaway recursion?)" before the stack runs out (about 300 000 levels in
  a release build), instead of crashing.

## Regex (phase 6)

hawk cannot use the `regex` crate (no dependencies), and that crate does not
implement POSIX leftmost-longest matching anyway. hawk has its own engine in
`src/regex/`, modelled on pyawk v1.1's `regex.py` (semantics, error messages
and limits):

* `parse.rs`: ERE text to an AST (`Node`). Bracket expressions are resolved
  at parse time into a `ByteSet` (256 bits), so `[[:alpha:]_-]`, ranges,
  negation, `[.x.]` and `[=x=]` all become one set. Intervals `{m,n}` are
  checked against `DUP_MAX` = 32767 and expanded by `repeat()`. Errors are
  pyawk's: `bad regular expression /src/: why`.
* `nfa.rs`: the AST to a Thompson NFA (byte-set, split, `^`, `$`, match
  instructions). Its size is computed before it is built and capped at
  `NFA_LIMIT` = 1,000,000 instructions, as in pyawk, so `(a{1000}){1000}`
  is an error rather than a memory blow-up. The same AST also builds a
  *reversed* NFA (used to find match starts, below).
* `dfa.rs`: a lazy DFA. A state is the set of NFA instructions reachable
  without consuming a byte; states and transitions are built only when a
  byte is first seen in that state. Bytes the regex cannot tell apart share
  one transition column (byte classes). When a DFA grows past `DFA_LIMIT` =
  10,000 states its cache is flushed and rebuilt on demand, so memory stays
  bounded while the run time stays linear.
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
any other single byte, regex-special or not (`|`, `.`, `\\`), is a literal
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

Until phase 5, `format.rs` holds an *interim* formatter, clearly marked as
such in its comments: it uses Rust's own float formatting (which is exact
and rounds ties to even, as glibc does) and covers the common
conversions. Phase 5 replaces or audits it against a C `printf` table.

Rust's `format!` is not C's `printf`. hawk has a hand-written formatter
that reproduces glibc output for the conversions AWK uses, including
`%g` precision and exponent rules, `%.30g`, rounding (exact decimal
expansion of the double, round-half-even on ties, as glibc does), `inf`
and `nan`, flags `-+ #0`, `*` widths, and `%c` with a number or a string.
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
  line a later error message names.
* **`cmd | getline` result in arithmetic.** `"cmd" | getline x + 1` is
  `("cmd" | getline x) + 1` in hawk, as in gawk and mawk; the same holds for
  `- * / % ^`. pyawk v1.0 rejects it as a syntax error, and onetrue-awk
  reads it as a concatenation with `+1`. hawk follows the reference
  majority (`gawk/getline.awk` in the corpus; `tools/ast_check.py` lists it
  as a deliberate difference).
* **ASCII-only names.** `-v name=value`, `name=value` operands and program
  identifiers accept only ASCII letters, digits and `_`, as POSIX
  specifies. pyawk v1.0 used Python's `str.isidentifier()`, which also
  accepts non-ASCII letters for `-v`.
* **`tolower`/`toupper`** change ASCII letters only (bytes, the C locale);
  pyawk v1.0 used Python's Unicode case mapping.
