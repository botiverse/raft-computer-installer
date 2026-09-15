#!/usr/bin/env node
// End to end against a real Computer build, everything but a live-service
// upgrade (that needs a login):
//
//   node scripts/e2e-real-computer.mjs <path/to/raft-computer.js>
//
// Serves two fake release versions of the given build from a local release
// base, then drives the bootstrap: fresh install, cold upgrade, rollback,
// a broken machine repaired, `raft-computer upgrade` through the bootstrap,
// and status. Uses temp homes only; touches nothing of yours.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const bin = process.argv[2];
if (!bin) { console.error("usage: e2e-real-computer.mjs <raft-computer.js>"); process.exit(2); }
const dist = join(root, "dist");
if (!existsSync(join(dist, "install.sh"))) { console.error("run `npm run build` first"); process.exit(2); }

const platform = `${process.platform}-${process.arch}`;
const sha = (b) => createHash("sha256").update(b).digest("hex");
const run = (cmd, args, env, opts = {}) => new Promise((res) => {
  const p = spawn(cmd, args, { env, stdio: ["ignore", "pipe", "pipe"] });
  let out = "", err = "";
  p.stdout.on("data", (d) => { out += d; });
  p.stderr.on("data", (d) => { err += d; });
  p.on("error", (e) => res({ code: 127, out, err: String(e) }));
  p.on("close", (code) => res({ code: code ?? 1, out, err }));
});

// Two versions of the same build. The portable build is not one file (it
// resolves node_modules beside it), so each version is a copy of the package
// with its own package.json version, and the published artifact is a wrapper
// that execs it. The SEA build would be one real file; this is the same
// contract exercised through the same installer.
const pkgDir = resolve(dirname(bin), "..");
const versions = ["9.0.0", "9.0.1"];
const releases = new Map();
const trees = mkdtempSync(join(tmpdir(), "rci-e2e-trees-"));
for (const v of versions) {
  const tree = join(trees, v);
  mkdirSync(tree, { recursive: true });
  cpSync(join(pkgDir, "dist"), join(tree, "dist"), { recursive: true });
  const pkg = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
  pkg.version = v;
  writeFileSync(join(tree, "package.json"), JSON.stringify(pkg, null, 2));
  const { symlinkSync } = await import("node:fs");
  symlinkSync(join(pkgDir, "node_modules"), join(tree, "node_modules"));
  releases.set(v, Buffer.from(`#!/bin/sh\nexec "${process.execPath}" "${join(tree, "dist", "raft-computer.js")}" "$@"\n`));
}
const realVersion = (await run(process.execPath, [bin, "--version"], { ...process.env, RAFT_HOME: mkdtempSync(join(tmpdir(), "rci-v-")) })).out.trim();
console.log(`build reports ${realVersion}; serving it as ${versions.join(" and ")}`);
const wasm = Buffer.from("e2e sidecar");

