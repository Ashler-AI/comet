#!/usr/bin/env python3
"""Failures: work starts unprotected; failed renewal leaves work running; cleanup removes another job."""
import importlib.util
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "devbox_keepalive", Path(__file__).parents[1] / "skills/devbox/scripts/keepalive.py"
)
keepalive = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(keepalive)


class WorkHoldChecks(unittest.TestCase):
    def test_initial_failure_prevents_work(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "started"
            command = [sys.executable, "-c", "from pathlib import Path; import sys; Path(sys.argv[1]).touch()", str(output)]
            with patch.object(keepalive, "renew_boat", return_value=False):
                self.assertEqual(keepalive.run(command, "boat", "bx_23456789"), 1)
            self.assertFalse(output.exists())

    def test_failed_renewal_stops_work_before_its_side_effect(self):
        with tempfile.TemporaryDirectory() as directory:
            started, completed = Path(directory) / "started", Path(directory) / "completed"
            command = [sys.executable, "-c", "from pathlib import Path; import sys,time; Path(sys.argv[1]).touch(); time.sleep(30); Path(sys.argv[2]).touch()", str(started), str(completed)]
            calls = 0
            def renew(_sandbox):
                nonlocal calls
                calls += 1
                if calls == 1:
                    return True
                deadline = time.monotonic() + 5
                while not started.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                return False
            with patch.object(keepalive, "renew_boat", side_effect=renew), patch.object(keepalive, "RENEW_SECONDS", 0.05):
                self.assertEqual(keepalive.run(command, "boat", "bx_23456789"), 1)
            self.assertTrue(started.exists())
            self.assertFalse(completed.exists())

    def test_namespace_hold_is_visible_and_only_its_marker_is_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            tasks = Path(directory) / "tasks"
            tasks.mkdir()
            unrelated = tasks / "someone-elses-work"
            unrelated.touch()
            output = Path(directory) / "held"
            command = [sys.executable, "-c", "from pathlib import Path; import sys; tasks=Path(sys.argv[1]); assert len(list(tasks.iterdir())) == 2; Path(sys.argv[2]).touch()", str(tasks), str(output)]
            self.assertEqual(keepalive.run(command, "namespace", "ofpf7g22n4412", tasks), 0)
            self.assertTrue(output.exists())
            self.assertEqual(list(tasks.iterdir()), [unrelated])


if __name__ == "__main__":
    unittest.main()
