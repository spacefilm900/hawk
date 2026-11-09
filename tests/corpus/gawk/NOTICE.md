# Notice — imported GNU Awk (gawk) tests

The awk programs and their input data in this directory come from the
**GNU Awk (gawk) 5.4.1** test suite (`test/` in the gawk 5.4.1 source
distribution), Copyright (C) Free Software Foundation, Inc.

They are licensed under the **GNU General Public License version 3 or
later**. See [LICENSE-GPL-3.0.txt](LICENSE-GPL-3.0.txt) in this directory
for the full licence text.

These files are **test data only**. They are not part of the hawk
interpreter. The hawk interpreter itself remains under the MIT licence
(see the repository root [LICENSE](../../../LICENSE)).

## What is from gawk, and what is ours

From gawk 5.4.1 `test/` (GPL v3 or later):

- `*.awk` — the test programs (copied from gawk, or for a few skipped
  cases a short placeholder comment written by the importer)
- `*.stdin` / `*.in` — input data copied from gawk's corresponding
  `*.in` files (and, for a few special cases, a short stdin literal
  written by the importer to match gawk's Makefile rule)

Produced by pyawk's importer (`scripts/import_gawk.py`) when this
corpus was copied into hawk, and **not** part of the gawk
distribution:

- `*.operands`, `*.args` — how the harness invokes each case
- `*.out` — expected stdout, taken from gawk's `.ok` only when a
  majority of onetrue-awk, mawk and `gawk --posix` agree; otherwise
  from that majority (or empty for skipped cases)
- `MANIFEST.tsv` — per-case run/skip classification
- this `NOTICE.md` and `LICENSE-GPL-3.0.txt`
