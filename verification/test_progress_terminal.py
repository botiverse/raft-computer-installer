"""Download progress in a terminal is one line redrawn in place (a bar plus a
percentage); redirected output keeps one plain line per 10%."""
import faulthandler
import os
import subprocess
import unittest

from harness import WINDOWS, ReleaseServer


@unittest.skipIf(WINDOWS, "pty is POSIX-only")
class TerminalProgress(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ReleaseServer()
        cls.server.publish("1.0.0")

    @classmethod
    def tearDownClass(cls):
        cls.server.close()

    def setUp(self):
        faulthandler.dump_traceback_later(60, repeat=True)
        self.addCleanup(faulthandler.cancel_dump_traceback_later)
        self.machine = self.server.machine()
        self.addCleanup(self.machine.close)

    def test_terminal_gets_one_redrawn_bar(self):
        import pty  # POSIX-only; a module-level import breaks discovery on Windows

        primary, secondary = pty.openpty()
        try:
            process = subprocess.Popen(
                self.machine.command(["install", "--version", "1.0.0"], None, False),
                env=self.machine.env(None),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=secondary,
            )
            os.close(secondary)
            chunks = []
            while True:
                try:
                    chunk = os.read(primary, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                chunks.append(chunk)
            process.wait(timeout=180)
        finally:
            os.close(primary)
        terminal = b"".join(chunks).decode("utf-8", "replace")
        self.assertEqual(process.returncode, 0, terminal)
        self.assertIn("\rDownloading Raft Computer [" + "#" * 30 + "] 100%", terminal)
        self.assertIn("\rDownloading Raft Computer support file [" + "#" * 30 + "] 100%", terminal)
        self.assertNotIn("Downloading Raft Computer: 10%", terminal)

    def test_redirected_output_keeps_plain_lines(self):
        result = self.machine.run(["install", "--version", "1.0.0"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Downloading Raft Computer: 100%", result.stderr)
        self.assertNotIn("\r", result.stderr)


if __name__ == "__main__":
    unittest.main()
