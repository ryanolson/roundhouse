# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Stdlib-only guard for run_local.py's usage parsing.

Run: python3 -m unittest use-cases/cache-aware-routing/test_run_local.py
"""
import contextlib
import io
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_local  # noqa: E402


def _stub(usage):
    return lambda _system, _conversation: {"text": "x", "usage": usage, "error": None, "bytes_sent": 1}


class UsageParsing(unittest.TestCase):
    def _run(self, usage):
        original = run_local.chat_turn
        run_local.chat_turn = _stub(usage)
        try:
            with contextlib.redirect_stdout(io.StringIO()):
                return run_local.run_session("s", "system", ["q"])
        finally:
            run_local.chat_turn = original

    def test_null_prompt_tokens_details_counts_as_uncached(self):
        # A server with prompt-token details disabled sends the key as an
        # explicit null; a `.get(..., {})` default does not cover that and
        # the whole replay died with AttributeError on turn 1.
        totals = self._run({"prompt_tokens": 10, "completion_tokens": 1, "prompt_tokens_details": None})
        self.assertEqual((totals["input"], totals["cached"]), (10, 0))

    def test_reported_cached_tokens_are_read(self):
        # Control: the parse is not simply zeroing everything.
        totals = self._run(
            {"prompt_tokens": 10, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 7}}
        )
        self.assertEqual((totals["input"], totals["cached"]), (10, 7))


if __name__ == "__main__":
    unittest.main()
