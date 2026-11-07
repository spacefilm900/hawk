#!/usr/bin/env python3
"""Live differential and stress checks of hawk against reference awks.

usage: tools/differential.py [--awk CMD] [--ref NAME=CMD ...] [--only SECTION ...]
                             [--scale N] [--jobs N] [--summary FILE]

This is the slow check, which needs reference awks installed. CI runs it as
a separate job (.github/workflows/ci.yml, job ``differential``). By default
it looks for these references on PATH and skips any that are missing:

    mawk            mawk
    onetrue         original-awk, nawk or onetrue-awk
    gawk --posix    gawk --posix
    busybox         busybox awk

Use ``--ref NAME=CMD`` (repeatable) to name the references yourself.

Sections:

* ``stress`` (the GATE). Deterministic programs on large generated inputs
  (stdlib ``random``, fixed seed): field splitting and rebuilding, big
  associative arrays, delete, split/sub/gsub, printf, getline from files,
  many output files with close(), paragraph mode, recursion, NR/FNR over
  several files, and very long records. Every program uses only POSIX
  behaviour. If at least two references produce the same output and exit
  status, hawk must match it, or the section fails. A program where the
  references disagree is reported, not gated.
* ``regex`` (report only). The probe from tools/regex_check.py is run live
  on every pattern of tests/corpus/regex_differential*.json. The report
  counts lines where hawk differs from the unanimous live references, and
  lines where the live references differ from the recorded majority, which
  is drift between reference versions. tools/regex_check.py remains the
  gate for the recorded answers.
* ``golden`` (report only). Each golden corpus case runs under every
  reference, and the report lists where hawk differs from all references
  that agree. The golden .out files, which tools/run_corpus.py gates,
  remain the specification.

Exit status: 1 if a ``stress`` program fails its gate, 2 if hawk or every
reference is missing, otherwise 0. Only the Python standard library is
used.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import corpus  # noqa: E402
import regex_check  # noqa: E402

DEFAULT_REFS = [
    ("mawk", [["mawk"]]),
    ("onetrue", [["original-awk"], ["nawk"], ["onetrue-awk"]]),
    ("gawk --posix", [["gawk", "--posix"]]),
    ("busybox", [["busybox", "awk"]]),
]


def find_refs(spec: list[str]) -> dict[str, list[str]]:
    refs: dict[str, list[str]] = {}
    if spec:
        for s in spec:
            name, _, cmd = s.partition("=")
            argv = shlex.split(cmd)
            if argv and shutil.which(argv[0]):
                refs[name] = argv
            else:
                print(f"differential: skipping {name}: {cmd!r} not found", file=sys.stderr)
        return refs
    for name, candidates in DEFAULT_REFS:
        for argv in candidates:
            if shutil.which(argv[0]) and _works(argv):
                refs[name] = argv
                break
        else:
            print(f"differential: skipping {name}: not installed", file=sys.stderr)
    return refs


def _works(argv: list[str]) -> bool:
    try:
        p = subprocess.run([*argv, "BEGIN { print 1 + 1 }"], capture_output=True, timeout=10,
                           stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired):
        return False
    return p.stdout == b"2\n"


# --- stress -------------------------------------------------------------------

def gen_data(root: Path, scale: int) -> None:
    rnd = random.Random(20261107)
    words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
             "lambda", "mu", "nu", "xi", "omicron", "pi", "rho", "sigma", "tau", "upsilon"]
    with open(root / "words.txt", "w") as f:
        for _ in range(20000 * scale):
            f.write(" ".join(rnd.choice(words) + str(rnd.randrange(50)) for _ in range(rnd.randrange(1, 12))) + "\n")
    with open(root / "nums.txt", "w") as f:
        for i in range(20000 * scale):
            f.write(f"{i}\t{rnd.randrange(-10**6, 10**6)}\t{rnd.randrange(10**4)}.{rnd.randrange(100):02d}"
                    f"\t{rnd.choice(['0x1A', '1e3', ' 42 ', '+7', '-0', '.5', 'abc', ''])}\n")
    with open(root / "csv.txt", "w") as f:
        for i in range(10000 * scale):
            f.write(",".join(str(rnd.randrange(1000)) for _ in range(rnd.randrange(1, 40))) + "\n")
    with open(root / "paras.txt", "w") as f:
        for i in range(3000 * scale):
            f.write("\n" * rnd.randrange(1, 4))
            for _ in range(rnd.randrange(1, 6)):
                f.write(" ".join(rnd.choice(words) for _ in range(rnd.randrange(1, 6))) + "\n")
    with open(root / "long.txt", "w") as f:
        for i in range(4):
            f.write(("x" * 997 + "yz ") * (300 * scale) + "\n")
    for k in range(3):
        with open(root / f"part{k}.txt", "w") as f:
            for i in range(rnd.randrange(1000, 3000) * scale):
                f.write(f"p{k} {i} {rnd.randrange(100)}\n")


# (name, program, operands, options)
STRESS: list[tuple[str, str, list[str], list[str]]] = [
    ("word-frequency",
     '{ for (i = 1; i <= NF; i++) c[$i]++ } END { cmd = "LC_ALL=C sort -k2"; for (w in c) print c[w], w | cmd; close(cmd) }',
     ["words.txt"], []),
    ("numeric-sums",
     '{ a += $2; b += $3; if ($4 + 0 == $4) n4++; d[$4] += 1 } END { printf "%d %.2f %d\\n", a, b, n4;'
     ' cmd = "LC_ALL=C sort"; for (k in d) print "[" k "]", d[k] | cmd; close(cmd) }',
     ["nums.txt"], ["-F", "\t"]),
    ("numeric-compare",
     '{ if ($4 < 10) lt++; else ge++; if ($2 > $3) g++ } END { print lt+0, ge+0, g+0 }',
     ["nums.txt"], ["-F", "\t"]),
    ("field-rebuild",
     'BEGIN { OFS = "-" } { $(NF + 2) = NR; t = t + NF; $1 = $1; if (NR % 997 == 0) print } END { print t, NR }',
     ["csv.txt"], ["-F", ","]),
    ("nf-shrink",
     '{ n = NF; NF = int(NF / 2); s += length($0); if (NR % 1999 == 0) print n, NF, $0 } END { print s }',
     ["csv.txt"], ["-F", ","]),
    ("split-sub-gsub",
     '{ n = split($0, A, /[0-9]+/); m += n; k += gsub(/a+/, "<&>"); j += sub(/^[^ ]+/, "[&]"); h = h + length($0) }'
     ' NR % 1501 == 0 { print } END { print m, k, j, h }',
     ["words.txt"], []),
    ("big-array-delete",
     '{ for (i = 1; i <= NF; i++) a[NR "," i] = $i } END { n = 0; for (k in a) n++; for (k in a) if (a[k] % 3 == 0) delete a[k];'
     ' m = 0; for (k in a) m++; print n, m, length(a); delete a; print length(a) }',
     ["csv.txt"], ["-F", ","]),
    ("printf-formats",
     '{ printf "%d|%5d|%-5d|%05d|%o|%x|%X|%e|%.3g|%10.4f|%s|%.3s|%c\\n", $1, $1, $1, $1, $1, $1, $1, $2, $2, $3, $4, $4, 65 + $1 % 26 }',
     ["nums.txt"], ["-F", "\t"]),
    ("getline-files",
     'BEGIN { while ((getline line < "part0.txt") > 0) n++; close("part0.txt");'
     ' while ((getline < "part1.txt") > 0) s += $3; print n, s, NR; f = "part2.txt";'
     ' while ((r = getline x < f) > 0) c++; print c, r, close(f) }',
     [], []),
    ("many-output-files",
     '{ f = "out" ($1 % 50) ".txt"; print > f; if (NR % 7 == 0) close(f) }'
     ' END { for (i = 0; i < 50; i++) { f = "out" i ".txt"; close(f); n = 0; while ((getline l < f) > 0) n++; close(f); t += n } print t, NR }',
     ["nums.txt"], []),
    ("paragraph-mode",
     'BEGIN { RS = "" } { n += NF; r++; if (NR % 211 == 0) print NR ": " $1 "/" $NF } END { print r, n }',
     ["paras.txt"], []),
    ("paragraph-mode-fs",
     'BEGIN { RS = ""; FS = " " } { for (i = 1; i <= NF; i++) w[$i]++ } END { cmd = "LC_ALL=C sort"; for (k in w) print k, w[k] | cmd; close(cmd) }',
     ["paras.txt"], []),
    ("recursion",
     'function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2) }'
     ' function depth(n) { return n == 0 ? 0 : 1 + depth(n - 1) }'
     ' function ack(m, n) { if (m == 0) return n + 1; if (n == 0) return ack(m - 1, 1); return ack(m - 1, ack(m, n - 1)) }'
     ' BEGIN { print fib(22), depth(5000), ack(2, 300) }',
     [], []),
    ("nr-fnr-multifile",
     'FNR == 1 { files++; print FILENAME, NR } { s[FILENAME] += $3 } END { print files, NR, FNR; print s["part0.txt"] + s["part1.txt"] + s["part2.txt"] }',
     ["part0.txt", "part1.txt", "part2.txt"], []),
    ("long-records",
     '{ print NR, length($0), NF, substr($0, 1000, 5), index($0, "yz"), match($0, /z x+y/), RSTART, RLENGTH; n = gsub(/yz/, "Q"); print n, length($0) }',
     ["long.txt"], []),
    ("string-building",
     'BEGIN { for (i = 0; i < 200000; i++) s = s "ab"; print length(s), substr(s, 199999, 4); t = s; gsub(/b/, "", t); print length(t) }',
     [], []),
    ("conversions",
     'BEGIN { CONVFMT = "%.4g"; OFMT = "%.2f" } { x = $2 / 7; k = x ""; y[x] = 1; print x, k, ($3 * 1), int($2 / 3) } END { n = 0; for (z in y) n++; print n }',
     ["nums.txt"], ["-F", "\t"]),
    ("uninitialized-and-compare",
     '{ if ($5 == 0) z++; if ($5 == "") e++; if (u == 0 && u == "") b++ } END { print z, e, b, length(u), u + 0 }',
     ["nums.txt"], ["-F", "\t"]),
]


def run_awk(argv: list[str], prog: str, opts: list[str], operands: list[str], cwd: Path,
            timeout: float) -> tuple[bytes, int | None, float]:
    t = time.monotonic()
    try:
        p = subprocess.run([*argv, *opts, prog, *operands], cwd=cwd, capture_output=True, timeout=timeout,
                           stdin=subprocess.DEVNULL, env=dict(os.environ, LC_ALL="C"))
        return p.stdout, p.returncode, time.monotonic() - t
    except subprocess.TimeoutExpired:
        return b"<timeout>", None, time.monotonic() - t


def section_stress(hawk: list[str], refs: dict[str, list[str]], scale: int, report: list[str]) -> int:
    failures = 0
    with tempfile.TemporaryDirectory(prefix="hawk-stress-") as tmp:
        root = Path(tmp) / "data"
        root.mkdir()
        gen_data(root, scale)
        report.append("## stress (gate)\n")
        report.append("| program | references agreeing | hawk | hawk s | " + " | ".join(f"{n} s" for n in refs) + " |")
        report.append("|---|---|---|---:|" + "---:|" * len(refs))
        for name, prog, operands, opts in STRESS:
            results = {}
            for rname, argv in refs.items():
                work = Path(tmp) / f"w-{rname.replace(' ', '_')}"
                if work.exists():
                    shutil.rmtree(work)
                shutil.copytree(root, work)
                results[rname] = run_awk(argv, prog, opts, operands, work, 300)
            work = Path(tmp) / "w-hawk"
            if work.exists():
                shutil.rmtree(work)
            shutil.copytree(root, work)
            h_out, h_rc, h_t = run_awk(hawk, prog, opts, operands, work, 300)
            votes = Counter((out, rc) for out, rc, _ in results.values() if rc is not None)
            top = votes.most_common(1)
            agree = [r for r, (o, c, _) in results.items() if top and (o, c) == top[0][0]]
            if top and top[0][1] >= 2:
                ok = (h_out, h_rc) == top[0][0]
                verdict = "PASS" if ok else "FAIL"
                if not ok:
                    failures += 1
                    print(f"stress FAIL {name}: hawk differs from {', '.join(agree)}", file=sys.stderr)
                    _show_diff(top[0][0][0], h_out)
            else:
                verdict = "no majority (not gated)"
            print(f"stress {name}: {verdict} (agree: {', '.join(agree) or '-'})")
            times = " | ".join(f"{results[r][2]:.2f}" for r in refs)
            report.append(f"| {name} | {', '.join(agree) or '-'} | {verdict} | {h_t:.2f} | {times} |")
    report.append("")
    return failures


def _show_diff(want: bytes, got: bytes) -> None:
    import difflib
    a = want.decode("utf-8", "replace").splitlines()
    b = got.decode("utf-8", "replace").splitlines()
    for line in list(difflib.unified_diff(a, b, "references", "hawk", lineterm="", n=1))[:30]:
        print("    " + line, file=sys.stderr)


# --- regex --------------------------------------------------------------------

def section_regex(hawk: list[str], refs: dict[str, list[str]], jobs: int, report: list[str]) -> None:
    records = (json.loads(regex_check.RECORD.read_text(encoding="utf-8"))["records"]
               + json.loads(regex_check.RECORD_UTF8.read_text(encoding="utf-8"))["records"])
    voters_c = [r for r in ("mawk", "onetrue", "gawk --posix") if r in refs]
    voters_u = [r for r in ("onetrue", "gawk --posix") if r in refs]

    def one(rec):
        voters = voters_u if regex_check.table_of(rec) == "utf8" else voters_c
        outs = {v: regex_check.run_probe(refs[v], rec) for v in voters}
        h = regex_check.run_probe(hawk, rec)
        return rec, voters, outs, h

    hawk_vs_unanimous = drift = lines = 0
    examples: list[str] = []
    with ThreadPoolExecutor(jobs) as pool:
        for rec, voters, outs, h in pool.map(one, records):
            for i, inp in enumerate(rec["inputs"]):
                lines += 1
                answers = {outs[v][i] if i < len(outs[v]) else None for v in voters}
                if len(voters) >= 2 and len(answers) == 1:
                    live = answers.pop()
                    hv = h[i] if i < len(h) else None
                    if hv != live:
                        hawk_vs_unanimous += 1
                        if len(examples) < 15:
                            examples.append(f"/{rec['pattern']}/ on {inp!r}: refs {live!r}, hawk {hv!r}")
                    if rec["expected"][i] is not None and live != rec["expected"][i]:
                        drift += 1
    report.append("## regex (report only)\n")
    report.append(f"- voters: C table {', '.join(voters_c) or 'none'}; UTF-8 table {', '.join(voters_u) or 'none'}")
    report.append(f"- probe lines: {lines}")
    report.append(f"- hawk differs from unanimous live references: {hawk_vs_unanimous}")
    report.append(f"- live references differ from the recorded majority (reference-version drift): {drift}")
    if examples:
        report.append("\n```\n" + "\n".join(examples) + "\n```")
    report.append("")
    print(f"regex: {lines} lines; hawk vs unanimous refs: {hawk_vs_unanimous}; reference drift: {drift}")


# --- golden -------------------------------------------------------------------

def section_golden(hawk: list[str], refs: dict[str, list[str]], jobs: int, report: list[str]) -> None:
    cases = [c for c in corpus.discover(corpus.CATEGORIES) if c.status != "skip"]

    def norm(case, out):
        return sorted(out.splitlines()) if case.status == "sorted" else out

    def one(case):
        res = {}
        for name, argv in {"hawk": hawk, **refs}.items():
            try:
                p = corpus.run_case(case, argv, timeout=30)
                res[name] = (norm(case, p.stdout), p.returncode)
            except subprocess.TimeoutExpired:
                res[name] = ("<timeout>", None)
        return case, res

    # BusyBox is left out of the vote when two or more other references are present: it
    # differs from the rest often enough that requiring it would hide most signal.
    voters = [r for r in refs if r != "busybox"]
    if len(voters) < 2:
        voters = list(refs)
    differs: list[str] = []
    with ThreadPoolExecutor(jobs) as pool:
        for case, res in pool.map(one, cases):
            ref_answers = {res[r] for r in voters}
            if len(voters) >= 2 and len(ref_answers) == 1 and res["hawk"] not in ref_answers:
                differs.append(case.id)
    report.append("## golden corpus (report only)\n")
    report.append(f"- cases run: {len(cases)}; voters: {', '.join(voters)}")
    report.append(f"- hawk differs from all references, which agree with each other: {len(differs)}"
                  + (f" ({', '.join(differs[:40])})" if differs else ""))
    report.append("- The golden .out files are the specification; see tools/run_corpus.py.\n")
    print(f"golden: {len(cases)} cases; hawk vs unanimous refs: {len(differs)} {differs[:20]}")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--awk", default=str(corpus.DEFAULT_HAWK))
    ap.add_argument("--ref", action="append", default=[], metavar="NAME=CMD")
    ap.add_argument("--only", nargs="*", choices=["stress", "regex", "golden"])
    ap.add_argument("--scale", type=int, default=1, help="multiply stress input sizes")
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    ap.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY"),
                    help="append a Markdown report here (default: $GITHUB_STEP_SUMMARY)")
    opts = ap.parse_args()
    hawk = shlex.split(opts.awk)
    if "/" in hawk[0]:
        if not Path(hawk[0]).exists():
            print(f"differential: {hawk[0]} not found (run `cargo build --release` first)", file=sys.stderr)
            return 2
        hawk[0] = str(Path(hawk[0]).resolve())
    refs = find_refs(opts.ref)
    if not refs:
        print("differential: no reference awk found", file=sys.stderr)
        return 2
    print("references: " + "; ".join(f"{n} = {' '.join(a)}" for n, a in refs.items()))
    report = ["# hawk differential and stress check\n",
              "References: " + ", ".join(f"`{n}` (`{' '.join(a)}`)" for n, a in refs.items()) + "\n"]
    sections = opts.only or ["stress", "regex", "golden"]
    failures = 0
    if "stress" in sections:
        failures += section_stress(hawk, refs, opts.scale, report)
    if "regex" in sections:
        section_regex(hawk, refs, opts.jobs, report)
    if "golden" in sections:
        section_golden(hawk, refs, opts.jobs, report)
    if opts.summary:
        with open(opts.summary, "a", encoding="utf-8") as f:
            f.write("\n".join(report) + "\n")
    if failures:
        print(f"\nSTRESS GATE FAILED: {failures} program(s)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
