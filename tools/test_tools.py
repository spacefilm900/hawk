#!/usr/bin/env python3
"""Unit tests for the Python harness itself (run by tools/regress.sh).

Run with `python3 tools/test_tools.py`.
"""

import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import corpus  # noqa: E402  (needs the sys.path line above)


class ExtraEnv(unittest.TestCase):
    """`Case.extra_env` reads `<name>.environ`: one NAME=value per line."""

    def case_with(self, text: str) -> corpus.Case:
        directory = Path(tempfile.mkdtemp(prefix="hawk-test-tools-"))
        self.addCleanup(shutil.rmtree, directory)
        (directory / "x.environ").write_text(text, encoding="utf-8")
        return corpus.Case("golden", "x", directory)

    def test_valid_lines(self):
        case = self.case_with("A=1\n\nB=\nC= x=y \n")
        self.assertEqual(case.extra_env(), {"A": "1", "B": "", "C": " x=y "})

    def test_line_without_equals_is_an_error(self):
        # Silently reading `LC_ALL` as LC_ALL="" would change the locale.
        with self.assertRaisesRegex(ValueError, "LC_ALL"):
            self.case_with("A=1\nLC_ALL\n").extra_env()

    def test_empty_name_is_an_error(self):
        with self.assertRaisesRegex(ValueError, "=v"):
            self.case_with("=v\n").extra_env()


if __name__ == "__main__":
    unittest.main()
