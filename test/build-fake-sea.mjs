#!/usr/bin/env node
// Windows only: the fake Computer must be a real executable, so it is built
// once as a Node single executable. Per-version behaviour is a JSON marker
// appended to the bytes, which the fake reads from its own file.
import { build } from "esbuild";
import { execFileSync } from "node:child_process";
import { chmod, copyFile, mkdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";

export async function buildFakeSea(outDir, fakeSource) {
  await mkdir(outDir, { recursive: true });
  const cjs = join(outDir, "fake-computer.cjs");
  await build({ entryPoints: [fakeSource], bundle: true, platform: "node", format: "cjs", target: "node24", outfile: cjs, logLevel: "warning" });
  const blob = join(outDir, "fake-computer.blob");
  const config = join(outDir, "fake-computer.sea.json");
  await writeFile(config, JSON.stringify({ main: cjs, output: blob, disableExperimentalSEAWarning: true }));
  execFileSync(process.execPath, ["--experimental-sea-config", config], { stdio: "inherit" });
  const exe = join(outDir, process.platform === "win32" ? "fake-computer.exe" : "fake-computer");
  await rm(exe, { force: true });
  await copyFile(process.execPath, exe);
  await chmod(exe, 0o755);
  if (process.platform === "darwin") execFileSync("codesign", ["--remove-signature", exe], { stdio: "inherit" });
  const postject = ["postject", exe, "NODE_SEA_BLOB", blob, "--sentinel-fuse", "NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2",
    ...(process.platform === "darwin" ? ["--macho-segment-name", "NODE_SEA"] : [])];
  execFileSync(process.platform === "win32" ? "npx.cmd" : "npx", postject, { stdio: "inherit", shell: process.platform === "win32" });
  if (process.platform === "darwin") execFileSync("codesign", ["--sign", "-", exe], { stdio: "inherit" });
  return exe;
}
