// Builds the installer once, serves fake releases over HTTP, and drives the
// built cli.cjs as a real process with a fake Computer.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(fileURLToPath(import.meta.url), "..", "..");
export const fakePath = join(root, "test", "fake-computer.mjs");
const platformKey = `${process.platform}-${process.arch}`;
export const WINDOWS = process.platform === "win32";
/** The fake as a real executable: always on Windows, or RCI_FAKE_SEA=1 anywhere to exercise it. */
const SEA_FAKE = WINDOWS || process.env.RCI_FAKE_SEA === "1";
const BIN = WINDOWS ? "raft-computer.exe" : "raft-computer";
/** RCI_NATIVE=1 drives the single executables instead of node + cli.cjs. */
export const NATIVE = process.env.RCI_NATIVE === "1";

export interface FakeRelease { version: string; startFail?: boolean; reportedVersion?: string; nextStep?: string; stopBroken?: boolean }

export class Harness {
  dist = "";
  server!: Server;
  base = "";
  releases = new Map<string, Buffer>();
  channel: { main?: string; alpha?: string } = {};
  sidecar = Buffer.from("not really wasm but verified all the same");

  fakeSea: Buffer | null = null;
  async start(): Promise<void> {
    this.dist = mkdtempSync(join(tmpdir(), "rci-dist-"));
    await run(process.execPath, [join(root, "scripts", "build.mjs"), this.dist, ...(NATIVE ? ["--native"] : [])], { cwd: root });
    if (SEA_FAKE) {
      const { buildFakeSea } = await import("./build-fake-sea.mjs");
      const { readFileSync } = await import("node:fs");
      this.fakeSea = readFileSync(await buildFakeSea(join(this.dist, "fake"), fakePath));
    }
    this.server = createServer((req, res) => {
      const url = new URL(req.url ?? "/", "http://x");
      const m = /^\/computer\/([^/]+)\/(.+)$/.exec(url.pathname);
      if (m) {
        const [, version, file] = m;
        const bytes = this.releases.get(version);
        if (!bytes) { res.statusCode = 404; return res.end(); }
        if (file === "manifest.json") { res.setHeader("content-type", "application/json"); return res.end(JSON.stringify(this.manifest(version, bytes))); }
        if (file === "raft-computer") return res.end(bytes);
        if (file === "photon_rs_bg.wasm") return res.end(this.sidecar);
      }
      const i = /^\/installer\/(.+)$/.exec(url.pathname);
      if (i) {
        const f = join(this.dist, ...i[1].split("/"));
        if (existsSync(f)) return res.end(readFileSync(f));
        res.statusCode = 404; return res.end();
      }
      if (url.pathname === "/computer/install.sh") return res.end(readFileSync(join(this.dist, "install.sh")));
      const h = /^\/public\/v2\/apps\/([^/]+)\/latest$/.exec(url.pathname);
      if (h) {
        const channel = url.searchParams.get("channel") === "alpha" ? "alpha" : "main";
        const version = this.channel[channel];
        const bytes = version ? this.releases.get(version) : undefined;
        if (!version || !bytes) { res.statusCode = 404; return res.end(); }
        const [platform, arch] = platformKey.split("-");
        return res.end(JSON.stringify({ build: { version }, assets: [{ platform, arch, variant: null, filetype: "binary", size_bytes: bytes.length, sha256: sha(bytes) }] }));
      }
      res.statusCode = 404; res.end();
    });
    await new Promise<void>((r) => this.server.listen(0, "127.0.0.1", r));
    const addr = this.server.address();
    this.base = `http://127.0.0.1:${typeof addr === "object" && addr ? addr.port : 0}`;
  }

  async stop(): Promise<void> {
    this.server.closeAllConnections();
    await new Promise<void>((r) => this.server.close(() => r()));
    rmSync(this.dist, { recursive: true, force: true });
  }

  publish(r: FakeRelease): Buffer {
    const behaviour: Record<string, string> = { RAFT_FAKE_VERSION: r.reportedVersion ?? r.version };
    if (r.startFail) behaviour.RAFT_FAKE_START_FAIL = "1";
    if (r.nextStep) behaviour.RAFT_FAKE_NEXT_STEP = r.nextStep;
    if (r.stopBroken) behaviour.RAFT_FAKE_STOP_BROKEN = "1";
    const bytes = this.fakeSea
      ? Buffer.concat([this.fakeSea, Buffer.from(`\n#RAFT_FAKE:${JSON.stringify(behaviour)}\n`)])
      : Buffer.from(`#!/bin/sh\n${Object.entries(behaviour).map(([k, v]) => `${k}=${JSON.stringify(v)}`).join(" ")} RAFT_FAKE_SELF="$0" exec "${process.execPath}" "${fakePath}" "$@"\n`);
    this.releases.set(r.version, bytes);
    return bytes;
  }

  manifest(version: string, bytes: Buffer) {
    return {
      name: "raft-computer", version,
      photonWasm: { file: "photon_rs_bg.wasm", sha256: sha(this.sidecar), size: this.sidecar.length },
      targets: { [platformKey]: { file: "raft-computer", sha256: sha(bytes), size: bytes.length } },
    };
  }

  machine(): Machine {
    const home = mkdtempSync(join(tmpdir(), "rci-machine-"));
    const installDir = join(home, "bin");
    mkdirSync(installDir, { recursive: true });
    return new Machine(this, home, installDir);
  }
}

