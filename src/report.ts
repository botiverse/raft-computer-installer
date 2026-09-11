// One line, one exit code, one receipt. The line says what happened, what
// did not change, and what to do next.
import { mkdir, rename, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { INSTALLER_VERSION, receiptPath, type Config } from "./config.js";

export type ExitCode = 0 | 1 | 2 | 3;

export interface Outcome {
  code: ExitCode;
  /** promoted | up-to-date | installed | repaired | failed | rolled-back | held | refused | unresolved */
  status: string;
  line: string;
  detail?: Record<string, unknown>;
}

export interface Receipt extends Outcome {
  protocol: "raft-computer-installer/v2";
  installerVersion: string;
  id: string;
  operation: string;
  presence: "attended" | "unattended";
  targetVersion: string | null;
  approvedBy: string | null;
  settled: string | null;
  finishedAt: string;
}

export async function writeReceipt(cfg: Config, r: Receipt): Promise<void> {
  const path = receiptPath(cfg, r.id);
  await mkdir(join(path, ".."), { recursive: true });
  const tmp = `${path}.${process.pid}.tmp`;
  await writeFile(tmp, `${JSON.stringify(r, null, 2)}\n`, { mode: 0o600 });
  await rename(tmp, path);
}

export function receipt(cfg: Config, base: Omit<Receipt, "protocol" | "installerVersion" | "finishedAt">): Receipt {
  return { protocol: "raft-computer-installer/v2", installerVersion: INSTALLER_VERSION, finishedAt: new Date().toISOString(), ...base };
}

export const held = (reason: string): Outcome => ({ code: 2, status: "held", line: `Held: ${reason}. Nothing changed. Retry with a new id.` });
export const refused = (line: string): Outcome => ({ code: 2, status: "refused", line });
export const failedBefore = (reason: string, running: string | null): Outcome => ({
  code: 1, status: "failed", line: `Failed before any change: ${reason}.${running ? ` ${running} still running.` : ""}`,
});
