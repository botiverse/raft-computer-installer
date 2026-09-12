// The real thing: the real release authority, the real CDN, the real Raft
// Computer binaries, through the built bootstrap, in temp homes. Needs the
// network and downloads a few hundred megabytes; run with RCI_REAL=1.
//
// Nobody is logged in, so every path here is the cold one: install, adopt
// and upgrade, an intended downgrade, and a reinstall over records K cannot
// read. What runs after a login is the live path, covered by the fake.
import assert from "node:assert/strict";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { after, before, describe, it } from "node:test";
import { Harness, NATIVE, WINDOWS, type Machine } from "./harness.ts";

const REAL = process.env.RCI_REAL === "1";
const HANDS = "https://hands.build";
const CDN = "https://cdn.raft.build/computer";
const h = new Harness();
const machines: Machine[] = [];
const machine = () => { const m = h.machine(); machines.push(m); return m; };
let current = "";
let older = "";

async function versionExists(v: string): Promise<boolean> {
  const r = await fetch(`${CDN}/${v}/manifest.json`).catch(() => null);
  return r?.ok === true;
}

/** Bootstrap against the real world: only the installer itself is served locally. */
function real(m: Machine, args: string[], extra: NodeJS.ProcessEnv = {}) {
  return m.bootstrap(args, { RAFT_COMPUTER_RELEASE_BASE: CDN, RAFT_COMPUTER_HANDS_ORIGIN: HANDS, RAFT_COMPUTER_INSTALL_URL: "", ...extra });
}

describe("the real Raft Computer through the real bootstrap", { skip: !REAL && "set RCI_REAL=1; needs the network and downloads real releases" }, () => {
  before(async () => {
    await h.start();
    const latest = await (await fetch(`${HANDS}/public/v2/apps/raft-computer-cli/latest?channel=main&product_type=cli-binary`)).json() as { build: { version: string } };
    current = latest.build.version;
    // The newest published version below the current one, to upgrade from.
    const [maj, min, pat] = current.split(".").map(Number);
    for (let p = pat - 1; p >= Math.max(0, pat - 6) && !older; p--) if (await versionExists(`${maj}.${min}.${p}`)) older = `${maj}.${min}.${p}`;
    assert.ok(older, `an older published version below ${current}`);
  });
  after(async () => { for (const m of machines) await m.cleanup(); await h.stop(); });

  it("installs the channel's current release from Hands and the CDN", async () => {
    const m = machine();
    const r = await real(m, []);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, new RegExp(`^Installed ${current.replace(/\\./g, "\\\\.")}\\.`, "m"));
    assert.equal(await m.selfVersion(), current);
    assert.ok(existsSync(join(m.installDir, "photon_rs_bg.wasm")), "the sidecar is published beside the binary");
    assert.equal(readFileSync(join(m.kStateDir, "slots", "stable", "VERSION"), "utf8").trim(), current);
  });

  it("installs one exact older version, then upgrades to the current one, not running", async () => {
    const m = machine();
    const first = await real(m, ["--version", older]);
    assert.equal(first.code, 0, first.stdout + first.stderr);
    assert.equal(await m.selfVersion(), older);
    const r = await real(m, []);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, new RegExp(`^Upgraded ${older.replace(/\\./g, "\\\\.")} → ${current.replace(/\\./g, "\\\\.")}\\.`, "m"));
    assert.equal(await m.selfVersion(), current);
    const again = await real(m, []);
    assert.match(again.stdout, /already installed\. Nothing to do\./);
  });

  it("holds a downgrade, then does it when intended", async () => {
    const m = machine();
    const first = await real(m, ["--version", current]);
    assert.equal(first.code, 0, first.stdout + first.stderr);
    const held = await real(m, ["--version", older]);
    assert.equal(held.code, 2, held.stdout + held.stderr);
    assert.match(held.stdout, /^Not done: .* is older than the installed /m);
    const back = await real(m, ["--version", older, "--allow-downgrade"]);
    assert.equal(back.code, 0, back.stdout + back.stderr);
    assert.equal(await m.selfVersion(), older);
  });

  it("adopts a Computer the old script installed, then upgrades it", async () => {
    // What the old install.sh left behind: the binary and the sidecar on PATH, no K state.
    const donor = machine();
    const first = await real(donor, ["--version", older]);
    assert.equal(first.code, 0, first.stdout + first.stderr);
    const m = machine();
    mkdirSync(m.installDir, { recursive: true });
    const { copyFileSync } = await import("node:fs");
    copyFileSync(donor.binary, m.binary);
    copyFileSync(join(donor.installDir, "photon_rs_bg.wasm"), join(m.installDir, "photon_rs_bg.wasm"));
    const status = await real(m, ["status"]);
    assert.match(status.stdout, new RegExp(`^${older.replace(/\\./g, "\\\\.")} is installed \\(not yet managed by this installer\\)\\.$`, "m"));
    const r = await real(m, []);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, new RegExp(`^Upgraded ${older.replace(/\\./g, "\\\\.")} → ${current.replace(/\\./g, "\\\\.")}\\.`, "m"));
    assert.equal(await m.selfVersion(), current);
  });

  it("reinstalls over records K cannot read, keeping them aside", async () => {
    const m = machine();
    const first = await real(m, ["--version", older]);
    assert.equal(first.code, 0, first.stdout + first.stderr);
    writeFileSync(join(m.kStateDir, "journal.jsonl"), "{\"seq\":1}\n{garbage\n");
    writeFileSync(join(m.kStateDir, "operation.json"), "not json");
    const r = await real(m, []);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, new RegExp(`^Reinstalled ${current.replace(/\\./g, "\\\\.")}\\. The previous installation was kept at `, "m"));
    assert.equal(await m.selfVersion(), current);
  });
});
