// K's HostAdapter over the Computer CLI. Every lifecycle call goes through
// the PATH-visible binary; slot bytes are published there before start, so
// nothing ever runs from inside a slot.
//
// Two modes, decided once at quiesce and remembered on disk for recovery:
// a service that was running is stopped and must come back as a live
// service; a machine where nothing runs (installed, not logged in) is
// verified by running the candidate itself. Either way the readback comes
// from a process of the new bytes.
import { chmod, copyFile, mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { HostAdapter, ProcessEvidence, Slot } from "@botiverse/k-carrier";
import { slotArtifactPath } from "@botiverse/k-carrier";
import { attest, exists, isLive, selfReport, startComputer, stopComputer } from "./computer.js";
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

function modePath(cfg: Config): string { return join(cfg.installerDir, "host-mode.json"); }

export interface HostRecord { mode: "service" | "cold"; forcedStops: number[] }

export async function readHostRecord(cfg: Config): Promise<HostRecord | null> {
  try {
    const parsed = JSON.parse(await readFile(modePath(cfg), "utf8")) as Partial<HostRecord>;
    if (parsed.mode !== "service" && parsed.mode !== "cold") return null;
    return { mode: parsed.mode, forcedStops: Array.isArray(parsed.forcedStops) ? parsed.forcedStops.filter((p): p is number => typeof p === "number") : [] };
  } catch { return null; }
}
export async function readMode(cfg: Config): Promise<"service" | "cold" | null> { return (await readHostRecord(cfg))?.mode ?? null; }

async function writeHostRecord(cfg: Config, record: HostRecord): Promise<void> {
  await mkdir(cfg.installerDir, { recursive: true });
  const tmp = `${modePath(cfg)}.${process.pid}.tmp`;
  await writeFile(tmp, JSON.stringify(record));
  await rename(tmp, modePath(cfg));
}
const writeMode = (cfg: Config, mode: "service" | "cold") => writeHostRecord(cfg, { mode, forcedStops: [] });

export function createHostAdapter(cfg: Config, deps: HostDeps = {}): HostAdapter {
  const env = { ...(deps.env ?? process.env), RAFT_HOME: cfg.stateHome, SLOCK_HOME: cfg.stateHome };
  const readyTimeout = deps.startReadyTimeoutMs ?? 60_000;
  const poll = deps.pollMs ?? 250;
  let mode: "service" | "cold" | null = null;
  const currentMode = async (): Promise<"service" | "cold"> => {
    mode ??= await readMode(cfg);
    if (mode === null) { mode = (await isLive(cfg.binaryPath, env)) ? "service" : "cold"; await writeMode(cfg, mode); }
    return mode;
  };
  return {
    // Every controller call is awaited to completion; nothing is queued that
    // could land after the worker is gone. Acknowledge.
    async fence() {},
    // Decide the mode from what is running now, before anything is stopped,
    // and remember it so a recovery driver stops and starts the same way.
    async quiesce() {
      mode = (await isLive(cfg.binaryPath, env)) ? "service" : "cold";
      await writeMode(cfg, mode);
    },
    async resume() {},
    async stop(_slot: Slot) {
      if ((await currentMode()) !== "service") return;
      const { forced } = await stopComputer(cfg.binaryPath, env);
      if (forced.length) {
        // The receipt says the installer had to finish the stop itself.
        const record = (await readHostRecord(cfg)) ?? { mode: "service" as const, forcedStops: [] };
        await writeHostRecord(cfg, { ...record, forcedStops: [...record.forcedStops, ...forced] });
      }
    },
    async start(slot: Slot) {
      // Never throw for a world-state failure: the probe is the judge, and a
      // throwing start would skip K's rollback path.
      try { await publishSlot(cfg, slot); } catch { return; }
      if ((await currentMode()) !== "service") return;
      const started = await startComputer(cfg.binaryPath, env).catch(() => null);
      if (!started || started.code !== 0) return;
      const deadline = Date.now() + readyTimeout;
      while (Date.now() < deadline) {
        try { await attest(cfg.binaryPath, env); return; } catch { /* not up yet */ }
        await new Promise((r) => setTimeout(r, poll));
      }
    },
    async healthProbe(): Promise<ProcessEvidence> {
      return (await currentMode()) === "service" ? attest(cfg.binaryPath, env) : selfReport(cfg.binaryPath, env);
    },
  };
}
