#!/usr/bin/env python3
"""A rough speed comparison of hawk against other awks.

usage: tools/bench.py [--lines N] [--runs N] AWK...

Generates a file of N lines (seeded, so every run is the same), runs a few
typical programs with each awk, checks that they all print the same thing,
and prints the best wall-clock time of each.  Not a rigorous benchmark:
one machine, one file, best of a few runs.
"""
import argparse
import os
import random
import subprocess
import sys
import tempfile
import time

PROGRAMS = [
    ("field sum", ["{ s += $2 } END { print s }"]),
    ("regex filter", ["/[a-c]+[0-9]+x/ { n++ } END { print n }"]),
    ("gsub", ['{ gsub(/a/, "A"); n += length($0) } END { print n }']),
    ("regex FS + arrays", ["-F", "[ ]+", "{ c[$1]++ } END { for (k in c) t += c[k]; print t }"]),
    ("printf", ['{ printf "%5.2f %d %s\\n", $3, $2, $1 }']),
]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--lines", type=int, default=500_000)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("awks", nargs="+")
    opts = ap.parse_args()
    rng = random.Random(1)
    words = ["alpha", "beta", "gamma", "delta", "abc123x", "foo", "bar", "zeta99"]
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
        for _ in range(opts.lines):
            f.write(f"{rng.choice(words)} {rng.randint(1, 1000)} "
                    f"{rng.random() * 100:.4f} {rng.choice(words)}\n")
        data = f.name
    try:
        return run(opts, data)
    finally:
        os.unlink(data)  # about 20 MB at the default size


def run(opts, data):
    print(f"{opts.lines} lines; best of {opts.runs} runs, seconds")
    print(f"{'program':20}" + "".join(f"{a.split('/')[-1]:>12}" for a in opts.awks))
    for name, args in PROGRAMS:
        row, outs = [], set()
        for awk in opts.awks:
            best = None
            for _ in range(opts.runs):
                t = time.perf_counter()
                p = subprocess.run(awk.split() + args + [data], capture_output=True)
                dt = time.perf_counter() - t
                # A failed run is fast and meaningless: stop rather than
                # print its time.
                if p.returncode != 0:
                    err = p.stderr.decode("utf-8", "replace").strip()[:200]
                    print(f"{awk}: {name}: exit status {p.returncode}: {err}", file=sys.stderr)
                    return 1
                best = dt if best is None else min(best, dt)
                outs.add(p.stdout)
            row.append(best)
        flag = "" if len(outs) == 1 else "   (outputs differ!)"
        print(f"{name:20}" + "".join(f"{t:12.2f}" for t in row) + flag)
    return 0


if __name__ == "__main__":
    sys.exit(main())
