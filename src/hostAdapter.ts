// K's HostAdapter over the Computer CLI. Every lifecycle call goes through
// the PATH-visible binary; slot bytes are published there before start, so
// nothing ever runs from inside a slot directory.
import { chmod, copyFile, mkdir, readFile, rename, rm } from "node:fs/promises";
import { join } from "node:path";
import type { HostAdapter, ProcessEvidence, Slot } from "@botiverse/k-carrier";
import { slotArtifactPath } from "@botiverse/k-carrier";
import { attest, exists, startComputer, stopComputer } from "./computer.js";
import { SIDECAR_NAME, sidecarDir, type Config } from "./config.js";

export interface HostDeps { env?: NodeJS.ProcessEnv; startReadyTimeoutMs?: number; pollMs?: number }

async function publishFile(source: string, target: string, mode: number): Promise<void> {
  await mkdir(join(target, ".."), { recursive: true });
  const staged = `${target}.${process.pid}.staged`;
  await rm(staged, { force: true });
  await copyFile(source, staged);
  await chmod(staged, mode);
  await rename(staged, target);
}

/** Copy a slot's bytes and matching sidecar onto PATH atomically. */
export async function publishSlot(cfg: Config, slot: Slot): Promise<void> {
  const artifact = slotArtifactPath(cfg.kStateDir, slot);
  await publishFile(artifact, cfg.binaryPath, 0o755);
  let version = "";
  try { version = (await readFile(join(cfg.kStateDir, "slots", slot, "VERSION"), "utf8")).trim(); } catch { /* no version file */ }
  const sidecar = version ? join(sidecarDir(cfg, version), SIDECAR_NAME) : "";
  if (sidecar && await exists(sidecar)) await publishFile(sidecar, cfg.sidecarPath, 0o644);
}

export function createHostAdapter(cfg: Config, deps: HostDeps = {}): HostAdapter {
  const env = { ...(deps.env ?? process.env), RAFT_HOME: cfg.stateHome, SLOCK_HOME: cfg.stateHome };
  const readyTimeout = deps.startReadyTimeoutMs ?? 60_000;
  const poll = deps.pollMs ?? 250;
  return {
    // Every controller call is awaited to completion; nothing is queued that
    // could land after the worker is gone. Acknowledge.
    async fence() {},
    // Computer parks and restores its own workloads across stop/start.
    async quiesce() {},
    async resume() {},
    async stop(_slot: Slot) { await stopComputer(cfg.binaryPath, env); },
    async start(slot: Slot) {
      // Never throw for a world-state failure: the probe is the judge, and a
      // throwing start would skip K's rollback path.
      try { await publishSlot(cfg, slot); } catch { return; }
      const started = await startComputer(cfg.binaryPath, env).catch(() => null);
      if (!started || started.code !== 0) return;
      const deadline = Date.now() + readyTimeout;
      while (Date.now() < deadline) {
        try { await attest(cfg.binaryPath, env); return; } catch { /* not up yet */ }
        await new Promise((r) => setTimeout(r, poll));
      }
    },
    async healthProbe(): Promise<ProcessEvidence> {
      return attest(cfg.binaryPath, env);
    },
  };
}
