import assert from "node:assert/strict";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { after, before, describe, it } from "node:test";
import { Harness, type Machine } from "./harness.ts";

const h = new Harness();
const machines: Machine[] = [];
const machine = () => { const m = h.machine(); machines.push(m); return m; };

before(async () => {
  await h.start();
  h.publish({ version: "1.0.0" });
  h.publish({ version: "1.1.0" });
  h.publish({ version: "1.2.0", startFail: true });
  h.publish({ version: "1.3.0", reportedVersion: "9.9.9" });
  h.channel.main = "1.1.0";
});
after(async () => { for (const m of machines) await m.cleanup(); await h.stop(); });

describe("unattended", () => {
  it("installs the channel's current release without a version or --yes, and records the consent", async () => {
    const m = machine();
    const r = await m.installer(["install"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Installed 1\.1\.0\. Add .*\/bin to your PATH\. Next: run raft-computer login$/m);
    assert.equal(await m.live(), null, "nothing is started before login");
    assert.equal(await m.selfVersion(), "1.1.0");
    const { readdirSync } = await import("node:fs");
    const dir = join(m.home, "computer", "installer", "receipts");
    const receipt = JSON.parse(readFileSync(join(dir, readdirSync(dir)[0]), "utf8"));
    assert.match(receipt.approvedBy, /\(unattended\)$/);
    assert.equal(receipt.targetVersion, "1.1.0");
  });
  it("fails before any change when the authority cannot be reached", async () => {
    const m = machine();
    const r = await m.installer(["install"], { RAFT_COMPUTER_HANDS_ORIGIN: "http://127.0.0.1:9" });
    assert.equal(r.code, 1, r.stdout + r.stderr);
    assert.match(r.stdout, /^Could not install: the release server could not be reached\. Nothing changed\.$/m);
    assert.equal(existsSync(m.binary), false);
    assert.equal(existsSync(m.kStateDir), false);
  });
  it("holds an explicit repair on a machine that is not broken", async () => {
    const m = machine();
    const r = await m.installer(["repair", "--version", "1.0.0"]);
    assert.equal(r.code, 2, r.stdout + r.stderr);
    assert.match(r.stdout, /^Not done: nothing here needs repair\. Nothing changed\. Run install or upgrade instead\.$/m);
  });
});

describe("fresh, managed, replay, rollback, downgrade", () => {
  const m = h.machine(); machines.push(m);
  it("installs on a fresh machine: verify, seed stable, publish, self-check; unattended, nothing is set up or started", async () => {
    const r = await m.installer(["install", "--version", "1.0.0", "--yes"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Installed 1\.0\.0\. Add .*\/bin to your PATH\. Next: run raft-computer login$/m);
    assert.equal(await m.live(), null);
    assert.equal(await m.selfVersion(), "1.0.0");
    assert.equal(readFileSync(join(m.kStateDir, "slots", "stable", "VERSION"), "utf8").trim(), "1.0.0");
    assert.ok(existsSync(join(m.installDir, "photon_rs_bg.wasm")), "sidecar published beside the binary");
  });
  it("upgrades an installed machine where nothing runs: the new bytes must check out, nothing is started", async () => {
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--yes", "--id", "cold-1"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Upgraded 1\.0\.0 → 1\.1\.0\. Next: run raft-computer login$/m);
    assert.equal(await m.live(), null);
    assert.equal(await m.selfVersion(), "1.1.0");
  });
  it("rolls back a cold upgrade whose bytes answer as the wrong version", async () => {
    const r = await m.installer(["upgrade", "--version", "1.3.0", "--yes"]);
    assert.equal(r.code, 1, r.stdout + r.stderr);
    assert.match(r.stdout, /^1\.3\.0 did not check out correctly, so 1\.1\.0 was put back\.$/m);
    assert.equal(await m.selfVersion(), "1.1.0");
    const back = await m.installer(["upgrade", "--version", "1.0.0", "--yes", "--allow-downgrade"]);
    assert.equal(back.code, 0, back.stdout + back.stderr);
    await m.loginAndStart();
    assert.equal((await m.live())?.version, "1.0.0");
  });
  it("upgrades a managed machine through K and reports the live readback", async () => {
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--yes", "--id", "up-1"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Upgraded 1\.0\.0 → 1\.1\.0\. It is running\.$/m);
    assert.equal((await m.live())?.version, "1.1.0");
    const receipt = JSON.parse(readFileSync(join(m.home, "computer", "installer", "receipts", "up-1.json"), "utf8"));
    assert.equal(receipt.status, "promoted");
    assert.equal(receipt.presence, "unattended");
  });
  it("replays the same id without touching the service", async () => {
    const before = await m.live();
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--yes", "--id", "up-1"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Upgraded 1\.0\.0 → 1\.1\.0 earlier\. It is running\.$/m);
    assert.deepEqual(await m.live(), before);
  });
  it("says up to date when the target is what is running", async () => {
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--yes"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^1\.1\.0 is already installed\. Nothing to do\.$/m);
  });
  it("rolls back a candidate that will not start, exit 1, old version running", async () => {
    const r = await m.installer(["upgrade", "--version", "1.2.0", "--yes"]);
    assert.equal(r.code, 1, r.stdout + r.stderr);
    assert.match(r.stdout, /^1\.2\.0 did not start correctly, so 1\.1\.0 was put back and is running\.$/m);
    assert.equal((await m.live())?.version, "1.1.0");
  });
  it("fails before any change when the bytes report the wrong version", async () => {
    // K stages and probes; the fake reports 9.9.9, so the readback refuses and rolls back.
    const r = await m.installer(["upgrade", "--version", "1.3.0", "--yes"]);
    assert.equal(r.code, 1, r.stdout + r.stderr);
    assert.equal((await m.live())?.version, "1.1.0");
  });
  it("holds a downgrade unless it is intended", async () => {
    const r = await m.installer(["upgrade", "--version", "1.0.0", "--yes"]);
    assert.equal(r.code, 2, r.stdout + r.stderr);
    assert.match(r.stdout, /^Not done: 1\.0\.0 is older than the installed 1\.1\.0\. Nothing changed\. Add --allow-downgrade to install it anyway\.$/m);
    const r2 = await m.installer(["upgrade", "--version", "1.0.0", "--yes", "--allow-downgrade"]);
    assert.equal(r2.code, 0, r2.stdout + r2.stderr);
    assert.equal((await m.live())?.version, "1.0.0");
  });
  it("rolls back to the previous stable as an explicit target", async () => {
    const r = await m.installer(["rollback", "--yes"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.equal((await m.live())?.version, "1.1.0");
  });
  it("status reads the world and the live service", async () => {
    const r = await m.installer(["status"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^1\.1\.0 is installed and running\.$/m);
  });
});

describe("PATH", () => {
  it("puts the default install directory on PATH in the shell profile and says so", async () => {
    const m = machine();
    const installDir = join(m.home, ".local", "bin");
    const r = await m.installer(["install", "--version", "1.0.0"], { RAFT_COMPUTER_INSTALL_DIR: installDir });
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Installed 1\.0\.0\. Added .*\.local\/bin to PATH in .*\.zshrc; open a new terminal\. Next: run raft-computer login$/m);
    assert.match(readFileSync(join(m.home, ".zshrc"), "utf8"), /export PATH="\$HOME\/\.local\/bin:\$PATH"/);
    const again = await m.installer(["upgrade", "--version", "1.1.0"], { RAFT_COMPUTER_INSTALL_DIR: installDir });
    assert.equal(again.code, 0, again.stdout + again.stderr);
    assert.equal((readFileSync(join(m.home, ".zshrc"), "utf8").match(/raft-computer/g) ?? []).length, 1, "written once");
  });
});

describe("first setup", () => {
  it("attended, a fresh install logs in, starts and reads back", async () => {
    const m = machine();
    const { freshInstall } = await import("../src/install.ts");
    const { loadConfig } = await import("../src/config.ts");
    const { fetchManifest } = await import("../src/source.ts");
    const cfg = loadConfig(m.env());
    const outcome = await freshInstall(cfg, await fetchManifest(cfg, "1.0.0"), m.env(), "setup-1", "attended");
    assert.equal(outcome.code, 0, outcome.line);
    assert.match(outcome.line, /^Installed 1\.0\.0\. Add .*\/bin to your PATH\. Set up and running\.$/);
    assert.equal((await m.live())?.version, "1.0.0");
  });
});

describe("adopted and broken", () => {
  it("adopts a pre-K install, then upgrades it", async () => {
    const m = machine();
    await m.preinstall("1.0.0");
    assert.equal(existsSync(m.kStateDir), false);
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--yes"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Upgraded 1\.0\.0 → 1\.1\.0\. It is running\.$/m);
    assert.equal((await m.live())?.version, "1.1.0");
    assert.ok(existsSync(join(m.kStateDir, "slots", "stable")));
  });
  it("holds when another manager owns the binary", async () => {
    const m = machine();
    const { writeFileSync, chmodSync } = await import("node:fs");
    writeFileSync(m.binary, "#!/usr/bin/env node\nconsole.log('1.0.0')\n", { mode: 0o755 });
    chmodSync(m.binary, 0o755);
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--yes"]);
    assert.equal(r.code, 2, r.stdout + r.stderr);
    assert.match(r.stdout, /^Not done: .* was installed by npm; remove it, or let this installer's directory come first on PATH\. Nothing changed\.$/m);
  });
  it("repairs a broken machine: quarantine, fresh install, reported as a repair", async () => {
    const m = machine();
    const first = await m.installer(["install", "--version", "1.0.0"]);
    assert.equal(first.code, 0, first.stdout);
    rmSync(join(m.kStateDir, "slots", "stable", "artifact.bin"));
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--id", "repair-1"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Reinstalled 1\.1\.0\. The previous installation was kept at .*repair-1\. Next: run raft-computer login$/m);
    assert.equal(await m.selfVersion(), "1.1.0");
    assert.ok(existsSync(join(m.home, "computer", "installer", "quarantine", "repair-1", "slots", "stable", "VERSION")), "old state kept whole");
    const receipt = JSON.parse(readFileSync(join(m.home, "computer", "installer", "receipts", "repair-1.json"), "utf8"));
    assert.equal(receipt.status, "repaired");
    assert.equal(receipt.operation, "repair");
  });
});
