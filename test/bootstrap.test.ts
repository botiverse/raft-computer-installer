// The cases a person hits first, driven through the published entry point.
import assert from "node:assert/strict";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { after, before, describe, it } from "node:test";
import { Harness, NATIVE, WINDOWS, type Machine } from "./harness.ts";

const h = new Harness();
const machines: Machine[] = [];
const machine = () => { const m = h.machine(); machines.push(m); return m; };

before(async () => {
  await h.start();
  h.publish({ version: "1.0.0" });
  h.publish({ version: "1.1.0" });
  h.channel.main = "1.1.0";
  h.channel.alpha = "1.1.0";
});
after(async () => { for (const m of machines) await m.cleanup(); await h.stop(); });

/** The system's own PATH only: sh, curl, awk, shasum, and no node anywhere. */
function pathWithoutNode(): string {
  return WINDOWS ? `${process.env.SystemRoot ?? "C:\\Windows"}\\System32;${process.env.SystemRoot ?? "C:\\Windows"}\\System32\\WindowsPowerShell\\v1.0` : "/usr/bin:/bin:/usr/sbin:/sbin";
}

describe("clean install through the bootstrap", () => {
  it("installs the channel's current release", async () => {
    const m = machine();
    const r = await m.bootstrap([]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Installed 1\.1\.0\. Add .*bin to your PATH\. Next: run raft-computer login$/m);
    assert.equal(await m.selfVersion(), "1.1.0");
    assert.ok(existsSync(join(m.installDir, "photon_rs_bg.wasm")));
  });
  it("installs one exact version", async () => {
    const m = machine();
    const r = await m.bootstrap(["--version", "1.0.0"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Installed 1\.0\.0\./m);
    assert.equal(await m.selfVersion(), "1.0.0");
  });
  it("reads status through the bootstrap", async () => {
    const m = machine();
    const r = await m.bootstrap(["status"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Nothing is installed\.$/m);
  });
  it("refuses a tampered installer before anything changes", async () => {
    const m = machine();
    const sums = join(h.dist, "SHA256SUMS");
    const original = readFileSync(sums, "utf8");
    // Corrupt the checksum of the file the bootstrap will actually fetch.
    const fetched = NATIVE ? `raft-computer-installer${WINDOWS ? ".exe" : ""}` : "cli.cjs";
    const tampered = original.split("\n").map((line) => line.endsWith(`/${fetched}`) || line.endsWith(`  ${fetched}`) ? (line[0] === "0" ? "1" : "0") + line.slice(1) : line).join("\n");
    assert.notEqual(tampered, original, "a line to tamper with");
    writeFileSync(sums, tampered);
    try {
      const r = await m.bootstrap([]);
      assert.notEqual(r.code, 0);
      assert.match(r.stdout + r.stderr, /does not match its published checksum/);
      assert.equal(existsSync(m.binary), false);
    } finally {
      writeFileSync(sums, original);
    }
  });
});

describe("a machine without Node", () => {
  it(NATIVE ? "installs with the single executable, no Node anywhere" : "says plainly that it needs Node or a published executable", async () => {
    const m = machine();
    const r = await m.bootstrap([], { PATH: pathWithoutNode(), RAFT_COMPUTER_INSTALLER_NODE: "" });
    if (NATIVE) {
      assert.equal(r.code, 0, r.stdout + r.stderr);
      assert.match(r.stdout, /^Installed 1\.1\.0\./m);
    } else {
      assert.notEqual(r.code, 0);
      assert.match(r.stdout + r.stderr, /Node 24 or newer is required/);
      assert.equal(existsSync(m.binary), false);
    }
  });
});

describe("dirty state left by an earlier K", () => {
  const dirty: Array<[string, (k: string) => void]> = [
    ["a journal that is not JSON", (k) => writeFileSync(join(k, "journal.jsonl"), "{\"seq\":1}\n{garbage\n")],
    ["an operation record of an unknown shape", (k) => writeFileSync(join(k, "operation.json"), JSON.stringify({ formatVersion: 99, weird: true }))],
    ["a stable slot with no artifact", (k) => rmSync(join(k, "slots", "stable", "artifact.bin"))],
    ["a stable slot with an empty version", (k) => writeFileSync(join(k, "slots", "stable", "VERSION"), "")],
    ["files nobody recognises", (k) => { writeFileSync(join(k, "host-runner-hold.json"), "{\"held\":true}"); writeFileSync(join(k, "something.lock"), "x"); }],
  ];
  for (const [name, dirtify] of dirty) {
    it(`reinstalls over ${name}`, async () => {
      const m = machine();
      const first = await m.bootstrap(["--version", "1.0.0"]);
      assert.equal(first.code, 0, first.stdout + first.stderr);
      dirtify(m.kStateDir);
      const r = await m.bootstrap(["--version", "1.1.0"]);
      assert.equal(r.code, 0, r.stdout + r.stderr);
      assert.match(r.stdout, /^(Reinstalled|Upgraded 1\.0\.0 → ) ?1\.1\.0/m);
      assert.equal(await m.selfVersion(), "1.1.0");
    });
  }
  it("takes over a state directory that is only junk", async () => {
    const m = machine();
    mkdirSync(m.kStateDir, { recursive: true });
    writeFileSync(join(m.kStateDir, "operation.json"), "not even json");
    writeFileSync(join(m.kStateDir, "journal.jsonl"), "??");
    const r = await m.bootstrap(["--version", "1.0.0"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Reinstalled 1\.0\.0\. The previous installation was kept at /m);
    assert.equal(await m.selfVersion(), "1.0.0");
  });
});
