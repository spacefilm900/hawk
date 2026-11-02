# hawk: Haviland Awk

hawk is a POSIX AWK interpreter written in Rust. It is a port of
[pyawk](https://github.com/spacefilm900/pyawk), the Python interpreter that
came first, and it uses the same test corpus and the same semantic decisions.

* **Zero dependencies.** It uses only Rust's standard library: `[dependencies]`
  and `[dev-dependencies]` in `Cargo.toml` are empty, and unit tests use
  Rust's built-in test framework. The test runners in `tools/` are small
  Python scripts. They are test tooling only and do not ship with hawk.
* **Bytes, not text.** Programs and data are handled as bytes (`Vec<u8>`), so
  input that is not valid UTF-8 passes through unchanged. A UTF-8 locale mode
  can be added on top later, as pyawk v1.1 did.
* **Scope.** The first target is pyawk v1.0's feature set (tag `v1.0` of
  pyawk). The v1.1 extras come later: UTF-8 locales, `ENVIRON` and the full
  I/O audit, `nextfile`, and the POSIX regex engine. See [PLAN.md](PLAN.md).

**Status:** the lexer, parser, interpreter and the POSIX regex engine are
done; hawk passes all 504 corpus cases that pyawk v1.0 passes and all 570
regex patterns. `printf` still uses an interim formatter (phase 5). See
PLAN.md for progress and the current test numbers, and `tools/bench.py` for
a rough speed comparison with other awks.

## Building

```sh
cargo build --release          # produces target/release/hawk
target/release/hawk --version
```

You need a stable Rust toolchain. With rustup, `rust-toolchain.toml` picks
stable and adds `rustfmt` and `clippy`.

## Usage

```sh
hawk 'program' [file ...]
hawk -F: -v limit=10 -f prog.awk data.txt
```

The exit status is the value of `exit expr` (0 by default). It is 2 for
syntax and runtime errors.

## Tests

```sh
tools/regress.sh               # the full regression: run it after every change
cargo test                     # unit tests only
tools/run_corpus.py [-v] [--diff] [CATEGORY | CATEGORY/NAME ...]
tools/regex_check.py [--show N]
```

* `tests/corpus/golden/` is pyawk's golden corpus: 99 cases in 9 categories.
  The expected output comes from the reference awks.
* `tests/corpus/onetrue/` and `tests/corpus/gawk/` are the test suites
  imported from onetrue-awk and GNU awk. `MANIFEST.tsv` marks each case to
  run, to compare in sorted order, or to skip.
* `tests/corpus/regex_differential.json` records the majority answer of mawk,
  onetrue-awk and `gawk --posix` for 570 regex patterns (13,503 lines).

How the runners judge progress: `tests/corpus/PASSING` and `REGEX_PASSING`
list the cases hawk passed last time. A run fails only if one of those cases
fails now, which is a regression. After an improvement, record it with
`--update-passing`. Each runner takes `--awk CMD` to test another awk
instead. `tools/ast_check.py` compares hawk's parse of every corpus program
with pyawk v1.0's, using the hidden `hawk --dump-ast` flag (regress.sh runs
it when a pyawk checkout sits next to this repository). That is how the harness itself was checked: pyawk passes 504 of
504 corpus cases and 570 of 570 regex patterns.

## Layout

| Path | What |
|---|---|
| `src/main.rs` | the executable: hands the command line to the library |
| `src/lib.rs` | the library root; declares the modules |
| `src/cli.rs` | command-line options |
| `src/error.rs` | the error type: syntax errors with a source line, runtime errors |
| `src/lexer.rs` | program text to tokens; string escapes |
| `src/ast.rs` | the syntax tree, and its canonical text dump |
| `src/parser.rs` | tokens to syntax tree (recursive descent), parse-time checks |
| `src/value.rs` | AWK values: numbers, strings, strnums; conversions and comparison |
| `src/format.rs` | `printf` formatting (interim until phase 5) |
| `src/interp.rs` | the interpreter: walks the syntax tree |
| `src/array.rs` | insertion-ordered AWK arrays |
| `src/input.rs` | record reading (`RS`) |
| `src/io.rs` | files, pipes, `close`, `system` |
| `src/rand.rs` | `rand`/`srand`, identical to pyawk's |
| `src/regex/` | the regex engine: ERE parser, NFA, lazy DFA, leftmost-longest search |
| `src/subst.rs` | sub/gsub replacement and regex splitting |
| `tests/run.rs` | end-to-end tests of the `hawk` binary |
| `DESIGN.md` | architecture and Rust notes |
| `PLAN.md` | phased plan and progress |
| `docs/pyawk-v1.0-DESIGN.md` | pyawk's design notes at v1.0: the semantic decisions hawk follows |
| `tools/` | test runners (Python, test tooling only) |

## License

MIT; see [LICENSE](LICENSE).
