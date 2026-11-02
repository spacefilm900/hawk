#!/bin/sh
# The full regression, run after every change:
#   formatting, lints, unit tests, release build, the corpus and the regex check.
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
python3 tools/run_corpus.py "$@"
python3 tools/regex_check.py
