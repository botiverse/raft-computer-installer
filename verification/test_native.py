import faulthandler
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import time
import unittest

from harness import DIST, TARGET, WINDOWS, Machine, ReleaseServer, exchange, sha, wait_for


class InstallerContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ReleaseServer()
        for version, behavior in (
            ("0.9.0", {"statusUnsupported": True, "stopBroken": True}),
            ("1.0.0", {}), ("1.1.0", {}), ("1.2.0", {"startFail": True}),
            ("1.3.0", {"reportedVersion": "9.9.9"}), ("1.4.0", {"stopBroken": True}),
            ("1.5.0", {"liveVersion": "9.9.9"}), ("1.6.0-fixture.1", {}),
        ):
            cls.server.publish(version, **behavior)

    @classmethod
    def tearDownClass(cls):
        cls.server.close()

    def setUp(self):
        faulthandler.dump_traceback_later(60, repeat=True)
        self.addCleanup(faulthandler.cancel_dump_traceback_later)
        self.server.requests.clear()
        self.server.channel_resolutions = 0
        self.server.mutable_redirect = False
        self.server.channels = {"main": "1.1.0", "alpha": "1.1.0", "fixture-channel": "1.6.0-fixture.1"}
        self.server.wrong_hash.clear()
        self.server.authority_lies.clear()
        self.server.manifest_changes.clear()
        self.server.tamper_installer = False
        self.server.missing_checksums = False

    def machine(self):
        machine = self.server.machine()
        self.addCleanup(machine.close)
        return machine

    def test_short_lived_protocol_replies_are_flushed(self):
        machine = self.machine()
        for _ in range(20):
            # These tiny responses race process::exit unless the same stdout
            # instance is flushed before the worker/controller exits.
            reply = machine.json(["status"])
            self.assertEqual(reply["exitCode"], 0)
            controller = subprocess.run([str(self.server.installer), "--host-controller"],
                input=json.dumps({"protocolVersion": 1, "action": "fence"}),
                env=machine.env(), text=True, capture_output=True, timeout=20)
            self.assertEqual(controller.returncode, 0, controller.stderr)
            self.assertEqual(json.loads(controller.stdout), {"protocolVersion": 1, "ok": True})

    def test_controller_keeps_missing_transaction_state_uncertain(self):
        machine = self.machine()
        response = subprocess.run([str(self.server.installer), "--host-controller"],
            input=json.dumps({"protocolVersion": 1, "action": "probe"}),
            env=machine.env(), text=True, capture_output=True, timeout=20)
        self.assertEqual(response.returncode, 1)
        reply = json.loads(response.stdout)
        self.assertFalse(reply["ok"])
        self.assertTrue(reply["uncertain"])
        self.assertFalse(machine.binary.exists())

    def test_unattended_channel_and_explicit_consent_receipt(self):
        machine = self.machine()
        reply = machine.json(["install"], extra={"RAFT_COMPUTER_OPERATION_ID": "channel-consent"})
        self.assertEqual(reply["receipt"]["targetVersion"], "1.1.0")
        self.assertEqual(reply["receipt"]["presence"], "unattended")
        self.assertTrue(reply["receipt"]["approvedBy"])
        self.assertEqual(machine.self_version(), "1.1.0")
        self.assertIsNone(machine.live())
        self.assertEqual((machine.install_dir / "photon_rs_bg.wasm").read_bytes(), self.server.sidecar)
        self.assertEqual(machine.receipt("channel-consent"), reply["receipt"])
        self.assertEqual(sum("/latest?" in path for path in self.server.requests), 1)
        self.assertEqual(self.server.requests.count("/computer/1.1.0/manifest.json"), 1)

    def test_fresh_and_cold_upgrade_preserve_stopped_state(self):
        machine = self.machine()
        machine.json(["install", "--version", "1.0.0"])
        machine.json(["upgrade", "--version", "1.1.0"])
        self.assertEqual(machine.self_version(), "1.1.0")
        self.assertIsNone(machine.live())
        failed = machine.json(["upgrade", "--version", "1.3.0"], expected=1)
        self.assertEqual(failed["receipt"]["outcome"], "failed", "wrong self-version must fail before handover")
        self.assertEqual(machine.self_version(), "1.1.0")
        self.assertIsNone(machine.live())

    def test_warm_upgrade_replay_and_live_rollback(self):
        machine = self.machine()
        machine.json(["install", "--version", "1.0.0"])
        before = machine.login_start()
        first = machine.json(["upgrade", "--version", "1.1.0"], extra={"RAFT_COMPUTER_OPERATION_ID": "warm"})
        dead = json.loads(first["receipt"]["detail"]["deadProcessIdentities"])
        self.assertTrue(any(identity.startswith(f"pid:{before['pid']}:created:") for identity in dead))
        live = machine.live()
        self.assertEqual(live["version"], "1.1.0")
        self.assertNotEqual(live["generation"], before["generation"])
        downloads = len(self.server.requests)
        replay = machine.json(["upgrade", "--version", "1.1.0"], extra={"RAFT_COMPUTER_OPERATION_ID": "warm"})
        self.assertEqual(replay["receipt"], first["receipt"])
        self.assertEqual(machine.live(), live)
        self.assertEqual(len(self.server.requests), downloads)
        conflict = machine.json(["upgrade", "--version", "1.0.0"], expected=2, extra={"RAFT_COMPUTER_OPERATION_ID": "warm"})
        self.assertIsNone(conflict["receipt"])
        self.assertEqual(machine.live(), live)
        for candidate in ("1.2.0", "1.5.0"):
            with self.subTest(candidate=candidate):
                failed = machine.json(["upgrade", "--version", candidate], expected=1)
                self.assertEqual(failed["receipt"]["outcome"], "rolled-back")
                self.assertEqual(machine.live()["version"], "1.1.0")
        healthy = machine.live()
        failed = machine.json(["upgrade", "--version", "1.3.0"], expected=1)
        self.assertEqual(failed["receipt"]["outcome"], "failed")
        self.assertEqual(machine.live(), healthy, "candidate precheck must not restart the old service")

    def test_replay_is_history_and_status_is_live(self):
        machine = self.machine()
        machine.json(["install", "--version", "1.0.0"])
        machine.login_start()
        original = machine.json(["upgrade", "--version", "1.1.0"], extra={"RAFT_COMPUTER_OPERATION_ID": "history"})
        machine.product(["stop"])
        repeated = machine.json(["upgrade", "--version", "1.1.0"], extra={"RAFT_COMPUTER_OPERATION_ID": "history"})
        self.assertEqual(repeated["receipt"], original["receipt"])
        self.assertIsNone(machine.live())
        status = machine.json(["status"], extra={"RAFT_COMPUTER_OPERATION_ID": "history"})
        self.assertIn("stopped", status["line"])
        self.assertIsNone(status["receipt"])
        self.assertIsNone(machine.live())

    def test_completed_rollback_does_not_restart_later_stopped_service(self):
        machine = self.machine()
        machine.json(["install", "--version", "1.0.0"])
        machine.login_start()
        machine.json(["upgrade", "--version", "1.2.0"], expected=1)
        machine.product(["stop"])
        machine.json(["recover"])
        machine.json(["upgrade", "--version", "1.1.0"])
        self.assertIsNone(machine.live())
        self.assertEqual(machine.self_version(), "1.1.0")

    def test_up_to_date_and_downgrade_policy(self):
        machine = self.machine()
        machine.json(["install", "--version", "1.1.0"])
        before = machine.login_start()
        reply = machine.json(["upgrade", "--version", "1.1.0"])
        self.assertEqual(reply["receipt"]["outcome"], "up-to-date")
        machine.json(["upgrade", "--version", "1.0.0"], expected=2)
        self.assertEqual(machine.live(), before)
        machine.json(["upgrade", "--version", "1.0.0", "--allow-downgrade"])
        self.assertEqual(machine.live()["version"], "1.0.0")

    def test_adoption_preserves_running_and_stopped_modes(self):
        for running in (False, True):
            with self.subTest(running=running):
                machine = self.machine()
                machine.preinstall("1.0.0", running=running)
                self.assertFalse(machine.k.exists())
                machine.json(["upgrade", "--version", "1.1.0"])
                self.assertEqual(machine.self_version(), "1.1.0")
                self.assertEqual(machine.live() is not None, running)
        same = self.machine()
        same.preinstall("1.1.0")
        reply = same.json(["upgrade", "--version", "1.1.0"])
        self.assertEqual(reply["receipt"]["outcome"], "up-to-date")

    def test_legacy_without_status_is_upgraded_by_process_identity(self):
        for running in (False, True):
            with self.subTest(running=running):
                machine, unrelated = self.machine(), self.machine()
                machine.preinstall("0.9.0", running=running)
                unrelated.preinstall("0.9.0", running=True)
                other = unrelated.live()
                payload = machine.home / "user-data"
                payload.write_bytes(b"preserve application data")
                old = machine.live()
                result = machine.json(["upgrade", "--version", "1.1.0"])
                self.assertEqual(result["receipt"]["outcome"], "promoted")
                self.assertEqual(machine.self_version(), "1.1.0")
                self.assertEqual(machine.live() is not None, running)
                if running:
                    self.assertEqual(machine.live()["version"], "1.1.0")
                    self.assertNotEqual(machine.live()["generation"], old["generation"])
                    state = json.loads((machine.state / "product-state.json").read_text())
                    self.assertIn(old["pid"], state["forcedStops"])
                self.assertEqual(unrelated.live(), other)
                self.assertEqual(payload.read_bytes(), b"preserve application data")

    def test_forced_stop_records_identity_and_leaves_unrelated_service(self):
        machine, other = self.machine(), self.machine()
        machine.preinstall("1.4.0", running=True)
        other.preinstall("1.0.0", running=True)
        before, unrelated = machine.live(), other.live()
        machine.json(["upgrade", "--version", "1.1.0", "--allow-downgrade"])
        state = json.loads((machine.state / "product-state.json").read_text())
        self.assertIn(before["pid"], state["forcedStops"])
        self.assertEqual(machine.live()["version"], "1.1.0")
        self.assertEqual(other.live(), unrelated)

    def test_foreign_manager_and_healthy_repair_are_held(self):
        machine = self.machine()
        machine.json(["repair", "--version", "1.0.0"], expected=2)
        script = b"#!/usr/bin/env node\n// historical package-manager shim; never executed\n"
        machine.binary.write_bytes(script)
        machine.binary.chmod(0o755)
        machine.json(["install", "--version", "1.1.0"], expected=2)
        self.assertEqual(machine.binary.read_bytes(), script)

    def test_broken_states_are_repaired_then_recovery_payloads_are_removed(self):
        def damage_artifact(machine):
            (machine.k / "slots/stable/artifact.bin").unlink()

        mutations = {
            "artifact": damage_artifact,
            "empty-version": lambda m: (m.k / "slots/stable/VERSION").write_text(""),
            "journal": lambda m: (m.k / "journal.jsonl").write_text('{"seq":1}\n{invalid\n'),
            "operation": lambda m: (m.k / "operation.json").write_text('{"formatVersion":99}'),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                machine = self.machine()
                machine.json(["install", "--version", "1.0.0"])
                user_data = machine.home / "product-identity"
                user_data.write_bytes(b"synthetic identity to preserve")
                mutate(machine)
                result = machine.json(["install", "--version", "1.1.0"])
                self.assertEqual(result["receipt"]["outcome"], "repaired")
                kept = Path(result["receipt"]["detail"]["quarantine"])
                self.assertFalse(kept.exists(), "verified repair must remove old installation copies")
                self.assertEqual(user_data.read_bytes(), b"synthetic identity to preserve")
                self.assertEqual(machine.self_version(), "1.1.0")
                self.assertIsNone(machine.live())
        junk = self.machine()
        junk.k.mkdir(parents=True)
        (junk.k / "operation.json").write_text("unreadable")
        junk.json(["install", "--version", "1.0.0"])
        self.assertEqual(junk.self_version(), "1.0.0")

    def test_failed_repair_keeps_unresolved_and_download_precedes_stop(self):
        machine = self.machine()
        machine.json(["install", "--version", "1.0.0"])
        before = machine.login_start()
        old_binary = machine.binary.read_bytes()
        (machine.k / "operation.json").write_text("broken")
        self.server.wrong_hash.add("1.1.0")
        failed = machine.json(["repair", "--version", "1.1.0"], expected=3)
        self.assertEqual(failed["receipt"]["outcome"], "unresolved")
        self.assertEqual(machine.binary.read_bytes(), old_binary)
        self.assertEqual(machine.live(), before)
        self.assertFalse((machine.state / "quarantine").exists())
        machine.json(["status"], expected=3)
        self.server.wrong_hash.clear()
        machine.json(["repair", "--version", "1.1.0"])
        self.assertEqual(machine.live()["version"], "1.1.0")

    def test_installer_metadata_damage_is_reported_then_cleaned_after_repair(self):
        for damaged in ("active", "plan", "receipt"):
            with self.subTest(damaged=damaged):
                machine = self.machine()
                machine.json(["install", "--version", "1.0.0"],
                    extra={"RAFT_COMPUTER_OPERATION_ID": "original"})
                active = machine.state / "active.json"
                active.write_text(json.dumps({"formatVersion": 1, "id": "original"}))
                paths = {
                    "active": active,
                    "plan": machine.state / "operations" / sha(b"original") / "plan.json",
                    "receipt": machine.state / "receipts" / (sha(b"original") + ".json"),
                }
                broken = b"{damaged installer metadata"
                paths[damaged].parent.mkdir(parents=True, exist_ok=True)
                paths[damaged].write_bytes(broken)
                self.server.requests.clear()
                observed = machine.json(["status"], expected=3)
                self.assertEqual(observed["world"]["kind"], "broken")
                self.assertEqual(self.server.requests, [], "status recovery must remain offline")
                result = machine.json(["repair", "--version", "1.1.0"])
                self.assertEqual(result["receipt"]["outcome"], "repaired")
                preserved = Path(result["receipt"]["detail"]["metadataQuarantine"])
                self.assertFalse(preserved.exists(), "obsolete metadata backup must not accumulate")
                self.assertFalse((machine.state / "metadata-damage.json").exists())
                self.assertEqual(machine.json(["status"])["world"]["kind"], "managed")
                self.assertIsNone(machine.live())

    def test_sidecar_damage_is_repaired_and_obsolete_cache_is_removed(self):
        for damaged in ("missing", "installed", "identity"):
            with self.subTest(damaged=damaged):
                machine = self.machine()
                machine.json(["install", "--version", "1.0.0"])
                sidecar = machine.install_dir / "photon_rs_bg.wasm"
                if damaged == "missing":
                    sidecar.unlink()
                elif damaged == "installed":
                    sidecar.write_bytes(b"damaged sidecar")
                else:
                    (machine.state / "sidecars/1.0.0/identity.json").write_bytes(b"{damaged cache")
                self.assertEqual(machine.json(["status"], expected=3)["world"]["kind"], "broken")
                result = machine.json(["repair", "--version", "1.0.0"])
                self.assertEqual(result["receipt"]["outcome"], "repaired")
                kept = Path(result["receipt"]["detail"]["sidecarQuarantine"])
                self.assertFalse(kept.exists())
                self.assertEqual(sidecar.read_bytes(), self.server.sidecar)
                self.assertIsNone(machine.live())

    def test_unresolved_repair_keeps_payloads_until_verified_replacement(self):
        machine = self.machine()
        machine.json(["install", "--version", "1.0.0"])
        machine.login_start()
        (machine.k / "operation.json").write_text("broken")
        result = machine.json(["repair", "--version", "1.2.0"], expected=3)
        self.assertEqual(result["receipt"]["outcome"], "unresolved")
        self.assertTrue((machine.state / "quarantine").exists())
        self.assertTrue(list((machine.state / "operations").glob("*/artifact.bin")))
        self.assertTrue(list((machine.state / "scratch/supervisors").glob("operation-*")))
        completed = machine.json(["repair", "--version", "1.1.0", "--allow-downgrade"])
        self.assertEqual(completed["receipt"]["outcome"], "repaired")
        self.assertEqual(machine.live()["version"], "1.1.0")
        self.assertFalse((machine.state / "quarantine").exists())
        self.assertFalse((machine.state / "operations").exists())
        self.assertFalse((machine.k / "incoming").exists())
        self.assertEqual(sorted(p.name for p in (machine.state / "sidecars").iterdir()), ["1.1.0"])
        self.assertEqual(list((machine.state / "scratch/supervisors").glob("operation-*")), [])

    def test_completed_cleanup_retries_without_installing_again(self):
        machine = self.machine()
        machine.json(["install", "--version", "1.0.0"], extra={"RAFT_COMPUTER_OPERATION_ID": "cleanup-retry"})
        current = machine.binary.read_bytes()
        user_data = machine.home / "user-data"
        user_data.write_text("keep me")
        obsolete = [machine.state / "quarantine/old", machine.state / "operations/old",
            machine.k / "incoming/old", machine.state / "sidecars/0.9.0"]
        for directory in obsolete:
            directory.mkdir(parents=True, exist_ok=True)
            (directory / "payload").write_bytes(b"obsolete")
        (machine.state / "cleanup.json").write_text(json.dumps({"formatVersion": 1, "receiptId": "cleanup-retry"}))
        self.server.requests.clear()
        machine.json(["status"])
        self.assertEqual(self.server.requests, [], "cleanup must not need the network")
        self.assertEqual(machine.binary.read_bytes(), current)
        self.assertEqual(user_data.read_text(), "keep me")
        self.assertTrue((machine.state / "sidecars/1.0.0").exists())
        self.assertFalse((machine.state / "cleanup.json").exists())
        for directory in obsolete:
            self.assertFalse(directory.exists())

    def test_bad_sources_fail_before_publication(self):
        cases = (
            ("authority", {"RAFT_COMPUTER_HANDS_ORIGIN": "http://127.0.0.1:9"}),
            ("manifest", {"RAFT_COMPUTER_RELEASE_BASE": "http://127.0.0.1:9"}),
        )
        for name, extra in cases:
            with self.subTest(name=name):
                machine = self.machine()
                machine.json(["install"], expected=1, extra=extra)
                self.assertFalse(machine.binary.exists())
        self.server.authority_lies.add("1.1.0")
        machine = self.machine()
        self.server.requests.clear()
        machine.json(["install"], expected=1)
        self.assertFalse(any(path.endswith("/raft-computer") for path in self.server.requests))
        self.server.authority_lies.clear()
        self.server.manifest_changes["1.1.0"] = lambda m: m["targets"][TARGET].update(file="../outside")
        machine = self.machine()
        machine.json(["install", "--version", "1.1.0"], expected=1)
        self.assertFalse(machine.binary.exists())

    def test_feature_channel_and_bootstrap_freeze_cleanup(self):
        machine = self.machine()
        temp = machine.home / "downloads"
        temp.mkdir()
        result = machine.run(["--channel", "fixture-channel"], bootstrap=True,
            extra={"TMPDIR": str(temp), "TEMP": str(temp), "TMP": str(temp)})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.server.channel_resolutions, 1)
        self.assertEqual(machine.self_version(), "1.6.0-fixture.1")
        self.assertFalse(list(temp.iterdir()), "bootstrap and self-probe temporary files must be cleaned")
        self.server.channel_resolutions = 0
        self.server.tamper_installer = True
        failed = self.machine()
        result = failed.run([], bootstrap=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(failed.binary.exists())
        self.server.channel_resolutions = 0
        self.server.tamper_installer = False
        self.server.missing_checksums = True
        result = failed.run([], bootstrap=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(failed.binary.exists())

    def test_bootstrap_rejects_a_mutable_redirect_and_preserves_status(self):
        machine = self.machine()
        self.server.mutable_redirect = True
        result = machine.run([], bootstrap=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(machine.binary.exists())
        self.assertEqual(self.server.channel_resolutions, 1)
        self.assertFalse(any("sha256sums" in path for path in self.server.requests))
        self.server.mutable_redirect = False
        self.server.channel_resolutions = 0
        result = machine.run(["status", "--json"], bootstrap=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(json.loads(result.stdout)["world"]["kind"], "fresh")
        self.assertFalse(machine.binary.exists())

    def test_invalid_channel_is_rejected_without_release_downloads(self):
        machine = self.machine()
        result = machine.run(["install", "--channel", "../outside"])
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(machine.binary.exists())
        self.assertFalse(self.server.requests)

    def test_static_bootstrap_without_node_on_path(self):
        machine = self.machine()
        if WINDOWS:
            search = str(Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0")
        else:
            tools = machine.home / "bootstrap-tools"
            tools.mkdir()
            for name in ("curl", "mktemp", "uname", "awk", "sha256sum", "shasum", "sed", "tr", "tail", "rm", "chmod"):
                executable = shutil.which(name)
                if executable:
                    (tools / name).symlink_to(executable)
            search = str(tools)
        result = machine.run(["--version", "1.0.0"], bootstrap=True,
            extra={"PATH": search, "RAFT_COMPUTER_INSTALLER_RELEASE_BASE": self.server.base + "/installer"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(machine.self_version(), "1.0.0")

    def test_proxy_and_no_proxy_cover_authority_manifest_and_bytes(self):
        machine = self.machine()
        proxy = self.server.proxy()
        fake_origin = "http://release.fixture.invalid"
        extra = {"HTTP_PROXY": proxy, "http_proxy": proxy, "HTTPS_PROXY": proxy, "NO_PROXY": "", "no_proxy": "",
            "RAFT_COMPUTER_HANDS_ORIGIN": fake_origin, "RAFT_COMPUTER_RELEASE_BASE": fake_origin + "/computer"}
        machine.json(["install"], extra=extra)
        forwarded = [path for path in self.server.requests if path.startswith(fake_origin)]
        self.assertTrue(any("/latest?" in path for path in forwarded))
        self.assertTrue(any(path.endswith("/manifest.json") for path in forwarded))
        self.assertTrue(any(path.endswith("/raft-computer") for path in forwarded))
        bypass = self.machine()
        bypass.json(["install"], extra={"HTTP_PROXY": "http://127.0.0.1:9", "http_proxy": "http://127.0.0.1:9", "NO_PROXY": "127.0.0.1", "no_proxy": "127.0.0.1"})
        broken = self.machine()
        broken.json(["install"], expected=1, extra={**extra, "HTTP_PROXY": "http://127.0.0.1:9", "http_proxy": "http://127.0.0.1:9"})
        self.assertFalse(broken.binary.exists())

    def test_worker_kill_recovers_warm_and_cold_handover(self):
        for running in (False, True):
            with self.subTest(running=running):
                machine = self.machine()
                machine.json(["install", "--version", "1.0.0"])
                if running:
                    machine.login_start()
                gate = machine.home / "effect.gate"
                extra = {"RAFT_COMPUTER_OPERATION_ID": "interrupted"}
                if running:
                    extra["RCI_FIXTURE_STOP_GATE"] = str(gate)
                else:
                    extra.update(RCI_FIXTURE_PUBLISHED_GATE=str(gate), RCI_FIXTURE_PUBLISHED_PATH=str(machine.binary), RCI_FIXTURE_PUBLISHED_VERSION="1.1.0")
                parent = subprocess.Popen(machine.command(["upgrade", "--version", "1.1.0", "--json"]),
                    env=machine.env(extra), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                try:
                    wait_for(gate.exists)
                    if not running:
                        operation = json.loads((machine.k / "operation.json").read_text())
                        self.assertEqual(operation["targetVersion"], "1.1.0")
                        self.assertIsNone(operation["outcome"])
                        self.assertEqual(machine.binary.read_bytes(), self.server.releases["1.1.0"])
                    owner = json.loads((machine.state / "gate/upgrade.lock").read_text())["pid"]
                    self.assertNotIn(owner, (os.getpid(), parent.pid, 0, 1))
                    busy = machine.run(["status"])
                    self.assertEqual(busy.returncode, 2, busy.stdout + busy.stderr)
                    if WINDOWS:
                        subprocess.run(["taskkill", "/PID", str(owner), "/F"], check=True, capture_output=True)
                    else:
                        os.kill(owner, signal.SIGKILL)
                    gate.with_suffix(".release").touch()
                    stdout, stderr = parent.communicate(timeout=150)
                    self.assertEqual(parent.returncode, 1, stdout + stderr)
                    self.assertEqual(json.loads(stdout)["receipt"]["outcome"], "rolled-back")
                    self.assertEqual(machine.self_version(), "1.0.0")
                    self.assertEqual(machine.live() is not None, running)
                finally:
                    gate.with_suffix(".release").touch()
                    if parent.poll() is None:
                        parent.kill()
                        parent.communicate(timeout=10)

    def test_attended_first_setup_uses_a_real_terminal(self):
        machine = self.machine()
        environment = machine.env({"CI": "", "RAFT_COMPUTER_NON_INTERACTIVE": "0"})
        command = machine.command(["install", "--version", "1.0.0", "--yes"])
        if WINDOWS:
            child = subprocess.run(command, env=environment, creationflags=subprocess.CREATE_NEW_CONSOLE,
                capture_output=True, text=True, timeout=120)
            self.assertEqual(child.returncode, 0, child.stdout + child.stderr)
        else:
            import pty
            pid, terminal = pty.fork()
            if pid == 0:
                os.execve(command[0], command, environment)
            captured = bytearray()
            deadline = time.monotonic() + 120
            try:
                while time.monotonic() < deadline:
                    if select.select([terminal], [], [], 0.1)[0]:
                        try:
                            data = os.read(terminal, 8192)
                        except OSError:
                            break
                        if not data:
                            break
                        captured.extend(data)
                else:
                    os.kill(pid, signal.SIGKILL)
                    self.fail("attended native installer timed out")
                _, status = os.waitpid(pid, 0)
                self.assertEqual(os.waitstatus_to_exitcode(status), 0, captured.decode(errors="replace"))
            finally:
                os.close(terminal)
        self.assertEqual(machine.live()["version"], "1.0.0")
        self.assertTrue((machine.home / "fixture-login").exists())

    def test_default_path_update_is_idempotent(self):
        machine = self.machine()
        machine.install_dir = machine.home / ".local/bin"
        machine.install_dir.mkdir(parents=True)
        if WINDOWS:
            # GitHub's Windows job is a disposable account. Local verification
            # leaves the real user's registry untouched and reports this scope.
            if os.environ.get("GITHUB_ACTIONS") != "true":
                self.skipTest("Windows user PATH mutation is exercised only in the disposable CI account")
            shell = str(Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe")
            old = subprocess.check_output([shell, "-NoProfile", "-Command", "[Environment]::GetEnvironmentVariable('Path','User')"], text=True).strip()
            try:
                machine.json(["install", "--version", "1.0.0"], extra={"RAFT_COMPUTER_NO_MODIFY_PATH": "0"})
                machine.json(["upgrade", "--version", "1.1.0"], extra={"RAFT_COMPUTER_NO_MODIFY_PATH": "0"})
                new = subprocess.check_output([shell, "-NoProfile", "-Command", "[Environment]::GetEnvironmentVariable('Path','User')"], text=True).strip()
                self.assertEqual(new.lower().split(";").count(str(machine.install_dir).lower()), 1)
            finally:
                subprocess.run([shell, "-NoProfile", "-Command", "[Environment]::SetEnvironmentVariable('Path',$env:RCI_RESTORE_PATH,'User')"],
                    env={**os.environ, "RCI_RESTORE_PATH": old}, check=True)
        else:
            machine.json(["install", "--version", "1.0.0"])
            machine.json(["upgrade", "--version", "1.1.0"])
            profile = (machine.home / ".zshrc").read_text()
            self.assertEqual(profile.count('export PATH="$HOME/.local/bin:$PATH"'), 1)


if __name__ == "__main__":
    unittest.main()
