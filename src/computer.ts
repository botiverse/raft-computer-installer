// The Computer CLI as the installer sees it: a binary that answers
// `--version`, `start`, `stop` and `status --json`. Every call is bounded.
import { spawn } from "node:child_process";
import { access, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { IS_WINDOWS } from "./config.js";

/** Test fakes on Windows are .cmd files, which only a shell can start. The product is an .exe. */
function needsShell(binary: string): boolean {
  return IS_WINDOWS && /\.(cmd|bat)$/i.test(binary);
}

export interface CommandResult { code: number; stdout: string; stderr: string; pid: number }

export function runCommand(binary: string, args: string[], env: NodeJS.ProcessEnv, timeoutMs = 60_000): Promise<CommandResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(binary, args, { env, stdio: ["ignore", "pipe", "pipe"], windowsHide: true, ...(needsShell(binary) ? { shell: true } : {}) });
    let stdout = "", stderr = "", done = false;
    const timer = setTimeout(() => {
      if (done) return;
      done = true;
      child.kill("SIGTERM");
      setTimeout(() => child.kill("SIGKILL"), 1000).unref();
      reject(new Error(`computer_command_timeout:${args.join(" ")}`));
    }, timeoutMs);
    child.stdout.on("data", (d: Buffer) => { stdout += d; if (stdout.length > 1 << 20) child.kill("SIGKILL"); });
    child.stderr.on("data", (d: Buffer) => { stderr += d; });
    child.on("error", (error) => { if (done) return; done = true; clearTimeout(timer); reject(error); });
    child.on("close", (code) => { if (done) return; done = true; clearTimeout(timer); resolve({ code: code ?? 1, stdout, stderr, pid: child.pid ?? 0 }); });
  });
}

export async function exists(p: string): Promise<boolean> {
  try { await access(p); return true; } catch { return false; }
}

/**
 * Ask a binary its own version with a cold, empty home, so an installed
 * dispatcher cannot hand the question to whatever K stable it fronts. The
 * answer describes the file asked, nothing else.
 */
export async function selfVersion(binary: string, env: NodeJS.ProcessEnv): Promise<string | null> {
  const home = await mkdtemp(join(tmpdir(), "raft-installer-probe-"));
  try {
    const r = await runCommand(binary, ["--version"], { ...env, RAFT_HOME: home, SLOCK_HOME: home }, 20_000);
    if (r.code !== 0) return null;
    const token = r.stdout.trim().split(/\s+/)[0] ?? "";
    return token.replace(/^v/, "") || null;
  } catch {
    return null;
  } finally {
    await rm(home, { recursive: true, force: true });
  }
}

export interface Attestation { version: string; pid: number; startId: string; nextStep: string | null }

/** Live evidence only: version, pid and start id answered by one running service. */
export async function attest(binary: string, env: NodeJS.ProcessEnv): Promise<Attestation> {
  const r = await runCommand(binary, ["status", "--json"], env, 30_000);
  if (r.code !== 0) throw new Error("computer_status_unavailable");
  let parsed: { attestation?: { servicePid?: unknown; computerVersion?: unknown; serviceGeneration?: unknown }; nextStep?: unknown };
  try { parsed = JSON.parse(r.stdout); } catch { throw new Error("computer_status_unparseable"); }
  const a = parsed.attestation;
  if (!a || typeof a.servicePid !== "number" || typeof a.computerVersion !== "string" || typeof a.serviceGeneration !== "string") {
    throw new Error("computer_attestation_missing");
  }
  // The product says what the user should do next, if anything; the installer repeats it.
  const nextStep = typeof parsed.nextStep === "string" && parsed.nextStep.trim() ? parsed.nextStep.trim() : null;
  return { version: a.computerVersion.replace(/^v/, ""), pid: a.servicePid, startId: a.serviceGeneration, nextStep };
}

/** The product's next step for the user, if it names one; works without a running service. */
export async function statusHint(binary: string, env: NodeJS.ProcessEnv): Promise<string | null> {
  try {
    const r = await runCommand(binary, ["status", "--json"], env, 30_000);
    const parsed = JSON.parse(r.stdout) as { nextStep?: unknown };
    return typeof parsed.nextStep === "string" && parsed.nextStep.trim() ? parsed.nextStep.trim() : null;
  } catch {
    return null;
  }
}

/** Is the service running: answering, or at least present as a process of the binary. */
export async function isLive(binary: string, env: NodeJS.ProcessEnv): Promise<boolean> {
  try { await attest(binary, env); return true; } catch { /* not answering */ }
  return (await productProcesses(binary)).length > 0;
}

/**
 * Cold self-report: run the binary itself and take its answer as the
 * evidence. Every run is a new process, so the start id is always new.
 * This is what "probe" means on a machine where nothing runs.
 */
export async function selfReport(binary: string, env: NodeJS.ProcessEnv): Promise<Attestation> {
  const home = await mkdtemp(join(tmpdir(), "raft-installer-probe-"));
  try {
    const r = await runCommand(binary, ["--version"], { ...env, RAFT_HOME: home, SLOCK_HOME: home }, 20_000);
    if (r.code !== 0) throw new Error("computer_self_report_failed");
    const token = (r.stdout.trim().split(/\s+/)[0] ?? "").replace(/^v/, "");
    if (!token) throw new Error("computer_self_report_empty");
    return { version: token, pid: r.pid, startId: `cold-${r.pid}-${Date.now()}`, nextStep: null };
  } finally {
    await rm(home, { recursive: true, force: true });
  }
}

