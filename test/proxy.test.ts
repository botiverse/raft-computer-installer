// The installer behind an HTTP proxy. curl in the bootstrap follows
// HTTP_PROXY/HTTPS_PROXY/NO_PROXY; the installer's own downloads (Hands,
// manifest, Computer bytes in the runner) must follow the same variables,
// or a machine behind a corporate proxy fetches the installer and then fails.
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { createServer, request as httpRequest, type Server } from "node:http";
import { after, before, describe, it } from "node:test";
import { Harness, type Machine } from "./harness.ts";

const h = new Harness();
const machines: Machine[] = [];
const machine = () => { const m = h.machine(); machines.push(m); return m; };

/** A minimal forward proxy for absolute-URI HTTP requests; records what passed through. */
class RecordingProxy {
  server!: Server;
  url = "";
  readonly paths: string[] = [];
  async start(): Promise<void> {
    this.server = createServer((req, res) => {
      const target = new URL(req.url ?? "/");
      this.paths.push(target.pathname);
      const upstream = httpRequest({ hostname: target.hostname, port: target.port, path: target.pathname + target.search, method: req.method, headers: { ...req.headers, host: target.host } }, (up) => {
        res.writeHead(up.statusCode ?? 502, up.headers);
        up.pipe(res);
      });
      upstream.on("error", () => { res.statusCode = 502; res.end(); });
      req.pipe(upstream);
    });
    await new Promise<void>((r) => this.server.listen(0, "127.0.0.1", r));
    const addr = this.server.address();
    this.url = `http://127.0.0.1:${typeof addr === "object" && addr ? addr.port : 0}`;
  }
  async stop(): Promise<void> {
    this.server.closeAllConnections();
    await new Promise<void>((r) => this.server.close(() => r()));
  }
}

const proxy = new RecordingProxy();
const DEAD_PROXY = "http://127.0.0.1:9";

before(async () => {
  await h.start();
  h.publish({ version: "1.1.0" });
  h.channel.main = "1.1.0";
  await proxy.start();
});
after(async () => { for (const m of machines) await m.cleanup(); await proxy.stop(); await h.stop(); });

describe("behind an HTTP proxy", () => {
  it("installs through the proxy: release authority, manifest and the runner's Computer download all pass through it", async () => {
    const m = machine();
    const before = proxy.paths.length;
    const r = await m.bootstrap([], { HTTP_PROXY: proxy.url, http_proxy: proxy.url, HTTPS_PROXY: proxy.url, https_proxy: proxy.url, NO_PROXY: "", no_proxy: "" });
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Installed 1\.1\.0\./m);
    assert.equal(await m.selfVersion(), "1.1.0");
    const seen = proxy.paths.slice(before);
    assert.ok(seen.some((p) => p.endsWith("/latest")), `release authority lookup must go through the proxy; saw ${seen.join(" ")}`);
    assert.ok(seen.includes("/computer/1.1.0/manifest.json"), `manifest must go through the proxy; saw ${seen.join(" ")}`);
    assert.ok(seen.includes("/computer/1.1.0/raft-computer"), `the runner's Computer download must go through the proxy; saw ${seen.join(" ")}`);
  });

  it("a proxy that is down fails the installer before any change, instead of being silently bypassed", async () => {
    const m = machine();
    // The installer binary directly: curl is not involved, so the failure is the installer's own fetch honouring the proxy.
    const r = await m.installer(["install"], { HTTP_PROXY: DEAD_PROXY, http_proxy: DEAD_PROXY, HTTPS_PROXY: DEAD_PROXY, https_proxy: DEAD_PROXY, NO_PROXY: "", no_proxy: "" });
    assert.notEqual(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout + r.stderr, /the release server could not be reached\. Nothing changed\./);
    assert.equal(existsSync(m.binary), false, "nothing is installed");
  });

  it("NO_PROXY exempts a host from a proxy that is down", async () => {
    const m = machine();
    const r = await m.installer(["install"], { HTTP_PROXY: DEAD_PROXY, http_proxy: DEAD_PROXY, HTTPS_PROXY: DEAD_PROXY, https_proxy: DEAD_PROXY, NO_PROXY: "127.0.0.1", no_proxy: "127.0.0.1" });
    assert.equal(r.code, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /^Installed 1\.1\.0\./m);
  });
});
