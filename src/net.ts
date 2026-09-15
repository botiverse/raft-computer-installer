// One fetch for everything the installer downloads. Node's global fetch does
// not read HTTP_PROXY / HTTPS_PROXY / NO_PROXY (curl in the bootstrap does),
// so a machine behind a corporate proxy fetched the installer fine and then
// failed at Hands and the CDN. When any proxy variable is set, route through
// undici's EnvHttpProxyAgent, which honours all three including NO_PROXY;
// otherwise stay on the platform fetch. Setting NODE_USE_ENV_PROXY in-process
// is too late (Node reads it at startup), so this is an explicit dispatcher.
import { EnvHttpProxyAgent, fetch as undiciFetch } from "undici";

const PROXY_VARS = ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "https_proxy", "http_proxy", "all_proxy"];

export function proxyConfigured(env: NodeJS.ProcessEnv = process.env): boolean {
  return PROXY_VARS.some((name) => (env[name] ?? "").trim() !== "");
}

let agent: EnvHttpProxyAgent | null = null;
function dispatcher(): EnvHttpProxyAgent {
  return (agent ??= new EnvHttpProxyAgent());
}

/** fetch that follows the proxy environment like curl does. */
export const netFetch: typeof fetch = (input, init) => {
  if (!proxyConfigured()) return fetch(input, init);
  const proxied = Object.assign({}, init ?? {}, { dispatcher: dispatcher() });
  return undiciFetch(input as never, proxied as never) as unknown as Promise<Response>;
};

/**
 * The runner is a separate Node process whose downloads run inside K, which
 * uses the platform fetch. Node only reads NODE_USE_ENV_PROXY at startup, so
 * the parent sets it on the child when a proxy is configured; the child then
 * honours the same variables curl and this process do.
 */
export function proxyEnvForChild(env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  return proxyConfigured(env) ? { ...env, NODE_USE_ENV_PROXY: "1" } : env;
}
