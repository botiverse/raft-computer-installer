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
  /** What was installed before this run changed anything, when known. */
  fromVersion: string | null;
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

/** The most recent promoted upgrade, from this installer's own receipts: what rollback goes back to. */
export async function lastPromoted(cfg: Config): Promise<{ fromVersion: string; targetVersion: string } | null> {
  const { readdir, readFile } = await import("node:fs/promises");
  const dir = join(cfg.installerDir, "receipts");
  let names: string[];
  try { names = await readdir(dir); } catch { return null; }
  const receipts: Receipt[] = [];
  for (const name of names) {
    try { receipts.push(JSON.parse(await readFile(join(dir, name), "utf8")) as Receipt); } catch { /* not a receipt */ }
  }
  const promoted = receipts
    .filter((r) => r.status === "promoted" && typeof r.fromVersion === "string" && typeof r.targetVersion === "string" && r.fromVersion !== r.targetVersion)
    .sort((a, b) => (a.finishedAt < b.finishedAt ? 1 : -1));
  const last = promoted[0];
  return last ? { fromVersion: last.fromVersion as string, targetVersion: last.targetVersion as string } : null;
}

// Lines are in the user's words: what happened, what did not change, what to
// do next. Slots, journals, receipts, ids and quarantines stay in --json.
export const held = (reason: string, next?: string): Outcome => ({ code: 2, status: "held", line: `Not done: ${reason}. Nothing changed.${next ? ` ${next}` : ""}` });
export const refused = (line: string): Outcome => ({ code: 2, status: "refused", line });
export const failedBefore = (reason: string, running: string | null): Outcome => ({
  code: 1, status: "failed", line: `Could not ${running ? "upgrade" : "install"}: ${plain(reason)}. Nothing changed${running ? `; ${running} is still running` : ""}.`,
});
/** Error text from below is for the receipt; the line gets a readable version. */
export function plain(reason: string): string {
  return reason
    .replace(/^release authority unreachable: .*/s, "the release server could not be reached")
    .replace(/^release manifest unreachable: .*/s, "the download server could not be reached")
    .replace(/^release manifest returned HTTP (\d+)$/, "the download server answered HTTP $1")
    .replace(/^release authority returned HTTP (\d+)$/, "the release server answered HTTP $1")
    .replace(/^no release for (\S+) in (\S+)$/, "$2 is not available for this machine ($1)")
    .replace(/^downloaded (\S+) is (.*)$/, "the download for $1 is $2")
    .replace(/^downloaded (\S+) reports version (.*)$/, "the download for $1 says it is $2")
    .replace(/^release authority and CDN disagree about (\S+) .*/s, "the release server and the download server disagree about $1")
    .replace(/^computer_command_timeout:(.*)$/, "the application did not answer `$1` in time")
    .replace(/^computer_stop_failed:(.*)$/, "the application could not be stopped ($1)")
    .replace(/^computer_(status_unavailable|status_unparseable|attestation_missing)$/, "the application did not answer")
    .replace(/^\[?BOOTSTRAP_\w+\]?\s*/, "")
    .replace(/^\[?QUARANTINE_ACTIVE_LOCK\]?\s*.*/s, "another installer is running on this machine")
    .replace(/\.$/, "");
}
