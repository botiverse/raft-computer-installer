"""Legacy two-layer installation: the installed path is a launcher that execs
into K's stable slot, so the live service runs from the slot artifact. Repair
must recognize that service as the product, stop it, and restore it."""
import faulthandler
import json
import subprocess
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

    # The product CLI on a legacy layout is the launcher exec'd into the slot,
    # so the installer's waiting caller runs from the slot artifact. The
    # declared waiting CLI must be attested there, not only at the installed
    # path, or every attended upgrade on such a machine is rejected with
    # "waiting caller is not the installed immediate parent" and a rerun hint
    # that cannot help.
    def test_running_legacy_launcher_waiting_cli_upgrade_is_accepted(self):
        machine = self.server.machine()
        self.addCleanup(machine.close)
        machine.json(["install", "--version", "1.0.0"])
        slot = machine.k / "slots/stable/artifact.bin"
        machine.binary.write_text(f'#!/bin/sh\nexec "{slot}" "$@"\n')
        machine.binary.chmod(0o755)
        self.assertTrue(machine.login_start())
        before = machine.live()
        parent = subprocess.Popen([str(machine.binary), "upgrade", "--version", "1.1.0", "--json"],
            env=machine.env({"RCI_FIXTURE_INSTALLER": str(self.server.installer),
                "RAFT_COMPUTER_INSTALLER_CALLER": "waiting-cli-v1"}),
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        stdout, stderr = parent.communicate(timeout=180)
        self.assertEqual(parent.returncode, 0, stdout + stderr)
        self.assertEqual(json.loads(stdout)["receipt"]["outcome"], "repaired", stdout)
        self.assertEqual(machine.self_version(), "1.1.0")
        self.assertTrue(wait_for(machine.live), "the running service must be restored after repair")
        # The product CLI returned the installer's exit code itself, so the
        # waiting caller was not stopped as a product process. A repair clears
        # product-state.json, so there is no record to read here.
        self.assertNotEqual(machine.live()["pid"], before["pid"])


if __name__ == "__main__":
    unittest.main()
