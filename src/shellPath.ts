// Putting the install directory on PATH is part of installing. Only the
// default directory is written into a shell profile; a custom one is the
// user's to arrange, and the line says so either way.
import { appendFile, mkdir, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { IS_WINDOWS, type Config } from "./config.js";
import { runCommand } from "./computer.js";

/** Windows: the user's PATH in the registry, which new consoles read. */
async function ensureOnUserPathWindows(cfg: Config): Promise<string> {
  const read = await runCommand("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", "[Environment]::GetEnvironmentVariable('Path','User')"], process.env, 30_000).catch(() => null);
  const current = read?.code === 0 ? read.stdout.trim() : "";
  const has = current.split(";").some((p) => p && resolve(p).toLowerCase() === cfg.installDir.toLowerCase());
  if (has) return ` Open a new terminal to use it.`;
  const next = current ? `${current};${cfg.installDir}` : cfg.installDir;
  const write = await runCommand("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command",
    `[Environment]::SetEnvironmentVariable('Path', ${JSON.stringify(next).replace(/\$/g, "`$")}, 'User')`], process.env, 30_000).catch(() => null);
  return write?.code === 0 ? ` Added ${cfg.installDir} to your PATH; open a new terminal.` : ` Add ${cfg.installDir} to your PATH.`;
}

export async function ensureOnPath(cfg: Config, env: NodeJS.ProcessEnv): Promise<string> {
  const sep = IS_WINDOWS ? ";" : ":";
  const onPath = (env.PATH ?? env.Path ?? "").split(sep).some((p) => p && (IS_WINDOWS ? resolve(p).toLowerCase() === cfg.installDir.toLowerCase() : resolve(p) === cfg.installDir));
  if (onPath) return "";
  const home = (IS_WINDOWS ? env.USERPROFILE : env.HOME) ?? homedir();
  if (cfg.installDir !== join(home, ".local", "bin")) return ` Add ${cfg.installDir} to your PATH.`;
  if (env.RAFT_COMPUTER_NO_MODIFY_PATH === "1") return ` Add ${cfg.installDir} to your PATH.`;
  if (IS_WINDOWS) return ensureOnUserPathWindows(cfg);
  const shell = (env.SHELL ?? "").split("/").pop();
  const profile = shell === "zsh" ? join(env.ZDOTDIR ?? home, ".zshrc") : shell === "bash" ? join(home, ".bashrc") : null;
  const line = 'export PATH="$HOME/.local/bin:$PATH"';
  if (!profile) return ` Add ${cfg.installDir} to your PATH: ${line}`;
  try {
    const existing = await readFile(profile, "utf8").catch(() => "");
    if (!existing.split("\n").includes(line)) {
      await mkdir(join(profile, ".."), { recursive: true });
      await appendFile(profile, `\n# raft-computer\n${line}\n`);
    }
    return ` Added ${cfg.installDir} to PATH in ${profile}; open a new terminal.`;
  } catch {
    return ` Add ${cfg.installDir} to your PATH: ${line}`;
  }
}