export class Machine {
  constructor(readonly h: Harness, readonly home: string, readonly installDir: string) {}
  get binary(): string { return join(this.installDir, BIN); }
  get kStateDir(): string { return join(this.home, "computer", "k"); }
  env(extra: NodeJS.ProcessEnv = {}): NodeJS.ProcessEnv {
    return {
      ...process.env, HOME: this.home, USERPROFILE: this.home, SHELL: "/bin/zsh", RAFT_HOME: this.home, RAFT_COMPUTER_INSTALL_DIR: this.installDir,
      RAFT_COMPUTER_NO_MODIFY_PATH: WINDOWS ? "1" : "0",
      RAFT_COMPUTER_RELEASE_BASE: `${this.h.base}/computer`, RAFT_COMPUTER_HANDS_ORIGIN: this.h.base,
      RAFT_COMPUTER_INSTALLER_RUNNER: NATIVE ? join(this.h.dist, "native", platformKey, WINDOWS ? "raft-computer-installer-runner.exe" : "raft-computer-installer-runner") : join(this.h.dist, "runner.mjs"),
      RAFT_COMPUTER_NON_INTERACTIVE: "1", ...extra,
    };
  }
  /** The published entry point: install.sh (or install.ps1) from the local installer release base. */
  async bootstrap(args: string[], extra: NodeJS.ProcessEnv = {}): Promise<Result> {
    // Portable mode names the Node to use; native mode and a test that
    // wants "no Node at all" (RAFT_COMPUTER_INSTALLER_NODE: "") leave it unset.
    const env = { ...this.env(extra), RAFT_COMPUTER_INSTALLER_RELEASE_BASE: `${this.h.base}/installer` };
    if (!NATIVE && extra.RAFT_COMPUTER_INSTALLER_NODE === undefined) env.RAFT_COMPUTER_INSTALLER_NODE = process.execPath;
    if (!env.RAFT_COMPUTER_INSTALLER_NODE) delete env.RAFT_COMPUTER_INSTALLER_NODE;
    delete env.RAFT_COMPUTER_INSTALLER_RUNNER;
    return WINDOWS
      ? run("powershell.exe", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", join(this.h.dist, "install.ps1"), ...args], { env, allowFailure: true })
      : run("/bin/sh", [join(this.h.dist, "install.sh"), ...args], { env, allowFailure: true });
  }
  async installer(args: string[], extra: NodeJS.ProcessEnv = {}): Promise<Result> {
    return NATIVE
      ? run(join(this.h.dist, "native", platformKey, WINDOWS ? "raft-computer-installer.exe" : "raft-computer-installer"), args, { env: this.env(extra), allowFailure: true })
      : run(process.execPath, [join(this.h.dist, "cli.cjs"), ...args], { env: this.env(extra), allowFailure: true });
  }
  /** Put a pre-K Computer on PATH, log in and start it, as the old install.sh plus a user would have. */
  async preinstall(version: string): Promise<void> {
    const bytes = this.h.releases.get(version) ?? this.h.publish({ version });
    writeFileSync(this.binary, bytes, { mode: 0o755 });
    chmodSync(this.binary, 0o755);
    await this.loginAndStart();
  }
  async loginAndStart(): Promise<void> {
    await run(this.binary, ["login"], { env: { ...process.env, RAFT_HOME: this.home } });
    await run(this.binary, ["start"], { env: { ...process.env, RAFT_HOME: this.home } });
  }
  async selfVersion(): Promise<string> {
    return (await run(this.binary, ["--version"], { env: { ...process.env, RAFT_HOME: this.home } })).stdout.trim();
  }
  async live(): Promise<{ version: string; pid: number } | null> {
    const r = await run(this.binary, ["status", "--json"], { env: { ...process.env, RAFT_HOME: this.home }, allowFailure: true });
    if (r.code !== 0) return null;
    const a = JSON.parse(r.stdout).attestation;
    return a ? { version: a.computerVersion, pid: a.servicePid } : null;
  }
  async cleanup(): Promise<void> {
    if (existsSync(this.binary)) {
      try { await run(this.binary, ["stop"], { env: { ...process.env, RAFT_HOME: this.home }, allowFailure: true }); } catch { /* none */ }
    }
    rmSync(this.home, { recursive: true, force: true });
  }
}

export interface Result { code: number; stdout: string; stderr: string }
export function run(cmd: string, args: string[], o: { cwd?: string; env?: NodeJS.ProcessEnv; allowFailure?: boolean } = {}): Promise<Result> {
  return new Promise((resolve, reject) => {
    const p = spawn(cmd, args, { cwd: o.cwd, env: o.env ?? process.env, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "", stderr = "";
    p.stdout.on("data", (d) => { stdout += d; });
    p.stderr.on("data", (d) => { stderr += d; });
    p.on("error", (error) => reject(error));
    p.on("close", (code) => {
      const r = { code: code ?? 1, stdout, stderr };
      if (r.code === 0 || o.allowFailure) resolve(r); else reject(new Error(`${cmd} ${args.join(" ")} exited ${r.code}\n${stderr}\n${stdout}`));
    });
  });
}
export function sha(b: Buffer): string { return createHash("sha256").update(b).digest("hex"); }
