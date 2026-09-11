// Putting the install directory on PATH is part of installing. Only the
// default directory is written into a shell profile; a custom one is the
// user's to arrange, and the line says so either way.
import { appendFile, mkdir, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import type { Config } from "./config.js";

export async function ensureOnPath(cfg: Config, env: NodeJS.ProcessEnv): Promise<string> {
  const onPath = (env.PATH ?? "").split(":").some((p) => p && resolve(p) === cfg.installDir);
  if (onPath) return "";
  const home = env.HOME ?? homedir();
  if (cfg.installDir !== join(home, ".local", "bin")) return ` Add ${cfg.installDir} to your PATH.`;
  if (env.RAFT_COMPUTER_NO_MODIFY_PATH === "1") return ` Add ${cfg.installDir} to your PATH.`;
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
