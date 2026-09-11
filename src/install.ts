// The flows the installer owns outside K's transaction: fresh install,
// adoption, repair. Each ends with a live readback or a named failure.
import { rm } from "node:fs/promises";
import { bootstrapStable, quarantineState } from "@botiverse/k-carrier";
import { acquireRelease, acquireSidecar } from "./artifact.js";
import { attest, exists, runCommand, stopComputer } from "./computer.js";
import { quarantineDir, type Config } from "./config.js";
import { createHostAdapter } from "./hostAdapter.js";
import type { Manifest } from "./source.js";
import { plain, type Outcome } from "./report.js";
import { readFile } from "node:fs/promises";
import { join } from "node:path";

async function readback(cfg: Config, env: NodeJS.ProcessEnv, version: string, timeoutMs = 60_000): Promise<{ pid: number; startId: string; nextStep: string | null }> {
  const deadline = Date.now() + timeoutMs;
  let last = "no answer";
  while (Date.now() < deadline) {
    try {
      const a = await attest(cfg.binaryPath, env);
      if (a.version === version) return { pid: a.pid, startId: a.startId, nextStep: a.nextStep };
      last = `live service reports ${a.version}`;
    } catch (error) { last = error instanceof Error ? error.message : String(error); }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(last);
}

/** What the product wants the user to do next, if it said so. */
export function nextStep(live: { nextStep: string | null } | null): string {
  return live?.nextStep ? ` Next: ${live.nextStep}` : "";
}

/** Fresh: verify, seed stable, start, probe. No transaction, no rollback. */
export async function freshInstall(cfg: Config, m: Manifest, env: NodeJS.ProcessEnv, id: string): Promise<Outcome> {
  let seeded = false;
  try {
    const artifact = await acquireRelease(cfg, m, env);
    await acquireSidecar(cfg, m);
    await bootstrapStable({ stateDir: cfg.kStateDir, version: m.version, artifactPath: artifact.path });
    seeded = true;
    await createHostAdapter(cfg, { env }).start("stable");
    const live = await readback(cfg, env, m.version);
    return { code: 0, status: "installed", line: `Installed ${m.version}. It is running.${nextStep(live)}`, detail: live };
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    if (seeded) {
      // Nothing was running before; the seeded slot is evidence, not a fallback.
      await stopComputer(cfg.binaryPath, env).catch(() => {});
      await quarantineState(cfg.kStateDir, { destination: quarantineDir(cfg, id), timestampMs: Date.now(), assertActiveHandoff: async () => {} }).catch(() => {});
    }
    return { code: 1, status: "failed", line: `Could not install ${m.version}: ${plain(reason)}. Nothing is installed.`, detail: { reason } };
  }
}

/** Adopted: seed stable from the running executable, then upgrade as managed. */
export async function adopt(cfg: Config, version: string): Promise<void> {
  await bootstrapStable({ stateDir: cfg.kStateDir, version, artifactPath: cfg.binaryPath });
}

/** Stop an unresponsive Computer only by an identity we recorded ourselves. */
async function stopByRecordedIdentity(cfg: Config): Promise<void> {
  let pid = 0;
  try { pid = Number((await readFile(join(cfg.stateHome, "computer", "run", "service.pid"), "utf8")).trim()); } catch { return; }
  if (!Number.isInteger(pid) || pid <= 1) return;
  try { process.kill(pid, 0); } catch { return; }
  const ps = await runCommand("ps", ["-p", String(pid), "-o", "command="], process.env, 10_000).catch(() => null);
  if (!ps || !ps.stdout.includes(cfg.binaryPath)) return; // not provably ours: leave it alone
  process.kill(pid, "SIGTERM");
  for (let i = 0; i < 60; i++) {
    await new Promise((r) => setTimeout(r, 500));
    try { process.kill(pid, 0); } catch { return; }
  }
  throw new Error(`service process ${pid} did not stop`);
}

/** Broken, with consent: stop, quarantine, reinstall, seed. Reported as a repair. */
export async function repair(cfg: Config, m: Manifest, env: NodeJS.ProcessEnv, id: string): Promise<Outcome> {
  let quarantine: string | null = null;
  try {
    if (await exists(cfg.binaryPath)) await stopComputer(cfg.binaryPath, env).catch(() => {});
    await stopByRecordedIdentity(cfg);
    const q = await quarantineState(cfg.kStateDir, {
      destination: quarantineDir(cfg, id), timestampMs: Date.now(),
      assertActiveHandoff: async () => { await stopByRecordedIdentity(cfg); },
    });
    quarantine = q.status === "not-found" ? null : q.quarantinePath;
    const artifact = await acquireRelease(cfg, m, env);
    await acquireSidecar(cfg, m);
    await bootstrapStable({ stateDir: cfg.kStateDir, version: m.version, artifactPath: artifact.path });
    await createHostAdapter(cfg, { env }).start("stable");
    const live = await readback(cfg, env, m.version);
    return {
      code: 0, status: "repaired",
      line: `Reinstalled ${m.version}. It is running.${quarantine ? ` The previous installation was kept at ${quarantine}.` : ""}${nextStep(live)}`,
      detail: { ...live, quarantine },
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
