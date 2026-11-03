#!/bin/sh
# The full regression, run after every change:
#   formatting, lints, unit tests (Rust and the Python harness), release
#   build, the corpus and the regex check.
# Exits non-zero on the first problem.  The corpus runners fail only on a
# *regression* (a case in tests/corpus/PASSING or REGEX_PASSING that no
# longer passes); see tools/corpus.py.
set -eu
cd "$(dirname "$0")/.."
# Prefer a rustup toolchain (it has rustfmt and clippy) over a distro cargo.
PATH="$HOME/.cargo/bin:$PATH"
export PATH

cargo fmt --check
cargo clippy --quiet --all-targets -- -D warnings
cargo test --quiet
cargo build --quiet --release
python3 tools/test_tools.py
python3 tools/run_corpus.py "$@"
python3 tools/regex_check.py "$@"
# Parser parity with pyawk v1.0 on every corpus program (needs a pyawk
# checkout next to this repository; skipped, with a notice, otherwise).
# `-e` rather than `-d`: in a git worktree `.git` is a file.
if [ -e ../pyawk/.git ]; then
    python3 tools/ast_check.py
else
    echo "regress: skipping tools/ast_check.py (no ../pyawk checkout)" >&2
fi
