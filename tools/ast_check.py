"""Compare hawk's parser with pyawk v1.0's on every corpus program.

For each ``*.awk`` file in tests/corpus, run ``hawk --dump-ast -f FILE`` and
pyawk v1.0's parser (through tools/ast_dump.py) and compare:

* both parse and the trees are identical: ``same``;
* both reject the program: ``both-reject`` (the message is compared too,
  and a different message is listed but not counted as a failure);
* anything else is a mismatch, printed and counted; exit status 1.

pyawk v1.0 is taken from the pyawk repository's ``v1.0`` tag with
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


def extract_pyawk(repo: Path, dest: Path) -> None:
    data = subprocess.run(
        ["git", "-C", str(repo), "archive", "--format=tar", "v1.0", "pyawk"],
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

    same = both_reject = msg_diff = 0
    mismatches = []
    for prog, h, p in results:
        name = prog.relative_to(CORPUS)
        if h == p:
            if h.startswith("ERROR"):
                both_reject += 1
            else:
                same += 1
        elif h.startswith("ERROR") and p.startswith("ERROR"):
            both_reject += 1
            msg_diff += 1
            if args.verbose:
                print(f"message differs: {name}\n  hawk:  {h}\n  pyawk: {p}")
        else:
            mismatches.append(name)
            print(f"MISMATCH {name}\n  hawk:  {h[:400]}\n  pyawk: {p[:400]}")
    print(
        f"{len(results)} programs: {same} same tree, {both_reject} rejected by both "
        f"({msg_diff} with a different message), {len(mismatches)} mismatches"
    )
    return 1 if mismatches else 0


if __name__ == "__main__":
    sys.exit(main())
