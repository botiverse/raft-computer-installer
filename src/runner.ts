// The K runner: K plus the Computer adapter, bundled into runner.mjs. It
// serves one request on stdin, prints one response, exits. The supervisor in
// cli.cjs launches and, if it dies, recovers it.
import { createRunner, serveRunner } from "@botiverse/k-carrier";
import { loadConfig } from "./config.js";
import { createHostAdapter } from "./hostAdapter.js";
import { createReleaseSource } from "./source.js";

const cfg = loadConfig();
process.exitCode = await serveRunner(() => createRunner({
  stateDir: cfg.kStateDir,
  host: createHostAdapter(cfg),
  source: createReleaseSource(cfg),
  policy: "confirm",
  notificationSink: async () => {},
  provenanceIdentity: { who: process.env.RAFT_COMPUTER_APPROVED_BY ?? "local-operator", carrier: "raft-computer-installer" },
}));
