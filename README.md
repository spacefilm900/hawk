# hawk: Haviland Awk

hawk is a POSIX AWK interpreter written in Rust. It is a port of
[pyawk](https://github.com/spacefilm900/pyawk), the Python interpreter that
came first, and it uses the same test corpus and the same semantic decisions.

* **Zero dependencies.** It uses only Rust's standard library: `[dependencies]`
  and `[dev-dependencies]` in `Cargo.toml` are empty, and unit tests use
  Rust's built-in test framework. The test runners in `tools/` are small
  Python scripts. They are test tooling only and do not ship with hawk.
* **Bytes, not text.** Programs and data are handled as bytes (`Vec<u8>`), so
  input that is not valid UTF-8 passes through unchanged. In a UTF-8 locale
  (`LC_ALL`, `LC_CTYPE` or `LANG` naming UTF-8) string functions, `printf`
  and regexes count characters instead, as in pyawk v1.1.
* **Scope.** The first target is pyawk v1.0's feature set (tag `v1.0` of
  pyawk). The v1.1 extras are being ported on the `enhancements` branch:
  the POSIX parse-time errors, `nextfile`, `ENVIRON`, the full I/O
  audit and UTF-8 locale mode are done. See [PLAN.md](PLAN.md).

**Status:** the lexer, parser, interpreter and the POSIX regex engine are
done; hawk passes all 504 corpus cases that pyawk v1.0 passes and all 570
regex patterns, and its `printf` agrees with the C library on an
18,000-case table (`tools/printf_table.py`). On `enhancements` it passes
all 561 corpus cases that pyawk v1.1 runs here, both regex tables (C and
UTF-8) line for line, and 0 AST mismatches with pyawk v1.1. See
PLAN.md for progress and the current test numbers.

`tools/bench.py` times five programs on 500,000 generated lines (best of
3). On the machine used for the performance work, seconds, hawk / mawk /
gawk, `LC_ALL=C`:

| program | hawk | mawk | gawk |
|---|---:|---:|---:|
| field sum | 0.07 | 0.05 | 0.07 |
| regex filter | 0.06 | 0.05 | 0.06 |
| gsub | 0.09 | 0.05 | 0.20 |
| regex FS + arrays | 0.08 | 0.08 | 0.12 |
| printf | 0.21 | 0.22 | 0.34 |

UTF-8 mode (`--locale C.UTF-8`) is within a hundredth of those hawk
numbers (hawk v1.2). At v1.1, before that work, the same runs were 0.26, 0.13, 0.25,
0.29 and 0.47. DESIGN.md, "Performance", says what changed and what a
compiler would still buy.

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
tools/test_tools.py            # unit tests of the Python harness
tools/run_corpus.py [-v] [--diff] [CATEGORY | CATEGORY/NAME ...]
tools/regex_check.py [--show N]
tools/differential.py [--ref NAME=CMD ...] [--only stress|regex|golden]   # needs reference awks
```

* `tests/corpus/golden/` is pyawk v1.1's golden corpus: 154 cases in 14
  categories. The expected output comes from the reference awks. The `utf8`
  category runs with `LC_ALL=C.UTF-8`, the rest with `LC_ALL=C`; a
  `NAME.environ` file adds environment variables, and each golden case runs
  in a throwaway copy of its directory.
* `tests/corpus/onetrue/` and `tests/corpus/gawk/` are the test suites
  imported from onetrue-awk and GNU awk. `MANIFEST.tsv` marks each case to
  run, to compare in sorted order, or to skip.
* `tests/corpus/regex_differential.json` records the majority answer of mawk,
  onetrue-awk and `gawk --posix` for 570 regex patterns (13,503 lines).
  This table is a hard gate: every line must agree.
* `tests/corpus/regex_differential_utf8.json` does the same for 88 patterns
  on UTF-8 text (2,152 lines, onetrue-awk and `gawk --posix` under
  `LC_ALL=C.UTF-8`). Also a hard gate.

How the runners judge progress: `tests/corpus/PASSING` and `REGEX_PASSING`
list the cases hawk passed last time. A run fails only if one of those cases
fails now, which is a regression. After an improvement, record it with
`--update-passing`. Each runner takes `--awk CMD` to test another awk
instead. That is how the harness itself was checked: pyawk v1.1 passes 561
of 561 corpus cases (121 skipped) and agrees on all 15,655 regex lines.

`tools/ast_check.py` compares hawk's parse of every corpus program with
pyawk v1.1's, using the hidden `hawk --dump-ast` flag (regress.sh runs it
when a pyawk checkout sits next to this repository, and says so when it
skips it).

`tools/differential.py` is the slow check, and it needs reference awks: mawk,
onetrue-awk (`original-awk`), `gawk --posix` and BusyBox, skipping any that
are missing.
* **Gate:** 18 stress programs on large generated inputs must match the
  majority of the references.
* **Reports only:** the regex probe run live, and the golden corpus, compared
  with the references that agree with each other.

### Continuous integration

`.github/workflows/ci.yml` (canonical copy `ci/github-ci.yml`) runs on push
to `enhancements`, on pull requests, on manual dispatch and weekly. It has
two jobs:
* `check`: `cargo fmt --check`, clippy with `-D warnings`, `cargo test`, the
  release build, the harness tests, `run_corpus.py` and `regex_check.py`.
  These are the same steps as `tools/regress.sh`, apart from `ast_check.py`,
  which needs the pyawk checkout.
* `differential`: installs the reference awks with apt, then runs
  `tools/differential.py`. It is separate and slower.

The workflow uses no third-party crates and no secrets.

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
| `src/format.rs` | `printf` formatting, checked against the C library |
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

MIT, copyright (c) 2026 Keith Haviland; see [LICENSE](LICENSE).

The interpreter code is MIT. The imported test suites under
`tests/corpus/` have their own licences:

- `tests/corpus/gawk/` — GNU Awk (gawk) 5.4.1 tests, GPL v3 or later;
  see [tests/corpus/gawk/NOTICE.md](tests/corpus/gawk/NOTICE.md) and
  [tests/corpus/gawk/LICENSE-GPL-3.0.txt](tests/corpus/gawk/LICENSE-GPL-3.0.txt).
- `tests/corpus/onetrue/` — onetrue awk tests, Lucent Technologies 1997
  notice; see [tests/corpus/onetrue/NOTICE.md](tests/corpus/onetrue/NOTICE.md).

Those files are test data only and are not part of the interpreter.
