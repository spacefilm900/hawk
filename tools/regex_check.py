#!/usr/bin/env python3
"""Check hawk's regex matching against the recorded majority of three awks.

usage: tools/regex_check.py [--awk CMD] [--show N] [--jobs N] [--update-passing]

tests/corpus/regex_differential.json (copied from pyawk, where
scripts/regex_fuzz.py records it) holds, for every pattern of pyawk's
regex corpus, the majority answer of mawk, onetrue-awk and gawk --posix
(LC_ALL=C) to a probe program that exercises ``~``, match(), gsub() and
split() with that pattern on each input line.  ``null`` lines are three-way
splits and are not checked.  No reference awk is needed here.

tests/corpus/regex_differential_utf8.json (pyawk v1.1) is the same kind
of record for a UTF-8 corpus (categories ``utf8-...``), run under
LC_ALL=C.UTF-8 and voted by onetrue-awk and gawk --posix only, because
mawk works on bytes.

Each pattern counts as one case.  The exit status is 1 when

* any line of a *gated* table disagrees (HARD_GATES: the C table is a hard
  gate since the v1.1 work started, the UTF-8 table becomes one once
  hawk's UTF-8 mode is in), or
* a pattern listed in tests/corpus/REGEX_PASSING fails (a regression).
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import corpus  # noqa: E402

RECORD = corpus.CORPUS / "regex_differential.json"
RECORD_UTF8 = corpus.CORPUS / "regex_differential_utf8.json"
# Which tables must agree on every line ("c" and/or "utf8").
HARD_GATES = {"c"}
PASSING = corpus.CORPUS / "REGEX_PASSING"
ERROR = "ERROR"


def probe_program(pattern: str) -> str:
    """The probe pyawk's regex_corpus.py uses (kept byte-identical)."""
    p = "/" + pattern + "/"
    return (
        "{ s = $0; r = match(s, P); printf \"%d %d %d\", r, RLENGTH, (s ~ P)\n"
        "  t = s; n = gsub(P, \"<&>\", t); printf \" %d %s\", n, t\n"
        "  n = split(s, A, P); printf \" %d\", n\n"
        "  for (i = 1; i <= n; i++) printf \":%s\", A[i]\n"
        "  print \"\" }\n"
    ).replace("P", p)


def table_of(rec: dict) -> str:
    """"utf8" for an entry of the UTF-8 record, "c" otherwise."""
    return "utf8" if rec["category"].startswith("utf8") else "c"


def locale_of(rec: dict) -> str:
    return "C.UTF-8" if table_of(rec) == "utf8" else "C"


def run_probe(awk: list[str], rec: dict) -> list[str]:
    data = "".join(line + "\n" for line in rec["inputs"]).encode("utf-8", "surrogateescape")
    env = dict(os.environ, LC_ALL=locale_of(rec))
    try:
        proc = subprocess.run([*awk, probe_program(rec["pattern"])], input=data, capture_output=True,
                              timeout=20, env=env)
    except (subprocess.TimeoutExpired, OSError):
        return [ERROR] * len(rec["inputs"])
    if proc.returncode != 0:
        return [ERROR] * len(rec["inputs"])
    return proc.stdout.decode("utf-8", "surrogateescape").split("\n")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--awk", default=str(corpus.DEFAULT_HAWK))
    ap.add_argument("--show", type=int, default=0, help="print the first N mismatching lines")
    ap.add_argument("--jobs", type=int, default=8)
    ap.add_argument("--update-passing", action="store_true")
    opts = ap.parse_args()
    awk = shlex.split(opts.awk)
    if "/" in awk[0]:
        if not Path(awk[0]).exists():
            print(f"regex_check: {awk[0]} not found (run `cargo build --release` first)", file=sys.stderr)
            return 2
        awk[0] = str(Path(awk[0]).resolve())

    records = json.loads(RECORD.read_text())["records"] + json.loads(RECORD_UTF8.read_text())["records"]

    def check(rec: dict) -> tuple[dict, list[tuple[str, str, str]]]:
        got = run_probe(awk, rec)
        bad = [(i, e, g) for i, e, g in zip(rec["inputs"], rec["expected"], got) if e is not None and g != e]
        return rec, bad

    per_cat: dict[str, list[int]] = defaultdict(lambda: [0, 0])  # [patterns ok, patterns failing]
    lines_ok = lines_bad = shown = 0
    passing_now: set[str] = set()
    failing_gated: list[str] = []
    per_table: dict[str, list[int]] = defaultdict(lambda: [0, 0])  # [lines agree, lines mismatch]
    with ThreadPoolExecutor(opts.jobs) as pool:
        for rec, bad in pool.map(check, records):
            key = f"{rec['category']}:{rec['pattern']}"
            checked = sum(e is not None for e in rec["expected"])
            lines_bad += len(bad)
            lines_ok += checked - len(bad)
            per_cat[rec["category"]][0 if not bad else 1] += 1
            per_table[table_of(rec)][0] += checked - len(bad)
            per_table[table_of(rec)][1] += len(bad)
            if not bad:
                passing_now.add(key)
            elif table_of(rec) in HARD_GATES:
                failing_gated.append(key)
            for inp, exp, g in bad:
                if shown < opts.show:
                    print(f"/{rec['pattern']}/ on {inp!r}: expected {exp!r}, got {g!r}")
                    shown += 1

    print(f"\n{'category':<24} {'ok':>5} {'fail':>5}   (patterns)")
    for cat, (ok, bad) in per_cat.items():
        print(f"{cat:<24} {ok:>5} {bad:>5}")
    print(f"{'TOTAL':<24} {len(passing_now):>5} {len(records) - len(passing_now):>5}")
    print(f"lines: {lines_ok} agree, {lines_bad} mismatch")
    for table, (ok, bad) in sorted(per_table.items()):
        gate = "hard gate" if table in HARD_GATES else "regressions only"
        print(f"  {table} table: {ok} agree, {bad} mismatch ({gate})")

    baseline = corpus.read_passing(PASSING)
    regressions = sorted(baseline - passing_now)
    new = passing_now - baseline
    if new:
        print(f"{len(new)} patterns newly passing (run with --update-passing to record)")
    if opts.update_passing:
        if Path(awk[0]).resolve() != corpus.DEFAULT_HAWK.resolve():
            print("--update-passing needs target/release/hawk", file=sys.stderr)
            return 2
        corpus.write_passing(PASSING, passing_now, "regex patterns")
        print(f"wrote {len(passing_now)} keys to {PASSING.relative_to(corpus.REPO)}")
    status = 0
    if regressions:
        print(f"\nREGRESSIONS ({len(regressions)}): " + ", ".join(regressions[:50]))
        status = 1
    if failing_gated:
        print(f"\nGATE FAILED ({len(failing_gated)}): " + ", ".join(sorted(failing_gated)[:50]))
        status = 1
    return status


if __name__ == "__main__":
    sys.exit(main())
