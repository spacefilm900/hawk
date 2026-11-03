"""Print pyawk's parse tree of an AWK program in hawk's canonical dump format.

Used by tools/ast_check.py to compare hawk's parser with pyawk v1.1's.
The format (see ``dump_program`` in src/ast.rs): ``Node(field, ...)`` with
the dataclass fields in order; strings quoted with ``\\xNN`` escapes for
bytes outside printable ASCII; integral numbers below 1e15 as integers,
other numbers as ``f:`` plus the hex bits of the double; ``None``/``True``/
``False``; ``Program.functions`` as the list of its values.

Usage: python3 tools/ast_dump.py PYAWK_DIR PROGRAM_FILE
Prints the dump, or ``ERROR: <message>`` for a syntax error.
"""

from __future__ import annotations

import dataclasses
import struct
import sys


def quoted(s: str) -> str:
    out = ['"']
    # pyawk v1.1 in the C locale holds byte N as the character U+00NN
    # (Latin-1), both for source text and for escapes like "\303".
    for b in s.encode("latin-1"):
        if b == 0x22:
            out.append('\\"')
        elif b == 0x5C:
            out.append("\\\\")
        elif 0x20 <= b <= 0x7E:
            out.append(chr(b))
        else:
            out.append(f"\\x{b:02x}")
    out.append('"')
    return "".join(out)


def num(v: float) -> str:
    if v == v and abs(v) != float("inf") and v == int(v) and abs(v) < 1e15:
        return str(int(v))
    return "f:" + struct.pack(">d", v).hex()


def dump(x: object) -> str:
    if x is None:
        return "None"
    if isinstance(x, bool):
        return "True" if x else "False"
    if isinstance(x, float):
        return num(x)
    if isinstance(x, str):
        return quoted(x)
    if isinstance(x, list):
        return "[" + ", ".join(dump(i) for i in x) + "]"
    if isinstance(x, dict):
        return dump(list(x.values()))
    if dataclasses.is_dataclass(x):
        fields = [getattr(x, f.name) for f in dataclasses.fields(x)]
        return type(x).__name__ + "(" + ", ".join(dump(f) for f in fields) + ")"
    raise TypeError(f"cannot dump {x!r}")


def main() -> int:
    sys.path.insert(0, sys.argv[1])
    from pyawk.errors import AwkError
    from pyawk.parser import parse

    # pyawk v1.1's CLI decodes the program with the locale's codec, which
    # is Latin-1 in the C locale (one character per byte).
    with open(sys.argv[2], "rb") as f:
        source = f.read().decode("latin-1")
    try:
        print(dump(parse(source)))
    except AwkError as exc:
        print(f"ERROR: {exc}")
    except RecursionError:
        print("ERROR: recursion")
    return 0


if __name__ == "__main__":
    sys.exit(main())
