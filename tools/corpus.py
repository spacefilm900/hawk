"""Shared helpers for hawk's external test runners (tools/*.py).

hawk itself is a Rust program with no dependencies; these Python scripts
are only test tooling.  They drive the compiled binary (by default
``target/release/hawk``) from outside, exactly as pyawk's runners drove
pyawk, so the same corpus checks both.

Corpus layout (copied from pyawk v1.0's ``awk_tests/``):

* ``tests/corpus/golden/<category>/<name>.{awk,in,out[,args]}``: the golden
  corpus.  Each case runs as ``hawk [args] -f NAME.awk NAME.in`` from the
  category directory; stdout must equal ``NAME.out`` byte for byte.
  ``golden/OVERRIDES`` lists the hand-corrected ``.out`` files.
* ``tests/corpus/onetrue/`` and ``tests/corpus/gawk/``: the suites imported
  from onetrue-awk and GNU awk.  Extra files: ``NAME.operands`` (replaces
  the single ``NAME.in`` operand, shell syntax), ``NAME.stdin`` (fed on
  standard input; otherwise stdin is empty) and ``MANIFEST.tsv`` (status
  per case: ``run``, ``sorted`` = compare lines order-blind, ``skip``).
* ``tests/corpus/regex_differential.json``: for each regex pattern, the
  majority answer of mawk, onetrue-awk and gawk --posix to a probe program
  (see tools/regex_check.py).

Progress tracking: ``tests/corpus/PASSING`` lists every case hawk passed
at the last ``--update-passing`` run.  A normal run fails (exit 1) only
if one of those cases now fails -- a *regression* -- so the suite stays
green while the port is incomplete, and a newly passing case is reported
so the list can be updated.
"""

from __future__ import annotations

import os
import shlex
import subprocess
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CORPUS = REPO / "tests" / "corpus"
GOLDEN = CORPUS / "golden"
CATEGORIES = ("basic", "fields", "patterns", "control", "arrays", "builtins", "functions", "ranges", "one_liners")
IMPORTED = {"onetrue": CORPUS / "onetrue", "gawk": CORPUS / "gawk"}
ALL_CATEGORIES = (*CATEGORIES, *IMPORTED)
DEFAULT_HAWK = REPO / "target" / "release" / "hawk"


def category_dir(cat: str) -> Path:
    return IMPORTED.get(cat, GOLDEN / cat)


@dataclass(frozen=True)
class Case:
    category: str
    name: str
    directory: Path
    status: str = "run"  # "run", "sorted" or "skip"
    reason: str = ""

    @property
    def id(self) -> str:
        return f"{self.category}/{self.name}"

    def path(self, ext: str) -> Path:
        return self.directory / f"{self.name}{ext}"

    def extra_args(self) -> list[str]:
        p = self.path(".args")
        return shlex.split(p.read_text()) if p.exists() else []

    def operands(self) -> list[str]:
        p = self.path(".operands")
        return shlex.split(p.read_text()) if p.exists() else [f"{self.name}.in"]

    def stdin_data(self) -> bytes | None:
        p = self.path(".stdin")
        return p.read_bytes() if p.exists() else None

    def command(self, awk: list[str]) -> list[str]:
        return [*awk, *self.extra_args(), "-f", f"{self.name}.awk", *self.operands()]

    def expected(self) -> bytes:
        return self.path(".out").read_bytes()

    def matches(self, stdout: bytes) -> bool:
        expected = self.expected()
        if self.status == "sorted":
            return sorted(stdout.splitlines(keepends=True)) == sorted(expected.splitlines(keepends=True))
        return stdout == expected


def read_manifest(directory: Path) -> dict[str, tuple[str, str]]:
    p = directory / "MANIFEST.tsv"
    result: dict[str, tuple[str, str]] = {}
    if not p.exists():
        return result
    for line in p.read_text().splitlines():
        if not line or line.startswith("#") or line.startswith("name\t"):
            continue
        name, status, reason, *_rest = line.split("\t")
        result[name] = (status, reason)
    return result


def discover(categories: tuple[str, ...] = ALL_CATEGORIES) -> list[Case]:
    cases: list[Case] = []
    for cat in categories:
        d = category_dir(cat)
        if not d.is_dir():
            continue
        manifest = read_manifest(d)
        for awk_file in sorted(d.glob("*.awk")):
            status, reason = manifest.get(awk_file.stem, ("run", ""))
            cases.append(Case(cat, awk_file.stem, d, status, reason))
    return cases


def run_case(case: Case, awk: list[str], timeout: float = 20.0) -> subprocess.CompletedProcess[bytes]:
    """Run one case (bytes in, bytes out) with LC_ALL=C, like pyawk's harness."""
    env = dict(os.environ)
    env["LC_ALL"] = "C"
    data = case.stdin_data()
    return subprocess.run(
        case.command(awk),
        cwd=case.directory,
        capture_output=True,
        timeout=timeout,
        env=env,
        input=data,
        stdin=subprocess.DEVNULL if data is None else None,
    )


def read_passing(path: Path) -> set[str]:
    if not path.exists():
        return set()
    return {ln.partition("#")[0].strip() for ln in path.read_text().splitlines()} - {""}


def write_passing(path: Path, ids: set[str], what: str) -> None:
    header = f"# {what} hawk passes; written by --update-passing (one id per line)\n"
    path.write_text(header + "".join(i + "\n" for i in sorted(ids)))
