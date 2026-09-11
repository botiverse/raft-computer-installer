#!/usr/bin/env node
// The entry. Decides presence, resolves the version, records consent, settles
// unfinished K work, reads the world, acts, and prints one line.
import { randomUUID } from "node:crypto";
import { userInfo } from "node:os";
import type { OperationRead, RunnerLaunchResult } from "@botiverse/k-carrier";
import { INSTALLER_VERSION, loadConfig, type Config } from "./config.js";
import { adopt, freshInstall, removeScratch, repair } from "./install.js";
import { askYesNo, decidePresence, type Presence } from "./presence.js";
import { failedBefore, held, receipt, refused, writeReceipt, type Outcome } from "./report.js";
import { compareSemver, isSemver } from "./semver.js";
import { assertSameIdentity, fetchManifest, resolveChannel, type Manifest } from "./source.js";
import { resumeRecovery, runRunner } from "./supervisor.js";
import { readWorld, type World } from "./worlds.js";
import { acquireSidecar } from "./artifact.js";

interface Args {
  command: string;
  version?: string;
  channel?: "main" | "alpha";
  yes: boolean;
  approvedBy?: string;
  id?: string;
  allowDowngrade: boolean;
  json: boolean;
  rest: string[];
}

function usage(): string {
  return [
    `raft-computer-installer ${INSTALLER_VERSION}`,
    "",
    "  install|upgrade [--version V | --channel main|alpha] [--yes] [--approved-by WHO] [--id ID] [--allow-downgrade]",
    "  rollback        [--yes]              upgrade to the previous stable version",
    "  repair          [--version V]        quarantine K state and reinstall; held unless the machine is broken",
    "  recover         <recovery.json>      retry an unresolved recovery offline",
    "  status                               what is installed, and the last receipt",
    "",
    "Unattended (CI, RAFT_COMPUTER_NON_INTERACTIVE=1, or no terminal) runs never ask: no --version means the",
    "channel's current release, and running the installer is the consent, repair included.",
    "Exit codes: 0 promoted, up to date or installed; 1 failed or rolled back; 2 held or refused; 3 unresolved.",
  ].join("\n");
}

function parseArgs(argv: string[]): Args {
  const a: Args = { command: argv[0] ?? "help", yes: false, allowDowngrade: false, json: false, rest: [] };
  for (let i = 1; i < argv.length; i++) {
    const t = argv[i];
    const val = (): string => { const v = argv[++i]; if (v === undefined) throw new Error(`${t} needs a value`); return v; };
    if (t === "--version") a.version = val().replace(/^v/, "");
    else if (t.startsWith("--version=")) a.version = t.slice(10).replace(/^v/, "");
    else if (t === "--channel") a.channel = channelOf(val());
    else if (t.startsWith("--channel=")) a.channel = channelOf(t.slice(10));
    else if (t === "--yes" || t === "-y") a.yes = true;
    else if (t === "--approved-by") a.approvedBy = val();
    else if (t === "--id") a.id = val();
    else if (t === "--allow-downgrade") a.allowDowngrade = true;
    else if (t === "--json") a.json = true;
    else if (t.startsWith("-")) throw new Error(`unknown option ${t}`);
    else a.rest.push(t);
  }
  return a;
}

function channelOf(v: string): "main" | "alpha" {
  if (v === "main" || v === "latest" || v === "stable") return "main";
  if (v === "alpha") return "alpha";
  throw new Error(`unknown channel ${v}; expected main or alpha`);
}

function describe(r: RunnerLaunchResult): { outcome: string | null; op: OperationRead | null; error: string | null } {
  const op = r.response?.operation ?? null;
  const outcome = op?.kind === "observed" ? op.operation.outcome : null;
  return { outcome, op, error: r.error ?? r.response?.error ?? null };
}

/** Settle unfinished K work before anything else. Returns what was settled. */
async function settle(cfg: Config, env: NodeJS.ProcessEnv): Promise<{ read: OperationRead | null; settled: string | null; unresolved: RunnerLaunchResult | null }> {
  const { exists } = await import("./computer.js");
  if (!(await exists(cfg.kStateDir))) return { read: null, settled: null, unresolved: null };
  const status = await runRunner(cfg, { protocolVersion: 1, action: "status" }, env);
  const op = status.response?.operation ?? null;
  if (!op) return { read: null, settled: null, unresolved: null };
  if (op.kind !== "observed" || op.operation.outcome !== null) return { read: op, settled: null, unresolved: null };
  const recovered = await runRunner(cfg, { protocolVersion: 1, action: "recover" }, env);
  const after = describe(recovered);
  if (recovered.exitCode === 3 || !after.op || (after.op.kind === "observed" && after.op.operation.outcome === null)) {
    return { read: after.op, settled: null, unresolved: recovered };
  }
  return { read: after.op, settled: `Settled interrupted upgrade ${op.operation.id}: ${after.outcome ?? "settled"}.`, unresolved: null };
}

