import gzip
"""Real native processes, isolated homes, loopback release authority and CDN."""
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from build import sign, target_key

WINDOWS = os.name == "nt"
SUFFIX = ".exe" if WINDOWS else ""
TARGET = target_key()
DIST = Path(os.environ.get("RCI_DIST", ROOT / "dist")).resolve()
TEMPLATE_TEXT = b'{"version":"0.0.0","marker":"RAFT_NATIVE_FIXTURE_CONFIGURATION_V1"}'
TEMPLATE = TEMPLATE_TEXT + b"\0" * (512 - len(TEMPLATE_TEXT))


def sha(data):
    return hashlib.sha256(data).hexdigest()


def wait_for(predicate, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.03)
    raise AssertionError("timed out waiting for the expected process state")


def exchange(home, action="probe"):
    try:
        record = json.loads((home / "fixture-service.json").read_text())
        host, port = record["address"].rsplit(":", 1)
        if host != "127.0.0.1":
            raise AssertionError("fixture address is not loopback")
        with socket.create_connection((host, int(port)), timeout=2) as stream:
            stream.sendall(json.dumps({"action": action, "token": record["token"]}).encode() + b"\n")
            line = stream.makefile("rb").readline(4097)
        result = json.loads(line)
        if result["generation"] != record["generation"]:
            raise AssertionError("fixture generation does not match")
        return {key: result[key] for key in ("pid", "version", "generation")}
    except (OSError, ValueError, KeyError):
        return None


