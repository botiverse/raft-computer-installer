// The flows the installer owns outside K's transaction: fresh install,
// adoption, repair. Each ends with a live readback or a named failure.
import { rm } from "node:fs/promises";
import { bootstrapStable, quarantineState } from "@botiverse/k-carrier";
import { acquireRelease, acquireSidecar } from "./artifact.js";
import { attest, exists, firstSetup, selfReport, startComputer, statusHint, stopComputer, terminateProduct } from "./computer.js";
import type { Presence } from "./presence.js";
import { ensureOnPath } from "./shellPath.js";
import { quarantineDir, type Config } from "./config.js";
import { publishSlot } from "./hostAdapter.js";
import type { Manifest } from "./source.js";
import { plain, type Outcome } from "./report.js";
import { join } from "node:path";

/** What the product wants the user to do next, if it said so. */
export function nextStep(live: { nextStep: string | null } | null): string {
  return live?.nextStep ? ` Next: ${live.nextStep}` : "";
}

/** Installed means usable: the published bytes run and answer as the version. */
async function verifyPublished(cfg: Config, env: NodeJS.ProcessEnv, version: string): Promise<void> {
  const report = await selfReport(cfg.binaryPath, env);
  if (report.version !== version) throw new Error(`the installed program answers as ${report.version}, not ${version}`);
}

/**
 * After the bytes are in place: the product's first setup, then start and
 * read back. Attended, setup runs on the terminal. Unattended, there is
 * nobody to set it up, so the line says what to do next. A setup that is
 * declined or fails leaves a usable installation, not a failed one.
 */
async function setUpAndStart(cfg: Config, env: NodeJS.ProcessEnv, version: string, presence: Presence): Promise<{ tail: string; detail: Record<string, unknown> }> {
  const hostEnv = { ...env, RAFT_HOME: cfg.stateHome, SLOCK_HOME: cfg.stateHome };
  if (presence === "attended") {
    const done = await firstSetup(cfg.binaryPath, hostEnv);
    if (done) {
      const started = await startComputer(cfg.binaryPath, hostEnv).catch(() => null);
      if (started?.code === 0) {
        const deadline = Date.now() + 60_000;
        while (Date.now() < deadline) {
          const live = await attest(cfg.binaryPath, hostEnv).catch(() => null);
          if (live?.version === version) return { tail: " Set up and running.", detail: { live } };
          await new Promise((r) => setTimeout(r, 250));
        }
      }
      return { tail: " Set up, but it did not start; check with: raft-computer status", detail: { setUp: true, started: false } };
    }
  }
  const hint = await statusHint(cfg.binaryPath, hostEnv);
  return { tail: hint ? ` Next: ${hint}` : "", detail: { setUp: false, nextStep: hint } };
}

/** Fresh: verify, seed stable, publish, self-check. Nothing is started; that is the user's next step. */
export async function freshInstall(cfg: Config, m: Manifest, env: NodeJS.ProcessEnv, id: string, presence: Presence): Promise<Outcome> {
  let seeded = false;
  try {
    const artifact = await acquireRelease(cfg, m, env);
    await acquireSidecar(cfg, m);
    await bootstrapStable({ stateDir: cfg.kStateDir, version: m.version, artifactPath: artifact.path });
    seeded = true;
    await publishSlot(cfg, "stable");
    await verifyPublished(cfg, env, m.version);
    const path = await ensureOnPath(cfg, env);
    const after = await setUpAndStart(cfg, env, m.version, presence);
    return { code: 0, status: "installed", line: `Installed ${m.version}.${path}${after.tail}`, detail: after.detail };
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    if (seeded) {
      // Nothing was running before; the seeded slot is evidence, not a fallback.
      await stopComputer(cfg.binaryPath, env).catch(() => ({ forced: [] }));
      await quarantineState(cfg.kStateDir, { destination: quarantineDir(cfg, id), timestampMs: Date.now(), assertActiveHandoff: async () => {} }).catch(() => {});
    }
    return { code: 1, status: "failed", line: `Could not install ${m.version}: ${plain(reason)}. Nothing is installed.`, detail: { reason } };
  }
}

/** Adopted: seed stable from the running executable, then upgrade as managed. */
export async function adopt(cfg: Config, version: string): Promise<void> {
  await bootstrapStable({ stateDir: cfg.kStateDir, version, artifactPath: cfg.binaryPath });
}

/** Broken, with consent: stop, quarantine, reinstall, seed. Reported as a repair. */
export async function repair(cfg: Config, m: Manifest, env: NodeJS.ProcessEnv, id: string, presence: Presence): Promise<Outcome> {
  let quarantine: string | null = null;
  let forced: number[] = [];
  try {
    forced = (await stopComputer(cfg.binaryPath, env)).forced;
    const q = await quarantineState(cfg.kStateDir, {
      destination: quarantineDir(cfg, id), timestampMs: Date.now(),
      assertActiveHandoff: async () => { forced = [...forced, ...(await terminateProduct(cfg.binaryPath))]; },
    });
    quarantine = q.status === "not-found" ? null : q.quarantinePath;
    const artifact = await acquireRelease(cfg, m, env);
    await acquireSidecar(cfg, m);
    await bootstrapStable({ stateDir: cfg.kStateDir, version: m.version, artifactPath: artifact.path });
    await publishSlot(cfg, "stable");
    await verifyPublished(cfg, env, m.version);
    const after = await setUpAndStart(cfg, env, m.version, presence);
    return {
      code: 0, status: "repaired",
      line: `Reinstalled ${m.version}.${quarantine ? ` The previous installation was kept at ${quarantine}.` : ""}${after.tail}`,
      detail: { ...after.detail, quarantine, forcedStops: forced },
    };
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    return { code: 1, status: "failed", line: `Could not reinstall ${m.version}: ${plain(reason)}.${quarantine ? ` The previous installation was kept at ${quarantine}.` : " Nothing changed."}`, detail: { quarantine, reason } };
  }
}

export async function removeScratch(cfg: Config, version: string): Promise<void> {
  const { scratchDir } = await import("./config.js");
  await rm(join(scratchDir(cfg), `release-${version}`), { recursive: true, force: true });
}
