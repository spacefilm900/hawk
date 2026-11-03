"""Compare hawk's parser with pyawk v1.1's on every corpus program.

For each ``*.awk`` file in tests/corpus, run ``hawk --dump-ast -f FILE`` and
pyawk v1.1's parser (through tools/ast_dump.py) and compare:

* both parse and the trees are identical: ``same``;
* both reject the program: ``both-reject`` (the message is compared too,
  and a different message is listed but not counted as a failure);
* a program listed in ``DELIBERATE`` (hawk differs from pyawk v1.1 on
  purpose, see DESIGN.md) must be accepted by hawk and rejected by pyawk:
  ``deliberate``;
* anything else is a mismatch, printed and counted; exit status 1.

pyawk v1.1 is taken from the pyawk repository's ``v1.1`` tag with
``git archive`` into a temporary directory (nothing in that repository is
changed).  Usage::

    python3 tools/ast_check.py [--pyawk-repo DIR] [-v]
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import tarfile
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from corpus import CORPUS, DEFAULT_HAWK, REPO


# The pyawk release whose parser hawk is compared with.
PYAWK_TAG = "v1.1"

# Programs where hawk's parse differs from pyawk v1.1 on purpose (hawk
# accepts, pyawk rejects).  Each entry names its DESIGN.md section.
DELIBERATE: set[str] = set()
# (pyawk v1.0 rejected "cmd | getline x + 1" in gawk/getline.awk; v1.1
# parses it the way hawk does, so that entry is gone.)

# Programs that pyawk v1.1 parses differently because hawk has not caught
# up yet (the v1.1 port in progress on branch ``enhancements``).  They are
# listed, not counted as failures; each stage empties its part.
PENDING: set[str] = set()  # empty: stages 1 and 2 are done


def extract_pyawk(repo: Path, dest: Path) -> None:
    data = subprocess.run(
        ["git", "-C", str(repo), "archive", "--format=tar", PYAWK_TAG, "pyawk"],
        check=True,
        capture_output=True,
    ).stdout
    tar = dest / "pyawk.tar"
    tar.write_bytes(data)
    with tarfile.open(tar) as t:
        t.extractall(dest, filter="data")


def hawk_dump(hawk: Path, prog: Path) -> str:
    r = subprocess.run([str(hawk), "--dump-ast", "-f", str(prog)], capture_output=True)
    if r.returncode == 0:
        return r.stdout.decode("ascii").rstrip("\n")
    msg = r.stderr.decode("utf-8", "replace").strip().removeprefix("hawk: ")
    return f"ERROR: {msg}"


def pyawk_dump(pyawk_dir: Path, prog: Path) -> str:
    r = subprocess.run(
        [sys.executable, str(REPO / "tools" / "ast_dump.py"), str(pyawk_dir), str(prog)],
        capture_output=True,
        check=True,
    )
    return r.stdout.decode("ascii", "backslashreplace").rstrip("\n")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--pyawk-repo", type=Path, default=REPO.parent / "pyawk")
    ap.add_argument("--hawk", type=Path, default=DEFAULT_HAWK)
    ap.add_argument("-v", "--verbose", action="store_true", help="list message differences too")
    args = ap.parse_args()

    programs = sorted(CORPUS.rglob("*.awk"))
    with tempfile.TemporaryDirectory() as tmp:
        pyawk_dir = Path(tmp)
        extract_pyawk(args.pyawk_repo, pyawk_dir)

        def compare(prog: Path) -> tuple[Path, str, str]:
            return prog, hawk_dump(args.hawk, prog), pyawk_dump(pyawk_dir, prog)

        with ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(compare, programs))

    same = both_reject = msg_diff = deliberate = pending = 0
    mismatches = []
    for prog, h, p in results:
        name = prog.relative_to(CORPUS)
        if str(name) in DELIBERATE and not h.startswith("ERROR") and p.startswith("ERROR"):
            deliberate += 1
        elif h == p:
            if h.startswith("ERROR"):
                both_reject += 1
            else:
                same += 1
        elif h.startswith("ERROR") and p.startswith("ERROR"):
            both_reject += 1
            msg_diff += 1
            if args.verbose:
                print(f"message differs: {name}\n  hawk:  {h}\n  pyawk: {p}")
        elif str(name) in PENDING:
            pending += 1
            if args.verbose:
                print(f"pending: {name}")
        else:
            mismatches.append(name)
            print(f"MISMATCH {name}\n  hawk:  {h[:400]}\n  pyawk: {p[:400]}")
    print(
        f"{len(results)} programs: {same} same tree, {both_reject} rejected by both "
        f"({msg_diff} with a different message), {deliberate} deliberate differences, "
        f"{pending} pending, {len(mismatches)} mismatches"
    )
    fixed = sorted(n for n in PENDING if n not in {str(m) for m in mismatches} and
                   any(str(r[0].relative_to(CORPUS)) == n and r[1] == r[2] for r in results))
    if fixed:
        print("no longer pending (remove from PENDING): " + ", ".join(fixed))
    return 1 if mismatches else 0


if __name__ == "__main__":
    sys.exit(main())
