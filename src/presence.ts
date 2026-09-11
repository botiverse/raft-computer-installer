// Whether a person is present is decided once, before anything else.
// An explicit CI or *_NON_INTERACTIVE=1 wins; otherwise a terminal decides.
import { closeSync, openSync, readSync, writeSync } from "node:fs";
import { IS_WINDOWS } from "./config.js";

export type Presence = "attended" | "unattended";

function truthy(v: string | undefined): boolean {
  return v !== undefined && v !== "" && !/^(0|false|no)$/i.test(v);
}

/** The terminal itself, so `curl | sh` and `irm | iex` can still hear the answer. */
function openTerminal(): { fd: number; close: () => void } | null {
  if (!IS_WINDOWS) {
    try { const fd = openSync("/dev/tty", "r+"); return { fd, close: () => closeSync(fd) }; } catch { /* no terminal */ }
  }
  // Windows has no /dev/tty; the console is stdin when it is one.
  if (process.stdin.isTTY) return { fd: 0, close: () => {} };
  return null;
}

export function decidePresence(env: NodeJS.ProcessEnv = process.env): Presence {
  if (truthy(env.RAFT_COMPUTER_NON_INTERACTIVE) || truthy(env.CI)) return "unattended";
  if (process.stdin.isTTY) return "attended";
  const t = openTerminal();
  if (!t) return "unattended";
  t.close();
  return "attended";
}

/** Ask on the terminal and read one line. Anything but yes is no. */
export function askYesNo(question: string): boolean {
  const t = openTerminal();
  if (!t) return false;
  try {
    writeSync(t.fd === 0 ? 1 : t.fd, `${question} [y/N] `);
    const buf = Buffer.alloc(256);
    let text = "";
    for (;;) {
      let n = 0;
      try { n = readSync(t.fd, buf, 0, buf.length, null); } catch { break; }
      if (n <= 0) break;
      text += buf.toString("utf8", 0, n);
      if (text.includes("\n")) break;
    }
    return /^\s*(y|yes)\s*$/i.test(text);
  } finally {
    t.close();
  }
}
