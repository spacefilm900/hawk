# hawk plan

The goal is pyawk v1.0 parity: everything in pyawk's tag `v1.0`, judged by
the same corpus. The work proceeds in small TDD commits, and
`tools/regress.sh` runs after each one. Each phase ends at a green, pushed
commit.

## Phases

| # | Phase | Contents | Done when |
|---|---|---|---|
| 0 | Bootstrap | cargo crate, README/DESIGN/PLAN/LICENSE, corpus copied from pyawk, Python runners driving `target/release/hawk`, `tools/regress.sh`, baseline recorded | ✅ |
| 1 | Lexer | tokens as an enum, keywords and builtins, `FUNC_NAME` (no space before `(`), newline and semicolon rules (newlines dropped after `, { && \|\| do else`, runs collapse, `\`-newline continuation, comments), regex-vs-division, regex literals (`\/`, `/` inside brackets, `[:class:]`), string escapes (shared with `-v`/`-F`/operands), numbers (`1e` is 1 then `e`), `**` and `**=`, line numbers for errors | unit tests port pyawk's `test_lexer.py` and more ✅ |
| 2 | Parser and AST | recursive descent with pyawk's precedence table; concatenation vs unary minus; `no_gt` inside `print`; `(a,b)` groups and `(a,b) in arr`; `getline` forms; ranges; function definitions and calls; lvalues; the context checks (`return`, `break`/`continue`, `next`); syntax errors with line numbers | parses every corpus program the references accept, and rejects pyawk's syntax-error tests |
| 3 | Values and core runtime | `Value` (num/str/strnum/uninit), number to string (integer rule, `CONVFMT`/`OFMT`), string to number (longest prefix), comparison rules; compile the AST to closures; variables as slots; expressions, assignment operators, `++`/`--`, `print` to stdout, `BEGIN`/`END`, `if`/`while`/`do`/`for`/`break`/`continue`/`exit`; control flow as an enum | `BEGIN`-only golden cases pass |
| 4 | Fields and records, and the CLI | the main loop over operands and stdin, `var=value` operands, `RS` (single char, `""` paragraph mode, regex), `FS` (`" "`, single char, `""`, regex), `$n` read and assign, `NF` assignment, `$0` rebuild with `OFS`, `NR FNR FILENAME`, strnum fields; `next`; options `-F` (with escapes, `t`), `-v`, `-f` (repeatable, `-`), `--` | the `basic`, `fields` and `one_liners` golden categories pass |
| 5 | printf and number formatting | hand-written, C-compatible `%d %i %o %x %X %u %c %s %e %E %f %F %g %G %%`, flags, width, precision, `*`; exact glibc rounding; inf/nan spellings; `strtod`-compatible parsing | `builtins/printf_formats`, `basic/number_output`, and a unit-test table checked against C `printf` |
| 6 | Regex | ERE parser (bracket expressions, classes, `{m,n}` intervals, anchors, escapes), Thompson NFA, lazy DFA with a capped cache, leftmost-longest `find_at`, the repeat limit 32767; dynamic regexes from strings; `~ !~`, regex patterns | most of `tools/regex_check.py`; collating symbols and equivalence classes may wait for v1.1 |
| 7 | Arrays, builtins, functions, ranges, I/O | `in`, `delete`, `SUBSEP`, `for (k in a)`; `length substr index split sub gsub match sprintf tolower toupper sin cos atan2 exp log sqrt int rand srand system close fflush`; user functions (scalars by value, arrays by reference, untyped parameters that become arrays, recursion); range patterns; `getline` (all five forms); `print`/`printf` with `> >> \|`; `close()` status; `ARGV`/`ARGC` | the golden corpus 99/99, onetrue 215/215, gawk 190/190, as pyawk v1.0 |
| 8 | v1.0 release | docs, a performance check against pyawk and mawk, tag `v1.0` | the user signs off |

Later, after v1.0 and in pyawk v1.1's order: `nextfile`, `ENVIRON` and the
full I/O audit, the POSIX parse-time errors, UTF-8 locale mode, collating
symbols and equivalence classes, and the regex differential at 570/570.

## Baseline (phase 0, stub binary)

The stub binary answers `hawk: not implemented yet` with exit status 2:

| Suite | Pass | Fail | Skip |
|---|---|---|---|
| golden (9 categories) | 0 | 99 | 0 |
| onetrue | 19 | 196 | 9 |
| gawk | 22 | 168 | 114 |
| **corpus total** | **41** | **463** | **123** |
| regex patterns | 37 | 533 | – (878 of 13,503 lines) |

The 41 corpus passes are cases whose expected stdout is empty. The 37 regex
passes are patterns that all three references reject with an error. They
are trivial passes, but they are recorded in `PASSING`/`REGEX_PASSING`, so
they count as regressions if they ever break. To check the harness itself,
pyawk (via `--awk`) scores 504/0/123 and 570/0, the same as its own runners.

## Progress log

* Phase 0: bootstrap and baseline.
* Phase 1: `error.rs` (syntax/runtime errors, pyawk's message format) and
  `lexer.rs` (`process_escapes`, `tokenize`). 16 unit tests: all of
  pyawk's `test_lexer.py` plus CRLF continuation, string continuation,
  line numbers, `[]/]`, `[^]/]`, `[[:alpha:]/]`, `.5`, `1.e2`, `1e999`,
  `0x1A`, octal wrap (`"\501"` is `A`, checked against all three
  references), and non-UTF-8 bytes. Corpus unchanged (41/463/123; regex
  37/533): nothing runs programs yet.
* Phase 2: `ast.rs` (node types in pyawk's shapes, canonical dump) and
  `parser.rs` (recursive descent, a port of pyawk v1.0's `parser.py`:
  precedence with the concatenation, `in` and `cmd | getline` quirks,
  assignment after `|| && ~ <`, `$` operand rules, print `>` redirection and
  `print (a, b)`, `(i, j) in arr`, ranges, functions, and every parse-time
  check with pyawk's messages). 38 parser unit tests (all of pyawk's
  `test_parser.py` plus extras); 55 unit tests in total. Hidden flag
  `hawk --dump-ast`; `tools/ast_check.py` compares it with pyawk v1.0 on all
  627 corpus programs: 599 identical trees, 28 rejected by both with the
  same message, 0 mismatches. Of the 28, 27 are rejected by the reference
  majority; `gawk/getline.awk` (`cmd | getline x + 1`) is accepted by all
  three references but rejected by pyawk v1.0 and so by hawk (the case is
  skipped: the references disagree on its output). Corpus unchanged.
