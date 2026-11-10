# Handover

## Purpose

This file exists so that another agent or developer can take over
maintenance of hawk without relying on the original assistant (Grok Bot)
or its chat history. It is the entry point: it says what to read, how to
build and verify the tree, and the rules that must be followed.

Start here, then read [README.md](README.md) for usage and the test map,
[PLAN.md](PLAN.md) for how the port was staged, and [DESIGN.md](DESIGN.md)
for architecture and semantic decisions. Do not treat this chat, or any
assistant's memory, as a source of truth once you have this file.

## Project overview and scope

hawk (Haviland Awk) is a POSIX AWK interpreter written in Rust. It is a
port of [pyawk](https://github.com/spacefilm900/pyawk) and uses the same
test corpus and the same semantic decisions.

It uses only Rust's standard library: `[dependencies]` and
`[dev-dependencies]` in `Cargo.toml` are empty, and unit tests use
Rust's built-in test framework. Do not add third-party crates. The
helpers in `tools/` are small Python scripts for testing only; they do
not ship with hawk.

Programs and data are handled as bytes, so input that is not valid UTF-8
passes through unchanged. In a UTF-8 locale, string functions, `printf`
and regexes count characters, as in pyawk. The interpreter walks the
syntax tree directly (pyawk compiles the tree to closures).

The target is POSIX AWK (including POSIX 2024 `nextfile`), matching
pyawk's scope. Gawk-only extensions such as `gensub()`,
`BEGINFILE`/`ENDFILE`, `IGNORECASE` and the time functions are out of
scope unless the owner asks for them as opt-in additions.

The current version on the `enhancements` branch is 1.2.1.

## Repository map

| Path | What |
|---|---|
| `src/main.rs` | The executable: hands the command line to the library |
| `src/lib.rs` | The library root; declares the modules |
| `src/lexer.rs`, `src/parser.rs`, `src/ast.rs` | Lexer, recursive-descent parser, syntax tree |
| `src/interp.rs` | Tree-walking interpreter |
| `src/value.rs`, `src/array.rs`, `src/format.rs` | Values, arrays, printf |
| `src/regex/` | POSIX ERE engine: parser, NFA, lazy DFA, leftmost-longest search |
| `src/unicode_tables.rs` | Generated Unicode class and case tables (from gawk under C.UTF-8) |
| `tests/run.rs`, `tests/io.rs`, `tests/utf8.rs` | End-to-end tests of the `hawk` binary |
| `tests/corpus/golden/` | Golden corpus copied from pyawk (154 cases; see below) |
| `tests/corpus/gawk/`, `tests/corpus/onetrue/` | Imported suites, with licence notices in those folders |
| `tests/corpus/regex_differential.json` | Recorded C-locale regex majority answers (a hard gate) |
| `tests/corpus/regex_differential_utf8.json` | Recorded UTF-8 regex answers (a hard gate) |
| `tests/corpus/PASSING`, `REGEX_PASSING` | Cases that passed last time; a run fails only on a regression |
| `tools/regress.sh` | Full local regression; stops on the first failure |
| `tools/check.sh` | The same suites, with a pass/fail summary for every suite |
| `tools/run_corpus.py`, `tools/regex_check.py` | Corpus and regex runners |
| `tools/ast_check.py` | Parser parity with pyawk, when `../pyawk` is present |
| `tools/differential.py` | Slow live comparison with reference awks (CI's separate job) |
| `ci/github-ci.yml` | Canonical GitHub Actions workflow (copied to `.github/workflows/ci.yml`) |
| `docs/` | Copies of pyawk's DESIGN notes at v1.0 and v1.1, for reference |
| `DESIGN.md`, `PLAN.md` | Architecture, and the staged port plan |
| `LICENSE` | MIT licence, copyright Keith Haviland |

## Setup and build

A stable Rust toolchain is required. `rust-toolchain.toml` asks rustup
for stable plus `rustfmt` and `clippy`.

```sh
cargo build --release          # produces target/release/hawk
target/release/hawk --version
target/release/hawk 'BEGIN { print "hello" }'
```

There is nothing to install from crates.io. The Python scripts in
`tools/` need Python 3 and the standard library only.

## Running the tests

The committed `.out` files and regex tables are enough for the main
gate. Reference awks are needed only for `tools/differential.py` and
for regenerating tables.

```sh
make check                     # the single command: every suite, then a summary
tools/check.sh                 # the same, invoked directly
tools/regress.sh               # the same suites; stops on the first failure
cargo test                     # Rust unit and integration tests only
python3 tools/test_tools.py    # unit tests of the Python harness
python3 tools/run_corpus.py [-v] [--diff]
python3 tools/regex_check.py
python3 tools/ast_check.py     # needs a pyawk checkout at ../pyawk
```

`make check` / `tools/check.sh` must not be given flags that rewrite
test data (`--update-passing` and the like). After a genuine
improvement, `--update-passing` on the individual runners records newly
passing cases; do that only on purpose, in its own commit.

`tools/differential.py` is the slow extra check (CI's `differential`
job). It needs reference awks and is not part of `make check`. See the
README.

## Expected current results

Measured on the `enhancements` branch on 10 November 2026 at 06:15 UK
time (Europe/London, UTC+0), with `make check` (about 35 seconds on this
machine, with a warm build cache). No test files were modified.

```
cargo fmt                    PASS  clean
cargo clippy                 PASS  no warnings
cargo test                   PASS  201 passed, 0 failed
release build                PASS  target/release/hawk
harness unit tests           PASS  tools/test_tools.py
golden corpus                PASS  154 passed, 0 failed, 0 skipped
onetrue suite                PASS  215 passed, 0 failed, 9 skipped
gawk suite                   PASS  192 passed, 0 failed, 112 skipped
regex differential (C)       PASS  13503 lines agree, 0 mismatch
regex differential (UTF-8)   PASS  2152 lines agree, 0 mismatch
ast check vs pyawk           PASS  682 programs: 654 same tree, 28 rejected by both, 0 mismatches
```

The golden corpus is pyawk's, minus four cases that are pyawk-specific
(`errors/missing_brace`, `errors/next_in_begin`,
`builtins/printf_c_strnum`, `control/exit_status`). The imported onetrue
and gawk counts match pyawk's. A later run should match these counts,
or beat them without editing tests. If a count drops, that is a
regression.

## Release and git workflow

* **`main` is frozen at v1.0** (commit `f6585db`, tag `v1.0`). Do not
  merge into it, push to it, or change it unless the owner has explicitly
  asked you to.
* **Do all work on `enhancements`.** That is the current development
  branch (v1.2.1 and later fixes).
* **The open pull request into `main` is a review vehicle only.** A
  pull request (PR) is a proposed change put up for review. PR #2 exists
  so that CodeRabbit can comment on the work. It must never be merged.
* **Tags and GitHub releases only with the owner's approval.** A tag is
  a named label for one commit, used to mark a release. Do not create
  one yourself.
* **CI** (GitHub Actions, `.github/workflows/ci.yml`) runs on push to
  `enhancements`, on pull requests into `enhancements` or `main`, on
  manual dispatch, and weekly. The `check` job is fmt, clippy with
  `-D warnings`, `cargo test`, the release build, the harness tests,
  `run_corpus.py` and `regex_check.py` (the same steps as
  `tools/regress.sh`, apart from `ast_check.py`, which needs pyawk).
  The `differential` job installs reference awks and runs
  `tools/differential.py`. The workflow uses no third-party crates and
  no secrets.

## Standing rules

These come from the owner (Keith Haviland) and apply to every change:

1. **Never alter tests.** If there is a problem with an imported test,
   a golden case, or a POSIX-challenge case, report it back. Do not edit
   the test to make it pass.
2. **Test first.** Add or extend a test that fails, then change the
   interpreter, then confirm the test passes.
3. **Small commits.** One idea per commit, with a clear message.
4. **Full regression after each change.** Run `make check` (or
   `tools/regress.sh`) after each change, not only at the end.
5. **Freeze results before a fix.** Record what fails today before you
   change anything, so you can tell a fix from a new breakage.
6. **Tie-break.** When behaviour is unclear: POSIX first; then the
   majority of onetrue awk, `gawk --posix` and mawk. Record the choice
   in DESIGN.md and pin it with a test. BusyBox is not part of the
   tie-break.
7. **No third-party crates.** hawk must keep an empty `[dependencies]`
   and `[dev-dependencies]`.

## Licences

* The interpreter is MIT, copyright (c) 2026 Keith Haviland; see
  [LICENSE](LICENSE).
* `tests/corpus/gawk/` is GNU Awk 5.4.1 test data, GPL v3 or later; see
  [tests/corpus/gawk/NOTICE.md](tests/corpus/gawk/NOTICE.md) and
  [tests/corpus/gawk/LICENSE-GPL-3.0.txt](tests/corpus/gawk/LICENSE-GPL-3.0.txt).
* `tests/corpus/onetrue/` is onetrue awk test data, Lucent Technologies
  1997 notice; see
  [tests/corpus/onetrue/NOTICE.md](tests/corpus/onetrue/NOTICE.md).

Those imported files are test data only and are not part of the
interpreter. hawk's import tooling does not delete those folders first,
so re-importing does not strip the notices.

## Related resources

These live beside this repository on the development machine, and as
sibling GitHub repositories under `spacefilm900`. They are not part of
the hawk tree.

* **[spacefilm900/pyawk](https://github.com/spacefilm900/pyawk)** — the
  Python original. `tools/ast_check.py` and `tools/regress.sh` look for
  a checkout at `../pyawk`.
* **[spacefilm900/awk-posix-challenge](https://github.com/spacefilm900/awk-posix-challenge)**
  — an independent POSIX AWK challenge (v0.6 is the final tagged
  corpus). Run it with `python3 tools/runner.py --cmd /path/to/hawk`.
  Observational cases (including those that cite Austin Group
  interpretation #1198) are not POSIX failures. Do not edit the case
  files.
* **[spacefilm900/awk-volume-tests](https://github.com/spacefilm900/awk-volume-tests)**
  — a large-input check that runs a portable program over the complete
  works of Shakespeare. Output must match the committed expected text
  byte for byte.
* **Reference awks** used for votes, imports and the differential job,
  on the development machine under `/workspace/refs` (and `mawk` from
  the system):

  | reference | usual name on PATH | path on this machine |
  |---|---|---|
  | mawk | `mawk` | `/usr/bin/mawk` |
  | onetrue awk | `original-awk`, `onetrue-awk` or `bwk-awk` | `/workspace/refs/onetrue-awk/a.out` |
  | gawk (the differential job runs `gawk --posix`) | `gawk` | `/workspace/refs/local/bin/gawk` (GNU Awk 5.4.1) |
  | BusyBox (comparison only, not a tie-break voter) | `busybox awk` | `/workspace/refs/busybox/busybox` |

  `tools/differential.py` finds these on `PATH` and skips any that are
  missing; `--ref NAME=CMD` overrides a name. `tools/gen_unicode_tables.py`
  uses `$HAWK_REF_GAWK`, else `/workspace/refs/local/bin/gawk` if that
  file exists, else `gawk` on `PATH`.

## Known open items

* **hawk PR #2, low-priority CodeRabbit note (scalar cache and `ARGC`).**
  On 7 November 2026 CodeRabbit reported that `var_num` and `var_str` in
  `src/interp.rs` read the `globals` map without first checking the
  one-name scalar cache. `store_scalar` writes a cached value back to
  `globals` only when another name is cached. A command-line operand
  such as `ARGC=2` goes through `command_line_assign` and updates the
  cache only, so the next `open_next_input` can still see the old
  `ARGC`. The example given was `hawk '{print}' ARGC=2 a b c`, which
  may open operands past the new count. It is an uncommon usage. The
  note was left unresolved as low priority; do not fix it unless the
  owner asks, and if you do, freeze a failing test first.
* CodeRabbit also raised an inferred note about the seeded FxHash used
  for array keys (a possible collision construction). That is described
  in the PR's architecture review and in DESIGN.md, "Values". It is not
  a demonstrated correctness bug.
* Reviews on PR #2 are paused: the last reviewed commit was `d24f8d1`;
  the branch has moved on (licence and later commits). The PR must not
  be merged.
* `PLAN.md` ends at the v1.1 port. The branch has since reached v1.2.1
  (performance work, CI, `FILENAME`, licence notices). Treat DESIGN.md
  and this file as more current than the later pages of PLAN.md.

## Suggested first task for a new maintainer

A good first task, if the owner wants to test a handover, is to add
**`gensub()`** as an *opt-in gawk extension*, first in pyawk if it is
not already there, then here, following pyawk's behaviour.

`gensub()` is gawk's general substitution function. It is self-contained,
well documented in the gawk manual, and gawk itself is the oracle (the
program you check correct output against). It exercises the regex engine,
the builtins and the test workflow without changing the POSIX core.

Rules for that work:

* Implement it only if the owner asks, and only on a branch created for
  the experiment (or in a separate copy of the repository). Do not put
  it straight onto `enhancements` unless the owner says to.
* It must be opt-in. A POSIX program that does not call `gensub()` must
  behave exactly as it does today.
* **Every POSIX result must stay unchanged.** Run `make check` before
  and after; the unit, golden, onetrue, gawk-imported, and regex counts
  must not regress. Do not edit existing tests to make a change look
  green.
* Add new tests for `gensub()` itself, rather than altering the imported
  suites.
* Do not add a third-party crate in order to implement it.

A harder second task, if that one goes well, is `BEGINFILE`/`ENDFILE`,
which touches the parser and the main input loop. Avoid `IGNORECASE`
(it changes behaviour almost everywhere) and the time functions (they
are hard to test reliably).
