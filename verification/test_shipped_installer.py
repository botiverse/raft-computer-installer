"""The installer that is published carries no test-only hook.

The native fixture is built with the `verification-fixture` feature; the
installer must not be (scripts/build.py builds them separately). The byte rule
is pinned on synthetic blobs, and the built artifact must pass it.
"""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import build  # noqa: E402
from harness import DIST, SUFFIX, TARGET  # noqa: E402


class ShippedInstaller(unittest.TestCase):
    def test_a_fixture_variable_is_rejected(self):
        blob = Path(self.id() + ".bin")
        blob.write_bytes(b"\0" * 64 + b"RCI_FIXTURE_ANYTHING" + b"\0" * 8)
        try:
            with self.assertRaises(ValueError):
                build.assert_no_fixture_hooks(blob)
        finally:
            blob.unlink()

    def test_the_built_installer_has_no_fixture_hook(self):
        binary = DIST / "native" / TARGET / ("raft-computer-installer" + SUFFIX)
        build.assert_no_fixture_hooks(binary)
        self.assertNotIn(build.FIXTURE_MARKER, binary.read_bytes())


if __name__ == "__main__":
    unittest.main()
