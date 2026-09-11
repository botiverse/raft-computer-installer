// Runs the K runner as a supervised child through K's own launcher: the
// runner bytes are verified before execution and retained for recovery.
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import type { Release, RunnerLaunchResult, RunnerRequest } from "@botiverse/k-carrier";
import { resumeRunner, superviseRunner } from "@botiverse/k-carrier";
import { scratchDir, type Config } from "./config.js";

async function runnerRelease(cfg: Config): Promise<Release> {
  const bytes = await readFile(cfg.runnerPath);
  return {
    version: "runner",
    url: `data:application/octet-stream;base64,${bytes.toString("base64")}`,
    sha256: createHash("sha256").update(bytes).digest("hex"),
    size: bytes.length,
  };
}

export async function runRunner(cfg: Config, request: RunnerRequest, env: NodeJS.ProcessEnv): Promise<RunnerLaunchResult> {
  const release = await runnerRelease(cfg);
  const saved = { ...process.env };
  Object.assign(process.env, env);
  try {
    return await superviseRunner({ release, request, scratchDir: scratchDir(cfg), interpreter: cfg.node });
  } finally {
    for (const k of Object.keys(process.env)) if (!(k in saved)) delete process.env[k];
    Object.assign(process.env, saved);
  }
}

export async function resumeRecovery(recoveryFile: string): Promise<RunnerLaunchResult> {
  return resumeRunner(recoveryFile, {});
}
