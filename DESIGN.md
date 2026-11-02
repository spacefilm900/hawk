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
source bytes --lexer.rs--> Vec<Token> --parser.rs--> ast.rs --compile.rs--> closures --runtime.rs--> output
                                                        value.rs, format.rs, regex/, builtins.rs, io.rs
```

| Module | Responsibility | Phase |
|---|---|---|
| `main.rs` | executable; calls `cli::main` | 0 |
| `cli.rs` | options `-F -v -f --`, `--version`, exit status | 0 / 4 |
| `error.rs` | `Error` type: syntax errors (with source line) and runtime errors | 1 |
| `lexer.rs` | tokens, newline rules, regex-vs-division, string escapes, numbers | 1 |
| `ast.rs` | expression and statement enums | 2 |
| `parser.rs` | recursive descent, one function per precedence level | 2 |
| `value.rs` | `Value` (number / string / strnum), conversions, comparison | 3 |
| `format.rs` | C-compatible `printf` (`%d %i %o %x %X %u %c %s %e %E %f %F %g %G`), number to string, `strtod`-style parsing | 5 |
| `regex/` | ERE parser, then NFA, then lazy DFA, with leftmost-longest matching | 6 |
| `compile.rs` | AST into closures (a tree of boxed functions) | 3 |
| `runtime.rs` | variables and statements (3); main loop, records and fields (4); ranges (7) | 3–7 |
| `builtins.rs` | builtin functions, including `sub`/`gsub` and `split` | 7 |
| `io.rs` | output redirections, pipes, `getline` sources, `close`, `system` | 7 |

The crate is a library (`src/lib.rs`) plus a thin binary (`src/main.rs`).
Each module keeps its unit tests at the bottom, in `#[cfg(test)] mod tests`.
That section is compiled only for `cargo test`. End-to-end behaviour is
checked by the external corpus runners (see README).

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

AWK's own control flow (`next`, `exit`, `return`, `break`, `continue`) is
*not* an error. pyawk used Python exceptions for it. hawk's statement
executor returns a small enum (`Flow::Normal`, `Flow::Next`,
`Flow::Exit(n)`, ...) that each loop and call site inspects. That is
explicit and costs nothing when nothing happens.

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

Arrays are `HashMap<Rc<[u8]>, Value>`. Arrays are passed to functions by
reference, and a variable can turn out to be an array only when a function
uses it as one. So an array slot is `Rc<RefCell<Array>>`: shared
ownership (`Rc`) of a value that may be changed through any of its owners
(`RefCell` checks the borrowing rules at run time instead of compile time).

## Execution model (phase 3)

As in pyawk, the AST is compiled once into closures. A closure is a
function value that captures the data it needs. In Rust each compiled
expression is a `Box<dyn Fn(&mut Runtime) -> Result<Value, Error>>`, a
heap-allocated function object called through a *trait object* (`dyn Fn`,
"any type that can be called like this function"). Variables are resolved
to slot numbers at compile time, so running the program never looks a
name up in a hash table. If closures turn out to be slow in Rust, a small
bytecode VM is the fallback, behind the same interface.

## Regex (phase 6)

hawk cannot use the `regex` crate (no dependencies), and that crate does not
implement POSIX leftmost-longest matching anyway. hawk has its own
engine in `src/regex/`. It parses ERE syntax into an AST, compiles that
into a Thompson NFA, and runs it through a lazily built DFA. A DFA state is
a set of NFA states, built on demand and cached with a size cap. Matching
never backtracks, so no pattern can take exponential time. The engine
returns the leftmost-longest match, as POSIX requires. Its design follows
pyawk v1.1's engine, including the repeat-count limit of 32767 and an NFA
state limit. The public interface is small (`Regex::new`, `is_match`,
`find_at`) so the rest of the interpreter does not depend on the internals.

## printf and numbers (phase 5)

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