const server = createServer((req, res) => {
  const url = new URL(req.url ?? "/", "http://x");
  let m = /^\/computer\/([^/]+)\/(.+)$/.exec(url.pathname);
  if (m) {
    const bytes = releases.get(m[1]);
    if (!bytes) { res.statusCode = 404; return res.end(); }
    if (m[2] === "manifest.json") return res.end(JSON.stringify({ name: "raft-computer", version: m[1],
      photonWasm: { file: "photon_rs_bg.wasm", sha256: sha(wasm), size: wasm.length },
      targets: { [platform]: { file: "raft-computer", sha256: sha(bytes), size: bytes.length } } }));
    if (m[2] === "raft-computer") return res.end(bytes);
    if (m[2] === "photon_rs_bg.wasm") return res.end(wasm);
  }
  m = /^\/installer\/(.+)$/.exec(url.pathname);
  if (m) {
    const f = join(dist, m[1]);
    if (existsSync(f)) return res.end(readFileSync(f));
  }
  // Hands download surface for the installer app, as the bootstrap sees it.
  m = /^\/dl\/raft-computer-installer\/main\/([a-z0-9]+-[a-z0-9_]+)$/.exec(url.pathname);
  if (m) { res.statusCode = 302; res.setHeader("location", `/dl/raft-computer-installer/releases/e2e/${m[1]}`); return res.end(); }
  m = /^\/dl\/raft-computer-installer\/releases\/e2e\/([a-z0-9]+-[a-z0-9_]+)$/.exec(url.pathname);
  if (m) {
    const exe = m[1].startsWith("win32") ? ".exe" : "";
    const primary = `native/${m[1]}/raft-computer-installer${exe}`, runner = `${primary.replace(/\.exe$/, "")}-runner${exe}`;
    const kind = url.searchParams.get("kind");
    if (kind === "sha256sums") return res.end(readFileSync(join(dist, "SHA256SUMS"), "utf8").split("\n").filter((l) => l.endsWith(`  ${primary}`) || l.endsWith(`  ${runner}`)).join("\n") + "\n");
    const f = join(dist, kind === null ? primary : kind === "runner" ? runner : "");
    if (f !== dist && existsSync(f)) return res.end(readFileSync(f));
    res.statusCode = kind === null || kind === "runner" ? 404 : 400; return res.end();
  }
  if (url.pathname === "/computer/install.sh") return res.end(readFileSync(join(dist, "install.sh")));
  const h = /^\/public\/v2\/apps\/([^/]+)\/latest$/.exec(url.pathname);
  if (h) {
    const v = url.searchParams.get("channel") === "alpha" ? "9.0.1" : "9.0.0";
    const bytes = releases.get(v);
    const [p, a] = platform.split("-");
    return res.end(JSON.stringify({ build: { version: v }, assets: [{ platform: p, arch: a, variant: null, filetype: "binary", size_bytes: bytes.length, sha256: sha(bytes) }] }));
  }
  res.statusCode = 404; res.end();
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;

const home = mkdtempSync(join(tmpdir(), "rci-e2e-home-"));
const installDir = join(home, "bin");
const env = {
  ...process.env, HOME: home, RAFT_HOME: home, SLOCK_HOME: home, RAFT_COMPUTER_INSTALL_DIR: installDir,
  RAFT_COMPUTER_RELEASE_BASE: `${base}/computer`, RAFT_COMPUTER_HANDS_ORIGIN: base,
  RAFT_COMPUTER_INSTALLER_DL_BASE: `${base}/dl/raft-computer-installer`, RAFT_COMPUTER_INSTALLER_NODE: process.execPath,
  RAFT_COMPUTER_INSTALL_URL: `${base}/computer/install.sh`, RAFT_COMPUTER_NON_INTERACTIVE: "1",
};
const bootstrap = (args, extra = {}) => run("/bin/sh", [join(dist, "install.sh"), ...args], { ...env, ...extra });
const computer = (args) => run(join(installDir, "raft-computer"), args, env);
let failed = 0;
const step = async (name, r, expectCode, pattern) => {
  const ok = r.code === expectCode && (!pattern || pattern.test(r.out));
  console.log(`${ok ? "ok " : "FAIL"} ${name}\n     exit ${r.code}: ${r.out.trim().split("\n").pop() ?? ""}${ok ? "" : `\n${r.err.slice(-800)}`}`);
  if (!ok) failed += 1;
  return r;
};

try {
  await step("status on a fresh machine", await bootstrap(["status"]), 0, /^Nothing is installed\./m);
  await step("fresh install, unattended, channel main", await bootstrap([]), 0, /^Installed 9\.0\.0\. Add .* to your PATH\. Next: run raft-computer login$/m);
  await step("installed binary answers", await computer(["--version"]), 0, /^9\.0\.0$/m);
  await step("status --json without a service carries nextStep", await computer(["status", "--json"]), 0, /"nextStep":"run raft-computer login"/);
  await step("cold upgrade to 9.0.1", await bootstrap(["upgrade", "--version", "9.0.1"]), 0, /^Upgraded 9\.0\.0 → 9\.0\.1\. Next: run raft-computer login$/m);
  await step("binary is 9.0.1", await computer(["--version"]), 0, /^9\.0\.1$/m);
  await step("same again is up to date", await bootstrap(["upgrade", "--version", "9.0.1"]), 0, /already installed/);
  await step("downgrade is held", await bootstrap(["upgrade", "--version", "9.0.0"]), 2, /^Not done: 9\.0\.0 is older/m);
  await step("back to 9.0.0 as an intended downgrade", await bootstrap(["upgrade", "--version", "9.0.0", "--allow-downgrade"]), 0, /^Upgraded 9\.0\.1 → 9\.0\.0\./m);
  await step("raft-computer upgrade runs the bootstrap", await computer(["upgrade", "--target-version", "9.0.1"]), 0, /^Upgraded 9\.0\.0 → 9\.0\.1\./m);
  await step("status reads the world", await bootstrap(["status"]), 0, /^9\.0\.1 is installed but not running\./m);
  rmSync(join(home, "computer", "k", "slots", "stable", "artifact.bin"));
  await step("broken machine is repaired", await bootstrap(["upgrade", "--version", "9.0.1"], { RAFT_COMPUTER_OPERATION_ID: "e2e-repair" }), 0, /^Reinstalled 9\.0\.1\. The previous installation was kept at .*e2e-repair\. Next: run raft-computer login$/m);
  await step("binary after repair", await computer(["--version"]), 0, /^9\.0\.1$/m);
} finally {
  server.close();
  rmSync(home, { recursive: true, force: true });
  rmSync(trees, { recursive: true, force: true });
}
console.log(failed ? `\n${failed} step(s) failed` : "\nall steps passed");
process.exit(failed ? 1 : 0);
