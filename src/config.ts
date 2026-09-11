// Where everything lives. The same module serves the entry (cli.cjs) and the
// runner (runner.mjs); the entry passes nothing but environment to the runner.
import { homedir } from "node:os";
import { join, resolve } from "node:path";

export const INSTALLER_VERSION = "0.2.0-rc.1";
export const IS_WINDOWS = process.platform === "win32";
export const BIN_NAME = IS_WINDOWS ? "raft-computer.exe" : "raft-computer";
export const SIDECAR_NAME = "photon_rs_bg.wasm";

export interface Config {
  /** Product state root: RAFT_HOME, SLOCK_HOME or ~/.slock. */
  stateHome: string;
  /** K-owned transaction state: <stateHome>/computer/k. */
  kStateDir: string;
  /** Installer-owned state: receipts, scratch, sidecars, quarantine. */
  installerDir: string;
  /** Where the PATH-visible binary and its sidecar are published. */
  installDir: string;
  binaryPath: string;
  sidecarPath: string;
  /** CDN holding <version>/manifest.json and the artifacts it names. */
  releaseBase: string;
  handsOrigin: string;
  handsApp: string;
  /** Verified runner bundle beside the entry, set by the bootstrap or build. */
  runnerPath: string;
  node: string;
}

function expandHome(p: string): string {
  if (p === "~") return homedir();
  if (p.startsWith("~/")) return join(homedir(), p.slice(2));
  return p;
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  const stateHome = resolve(expandHome(env.RAFT_HOME ?? env.SLOCK_HOME ?? join(homedir(), ".slock")));
  const installDir = resolve(expandHome(env.RAFT_COMPUTER_INSTALL_DIR ?? join(homedir(), ".local", "bin")));
  const binaryPath = env.RAFT_COMPUTER_BINARY ? resolve(env.RAFT_COMPUTER_BINARY) : join(installDir, BIN_NAME);
  return {
    stateHome,
    kStateDir: join(stateHome, "computer", "k"),
    installerDir: join(stateHome, "computer", "installer"),
    installDir,
    binaryPath,
    sidecarPath: join(installDir, SIDECAR_NAME),
    releaseBase: (env.RAFT_COMPUTER_RELEASE_BASE ?? "https://cdn.raft.build/computer").replace(/\/$/, ""),
    handsOrigin: (env.RAFT_COMPUTER_HANDS_ORIGIN ?? "https://hands.build").replace(/\/$/, ""),
    handsApp: env.RAFT_COMPUTER_HANDS_APP ?? "raft-computer-cli",
    runnerPath: resolve(env.RAFT_COMPUTER_INSTALLER_RUNNER ?? defaultRunnerPath()),
    node: env.RAFT_COMPUTER_INSTALLER_NODE ?? process.execPath,
  };
}

/** Beside the entry: runner.mjs next to cli.cjs, or the native runner next to the native entry. */
function defaultRunnerPath(): string {
  const here = process.argv[1] ? resolve(process.argv[1], "..") : process.cwd();
  const self = process.argv[1] ?? "";
  if (self.endsWith(".cjs") || self.endsWith(".js") || self.endsWith(".mjs")) return join(here, "runner.mjs");
  const exe = process.execPath.split(/[\\/]/).pop() ?? "raft-computer-installer";
  const stem = exe.replace(/\.exe$/i, "");
  return join(resolve(process.execPath, ".."), IS_WINDOWS ? `${stem}-runner.exe` : `${stem}-runner`);
}

export function platformKey(): string {
  return `${process.platform}-${process.arch}`;
}

export function sidecarDir(cfg: Config, version: string): string {
  return join(cfg.installerDir, "sidecars", version);
}
export function receiptPath(cfg: Config, id: string): string {
  return join(cfg.installerDir, "receipts", `${id}.json`);
}
export function quarantineDir(cfg: Config, id: string): string {
  return join(cfg.installerDir, "quarantine", id);
}
export function scratchDir(cfg: Config): string {
  return join(cfg.installerDir, "scratch");
}
