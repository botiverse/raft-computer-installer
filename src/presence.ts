// Whether a person is present is decided once, before anything else.
// An explicit CI or *_NON_INTERACTIVE=1 wins; otherwise a terminal decides.
import { closeSync, openSync, readSync, writeSync } from "node:fs";

export type Presence = "attended" | "unattended";

function truthy(v: string | undefined): boolean {
  return v !== undefined && v !== "" && !/^(0|false|no)$/i.test(v);
}

export function decidePresence(env: NodeJS.ProcessEnv = process.env): Presence {
  if (truthy(env.RAFT_COMPUTER_NON_INTERACTIVE) || truthy(env.CI)) return "unattended";
  if (process.stdin.isTTY) return "attended";
  try { closeSync(openSync("/dev/tty", "r+")); return "attended"; } catch { return "unattended"; }
}

/** Ask on the terminal itself, so `curl | sh` can still hear the answer. */
export function askYesNo(question: string): boolean {
  let fd: number;
  try { fd = openSync("/dev/tty", "r+"); } catch { return false; }
  try {
    writeSync(fd, `${question} [y/N] `);
    const buf = Buffer.alloc(256);
    let text = "";
    for (;;) {
      const n = readSync(fd, buf, 0, buf.length, null);
      if (n <= 0) break;
      text += buf.toString("utf8", 0, n);
      if (text.includes("\n")) break;
    }
    return /^\s*(y|yes)\s*$/i.test(text);
  } finally {
    closeSync(fd);
  }
}