class ReleaseServer:
    def __init__(self):
        self.directory = tempfile.TemporaryDirectory(prefix="rci-releases-")
        self.root = Path(self.directory.name)
        self.releases = {}
        self.channels = {"main": "1.1.0", "alpha": "1.1.0"}
        self.wrong_hash = set()
        self.authority_lies = set()
        self.authority_changes = {}
        self.gzip_versions = set()
        self.manifest_changes = {}
        self.requests = []
        self.request_times = []
        self.slow_checksums = 0.0
        self.tamper_installer = False
        self.missing_checksums = False
        self.channel_resolutions = 0
        self.mutable_redirect = False
        self.machines = []
        self.sidecar = b"isolated native fixture wasm\n"
        self.installer = DIST / "native" / TARGET / ("raft-computer-installer" + SUFFIX)
        self.fixture = DIST / "fixtures" / ("raft-computer-fixture" + SUFFIX)
        self.installer_bytes = self.installer.read_bytes()
        template = self.fixture.read_bytes()
        if template.count(TEMPLATE) != 1:
            raise AssertionError("fixture must contain exactly one configurable native blob")
        self.template = template
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                owner.respond(self)

            def log_message(self, *_):
                pass

        self.handler = Handler
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = f"http://127.0.0.1:{self.server.server_port}"
        self.proxies = []

    def publish(self, version, **behavior):
        data = json.dumps({"version": version, **behavior}, separators=(",", ":")).encode()
        if len(data) > 512:
            raise AssertionError("fixture behavior is too large")
        path = self.root / ("fixture-" + version + SUFFIX)
        path.write_bytes(self.template.replace(TEMPLATE, data + b"\0" * (512 - len(data))))
        path.chmod(0o755)
        sign(path)
        self.releases[version] = path.read_bytes()
        return self.releases[version]

    def manifest(self, version):
        data = self.releases[version]
        target = {"file": "raft-computer", "sha256": "0" * 64 if version in self.wrong_hash else sha(data), "size": len(data)}
        manifest = {"version": version, "targets": {TARGET: target},
            "photonWasm": {"file": "photon_rs_bg.wasm", "sha256": sha(self.sidecar), "size": len(self.sidecar)}}
        self.manifest_changes.get(version, lambda _: None)(manifest)
        return manifest

    def respond(self, handler):
        url = urllib.parse.urlsplit(handler.path)
        path = urllib.parse.unquote(url.path)
        query = urllib.parse.parse_qs(url.query)
        self.requests.append(handler.path)
        self.request_times.append((handler.path, time.monotonic()))
        status, body = 200, b""
        parts = path.strip("/").split("/")
        if path == "/public/v2/apps/raft-computer-cli/updates/check":
            version = query.get("version", [self.channels.get(query.get("channel", ["main"])[0])])[0]
            if version not in self.releases:
                status = 404
            else:
                platform, arch = TARGET.split("-")
                # Proxy requests preserve their original URL authority.
                origin = f"{url.scheme}://{url.netloc}" if url.netloc else self.base
                release_id = sha(version.encode())[:32]
                base = f"{origin}/dl/raft-computer-cli/releases/{release_id}/{TARGET}"
                manifest = self.manifest(version)
                raw = manifest["targets"][TARGET]
                wasm = manifest["photonWasm"]
                artifact = {"platform": platform, "arch": arch,
                    "download_url": base if raw["file"] == "raft-computer" else origin + "/outside",
                    "sha256": "0" * 64 if version in self.authority_lies else raw["sha256"], "size_bytes": raw["size"],
                    "photon_wasm": {"download_url": base + "?kind=photon-wasm", "sha256": wasm["sha256"], "size_bytes": wasm["size"]}}
                if version in self.gzip_versions:
                    compressed = gzip.compress(self.releases[version], mtime=0)
                    artifact["gzip"] = {"download_url": base + ".gz", "sha256": sha(compressed), "size_bytes": len(compressed)}
                response = {"update_available": True, "release": {"id": release_id, "version": version}, "artifact": artifact}
                self.authority_changes.get(version, lambda _: None)(response)
                body = json.dumps(response).encode()
        elif len(parts) == 5 and parts[:3] == ["dl", "raft-computer-cli", "releases"]:
            version = next((v for v in self.releases if sha(v.encode())[:32] == parts[3]), None)
            if version not in self.releases or parts[4] not in (TARGET, TARGET + ".gz"):
                status = 404
            elif parts[4].endswith(".gz"):
                if version in self.gzip_versions and not query:
                    body = gzip.compress(self.releases[version], mtime=0)
                else:
                    status = 404
            elif query.get("kind") == ["photon-wasm"]:
                body = self.sidecar
            elif not query:
                body = self.releases[version]
            else:
                status = 404
        elif path == f"/dl/raft-computer-installer/main/{TARGET}":
            self.channel_resolutions += 1
            # A second channel resolution names different bytes. A correct
            # bootstrap freezes this first Location for all subsequent fetches.
            handler.send_response(302)
            location = f"/dl/raft-computer-installer/releases/frozen-{self.channel_resolutions}/{TARGET}"
            if self.mutable_redirect:
                location = f"/dl/raft-computer-installer/alpha/{TARGET}"
            handler.send_header("Location", location)
            handler.end_headers()
            return
        elif path in (f"/dl/raft-computer-installer/releases/frozen-1/{TARGET}", "/installer/SHA256SUMS", f"/installer/native/{TARGET}/raft-computer-installer{SUFFIX}"):
            if path.endswith("SHA256SUMS") or query.get("kind") == ["sha256sums"]:
                time.sleep(self.slow_checksums)
                relative = f"native/{TARGET}/raft-computer-installer{SUFFIX}"
                body = b"" if self.missing_checksums else f"{sha(self.installer_bytes)}  {relative}\n".encode()
            else:
                body = self.installer_bytes + (b"tampered" if self.tamper_installer else b"")
        else:
            status = 404
        handler.send_response(status)
        handler.send_header("Content-Length", str(len(body)))
        handler.end_headers()
        try:
            handler.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def proxy(self):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self.handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.proxies.append((server, thread))
        return f"http://127.0.0.1:{server.server_port}"

    def machine(self):
        machine = Machine(self)
        self.machines.append(machine)
        return machine

    def close(self):
        for machine in self.machines:
            machine.close()
        for server, thread in [*self.proxies, (self.server, self.thread)]:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
        self.directory.cleanup()


