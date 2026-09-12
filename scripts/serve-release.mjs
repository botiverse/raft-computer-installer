#!/usr/bin/env node
// Serve a Computer build as a fake release, for driving the installer by
// hand against an existing machine:
//   node scripts/serve-release.mjs <path/to/raft-computer.js> <version> [<version>...]
// Prints the environment to export, then stays up until killed.
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, symlinkSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const [bin, ...versions] = process.argv.slice(2);
if (!bin || versions.length === 0) { console.error("usage: serve-release.mjs <raft-computer.js> <version>..."); process.exit(2); }
const pkgDir = resolve(dirname(bin), "..");
const platform = `${process.platform}-${process.arch}`;
const sha = (b) => createHash("sha256").update(b).digest("hex");
const trees = mkdtempSync(join(tmpdir(), "rci-serve-"));
const releases = new Map();
for (const v of versions) {
  const tree = join(trees, v);
  mkdirSync(tree, { recursive: true });
  cpSync(join(pkgDir, "dist"), join(tree, "dist"), { recursive: true });
  const pkg = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
  pkg.version = v;
  writeFileSync(join(tree, "package.json"), JSON.stringify(pkg, null, 2));
  symlinkSync(join(pkgDir, "node_modules"), join(tree, "node_modules"));
  releases.set(v, Buffer.from(`#!/bin/sh\nexec "${process.execPath}" "${join(tree, "dist", "raft-computer.js")}" "$@"\n`));
}
const wasm = Buffer.from("served sidecar");
const dist = join(root, "dist");
const server = createServer((req, res) => {
  const url = new URL(req.url ?? "/", "http://x");
  let m = /^\/computer\/([^/]+)\/(.+)$/.exec(url.pathname);
  if (m && releases.has(m[1])) {
    const bytes = releases.get(m[1]);
    if (m[2] === "manifest.json") return res.end(JSON.stringify({ name: "raft-computer", version: m[1], photonWasm: { file: "photon_rs_bg.wasm", sha256: sha(wasm), size: wasm.length }, targets: { [platform]: { file: "raft-computer", sha256: sha(bytes), size: bytes.length } } }));
    if (m[2] === "raft-computer") return res.end(bytes);
    if (m[2] === "photon_rs_bg.wasm") return res.end(wasm);
  }
  m = /^\/installer\/(.+)$/.exec(url.pathname);
  if (m && existsSync(join(dist, m[1]))) return res.end(readFileSync(join(dist, m[1])));
  if (url.pathname === "/computer/install.sh") return res.end(readFileSync(join(dist, "install.sh")));
  const h = /^\/public\/v2\/apps\/([^/]+)\/latest$/.exec(url.pathname);
  if (h) {
    const v = versions[versions.length - 1];
    const bytes = releases.get(v);
    const [p, a] = platform.split("-");
    return res.end(JSON.stringify({ build: { version: v }, assets: [{ platform: p, arch: a, variant: null, filetype: "binary", size_bytes: bytes.length, sha256: sha(bytes) }] }));
  }
  res.statusCode = 404; res.end();
});
server.listen(0, "127.0.0.1", () => {
  const base = `http://127.0.0.1:${server.address().port}`;
  console.log([`export RAFT_COMPUTER_RELEASE_BASE=${base}/computer`, `export RAFT_COMPUTER_HANDS_ORIGIN=${base}`, `export RAFT_COMPUTER_INSTALLER_RELEASE_BASE=${base}/installer`, `export RAFT_COMPUTER_INSTALL_URL=${base}/computer/install.sh`, `export RAFT_COMPUTER_INSTALLER_NODE=${process.execPath}`].join("\n"));
});
