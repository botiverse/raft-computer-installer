#!/usr/bin/env python3
"""Publish one complete hosted Hands build via its HTTP API, without Node.

Wire contract: botiverse/hands packages/cli/src/commands/builds.ts,
publish-cli-binary and uploadAndRegisterAsset. No release becomes active until
every platform's binary and checksums have been uploaded and verified.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import urllib.error
import urllib.parse
import urllib.request
import uuid

from build import ROOT, TARGETS, identity

from http_client import USER_AGENT, public_request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, newurl):
        return None


class Hands:
    def __init__(self, origin, token):
        parsed = urllib.parse.urlsplit(origin)
        if parsed.scheme != "https" or not parsed.netloc or parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.path not in ("", "/"):
            raise ValueError("Hands publish origin must be an HTTPS origin")
        self.origin = origin.rstrip("/")
        self.token = token
        self.opener = urllib.request.build_opener(NoRedirect())

    def api(self, path, body=None, content_type="application/json"):
        if not path.startswith("/api/"):
            raise ValueError("invalid API path")
        data = None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
        request = urllib.request.Request(self.origin + path, data=data,
            headers={"User-Agent": USER_AGENT, "Authorization": "Bearer " + self.token, "Content-Type": content_type, "Accept": "application/json"})
        try:
            with self.opener.open(request, timeout=180) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            # Do not print request headers, token, or a potentially sensitive
            # server response. A partial build stays unpublished for inspection.
            raise RuntimeError(f"Hands {path} returned HTTP {error.code}") from None

    def upload(self, app, path):
        boundary = "raft-installer-" + uuid.uuid4().hex
        content = path.read_bytes()
        prefix = (f'--{boundary}\r\nContent-Disposition: form-data; name="apk"; filename="{path.name}"\r\n'
                  'Content-Type: application/octet-stream\r\n\r\n').encode()
        body = prefix + content + f"\r\n--{boundary}--\r\n".encode()
        uploaded = self.api(f"/api/apps/{app}/upload", body, f"multipart/form-data; boundary={boundary}")
        if uploaded.get("file_hash") != hashlib.sha256(content).hexdigest() or uploaded.get("size_bytes") != len(content):
            raise RuntimeError("Hands upload identity does not match local bytes")
        return uploaded


def planned_files(output):
    manifest = json.loads((output / "installer-manifest.json").read_text())
    if manifest.get("schema") != "raft-computer-installer/release/v3":
        raise ValueError("expected a native v3 release manifest")
    for relative, expected in manifest["files"].items():
        path = (output / relative).resolve()
        if not path.is_relative_to(output) or identity(path) != expected:
            raise ValueError(f"local release identity mismatch: {relative}")
    files = []
    for target in sorted(TARGETS):
        platform, arch = target.split("-")
        name = "raft-computer-installer" + (".exe" if platform == "win32" else "")
        relative = f"native/{target}/{name}"
        binary = output / relative
        sums = binary.parent / "SHA256SUMS"
        if not binary.is_file() or sums.read_text() != f"{identity(binary)['sha256']}  {relative}\n":
            raise ValueError(f"missing or inconsistent target: {target}")
        for path, kind, filetype in ((binary, "installable", "binary"), (sums, "checksums", "sha256sums")):
            files.append((path, {"artifact_kind": kind, "platform": platform, "arch": arch,
                "filetype": filetype, "variant": None, "metadata_json": {"filename": path.name, "target": target}}))
    return manifest, files


def readback(origin, app, channel, release_id, files):
    for path, metadata in files:
        if metadata["artifact_kind"] != "installable":
            continue
        target = metadata["metadata_json"]["target"]
        url = f"{origin}/dl/{app}/{urllib.parse.quote(channel, safe='')}/{target}"
        try:
            urllib.request.build_opener(NoRedirect()).open(public_request(url), timeout=30)
            raise RuntimeError("public channel did not redirect to an immutable release")
        except urllib.error.HTTPError as error:
            if error.code not in (301, 302, 303, 307, 308):
                raise RuntimeError("public channel resolution failed") from None
            release_url = urllib.parse.urljoin(url, error.headers.get("Location", ""))
        parsed = urllib.parse.urlsplit(release_url)
        expected_path = f"/dl/{urllib.parse.quote(app, safe='')}/releases/{urllib.parse.quote(release_id, safe='')}/{target}"
        if parsed.scheme != "https" or parsed.netloc != urllib.parse.urlsplit(origin).netloc or parsed.path != expected_path or parsed.query or parsed.fragment:
            raise RuntimeError("public channel points at a different release")
        for suffix, local in (("", path), ("?kind=sha256sums", path.parent / "SHA256SUMS")):
            expected = identity(local)
            digest, size = hashlib.sha256(), 0
            with urllib.request.urlopen(public_request(release_url + suffix), timeout=180) as response:
                for block in iter(lambda: response.read(1024 * 1024), b""):
                    digest.update(block)
                    size += len(block)
                    if size > expected["size"]:
                        raise RuntimeError("public release download is larger than its local artifact")
            if {"sha256": digest.hexdigest(), "size": size} != expected:
                raise RuntimeError("public release download does not match local bytes")


def publish(args):
    output = args.output.resolve()
    manifest, files = planned_files(output)
    version = manifest["installerVersion"]
    channel = args.channel or ("alpha" if "-" in version else "main")
    plan = {"app": args.app, "version": version, "channel": channel, "versionCode": args.version_code,
            "files": [str(path.relative_to(output)) for path, _ in files]}
    if not args.publish:
        print(json.dumps(plan, indent=2))
        return
    if args.version_code is None or not 0 <= args.version_code <= 9_007_199_254_740_991:
        raise ValueError("publishing requires a nonnegative, monotonically increasing --version-code")
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", args.app):
        raise ValueError("invalid app slug")
    token = os.environ.get("HANDS_AUTH_TOKEN")
    if not token:
        raise ValueError("HANDS_AUTH_TOKEN is required for publishing")
    hands = Hands(args.origin, token)
    apps = hands.api("/api/apps")["apps"]
    app = next((app["id"] for app in apps if app["slug"] == args.app), None)
    if not app:
        raise ValueError("Hands app is not visible to the deploy token")
    channels = hands.api(f"/api/apps/{app}/channels")["channels"]
    channel_id = next((item["id"] for item in channels if item["slug"] == channel), None)
    if not channel_id:
        raise ValueError("Hands channel does not exist")
    query = urllib.parse.urlencode({"channel": channel_id, "product_type": "cli-binary", "release_type": "stable", "version_code": args.version_code})
    prior = hands.api(f"/api/apps/{app}/releases?{query}")["releases"]
    if any(item.get("version_code") == args.version_code and item.get("status") != "cancelled" for item in prior):
        raise ValueError("this Hands version code already belongs to a release; inspect it before retrying")
    provenance = {"source_commit": manifest.get("gitCommit"), "ci_provider": "github" if os.environ.get("GITHUB_ACTIONS") else None,
        "ci_run_id": os.environ.get("GITHUB_RUN_ID"), "ci_url": args.ci_url}
    build = hands.api(f"/api/apps/{app}/builds", {"channel_id": channel_id, "product_type": "cli-binary",
        "release_type": "stable", "version_name": version, "version_code": args.version_code,
        "source": "ci", "status": "succeeded", "build_metadata_json": {"hosted": True}, "provenance_json": provenance})
    print(json.dumps({"buildId": build["id"], "status": "uploading"}), flush=True)
    for path, metadata in files:
        uploaded = hands.upload(app, path)
        hands.api(f"/api/apps/{app}/builds/{build['id']}/assets", {**metadata,
            "r2_key": uploaded["r2_key"], "file_hash": uploaded["file_hash"], "size_bytes": uploaded["size_bytes"]})
    release = hands.api(f"/api/apps/{app}/releases", {"build_id": build["id"], "channel_id": channel_id,
        "product_type": "cli-binary", "release_type": "stable", "status": "active", "changelog": None,
        "provenance_json": provenance, "scopes": [{"scope_type": "full", "scope_value": "all"}]})
    readback(hands.origin, args.app, channel, release["id"], files)
    print(json.dumps({"buildId": build["id"], "releaseId": release["id"], "status": "published-and-read-back"}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    parser.add_argument("--app", default="raft-computer-installer")
    parser.add_argument("--origin", default="https://hands.build")
    parser.add_argument("--channel")
    parser.add_argument("--version-code", type=int)
    parser.add_argument("--ci-url")
    parser.add_argument("--publish", action="store_true", help="write the reviewed local release to Hands")
    publish(parser.parse_args())
