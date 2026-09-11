// SemVer 2.0.0 precedence without a dependency. Invalid input is an error,
// never a guess: a version we cannot order is a version we refuse to act on.
const CORE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/;

export interface SemVer { major: number; minor: number; patch: number; pre: string[] }

export function parseSemver(v: string): SemVer | null {
  const m = CORE.exec(v.trim().replace(/^v/, ""));
  if (!m) return null;
  return { major: Number(m[1]), minor: Number(m[2]), patch: Number(m[3]), pre: m[4] ? m[4].split(".") : [] };
}

export function isSemver(v: string): boolean { return parseSemver(v) !== null; }

/** -1 if a < b, 0 if equal, 1 if a > b. Throws on invalid input. */
export function compareSemver(a: string, b: string): -1 | 0 | 1 {
  const pa = parseSemver(a), pb = parseSemver(b);
  if (!pa) throw new Error(`invalid version: ${a}`);
  if (!pb) throw new Error(`invalid version: ${b}`);
  for (const k of ["major", "minor", "patch"] as const) {
    if (pa[k] !== pb[k]) return pa[k] < pb[k] ? -1 : 1;
  }
  if (pa.pre.length === 0 && pb.pre.length === 0) return 0;
  if (pa.pre.length === 0) return 1;
  if (pb.pre.length === 0) return -1;
  const n = Math.min(pa.pre.length, pb.pre.length);
  for (let i = 0; i < n; i++) {
    const x = pa.pre[i], y = pb.pre[i];
    if (x === y) continue;
    const xn = /^\d+$/.test(x), yn = /^\d+$/.test(y);
    if (xn && yn) return Number(x) < Number(y) ? -1 : 1;
    if (xn) return -1;
    if (yn) return 1;
    return x < y ? -1 : 1;
  }
  if (pa.pre.length === pb.pre.length) return 0;
  return pa.pre.length < pb.pre.length ? -1 : 1;
}
