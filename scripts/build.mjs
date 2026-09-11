#!/usr/bin/env node
// Bundle the entry (cli.cjs) and the K runner (runner.mjs), copy the
// bootstrap scripts, and write the installer manifest that SHA256SUMS and
// the bootstrap verify against.
import { build } from 'esbuild';
import { mkdir, cp, chmod, writeFile, readFile, stat } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { createHash } from 'node:crypto';

const out = resolve(process.argv[2] && !process.argv[2].startsWith('--') ? process.argv[2] : 'dist');
await mkdir(out, { recursive: true });
const common = { bundle: true, platform: 'node', format: 'cjs', target: 'node24', sourcemap: false, logLevel: 'warning' };
await build({ ...common, entryPoints: ['src/cli.ts'], outfile: join(out, 'cli.cjs') });
await build({ ...common, format: 'esm', entryPoints: ['src/runner.ts'], outfile: join(out, 'runner.mjs'), banner: { js: '#!/usr/bin/env node' } });
await chmod(join(out, 'cli.cjs'), 0o755);
await chmod(join(out, 'runner.mjs'), 0o755);
await cp('scripts/install.sh', join(out, 'install.sh'));
await chmod(join(out, 'install.sh'), 0o755);
const hash = async (p) => createHash('sha256').update(await readFile(p)).digest('hex');
const files = ['cli.cjs', 'runner.mjs', 'install.sh'];
const pkg = JSON.parse(await readFile('package.json', 'utf8'));
const inventory = { schema: 'raft-computer-installer/release/v2', installerVersion: pkg.version,
  files: Object.fromEntries(await Promise.all(files.map(async (f) => [f, { sha256: await hash(join(out, f)), size: (await stat(join(out, f))).size }]))) };
await writeFile(join(out, 'installer-manifest.json'), `${JSON.stringify(inventory, null, 2)}\n`);
await writeFile(join(out, 'SHA256SUMS'), Object.entries(inventory.files).map(([f, m]) => `${m.sha256}  ${f}\n`).join(''));
console.log(`built ${files.join(', ')} in ${out}`);