function unresolvedOutcome(r: RunnerLaunchResult): Outcome {
  return { code: 3, status: "unresolved", line: `Cannot settle. Recover with: raft-computer-installer recover ${r.recoveryFile ?? "<recovery.json>"}.`, detail: { recoveryFile: r.recoveryFile, error: r.error } };
}

async function upgradeManaged(cfg: Config, env: NodeJS.ProcessEnv, id: string, m: Manifest, current: string, allowDowngrade: boolean): Promise<Outcome> {
  if (isSemver(current) && isSemver(m.version) && compareSemver(m.version, current) < 0 && !allowDowngrade) {
    return held(`${m.version} is older than the installed ${current}; pass --allow-downgrade to intend it`);
  }
  try { await acquireSidecar(cfg, m); } catch (error) {
    return failedBefore(error instanceof Error ? error.message : String(error), current);
  }
  const r = await runRunner(cfg, { protocolVersion: 1, action: "upgrade", id, targetVersion: m.version, consented: true }, env);
  const { outcome, op, error } = describe(r);
  const record = op?.kind === "observed" ? op.operation : null;
  const receiptId = record ? ` Receipt ${record.id}.` : "";
  if (r.exitCode === 3) return unresolvedOutcome(r);
  switch (outcome) {
    case "promoted": {
      const { attest } = await import("./computer.js");
      const live = await attest(cfg.binaryPath, { ...env, RAFT_HOME: cfg.stateHome, SLOCK_HOME: cfg.stateHome }).catch(() => null);
      return { code: 0, status: "promoted", line: `${current} → ${m.version} promoted.${live ? ` Running pid ${live.pid}.` : ""}${receiptId}`, detail: { live, result: r.response?.result } };
    }
    case "up-to-date": return { code: 0, status: "up-to-date", line: `${m.version} already running. Nothing to do.` };
    case "rolled-back": return { code: 1, status: "rolled-back", line: `Candidate ${m.version} failed probe. Rolled back; ${current} running.${record?.reason ? ` Reason: ${record.reason}.` : ""}` };
    case "held": return held(record?.reason ?? "policy");
    case "failed": return failedBefore(record?.reason ?? error ?? "unknown", current);
    default: return { code: 1, status: "failed", line: `Failed: ${error ?? "no receipt"}. ${current} may still be running; run status.` };
  }
}

async function resolveTarget(cfg: Config, a: Args, presence: Presence): Promise<{ manifest: Manifest } | { outcome: Outcome }> {
  if (a.version) {
    if (!isSemver(a.version)) return { outcome: refused(`"${a.version}" is not a version. Nothing changed.`) };
    try { return { manifest: await fetchManifest(cfg, a.version) }; }
    catch (error) { return { outcome: failedBefore(error instanceof Error ? error.message : String(error), null) }; }
  }
  try {
    const hands = await resolveChannel(cfg, a.channel ?? "main");
    const manifest = await fetchManifest(cfg, hands.version);
    assertSameIdentity(hands, manifest);
    return { manifest };
  } catch (error) {
    return { outcome: failedBefore(error instanceof Error ? error.message : String(error), null) };
  }
}

function consentLine(world: World, target: string): string {
  switch (world.kind) {
    case "fresh": return `Nothing installed. Install ${target}?`;
    case "adopted": return `Adopt the running ${world.version}, then upgrade to ${target}?`;
    case "managed": return `Upgrade ${world.version} → ${target}?`;
    case "broken": return `Stable won't settle (${world.reason}). Repair = quarantine + fresh ${target}. No rollback. Proceed?`;
    default: return `Proceed with ${target}?`;
  }
}

