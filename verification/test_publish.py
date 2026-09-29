import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import urllib.error

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import publish as publisher
from publish import Hands, readback


class PublisherContract(unittest.TestCase):
    def test_publish_creates_a_draft_then_activates_with_exact_preconditions(self):
        calls = []

        class FakeHands:
            def __init__(self, origin, token):
                self.origin = origin

            def api(self, path, body=None, content_type="application/json"):
                calls.append((path, body))
                if path == "/api/apps":
                    return {"apps": [{"id": "app-id", "slug": "installer"}]}
                if path == "/api/apps/app-id/channels":
                    return {"channels": [{"id": "channel-id", "slug": "main"}]}
                if path.startswith("/api/apps/app-id/releases?"):
                    return {"releases": []}
                if path == "/api/apps/app-id/builds":
                    return {"id": "build-id"}
                if path == "/api/apps/app-id/builds/build-id/assets":
                    return {}
                if path == "/api/apps/app-id/releases/draft":
                    return {"id": "release-id", "status": "draft", "revision": 0}
                if path == "/api/apps/app-id/releases/release-id/publish":
                    return {"id": "release-id", "status": "active", "revision": 1}
                raise AssertionError(f"unexpected API path: {path}")

            def upload(self, app, path):
                return {"r2_key": "pending/object", "file_hash": "digest", "size_bytes": 6}

        output = Path.cwd() / "dist"
        path = output / "native/linux-x64/raft-computer-installer"
        files = [(path, {"artifact_kind": "installable", "metadata_json": {"target": "linux-x64"}})]
        args = SimpleNamespace(output=output, channel=None, app="installer", version_code=123,
            publish=True, origin="https://hands.example", ci_url="https://ci.example/run/123")
        manifest = {"installerVersion": "0.3.5", "gitCommit": "a" * 40}
        with patch.dict("os.environ", {"HANDS_AUTH_TOKEN": "test-token"}), \
             patch.object(publisher, "planned_files", return_value=(manifest, files)), \
             patch.object(publisher, "Hands", FakeHands), \
             patch.object(publisher, "readback") as public_readback:
            publisher.publish(args)

        draft = next(body for path, body in calls if path.endswith("/releases/draft"))
        self.assertNotIn("status", draft)
        self.assertEqual(draft["scopes"], [{"scope_type": "full", "scope_value": "all"}])
        activation = next(body for path, body in calls if path.endswith("/release-id/publish"))
        self.assertEqual(activation, {"expected_revision": 0, "expected_scopes": draft["scopes"]})
        public_readback.assert_called_once_with(
            "https://hands.example", "installer", "main", "release-id", files,
        )

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
