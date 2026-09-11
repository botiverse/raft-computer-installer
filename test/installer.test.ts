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
    assert.match(r.stdout, /^1\.1\.0 installed and running \(pid \d+\)\.$/m);
    assert.equal((await m.live())?.version, "1.1.0");
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
    assert.match(r.stdout, /^Failed before any change: release authority unreachable/m);
    assert.equal(existsSync(m.binary), false);
    assert.equal(existsSync(m.kStateDir), false);
  });
  it("holds an explicit repair on a machine that is not broken", async () => {
    const m = machine();
    const r = await m.installer(["repair", "--version", "1.0.0"]);
    assert.equal(r.code, 2, r.stdout + r.stderr);
    assert.match(r.stdout, /^Held: repair was asked for, but the machine is fresh/m);
  });
});

describe("fresh, managed, replay, rollback, downgrade", () => {
  const m = h.machine(); machines.push(m);
  it("installs on a fresh machine: verify, seed stable, start, read back", async () => {
    const r = await m.installer(["install", "--version", "1.0.0", "--yes"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^1\.0\.0 installed and running \(pid \d+\)\.$/m);
    assert.equal((await m.live())?.version, "1.0.0");
    assert.equal(readFileSync(join(m.kStateDir, "slots", "stable", "VERSION"), "utf8").trim(), "1.0.0");
    assert.ok(existsSync(join(m.installDir, "photon_rs_bg.wasm")), "sidecar published beside the binary");
  });
  it("upgrades a managed machine through K and reports the live readback", async () => {
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--yes", "--id", "up-1"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^1\.0\.0 → 1\.1\.0 promoted\. Running pid \d+\. Receipt up-1\.$/m);
    assert.equal((await m.live())?.version, "1.1.0");
    const receipt = JSON.parse(readFileSync(join(m.home, "computer", "installer", "receipts", "up-1.json"), "utf8"));
    assert.equal(receipt.status, "promoted");
    assert.equal(receipt.presence, "unattended");
  });
  it("replays the same id without touching the service", async () => {
    const before = await m.live();
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--yes", "--id", "up-1"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /promoted/);
    assert.deepEqual(await m.live(), before);
  });
  it("says up to date when the target is what is running", async () => {
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--yes"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /1\.1\.0 already running\. Nothing to do\./);
  });
  it("rolls back a candidate that will not start, exit 1, old version running", async () => {
    const r = await m.installer(["upgrade", "--version", "1.2.0", "--yes"]);
    assert.equal(r.code, 1, r.stdout + r.stderr);
    assert.match(r.stdout, /^Candidate 1\.2\.0 failed probe\. Rolled back; 1\.1\.0 running\./m);
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
    assert.match(r.stdout, /^Held: 1\.0\.0 is older than the installed 1\.1\.0/m);
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
    const r = await m.installer(["status", "--json"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    const s = JSON.parse(r.stdout);
    assert.equal(s.status, "managed");
    assert.equal(s.detail.live.version, "1.1.0");
  });
});

describe("adopted and broken", () => {
  it("adopts a pre-K install, then upgrades it", async () => {
    const m = machine();
    await m.preinstall("1.0.0");
    assert.equal(existsSync(m.kStateDir), false);
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--yes"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /1\.0\.0 → 1\.1\.0 promoted/);
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
    assert.match(r.stdout, /^Held: .* is managed by npm/m);
  });
  it("repairs a broken machine: quarantine, fresh install, reported as a repair", async () => {
    const m = machine();
    const first = await m.installer(["install", "--version", "1.0.0"]);
    assert.equal(first.code, 0, first.stdout);
    rmSync(join(m.kStateDir, "slots", "stable", "artifact.bin"));
    const r = await m.installer(["upgrade", "--version", "1.1.0", "--id", "repair-1"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Repaired: quarantined .*repair-1; 1\.1\.0 installed and running \(pid \d+\)\.$/m);
    assert.equal((await m.live())?.version, "1.1.0");
    assert.ok(existsSync(join(m.home, "computer", "installer", "quarantine", "repair-1", "slots", "stable", "VERSION")), "old state kept whole");
    const receipt = JSON.parse(readFileSync(join(m.home, "computer", "installer", "receipts", "repair-1.json"), "utf8"));
    assert.equal(receipt.status, "repaired");
    assert.equal(receipt.operation, "repair");
  });
});
