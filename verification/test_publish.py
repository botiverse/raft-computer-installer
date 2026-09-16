import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import urllib.error

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from publish import Hands, readback


class PublisherContract(unittest.TestCase):
    def test_api_identifies_client_without_following_redirects(self):
        hands = Hands("https://hands.example", "synthetic-test-token")
        seen = []
        def redirected(request, timeout):
            seen.append(request)
            raise urllib.error.HTTPError(request.full_url, 302, "redirect", {"Location": "https://other.example"}, None)
        with patch.object(hands.opener, "open", side_effect=redirected):
            with self.assertRaisesRegex(RuntimeError, "HTTP 302"):
                hands.api("/api/apps")
        self.assertEqual(len(seen), 1)
        self.assertTrue(seen[0].get_header("User-agent").startswith("raft-computer-installer/"))
        self.assertEqual(seen[0].get_header("Authorization"), "Bearer synthetic-test-token")

    def test_public_readback_checks_binary_and_sums_and_never_sends_auth(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "raft-computer-installer"
            binary.write_bytes(b"native binary")
            sums = binary.parent / "SHA256SUMS"
            sums.write_bytes(b"checksums")
            files = [(binary, {"artifact_kind": "installable", "metadata_json": {"target": "linux-x64"}})]
            public = "https://hands.example/dl/installer/releases/release-id/linux-x64"
            requests = []
            def redirect(request, timeout):
                requests.append(request)
                raise urllib.error.HTTPError(request.full_url, 302, "redirect", {"Location": public}, None)
            def download(request, timeout):
                requests.append(request)
                return io.BytesIO(sums.read_bytes() if request.full_url.endswith("?kind=sha256sums") else binary.read_bytes())
            with patch("publish.urllib.request.OpenerDirector.open", side_effect=redirect), patch("publish.urllib.request.urlopen", side_effect=download):
                readback("https://hands.example", "installer", "alpha", "release-id", files)
            self.assertEqual(len(requests), 3)
            for request in requests:
                self.assertTrue(request.get_header("User-agent").startswith("raft-computer-installer/"))
                self.assertIsNone(request.get_header("Authorization"))
            with patch("publish.urllib.request.OpenerDirector.open", side_effect=redirect), patch("publish.urllib.request.urlopen", return_value=io.BytesIO(b"corrupt")):
                with self.assertRaisesRegex(RuntimeError, "does not match local bytes"):
                    readback("https://hands.example", "installer", "alpha", "release-id", files)
