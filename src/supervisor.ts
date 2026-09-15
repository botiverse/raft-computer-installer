// Runs the K runner as a supervised child through K's own launcher: the
// runner bytes are verified before execution and retained for recovery.
//
// Two shapes. Under external Node the runner is runner.mjs beside the entry,
// run by that Node. Inside a single executable the runner is its own native
// binary, run directly; process.execPath would be this executable, so it is
// never used as an interpreter there.
import { createHash } from "node:crypto";
import { readFile, stat } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import type { Release, RunnerLaunchResult, RunnerRequest } from "@botiverse/k-carrier";
import { superviseRunner } from "@botiverse/k-carrier";
import { scratchDir, type Config } from "./config.js";
import { netFetch } from "./net.js";

function isSea(): boolean {
  try { return (require("node:sea") as { isSea(): boolean }).isSea(); } catch { return false; }
}

/** The runner as a Release: a local file, hashed, served through a file: URL. */
async function runnerRelease(cfg: Config): Promise<Release> {
  const bytes = await readFile(cfg.runnerPath);
  return { version: "runner", url: pathToFileURL(cfg.runnerPath).toString(), sha256: createHash("sha256").update(bytes).digest("hex"), size: bytes.length };
}

/** fetch that also serves file: URLs, so a runner beside the entry needs no network. */
async function localFetch(input: string | URL | Request, init?: RequestInit): Promise<Response> {
  const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
  if (!url.startsWith("file:")) return netFetch(input, init);
  const path = new URL(url);
  const size = (await stat(path)).size;
  const range = /^bytes=(\d+)-$/.exec(String((init?.headers as Record<string, string> | undefined)?.range ?? (init?.headers as Record<string, string> | undefined)?.Range ?? ""));
  const from = range ? Number(range[1]) : 0;
  const bytes = (await readFile(path)).subarray(from);
  return new Response(bytes, { status: from > 0 ? 206 : 200, headers: { "content-length": String(bytes.length), ...(from > 0 ? { "content-range": `bytes ${from}-${size - 1}/${size}` } : {}) } });
}

/** Native runner: run the file directly. Script runner: run it with Node. */
function interpreterFor(cfg: Config): string | undefined {
  if (cfg.runnerPath.endsWith(".mjs") || cfg.runnerPath.endsWith(".cjs") || cfg.runnerPath.endsWith(".js")) {
    if (isSea() && cfg.node === process.execPath) throw new Error("a script runner cannot be run from inside the single executable; set RAFT_COMPUTER_INSTALLER_NODE or ship the native runner");
    return cfg.node;
  }
  return undefined;
}

export async function runRunner(cfg: Config, request: RunnerRequest, env: NodeJS.ProcessEnv): Promise<RunnerLaunchResult> {
  const release = await runnerRelease(cfg);
  const interpreter = interpreterFor(cfg);
  const saved = { ...process.env };
  Object.assign(process.env, env);
  try {
    return await superviseRunner({ release, request, scratchDir: scratchDir(cfg), ...(interpreter ? { interpreter } : {}), fetchImpl: localFetch });
  } finally {
    for (const k of Object.keys(process.env)) if (!(k in saved)) delete process.env[k];
    Object.assign(process.env, saved);
  }
}
