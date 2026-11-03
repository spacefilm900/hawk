#!/usr/bin/env python3
"""Build tests/data/printf_table.tsv: printf results from the C library.

usage: tools/printf_table.py [--cc CC]

Writes a small C program that calls the C library's snprintf for a few
thousand (format, value) pairs, compiles it with the system C compiler,
runs it, and records the answers.  `cargo test` (format.rs) checks hawk's
formatter against every line, so the C library is the reference and
nothing has to be typed in by hand.  Run it again only to change the
cases; the table is committed, so the tests need no C compiler.

Each line is: kind TAB format TAB argument TAB expected output.
  kind f: argument is a double, as 16 hex digits of its IEEE bits;
  kind i: argument is an integer (decimal) passed as long long;
  kind s: argument is a string, hex-encoded.
For kind i the format gets C's `ll` length modifier inserted before the
conversion letter, since AWK formats integers at 64 bits.  Infinities and
NaNs are left out: AWK spells them differently ("+inf") on purpose.
"""
import argparse
import os
import random
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "tests" / "data" / "printf_table.tsv"

FLOAT_FORMATS = (
    ["%e", "%E", "%f", "%F", "%g", "%G"]
    + [f"%.{p}e" for p in (0, 1, 2, 3, 5, 10, 16, 17, 20, 30)]
    + [f"%.{p}f" for p in (0, 1, 2, 3, 5, 10, 20, 30, 60)]
    + [f"%.{p}g" for p in (0, 1, 2, 3, 5, 10, 15, 16, 17, 20, 30)]
    + ["%#g", "%#.3g", "%#.0g", "%#.0e", "%#.0f", "%#.10g", "%#G",
       "%+e", "% f", "%+g", "% g", "%012.3f", "%-12.3e|", "%+012g", "%08.3e",
       "%-+10.2f|", "%010g", "%#012.4g", "%15.7E", "%-20.10g|", "%0+15.3e",
       "%.1000f"]
)

INT_FORMATS = [
    "%d", "%i", "%5d", "%-5d|", "%05d", "%+d", "% d", "%+5d", "%-+5d|",
    "%.3d", "%.0d", "%.d", "%08.3d", "%-8.3d|", "%+.3d", "%020d",
    "%o", "%#o", "%.0o", "%#.0o", "%#5o", "%#.5o", "%08o",
    "%x", "%#x", "%X", "%#X", "%#08x", "%#.0x", "%.4x", "%-#8x|", "%u",
    "%5u", "%.0u", "%c",
]

STR_FORMATS = ["%s", "%5s", "%-5s|", "%.2s", "%.0s", "%8.3s", "%-8.3s|", "%.10s", "%05s"]


def floats():
    vals = [0.0, -0.0, 0.5, 1.5, 2.5, 3.5, -2.5, 0.125, 0.375, 0.625, 1e-5, 1e-4,
            9.9999e-5, 0.00009999995, 123456.5, 999999.5, 9999995.0, 9.5, 99.95,
            0.05, 0.15, 0.25, 0.35, 0.45, 1e15, 1e16, 1e17, 1e21, 1e22, 1e23,
            1.7976931348623157e308, 5e-324, 1e-320, 2.2250738585072014e-308,
            3.141592653589793, 2.718281828459045, 1 / 3, 2 / 3, 1e100, 123.456,
            0.1, 0.2, 0.3, 2.0**53, 2.0**53 + 2, 2.0**64, 2.0**63, 4.35, 1.005,
            2.675, 1e-7, 5e-5, 0.000123456, 100000.0, 1e6, 999999.0, 9.999999e5,
            -1234.5678, 1e300, 1e-300, 0.999999, 0.9999995, 9.9999995, 1.0, 10.0]
    rng = random.Random(20261102)
    for _ in range(150):
        vals.append(rng.uniform(-1, 1) * 10.0 ** rng.randint(-30, 30))
    for _ in range(40):  # exact ties at various places: k / 2^j
        vals.append(rng.randint(0, 10**6) / 2.0 ** rng.randint(1, 12))
    for _ in range(40):  # arbitrary bit patterns, finite ones only
        while True:
            x = struct.unpack("<d", rng.getrandbits(64).to_bytes(8, "little"))[0]
            if x == x and abs(x) != float("inf"):
                break
        vals.append(x)
    return vals


INTS = [0, 1, -1, 7, 8, 9, 10, 65, 97, 255, -255, 256, 12345, -12345, 2**31 - 1,
        -(2**31), 2**32, 2**53, -(2**53), 2**62, -(2**62), 10**15, 999999999]


def c_string(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cc", default="cc")
    opts = ap.parse_args()
    cases = []  # (kind, fmt, arg text for the table, C expression)
    for fmt in FLOAT_FORMATS:
        for x in floats():
            bits = struct.unpack("<Q", struct.pack("<d", x))[0]
            cases.append(("f", fmt, f"{bits:016x}", f"u2d(0x{bits:016x}ULL)"))
    for fmt in INT_FORMATS:
        for n in INTS:
            if fmt == "%c" and not 32 <= n < 127:
                continue
            cfmt = fmt[:-1] + "ll" + fmt[-1] if fmt[-1] != "|" else fmt[:-2] + "ll" + fmt[-2:]
            if fmt == "%c":
                cfmt = fmt
                cases.append(("i", fmt, str(n), f"(int){n}LL"))
            else:
                cases.append(("i", fmt, str(n), f"(long long){n}LL"))
            cases[-1] = cases[-1] + (cfmt,)
    for fmt in STR_FORMATS:
        for s in ["", "a", "ab", "hello", "hello world"]:
            cases.append(("s", fmt, s.encode().hex(), c_string(s)))
    lines = ["#include <stdio.h>", "#include <string.h>",
             "static double u2d(unsigned long long u){double d; memcpy(&d,&u,8); return d;}",
             "static char buf[4096];", "int main(void){"]
    for c in cases:
        kind, fmt, arg, expr = c[:4]
        cfmt = c[4] if len(c) > 4 else fmt
        lines.append(f'snprintf(buf, sizeof buf, {c_string(cfmt)}, {expr});')
        lines.append(f'printf("%s\\t%s\\t%s\\t%s\\n", "{kind}", {c_string(fmt)}, "{arg}", buf);')
    lines.append("return 0;}")
    with tempfile.TemporaryDirectory() as tmp:
        src = os.path.join(tmp, "t.c")
        exe = os.path.join(tmp, "t")
        Path(src).write_text("\n".join(lines) + "\n", encoding="utf-8")
        subprocess.run([opts.cc, "-O0", "-w", "-o", exe, src], check=True)
        out = subprocess.run([exe], check=True, capture_output=True).stdout
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_bytes(out)
    print(f"wrote {len(cases)} cases to {OUT.relative_to(REPO)}")


if __name__ == "__main__":
    sys.exit(main())