async function apply(cfg: Config, a: Args, presence: Presence, env: NodeJS.ProcessEnv): Promise<{ outcome: Outcome; settled: string | null; target: string | null }> {
  const id = a.id ?? `${a.command}-${randomUUID().slice(0, 8)}`;
  // Unattended: no questions. Running the installer is the consent to whatever
  // the machine needs, repair included.
  if (presence === "unattended") a.yes = true;
  const s = await settle(cfg, env);
  if (s.unresolved) return { outcome: unresolvedOutcome(s.unresolved), settled: null, target: a.version ?? null };
  const world = await readWorld(cfg, s.read, env);
  if (world.kind === "held") return { outcome: held(world.reason), settled: s.settled, target: a.version ?? null };

  let target: Manifest;
  if (a.command === "rollback") {
    if (world.kind !== "managed" || world.operation.kind !== "observed") return { outcome: held("nothing to roll back to"), settled: s.settled, target: null };
    const previous = world.operation.operation.previousStableVersion;
    if (!previous || previous === world.version) return { outcome: held("nothing to roll back to"), settled: s.settled, target: null };
    a.version = previous; a.allowDowngrade = true;
  }
  const resolved = await resolveTarget(cfg, a, presence);
  if ("outcome" in resolved) return { outcome: resolved.outcome, settled: s.settled, target: a.version ?? null };
  target = resolved.manifest;

  if (world.kind === "broken" && a.command !== "repair") a.command = "repair";
  if (a.command === "repair" && world.kind !== "broken") {
    return { outcome: held(`repair was asked for, but the machine is ${world.kind}; run upgrade instead`), settled: s.settled, target: target.version };
  }
  if (!a.yes) {
    if (!askYesNo(consentLine(world, target.version))) return { outcome: refused("Declined. Nothing changed."), settled: s.settled, target: target.version };
  }
  const runEnv = { ...env, RAFT_COMPUTER_APPROVED_BY: a.approvedBy ?? (presence === "unattended" ? `${userInfo().username} (unattended)` : userInfo().username) };

  let outcome: Outcome;
  if (a.command === "repair" || world.kind === "broken") outcome = await repair(cfg, target, runEnv, id);
  else if (world.kind === "fresh") outcome = await freshInstall(cfg, target, runEnv, id);
  else {
    if (world.kind === "adopted") {
      try { await adopt(cfg, world.version); } catch (error) {
        return { outcome: failedBefore(`could not adopt the running ${world.version}: ${error instanceof Error ? error.message : String(error)}`, world.version), settled: s.settled, target: target.version };
      }
    }
    outcome = await upgradeManaged(cfg, runEnv, id, target, world.version, a.allowDowngrade);
  }
  await removeScratch(cfg, target.version).catch(() => {});
  await writeReceipt(cfg, receipt(cfg, { ...outcome, id, operation: a.command, presence, targetVersion: target.version, approvedBy: runEnv.RAFT_COMPUTER_APPROVED_BY ?? null, settled: s.settled })).catch(() => {});
  return { outcome, settled: s.settled, target: target.version };
}

async function status(cfg: Config, env: NodeJS.ProcessEnv): Promise<Outcome> {
  const { exists } = await import("./computer.js");
  const read = (await exists(cfg.kStateDir)) ? (await runRunner(cfg, { protocolVersion: 1, action: "status" }, env)).response?.operation ?? null : null;
  const world = await readWorld(cfg, read, env);
  const { attest } = await import("./computer.js");
  const live = await attest(cfg.binaryPath, { ...env, RAFT_HOME: cfg.stateHome, SLOCK_HOME: cfg.stateHome }).catch(() => null);
  const line = world.kind === "managed" ? `${world.version} installed${live ? `, ${live.version} running (pid ${live.pid})` : ", not running"}. Last receipt is not a live observation.`
    : world.kind === "adopted" ? `${world.version} installed before K${live ? `, running (pid ${live.pid})` : ""}.`
    : world.kind === "fresh" ? "Nothing installed." : world.kind === "broken" ? `Broken: ${world.reason}.` : `Held: ${world.reason}.`;
  return { code: 0, status: world.kind, line, detail: { world, live, operation: read } };
}

async function main(): Promise<number> {
  const a = parseArgs(process.argv.slice(2));
  if (a.command === "--version" || a.command === "version") { console.log(INSTALLER_VERSION); return 0; }
  if (a.command === "help" || a.command === "--help" || a.command === "-h") { console.log(usage()); return 0; }
  const cfg = loadConfig();
  const env: NodeJS.ProcessEnv = { ...process.env, RAFT_HOME: cfg.stateHome, SLOCK_HOME: cfg.stateHome };
  const presence = decidePresence();
  let outcome: Outcome;
  let settled: string | null = null;
  switch (a.command) {
    case "install": case "upgrade": case "repair": case "rollback": {
      const r = await apply(cfg, a, presence, env);
      outcome = r.outcome; settled = r.settled;
      break;
    }
    case "recover": {
      const file = a.rest[0];
      if (!file) { outcome = refused("recover needs the recovery file printed by the unresolved run."); break; }
      const r = await resumeRecovery(file);
      const { outcome: o, op } = describe(r);
      const record = op?.kind === "observed" ? op.operation : null;
      outcome = r.exitCode === 3 ? unresolvedOutcome(r)
        : { code: r.exitCode as Outcome["code"], status: o ?? "recovered", line: `Settled ${record?.id ?? "operation"}: ${o ?? r.response?.result ?? "unknown"}.${o === "rolled-back" ? " The previous version is running." : ""}` };
      break;
    }
    case "status": outcome = await status(cfg, env); break;
    default: outcome = refused(`unknown command ${a.command}. ${usage()}`); break;
  }
  if (settled) process.stderr.write(`${settled}\n`);
  if (a.json) console.log(JSON.stringify({ ...outcome, settled }));
  else console.log(outcome.line);
  return outcome.code;
}

main().then((code) => { process.exitCode = code; }, (error) => {
  console.log(`Failed: ${error instanceof Error ? error.message : String(error)}.`);
  process.exitCode = 1;
});