class Machine:
    def __init__(self, server, prefix="rci-machine-"):
        self.server = server
        self.directory = tempfile.TemporaryDirectory(prefix=prefix)
        self.home = Path(self.directory.name).resolve()
        self.install_dir = self.home / "bin"
        self.install_dir.mkdir()
        self.k = self.home / "computer" / "k"
        self.state = self.home / "computer" / "installer"

    @property
    def binary(self):
        return self.install_dir / ("raft-computer" + SUFFIX)

    def env(self, extra=None):
        environment = {key: value for key, value in os.environ.items() if key.lower() not in (
            "http_proxy", "https_proxy", "all_proxy", "no_proxy") and not key.startswith(("RAFT_", "SLOCK_", "RCI_FIXTURE_"))}
        environment.update(HOME=str(self.home), USERPROFILE=str(self.home), SHELL="/bin/zsh", RAFT_HOME=str(self.home),
            RAFT_COMPUTER_INSTALL_DIR=str(self.install_dir), RAFT_COMPUTER_RELEASE_BASE=self.server.base + "/computer",
            RAFT_COMPUTER_HANDS_ORIGIN=self.server.base, RAFT_COMPUTER_NON_INTERACTIVE="1", RAFT_COMPUTER_NO_MODIFY_PATH="1" if WINDOWS else "0",
            RAFT_COMPUTER_INSTALLER_DL_BASE=self.server.base + "/dl/raft-computer-installer")
        environment.update(extra or {})
        return environment

    def command(self, args, extra=None, bootstrap=False, psreadline=False):
        if bootstrap:
            if WINDOWS:
                powershell = str(Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe")
                if psreadline:
                    # An interactive Windows PowerShell 5.1 console has PSReadLine
                    # loaded before the entry script runs. Reproduce that state
                    # explicitly: -File cannot, because it starts a bare session.
                    quoted = " ".join("'" + a.replace("'", "''") + "'" for a in args)
                    script = "Import-Module PSReadLine; & '" + str(DIST / "install.ps1") + "' " + quoted + "; exit $LASTEXITCODE"
                    return [powershell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", script]
                return [powershell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(DIST / "install.ps1"), *args]
            return ["/bin/sh", str(DIST / "install.sh"), *args]
        return [str(self.server.installer), *args]

    def run(self, args, extra=None, bootstrap=False, psreadline=False):
        return subprocess.run(self.command(args, extra, bootstrap, psreadline), env=self.env(extra), text=True, capture_output=True, timeout=180)

    def json(self, args, expected=0, extra=None):
        result = self.run([*args, "--json"], extra)
        assert result.returncode == expected, (result.returncode, result.stdout, result.stderr)
        reply = json.loads(result.stdout)
        assert reply["exitCode"] == expected
        return reply

    def product(self, args):
        result = subprocess.run([str(self.binary), *args], env=self.env(), text=True, capture_output=True, timeout=20)
        assert result.returncode == 0, (result.stdout, result.stderr)
        return result

    def self_version(self):
        return self.product(["--version"]).stdout.strip()

    def live(self):
        return exchange(self.home)

    def login_start(self):
        self.product(["login"])
        self.product(["start"])
        return wait_for(self.live)

    def preinstall(self, version, running=False):
        self.binary.write_bytes(self.server.releases[version])
        self.binary.chmod(0o755)
        (self.install_dir / "photon_rs_bg.wasm").write_bytes(self.server.sidecar)
        if running:
            self.login_start()

    def receipt(self, id):
        return json.loads((self.state / "receipts" / (sha(id.encode()) + ".json")).read_text())

    def close(self):
        exchange(self.home, "stop")
        deadline = time.monotonic() + 5
        while self.live() and time.monotonic() < deadline:
            time.sleep(0.05)
        if self.live():
            raise AssertionError("fixture service did not stop during cleanup")
        self.directory.cleanup()
