// Fetch and prove a release before it goes anywhere near PATH: bytes match
// the manifest, the executable matches this machine, and it says it is the
// version we asked for.
import { chmod, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { artifactTransferTimeouts, downloadVerified, type Release } from "@botiverse/k-carrier";

/** Budgets derived from the size, as K's own runner does: a 150 MB binary is not a 10 second download. */
function budgets(release: Release, resumeDir: string) {
  const t = artifactTransferTimeouts(release.size);
  return { resumeDir, timeoutMs: t.overallTimeoutMs, responseTimeoutMs: t.responseTimeoutMs, idleTimeoutMs: t.idleTimeoutMs };
}
import { selfVersion } from "./computer.js";
import { BIN_NAME, SIDECAR_NAME, scratchDir, sidecarDir, type Config } from "./config.js";
import type { Manifest } from "./source.js";
import { releaseOf, sidecarReleaseOf } from "./source.js";

/** Native executables must match the host; scripts are platform-neutral. */
export function platformMismatch(bytes: Uint8Array): string | null {
  const b = Buffer.from(bytes.buffer, bytes.byteOffset, Math.min(bytes.length, 32));
  if (b.length < 4) return "file too short to be an executable";
  const want = `${process.platform}-${process.arch}`;
  if (b[0] === 0x7f && b[1] === 0x45 && b[2] === 0x4c && b[3] === 0x46) {
    if (process.platform !== "linux") return `ELF executable on ${want}`;
    const machine = b.readUInt16LE(18);
    const arch = machine === 0x3e ? "x64" : machine === 0xb7 ? "arm64" : `machine ${machine}`;
    return arch === process.arch ? null : `linux-${arch} executable on ${want}`;
  }
  const magic = b.readUInt32LE(0);
  if (magic === 0xfeedfacf || magic === 0xfeedface || magic === 0xcafebabe || magic === 0xbebafeca) {
    if (process.platform !== "darwin") return `Mach-O executable on ${want}`;
    if (magic === 0xcafebabe || magic === 0xbebafeca) return null; // universal
    const cpu = b.readUInt32LE(4);
    const arch = cpu === 0x0100000c ? "arm64" : cpu === 0x01000007 ? "x64" : `cputype ${cpu}`;
    return arch === process.arch ? null : `darwin-${arch} executable on ${want}`;
  }
  if (b[0] === 0x4d && b[1] === 0x5a) return process.platform === "win32" ? null : `Windows executable on ${want}`;
  if (b[0] === 0x23 && b[1] === 0x21) return null; // #! script
  return "not a recognised executable";
}

export interface VerifiedArtifact { path: string; version: string }

/** Download, verify, platform-check and self-version-check one release. */
export async function acquireRelease(cfg: Config, m: Manifest, env: NodeJS.ProcessEnv): Promise<VerifiedArtifact> {
  const dir = join(scratchDir(cfg), `release-${m.version}`);
  await mkdir(dir, { recursive: true });
  const release: Release = releaseOf(m);
  const bytes = await downloadVerified(release, budgets(release, dir));
  const mismatch = platformMismatch(bytes);
  if (mismatch) throw new Error(`downloaded ${m.version} is ${mismatch}`);
  const path = join(dir, BIN_NAME);
  await rm(path, { force: true });
  await writeFile(path, bytes, { mode: 0o755 });
  await chmod(path, 0o755);
  const reported = await selfVersion(path, env);
  if (reported !== m.version) throw new Error(`downloaded ${m.version} reports version ${reported ?? "(none)"}`);
  return { path, version: m.version };
}

/** The sidecar for a version, verified, kept beside the installer state. */
export async function acquireSidecar(cfg: Config, m: Manifest): Promise<string | null> {
  const release = sidecarReleaseOf(m);
  if (!release) return null;
  const dir = sidecarDir(cfg, m.version);
  const path = join(dir, SIDECAR_NAME);
  try {
    const existing = await readFile(path);
    const { createHash } = await import("node:crypto");
    if (existing.length === release.size && createHash("sha256").update(existing).digest("hex") === release.sha256) return path;
  } catch { /* not there yet */ }
  await mkdir(dir, { recursive: true });
  const bytes = await downloadVerified(release, budgets(release, dir));
  await writeFile(`${path}.tmp`, bytes, { mode: 0o644 });
  const { rename } = await import("node:fs/promises");
  await rename(`${path}.tmp`, path);
  return path;
}
