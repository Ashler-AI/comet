"""Run with python3 -m unittest discover -s skills/devbox/scripts -p 'test_*.py'."""
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import keepalive


class ProtectionTest(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux" and hasattr(os, "waitid"), "Requires Linux process-group ownership")
    def test_namespace_marker_exists_only_during_work(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            unrelated = directory / "unrelated"
            unrelated.touch()
            command = [sys.executable, "-c", "from pathlib import Path; import sys; assert len(list(Path(sys.argv[1]).iterdir())) == 2", root]
            self.assertEqual(keepalive.run(command, "namespace", "abcdefgh23456", directory), 0)
            self.assertEqual(list(directory.iterdir()), [unrelated])

    def test_failed_boat_admission_never_starts_work(self):
        with patch.object(keepalive, "renew_boat", return_value=False), patch.object(keepalive.subprocess, "Popen") as spawn:
            self.assertEqual(keepalive.run(["must-not-run"], "boat", "bx_23456789"), 1)
            spawn.assert_not_called()


if __name__ == "__main__":
    unittest.main()
