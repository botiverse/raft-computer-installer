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

describe("an older version already installed", () => {
  it("installed by the old script, not running: the new version replaces it, nothing is started", async () => {
    const m = machine();
    await m.preinstall("1.0.0", { running: false });
    const r = await m.bootstrap([]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Upgraded 1\.0\.0 → 1\.1\.0\. Next: run raft-computer login$/m);
    assert.equal(await m.live(), null);
    assert.equal(await m.selfVersion(), "1.1.0");
  });
  it("installed by the old script, the same version: nothing to do", async () => {
    const m = machine();
    await m.preinstall("1.1.0", { running: false });
    const r = await m.bootstrap([]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^1\.1\.0 is already installed\. Nothing to do\.$/m);
  });
  it("installed by the old script and running: the new version comes back running", async () => {
    const m = machine();
    await m.preinstall("1.0.0");
    const r = await m.bootstrap(["--version", "1.1.0"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Upgraded 1\.0\.0 → 1\.1\.0\. It is running\.$/m);
    assert.equal((await m.live())?.version, "1.1.0");
  });
  it("the word upgrade on a machine with nothing installed simply installs", async () => {
    const m = machine();
    const r = await m.bootstrap(["upgrade", "--version", "1.0.0"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Installed 1\.0\.0\./m);
  });
  it("an explicit repair on a broken machine reinstalls it", async () => {
    const m = machine();
    const first = await m.bootstrap(["--version", "1.0.0"]);
    assert.equal(first.code, 0, first.stdout + first.stderr);
    rmSync(join(m.kStateDir, "slots", "stable", "artifact.bin"));
    const r = await m.bootstrap(["repair", "--version", "1.1.0"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Reinstalled 1\.1\.0\. The previous installation was kept at /m);
    assert.equal(await m.selfVersion(), "1.1.0");
  });
});

describe("bytes that do not check out", () => {
  it("a manifest whose sha256 does not match the download fails before any change", async () => {
    h.publish({ version: "1.2.0" });
    h.wrongSha.add("1.2.0");
    const m = machine();
    const r = await m.bootstrap(["--version", "1.2.0"]);
    assert.equal(r.code, 1, r.stdout + r.stderr);
    assert.match(r.stdout, /^Could not install 1\.2\.0: the downloaded bytes did not match the release's checksum\. Nothing is installed\.$/m);
    assert.equal(existsSync(m.binary), false);
    assert.equal(existsSync(m.kStateDir), false, "a failed fresh install leaves no state behind");
  });
  it("installs a named feature channel's current release", async () => {
    h.publish({ version: "1.2.0-fresh-install-flow.1" });
    h.channel["fresh-install-flow"] = "1.2.0-fresh-install-flow.1";
    const m = machine();
    const r = await m.bootstrap(["--channel", "fresh-install-flow"]);
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Installed 1\.2\.0-fresh-install-flow\.1\./m);
    assert.equal(await m.selfVersion(), "1.2.0-fresh-install-flow.1");
  });
  it("refuses a channel name that is not main, alpha, or a feature channel", async () => {
    const m = machine();
    const r = await m.bootstrap(["--channel", "Stable"]);
    assert.notEqual(r.code, 0);
    assert.match(r.stdout + r.stderr, /unknown channel Stable; expected main, alpha, or a feature channel name/);
    assert.equal(existsSync(m.binary), false);
  });
  it("an authority and a byte store that disagree fail before any download", async () => {
    h.publish({ version: "1.3.0" });
    h.authorityLies.add("1.3.0");
    h.channel.alpha = "1.3.0";
    const m = machine();
    const r = await m.bootstrap(["--channel", "alpha"]);
    assert.equal(r.code, 1, r.stdout + r.stderr);
    assert.match(r.stdout, /^Could not install: the release server and the download server disagree about 1\.3\.0\. Nothing changed\.$/m);
    assert.equal(existsSync(m.binary), false);
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