/**
 * The product's first setup, run on the user's terminal: for Computer,
 * `login`. Only attended; an unattended run has nobody to log in.
 */
export function firstSetup(binary: string, env: NodeJS.ProcessEnv): Promise<boolean> {
  return new Promise((resolve) => {
    const child = spawn(binary, ["login"], { env, stdio: "inherit", ...(needsShell(binary) ? { shell: true } : {}) });
    child.on("error", () => resolve(false));
    child.on("close", (code) => resolve(code === 0));
  });
}

/**
 * Processes that are provably the installed binary: the binary's path is a
 * whole token of their command line. Never a bare pid, never a name.
 */
export async function productProcesses(binary: string): Promise<number[]> {
  const listing = IS_WINDOWS
    ? await runCommand("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command",
        "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ProcessId)`t$($_.CommandLine)\" }"], process.env, 30_000).catch(() => null)
    : await runCommand("ps", ["-eo", "pid=,command="], process.env, 15_000).catch(() => null);
  if (!listing || listing.code !== 0) return [];
  // The same file can be spelled two ways (a symlinked /var on macOS, case on
  // Windows); identity is the real path.
  const { realpathSync } = await import("node:fs");
  const real = (p: string): string => { try { return realpathSync(p); } catch { return p; } };
  const wanted = new Set([binary, real(binary)].map((p) => IS_WINDOWS ? p.toLowerCase() : p));
  const same = (token: string) => {
    const t = IS_WINDOWS ? token.toLowerCase() : token;
    if (wanted.has(t)) return true;
    return (token.includes("/") || token.includes("\\")) && wanted.has(IS_WINDOWS ? real(token).toLowerCase() : real(token));
  };
  const pids: number[] = [];
  for (const line of listing.stdout.split(/\r?\n/)) {
    const m = IS_WINDOWS ? /^(\d+)\t(.*)$/.exec(line) : /^\s*(\d+)\s+(.*)$/.exec(line);
    if (!m) continue;
    const pid = Number(m[1]);
    if (pid === process.pid || pid <= 1) continue;
    // A Windows command line quotes paths with spaces; compare the token without its quotes.
    const tokens = m[2].match(/"[^"]*"|\S+/g) ?? [];
    if (tokens.some((t) => same(t.replace(/^"|"$/g, "")))) pids.push(pid);
  }
  return pids;
}

async function waitGone(pids: number[], ms: number): Promise<number[]> {
  const deadline = Date.now() + ms;
  let left = pids;
  while (left.length && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 250));
    left = left.filter((pid) => { try { process.kill(pid, 0); return true; } catch { return false; } });
  }
  return left;
}

/** Terminate, wait, kill. Returns the pids it had to touch. */
export async function terminateProduct(binary: string): Promise<number[]> {
  const pids = await productProcesses(binary);
  if (!pids.length) return [];
  for (const pid of pids) { try { process.kill(pid, "SIGTERM"); } catch { /* already gone */ } }
  let left = await waitGone(pids, 20_000);
  for (const pid of left) { try { process.kill(pid, "SIGKILL"); } catch { /* already gone */ } }
  left = await waitGone(left, 5_000);
  if (left.length) throw new Error(`computer_processes_survive:${left.join(",")}`);
  return pids;
}

/**
 * Stop is idempotent: a service that is not running is a stopped service.
 * The product's command goes first; if its processes are still there
 * afterwards, the installer finishes the job by identity.
 */
export async function stopComputer(binary: string, env: NodeJS.ProcessEnv): Promise<{ forced: number[] }> {
  if (await exists(binary)) {
    const r = await runCommand(binary, ["stop"], env, 90_000).catch(() => null);
    if (r && r.code !== 0 && !/not running|no service|not started/i.test(r.stdout + r.stderr)) {
      // A failed stop is not the end: the fallback below decides.
    }
  }
  const remaining = await waitGone(await productProcesses(binary), 5_000);
  if (!remaining.length) return { forced: [] };
  return { forced: await terminateProduct(binary) };
}

export async function startComputer(binary: string, env: NodeJS.ProcessEnv): Promise<CommandResult> {
  return runCommand(binary, ["start"], env, 90_000);
}

/**
 * Does the file on PATH look like something another manager owns? An npm or
 * bun shim is a Node script; a Homebrew path is a Homebrew path. We only
 * ever say so; we never remove it.
 */
export async function foreignManager(binary: string): Promise<string | null> {
  if (/\/(opt\/homebrew|usr\/local\/Cellar)\//.test(binary)) return "homebrew";
  if (/\/node_modules\//.test(binary)) return "npm";
  try {
    const { readFile } = await import("node:fs/promises");
    const head = (await readFile(binary)).subarray(0, 64).toString("utf8");
    if (/^#!.*\bnode\b/.test(head)) return /\.bun\b/.test(binary) ? "bun" : "npm";
  } catch { /* unreadable: not a foreign manager we can name */ }
  return null;
}
