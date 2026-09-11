// A stand-in for the Computer CLI: `--version`, `start`, `stop`, `status --json`.
// The version is baked into the wrapper that execs this file; the service it
// starts reports that version, a fresh generation per start, and its pid.
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const version = process.env.RAFT_FAKE_VERSION ?? "0.0.0";
const self = process.env.RAFT_FAKE_SELF ?? "";
if (process.argv[2] === "__service") {
  process.on("SIGTERM", () => process.exit(0));
  setInterval(() => {}, 1 << 30);
} else {
const home = process.env.RAFT_HOME ?? process.env.SLOCK_HOME;
const [cmd, flag] = process.argv.slice(2);
const runDir = home ? join(home, "computer", "run") : null;
const stateFile = runDir ? join(runDir, "service.json") : null;

function readState() {
  try { return JSON.parse(readFileSync(stateFile, "utf8")); } catch { return null; }
}
function alive(pid) { try { process.kill(pid, 0); return true; } catch { return false; } }

if (cmd === "--version") { console.log(version); process.exit(0); }
if (!home) { console.error("no home"); process.exit(1); }
const loginFile = join(home, "computer", "login");
const loggedIn = () => { try { readFileSync(loginFile); return true; } catch { return false; } };
if (cmd === "login") { mkdirSync(join(home, "computer"), { recursive: true }); writeFileSync(loginFile, "ok"); process.exit(0); }
if (cmd === "start") {
  if (!loggedIn()) { console.error("not logged in; run login first"); process.exit(1); }
  if (process.env.RAFT_FAKE_START_FAIL === "1") { console.error("refusing to start"); process.exit(1); }
  const existing = readState();
  if (existing && alive(existing.pid)) process.exit(0);
  mkdirSync(runDir, { recursive: true });
  // The service process names the installed binary on its command line, as a real one would.
  const child = spawn(process.execPath, [process.argv[1], "__service", self], { detached: true, stdio: "ignore", env: process.env });
  child.unref();
  writeFileSync(stateFile, JSON.stringify({ pid: child.pid, version, generation: randomUUID() }));
  writeFileSync(join(runDir, "service.pid"), String(child.pid));
  process.exit(0);
}
if (cmd === "stop") {
  const s = readState();
  if (process.env.RAFT_FAKE_STOP_BROKEN === "1") process.exit(0); // says yes, does nothing
  if (s && alive(s.pid)) { try { process.kill(s.pid, "SIGTERM"); } catch {} }
  rmSync(stateFile, { force: true });
  process.exit(0);
}
if (cmd === "status" && flag === "--json") {
  const s = readState();
  const running = Boolean(s && alive(s.pid));
  const nextStep = !loggedIn() ? "run raft-computer login" : (process.env.RAFT_FAKE_NEXT_STEP ?? null);
  console.log(JSON.stringify({ running, ...(running ? { attestation: { servicePid: s.pid, computerVersion: s.version, serviceGeneration: s.generation } } : {}), ...(nextStep ? { nextStep } : {}) }));
  process.exit(0);
}
console.error(`fake computer: unknown command ${cmd}`);
process.exit(2);
}
