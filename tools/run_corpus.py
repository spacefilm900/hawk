#!/usr/bin/env python3
"""Run the golden corpus and the imported onetrue/gawk suites against hawk.

usage: tools/run_corpus.py [--awk CMD] [--diff] [-v] [--jobs N]
                           [--update-passing] [CATEGORY or CATEGORY/NAME ...]

Prints pass/fail/skip per category.  Exit status: 1 if a case listed in
tests/corpus/PASSING fails (a regression), else 0.  ``--update-passing``
rewrites PASSING from this run (full runs only).  ``--awk`` runs another
awk instead of target/release/hawk (shell syntax, e.g. ``--awk 'gawk
--posix'``), which is how the harness itself is sanity-checked.
"""

from __future__ import annotations

import argparse
import difflib
import shlex
import subprocess
import sys
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import corpus  # noqa: E402

PASSING = corpus.CORPUS / "PASSING"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cases", nargs="*", help="CATEGORY or CATEGORY/NAME (default: everything)")
    ap.add_argument("--awk", default=str(corpus.DEFAULT_HAWK), help="awk command to test (default: %(default)s)")
    ap.add_argument("--diff", action="store_true", help="show a diff for each failure")
    ap.add_argument("-v", "--verbose", action="store_true", help="list every failing case")
    ap.add_argument("--jobs", type=int, default=8)
    ap.add_argument("--update-passing", action="store_true", help="rewrite tests/corpus/PASSING")
    opts = ap.parse_args()

    awk = shlex.split(opts.awk)
    if not Path(awk[0]).exists() and "/" in awk[0]:
        print(f"run_corpus: {awk[0]} not found (run `cargo build --release` first)", file=sys.stderr)
        return 2
    awk[0] = str(Path(awk[0]).resolve()) if "/" in awk[0] else awk[0]

    cases = corpus.discover()
    if opts.cases:
        cases = [c for c in cases if c.category in opts.cases or c.id in opts.cases]

    def run(case: corpus.Case) -> tuple[corpus.Case, bool, str]:
        try:
            proc = corpus.run_case(case, awk)
        except subprocess.TimeoutExpired:
            return case, False, "timeout"
        except OSError as e:
            return case, False, f"cannot run: {e}"
        if case.matches(proc.stdout):
            return case, True, ""
        detail = proc.stderr.decode("utf-8", "replace").strip()
        if opts.diff:
            diff = difflib.unified_diff(
                case.expected().decode("utf-8", "replace").splitlines(keepends=True),
                proc.stdout.decode("utf-8", "replace").splitlines(keepends=True),
                "expected", "got",
            )
            detail = "".join("    " + ln for ln in diff) + ("\n    stderr: " + detail if detail else "")
        return case, False, detail

    passed: dict[str, int] = defaultdict(int)
    skipped: dict[str, int] = defaultdict(int)
    failed: dict[str, list[str]] = defaultdict(list)
    passing_now: set[str] = set()
    todo = [c for c in cases if c.status != "skip"]
    for c in cases:
        if c.status == "skip":
            skipped[c.category] += 1
    with ThreadPoolExecutor(opts.jobs) as pool:
        for case, ok, detail in pool.map(run, todo):
            if ok:
                passed[case.category] += 1
                passing_now.add(case.id)
            else:
                failed[case.category].append(case.name)
                if opts.verbose or opts.diff:
                    print(f"FAIL {case.id}" + (f"\n{detail}" if opts.diff and detail else f"  {detail[:120]}"))

    print(f"\n{'category':<12} {'pass':>5} {'fail':>5} {'skip':>5}")
    tp = tf = ts = 0
    for cat in corpus.ALL_CATEGORIES:
        p, f, s = passed[cat], len(failed[cat]), skipped[cat]
        if p or f or s:
            print(f"{cat:<12} {p:>5} {f:>5} {s:>5}")
        tp, tf, ts = tp + p, tf + f, ts + s
    print(f"{'TOTAL':<12} {tp:>5} {tf:>5} {ts:>5}")

    ran = {c.id for c in todo}
    baseline = corpus.read_passing(PASSING) & ran
    regressions = sorted(baseline - passing_now)
    new = sorted(passing_now - corpus.read_passing(PASSING))
    if new:
        print(f"\n{len(new)} newly passing (run with --update-passing to record): " + ", ".join(new[:20])
              + (" ..." if len(new) > 20 else ""))
    if opts.update_passing:
        if opts.cases or Path(awk[0]).resolve() != corpus.DEFAULT_HAWK.resolve():
            print("--update-passing needs a full run of target/release/hawk", file=sys.stderr)
            return 2
        corpus.write_passing(PASSING, passing_now, "corpus cases")
        print(f"wrote {len(passing_now)} ids to {PASSING.relative_to(corpus.REPO)}")
    if regressions:
        print(f"\nREGRESSIONS ({len(regressions)}): " + ", ".join(regressions))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
