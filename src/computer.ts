// The Computer CLI as the installer sees it: a binary that answers
// `--version`, `start`, `stop` and `status --json`. Every call is bounded.
import { spawn } from "node:child_process";
import { access, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

export interface CommandResult { code: number; stdout: string; stderr: string; pid: number }

export function runCommand(binary: string, args: string[], env: NodeJS.ProcessEnv, timeoutMs = 60_000): Promise<CommandResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(binary, args, { env, stdio: ["ignore", "pipe", "pipe"] });
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

/** Is a service answering right now? */
export async function isLive(binary: string, env: NodeJS.ProcessEnv): Promise<boolean> {
  try { await attest(binary, env); return true; } catch { return false; }
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
    const child = spawn(binary, ["login"], { env, stdio: "inherit" });
    child.on("error", () => resolve(false));
    child.on("close", (code) => resolve(code === 0));
  });
}

/** Stop is idempotent: a service that is not running is a stopped service. */
export async function stopComputer(binary: string, env: NodeJS.ProcessEnv): Promise<void> {
  if (!(await exists(binary))) return;
  const r = await runCommand(binary, ["stop"], env, 90_000);
  if (r.code !== 0 && !/not running|no service|not started/i.test(r.stdout + r.stderr)) {
    throw new Error(`computer_stop_failed:${(r.stderr || r.stdout).trim().slice(0, 200)}`);
  }
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
