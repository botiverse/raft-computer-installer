// Where Computer releases come from. The CDN manifest for one exact version
// names the bytes; Hands decides which version a channel means. Both are
// consulted by the entry; the runner only ever asks for an exact version.
import type { Release, ReleaseSource } from "@botiverse/k-carrier";
import { platformKey, type Config } from "./config.js";

export interface ManifestTarget { file: string; sha256: string; size: number; gz?: { file: string; sha256: string; size: number } }
export interface Manifest { version: string; target: ManifestTarget; sidecar: { file: string; sha256: string; size: number } | null; base: string }

function hex64(v: unknown): v is string { return typeof v === "string" && /^[a-f0-9]{64}$/i.test(v); }
function size(v: unknown): v is number { return Number.isSafeInteger(v) && (v as number) > 0; }

async function fetchJson(url: string, what: string): Promise<Record<string, unknown>> {
  let r: Response;
  try { r = await fetch(url, { signal: AbortSignal.timeout(30_000) }); }
  catch (error) { throw new Error(`${what} unreachable: ${error instanceof Error ? error.message : String(error)}`); }
  if (!r.ok) throw new Error(`${what} returned HTTP ${r.status}`);
  return await r.json() as Record<string, unknown>;
}

/** The CDN manifest for exactly this version, for this machine. */
export async function fetchManifest(cfg: Config, version: string): Promise<Manifest> {
  const base = `${cfg.releaseBase}/${version}`;
  const m = await fetchJson(`${base}/manifest.json`, "release manifest");
  if (m.version !== version) throw new Error(`release manifest is for ${String(m.version)}, not ${version}`);
  const key = platformKey();
  const t = (m.targets as Record<string, Record<string, unknown>> | undefined)?.[key];
  if (!t || typeof t.file !== "string" || !hex64(t.sha256) || !size(t.size)) throw new Error(`no release for ${key} in ${version}`);
  const gzRaw = t.gz as Record<string, unknown> | undefined;
  const gz = gzRaw && typeof gzRaw.file === "string" && hex64(gzRaw.sha256) && size(gzRaw.size)
    ? { file: gzRaw.file, sha256: gzRaw.sha256.toLowerCase(), size: gzRaw.size } : undefined;
  const sc = m.photonWasm as Record<string, unknown> | undefined;
  const sidecar = sc && typeof sc.file === "string" && hex64(sc.sha256) && size(sc.size)
    ? { file: sc.file, sha256: sc.sha256.toLowerCase(), size: sc.size } : null;
  return { version, target: { file: t.file, sha256: t.sha256.toLowerCase(), size: t.size, ...(gz ? { gz } : {}) }, sidecar, base };
}

export function releaseOf(m: Manifest): Release {
  const url = (file: string) => new URL(file, `${m.base}/`).toString();
  return {
    version: m.version, url: url(m.target.file), sha256: m.target.sha256, size: m.target.size,
    ...(m.target.gz ? { gzip: { url: url(m.target.gz.file), sha256: m.target.gz.sha256, size: m.target.gz.size } } : {}),
  };
}

export function sidecarReleaseOf(m: Manifest): Release | null {
  if (!m.sidecar) return null;
  return { version: m.version, url: new URL(m.sidecar.file, `${m.base}/`).toString(), sha256: m.sidecar.sha256, size: m.sidecar.size };
}

/** The runner's source: exact versions only; the entry already chose one. */
export function createReleaseSource(cfg: Config): ReleaseSource {
  return {
    async checkForUpdate() { return null; },
    async fetchRelease(version) { return releaseOf(await fetchManifest(cfg, version)); },
  };
}

export interface HandsResolution { version: string; sha256: string; size: number }

/** Which version a channel means right now, and the bytes Hands says it is. */
export async function resolveChannel(cfg: Config, channel: "main" | "alpha"): Promise<HandsResolution> {
  const url = `${cfg.handsOrigin}/public/v2/apps/${cfg.handsApp}/latest?channel=${channel}&product_type=cli-binary`;
  const body = await fetchJson(url, "release authority");
  const version = (body.build as Record<string, unknown> | undefined)?.version;
  if (typeof version !== "string" || !version) throw new Error(`release authority named no version for channel ${channel}`);
  const [platform, arch] = platformKey().split("-");
  const assets = (Array.isArray(body.assets) ? body.assets : []) as Record<string, unknown>[];
  const raw = assets.filter((a) => a.platform === platform && a.arch === arch && a.filetype === "binary" && a.variant === null);
  if (raw.length !== 1) throw new Error(`release authority lists ${raw.length} raw binaries for ${platformKey()} at ${version}; expected one`);
  const a = raw[0];
  if (!hex64(a.sha256) || !size(a.size_bytes)) throw new Error("release authority asset carries no sha256/size");
  return { version, sha256: a.sha256.toLowerCase(), size: a.size_bytes };
}

/** The authority and the byte store must describe the same bytes. */
export function assertSameIdentity(hands: HandsResolution, m: Manifest): void {
  if (hands.sha256 !== m.target.sha256 || hands.size !== m.target.size) {
    throw new Error(`release authority and CDN disagree about ${m.version} for ${platformKey()}: sha256 ${hands.sha256}/${m.target.sha256}, size ${hands.size}/${m.target.size}`);
  }
}
