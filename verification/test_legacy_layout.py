"""Legacy two-layer installation: the installed path is a launcher that execs
into K's stable slot, so the live service runs from the slot artifact. Repair
must recognize that service as the product, stop it, and restore it."""
import faulthandler
import unittest

from harness import WINDOWS, ReleaseServer, wait_for


@unittest.skipIf(WINDOWS, "the legacy launcher is a POSIX exec shim")
class LegacyLayout(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ReleaseServer()
        for version in ("1.0.0", "1.1.0"):
            cls.server.publish(version)

    @classmethod
    def tearDownClass(cls):
        cls.server.close()

    def setUp(self):
        faulthandler.dump_traceback_later(60, repeat=True)
        self.addCleanup(faulthandler.cancel_dump_traceback_later)

    def test_running_legacy_launcher_install_is_repaired(self):
        machine = self.server.machine()
        self.addCleanup(machine.close)
        machine.json(["install", "--version", "1.0.0"])
        slot = machine.k / "slots/stable/artifact.bin"
        machine.binary.write_text(f'#!/bin/sh\nexec "{slot}" "$@"\n')
        machine.binary.chmod(0o755)
        self.assertTrue(machine.login_start())
        result = machine.json(["install", "--version", "1.1.0"])
        self.assertEqual(result["receipt"]["outcome"], "repaired", result)
        self.assertEqual(machine.self_version(), "1.1.0")
        self.assertTrue(wait_for(machine.live), "the running service must be restored after repair")


if __name__ == "__main__":
    unittest.main()
