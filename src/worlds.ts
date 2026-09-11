// Read the machine once, after settling. Five answers, and "held" for the
// two things that are not a world: another installer running, or another
// manager owning the binary.
import { readFile, stat } from "node:fs/promises";
import { join } from "node:path";
import type { OperationRead } from "@botiverse/k-carrier";
import { exists, foreignManager, selfVersion } from "./computer.js";
import type { Config } from "./config.js";

export type World =
  | { kind: "fresh" }
  | { kind: "adopted"; version: string }
  | { kind: "managed"; version: string; operation: OperationRead }
  | { kind: "broken"; reason: string; operation: OperationRead | null }
  | { kind: "held"; reason: string };

async function stableVersion(cfg: Config): Promise<string | null> {
  const dir = join(cfg.kStateDir, "slots", "stable");
  try {
    const version = (await readFile(join(dir, "VERSION"), "utf8")).trim();
    const artifact = await stat(join(dir, "artifact.bin"));
    return version && artifact.isFile() ? version : null;
  } catch {
    return null;
  }
}

/**
 * `operation` is K's receipt read, taken after unfinished work was settled.
 * An unsettled receipt here means settling failed; that is broken.
 */
export async function readWorld(cfg: Config, operation: OperationRead | null, env: NodeJS.ProcessEnv): Promise<World> {
  if (!(await exists(cfg.kStateDir))) {
    if (!(await exists(cfg.binaryPath))) return { kind: "fresh" };
    const manager = await foreignManager(cfg.binaryPath);
    if (manager) return { kind: "held", reason: `${cfg.binaryPath} was installed by ${manager}; remove it, or let this installer's directory come first on PATH` };
    const version = await selfVersion(cfg.binaryPath, env);
    if (!version) return { kind: "broken", reason: `the installed program at ${cfg.binaryPath} does not answer`, operation: null };
    return { kind: "adopted", version };
  }
  if (operation === null) return { kind: "broken", reason: "the installation records could not be read", operation: null };
  if (operation.kind === "unreadable") return { kind: "broken", reason: "the installation records are unreadable", operation };
  if (operation.kind === "observed" && operation.operation.outcome === null) {
    return { kind: "broken", reason: "an earlier upgrade could not be finished", operation };
  }
  const version = await stableVersion(cfg);
  if (!version) return { kind: "broken", reason: "the installation is incomplete", operation };
  return { kind: "managed", version, operation };
}
