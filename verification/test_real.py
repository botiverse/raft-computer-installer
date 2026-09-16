from pathlib import Path
import subprocess
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from real import assert_stopped


class PublishedStatusContract(unittest.TestCase):
    def test_legacy_status_requires_explicit_stopped_report(self):
        machine = SimpleNamespace(binary=Path("/fixture/raft-computer"), env=lambda extra: {})
        unsupported = subprocess.CompletedProcess([], 1, "", "error: unknown option '--json'\n")
        for text, stopped in (("Service:   stopped — run raft-computer start\n", True),
                              ("Service: running\n", False), ("", False)):
            with self.subTest(text=text), patch("real.subprocess.run", side_effect=[
                    unsupported, subprocess.CompletedProcess([], 0, text, "")]):
                if stopped:
                    self.assertEqual(assert_stopped(machine, {}), "legacy-stopped-status")
                else:
                    with self.assertRaises(AssertionError):
                        assert_stopped(machine, {})

    def test_arbitrary_status_failure_is_not_legacy_compatibility(self):
        machine = SimpleNamespace(binary=Path("/fixture/raft-computer"), env=lambda extra: {})
        with patch("real.subprocess.run", return_value=subprocess.CompletedProcess([], 1, "", "status unavailable")) as run:
            with self.assertRaises(AssertionError):
                assert_stopped(machine, {})
            self.assertEqual(run.call_count, 1)
