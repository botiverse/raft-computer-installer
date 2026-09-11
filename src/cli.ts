#!/usr/bin/env node
// The entry. Decides presence, resolves the version, records consent, settles
// unfinished K work, reads the world, acts, and prints one line.
import { randomUUID } from "node:crypto";
import { userInfo } from "node:os";
import type { OperationRead, RunnerLaunchResult } from "@botiverse/k-carrier";
import { INSTALLER_VERSION, loadConfig, type Config } from "./config.js";
import { adopt, freshInstall, nextStep, removeScratch, repair } from "./install.js";
import { askYesNo, decidePresence, type Presence } from "./presence.js";
import { failedBefore, held, plain, receipt, refused, writeReceipt, type Outcome } from "./report.js";
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
  const a: Args = { command: argv[0] ?? "help", yes: false, allowDowngrade: false, rest: [] };
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
  const what = after.outcome === "promoted" ? "it was completed" : after.outcome === "rolled-back" ? "the previous version was put back" : "it was settled";
  return { read: after.op, settled: `An earlier upgrade was interrupted; ${what}.`, unresolved: null };
}

function unresolvedOutcome(r: RunnerLaunchResult): Outcome {
  return { code: 3, status: "unresolved", line: `Could not finish safely. Nothing was lost. To try again, run: raft-computer-installer recover ${r.recoveryFile ?? "<recovery file>"}`, detail: { recoveryFile: r.recoveryFile, error: r.error } };
}

async function upgradeManaged(cfg: Config, env: NodeJS.ProcessEnv, id: string, m: Manifest, current: string, allowDowngrade: boolean): Promise<Outcome> {
  if (isSemver(current) && isSemver(m.version) && compareSemver(m.version, current) < 0 && !allowDowngrade) {
    return held(`${m.version} is older than the installed ${current}`, "Add --allow-downgrade to install it anyway.");
  }
  try { await acquireSidecar(cfg, m); } catch (error) {
    return failedBefore(error instanceof Error ? error.message : String(error), current);
  }
  const r = await runRunner(cfg, { protocolVersion: 1, action: "upgrade", id, targetVersion: m.version, consented: true }, env);
  const { outcome, op, error } = describe(r);
  const record = op?.kind === "observed" ? op.operation : null;
  if (r.exitCode === 3) return unresolvedOutcome(r);
  switch (outcome) {
    case "promoted": {
      const { attest, statusHint } = await import("./computer.js");
      const hostEnv = { ...env, RAFT_HOME: cfg.stateHome, SLOCK_HOME: cfg.stateHome };
      const live = await attest(cfg.binaryPath, hostEnv).catch(() => null);
      const hint = live ?? { nextStep: await statusHint(cfg.binaryPath, hostEnv) };
      const replayed = r.response?.result === "replayed";
      return { code: 0, status: "promoted", line: `Upgraded ${record?.fromVersion ?? current} → ${m.version}${replayed ? " earlier" : ""}.${live ? " It is running." : ""}${nextStep(hint)}`, detail: { live, receipt: record?.id, result: r.response?.result } };
    }
    case "up-to-date": return { code: 0, status: "up-to-date", line: `${m.version} is already installed. Nothing to do.` };
    case "rolled-back": {
      const { readMode } = await import("./hostAdapter.js");
      const running = (await readMode(cfg)) === "service";
      return { code: 1, status: "rolled-back", line: `${m.version} did not ${running ? "start" : "check out"} correctly, so ${current} was put back${running ? " and is running" : ""}.`, detail: { reason: record?.reason } };
    }
    case "held": return held(plain(record?.reason ?? "the upgrade was not allowed on this machine"));
    case "failed": return failedBefore(record?.reason ?? error ?? "unknown", current);
    default: return { code: 1, status: "failed", line: `Could not upgrade: ${plain(error ?? "no answer from the installer")}. Check with: raft-computer-installer status`, detail: { error } };
  }
}

async function resolveTarget(cfg: Config, a: Args, presence: Presence): Promise<{ manifest: Manifest } | { outcome: Outcome }> {
  if (a.version) {
    if (!isSemver(a.version)) return { outcome: refused(`"${a.version}" is not a version number. Nothing changed.`) };
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
    case "fresh": return `Nothing is installed. Install ${target}?`;
    case "adopted": case "managed": return `Upgrade ${world.version} → ${target}?`;
    case "broken": return `The current installation cannot be recovered (${world.reason}). Reinstall ${target} fresh? The old installation is kept aside, but this cannot be undone.`;
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
    if (world.kind !== "managed" || world.operation.kind !== "observed") return { outcome: held("there is no earlier version to go back to"), settled: s.settled, target: null };
    const previous = world.operation.operation.previousStableVersion;
    if (!previous || previous === world.version) return { outcome: held("there is no earlier version to go back to"), settled: s.settled, target: null };
    a.version = previous; a.allowDowngrade = true;
  }
  const resolved = await resolveTarget(cfg, a, presence);
  if ("outcome" in resolved) return { outcome: resolved.outcome, settled: s.settled, target: a.version ?? null };
  target = resolved.manifest;

  if (world.kind === "broken" && a.command !== "repair") a.command = "repair";
  if (a.command === "repair" && world.kind !== "broken") {
    return { outcome: held("nothing here needs repair", "Run install or upgrade instead."), settled: s.settled, target: target.version };
  }
  if (!a.yes) {
    if (!askYesNo(consentLine(world, target.version))) return { outcome: refused("Declined. Nothing changed."), settled: s.settled, target: target.version };
  }
  const runEnv = { ...env, RAFT_COMPUTER_APPROVED_BY: a.approvedBy ?? (presence === "unattended" ? `${userInfo().username} (unattended)` : userInfo().username) };

  let outcome: Outcome;
  if (a.command === "repair" || world.kind === "broken") outcome = await repair(cfg, target, runEnv, id, presence);
  else if (world.kind === "fresh") outcome = await freshInstall(cfg, target, runEnv, id, presence);
  else {
    if (world.kind === "adopted") {
      try { await adopt(cfg, world.version); } catch (error) {
        return { outcome: failedBefore(`the existing ${world.version} could not be taken over (${plain(error instanceof Error ? error.message : String(error))})`, world.version), settled: s.settled, target: target.version };
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
  const line = world.kind === "managed" ? `${world.version} is installed${live ? ` and ${live.version === world.version ? "running" : `${live.version} is running`}` : " but not running"}.`
    : world.kind === "adopted" ? `${world.version} is installed${live ? " and running" : ""} (not yet managed by this installer).`
    : world.kind === "fresh" ? "Nothing is installed." : world.kind === "broken" ? `Needs repair: ${world.reason}. Run install to reinstall.` : `Not done: ${world.reason}.`;
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
        : { code: r.exitCode as Outcome["code"], status: o ?? "recovered", line: o === "promoted" ? "Finished the interrupted upgrade. It is running." : o === "rolled-back" ? "Finished the interrupted upgrade: the previous version was put back and is running." : `Finished the interrupted upgrade (${o ?? r.response?.result ?? "settled"}).`, detail: { receipt: record?.id } };
      break;
    }
    case "status": outcome = await status(cfg, env); break;
    default: outcome = refused(`unknown command ${a.command}. ${usage()}`); break;
  }
  if (settled) process.stderr.write(`${settled}\n`);
  console.log(outcome.line);
  return outcome.code;
}

main().then((code) => { process.exitCode = code; }, (error) => {
  console.log(`Could not continue: ${plain(error instanceof Error ? error.message : String(error))}.`);
  process.exitCode = 1;
});
