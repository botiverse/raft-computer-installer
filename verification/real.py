#!/usr/bin/env python3
"""Exercise published native Computer releases in isolated, unauthenticated homes.

Explicit network check. This verifies cold product behavior, not a logged-in
live service; the native fixture suite exercises lifecycle and fault injection.
"""
import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import urllib.error
import urllib.parse
import urllib.request

from harness import ReleaseServer, TARGET
from http_client import public_request

HANDS = "https://hands.build"


def read_json(url):
    with urllib.request.urlopen(public_request(url), timeout=60) as response:
        body = response.read(1_048_577)
    if len(body) > 1_048_576:
        raise ValueError("release metadata exceeds its size limit")
    return json.loads(body)


def versions(current, older):
    if not current:
        current = read_json(HANDS + "/public/v2/apps/raft-computer-cli/latest?channel=main&product_type=cli-binary")["build"]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", current):
        raise ValueError("specify a stable --current version for published cold verification")
    if not older:
        major, minor, patch = map(int, current.split("."))
        for number in range(patch - 1, max(-1, patch - 9), -1):
            candidate = f"{major}.{minor}.{number}"
            try:
                platform, arch = TARGET.split("-")
                response = read_json(HANDS + "/public/v2/apps/raft-computer-cli/updates/check?" + urllib.parse.urlencode(dict(product_type="cli-binary", current_version="0.0.0", version=candidate, platform=platform, arch=arch)))
                if not response.get("update_available") or not response.get("artifact", {}).get("photon_wasm"):
                    continue
                older = candidate
                break
            except urllib.error.HTTPError as error:
                if error.code != 404:
                    raise
    if not older or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", older):
        raise ValueError("no suitable previous patch release; specify --older explicitly")
    if tuple(map(int, older.split("."))) >= tuple(map(int, current.split("."))):
        raise ValueError("--older must precede --current")
    return current, older



def assert_stopped(machine, extra):
    """Older published Computer versions predate status --json."""
    status = subprocess.run([str(machine.binary), "status", "--json"], env=machine.env(extra),
        text=True, capture_output=True, timeout=30)
    if status.returncode == 0:
        if json.loads(status.stdout).get("attestation"):
            raise AssertionError("cold installation unexpectedly started a real service")
        return "json"
    if "unknown option '--json'" not in status.stderr:
        raise AssertionError("product status failed; stopped state is unproven")
    legacy = subprocess.run([str(machine.binary), "status"], env=machine.env(extra),
        text=True, capture_output=True, timeout=30)
    if legacy.returncode or not re.search(r"(?m)^Service:\s+stopped(?:\s|$)", legacy.stdout):
        raise AssertionError("legacy product did not report a stopped service")
    return "legacy-stopped-status"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--current")
    parser.add_argument("--older")
    args = parser.parse_args()
    current, older = versions(args.current, args.older)
    server = ReleaseServer()
    machine = server.machine()
    extra = {"RAFT_COMPUTER_HANDS_ORIGIN": HANDS,
        "RAFT_COMPUTER_INSTALLER_RELEASE_BASE": server.base + "/installer"}

    def run(arguments, expected=0):
        result = subprocess.run(machine.command([*arguments, "--json"], bootstrap=True),
            env=machine.env(extra), text=True, capture_output=True, timeout=900)
        assert result.returncode == expected, (arguments, result.returncode, result.stdout, result.stderr)
        reply = json.loads(result.stdout)
        assert reply["exitCode"] == expected
        return reply

    status_evidence = set()

    def cold(version):
        assert machine.self_version() == version
        status_evidence.add(assert_stopped(machine, extra))

    try:
        run(["install", "--version", older])
        cold(older)
        run(["upgrade", "--version", current])
        cold(current)
        assert run(["upgrade", "--version", current])["receipt"]["outcome"] == "up-to-date"
        run(["upgrade", "--version", older], 2)
        cold(current)
        run(["upgrade", "--version", older, "--allow-downgrade"])
        cold(older)
        # Retain actual published bytes and sidecar, but remove only this
        # isolated machine's K metadata to reproduce a pre-K installation.
        shutil.rmtree(machine.k)
        run(["upgrade", "--version", current])
        cold(current)
        (machine.k / "operation.json").write_text("synthetic unreadable operation\n")
        assert run(["repair", "--version", current])["receipt"]["outcome"] == "repaired"
        cold(current)
        print(json.dumps({"scope": "published native Computer, cold, isolated home", "older": older, "current": current,
            "statusEvidence": sorted(status_evidence), "cases": ["install", "upgrade", "up-to-date", "held downgrade", "intended downgrade", "adopt", "repair"]}))
    finally:
        # No login occurs. If a regression did start this isolated product,
        # ask that product to stop before removing its temporary home.
        if machine.binary.exists():
            stopped = subprocess.run([str(machine.binary), "stop"], env=machine.env(extra),
                text=True, capture_output=True, timeout=60)
            try:
                assert_stopped(machine, extra)
            except (AssertionError, ValueError, subprocess.TimeoutExpired):
                # Preserve evidence instead of deleting state under a service.
                machine.directory._finalizer.detach()
                raise RuntimeError(f"isolated product cleanup unresolved; retained {machine.home}; stop exit {stopped.returncode}")
        server.close()


if __name__ == "__main__":
    main()
