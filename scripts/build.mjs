#!/usr/bin/env node
// Bundle the entry (cli.cjs) and the K runner (runner.mjs), copy the
// bootstrap, and write the installer manifest and SHA256SUMS.
//
//   node scripts/build.mjs [dist]            portable: cli.cjs + runner.mjs, needs Node 24
//   node scripts/build.mjs [dist] --native   also two single executables for this platform:
//                                            native/<platform-arch>/raft-computer-installer{,-runner}
import { build } from 'esbuild';
import { execFileSync } from 'node:child_process';
import { mkdir, cp, chmod, writeFile, readFile, stat, rm, copyFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { createHash } from 'node:crypto';

const args = process.argv.slice(2);
const out = resolve(args.find((a) => !a.startsWith('--')) ?? 'dist');
const native = args.includes('--native');
await mkdir(out, { recursive: true });
const common = { bundle: true, platform: 'node', format: 'cjs', target: 'node24', sourcemap: false, logLevel: 'warning' };
await build({ ...common, entryPoints: ['src/cli.ts'], outfile: join(out, 'cli.cjs') });
await build({ ...common, format: 'esm', entryPoints: ['src/runner.ts'], outfile: join(out, 'runner.mjs'), banner: { js: '#!/usr/bin/env node' } });
await chmod(join(out, 'cli.cjs'), 0o755);
await chmod(join(out, 'runner.mjs'), 0o755);
await cp('scripts/install.sh', join(out, 'install.sh'));
await chmod(join(out, 'install.sh'), 0o755);
const files = ['cli.cjs', 'runner.mjs', 'install.sh'];

if (native) {
  const platform = `${process.platform}-${process.arch}`;
  const dir = join(out, 'native', platform);
  await mkdir(dir, { recursive: true });
  const work = join(out, '.sea');
  await mkdir(work, { recursive: true });
  await build({ ...common, entryPoints: ['src/runner.ts'], outfile: join(work, 'runner.cjs') });
  for (const [name, entry] of [['raft-computer-installer', join(out, 'cli.cjs')], ['raft-computer-installer-runner', join(work, 'runner.cjs')]]) {
    const blob = join(work, `${name}.blob`);
    const config = join(work, `${name}.sea.json`);
    await writeFile(config, JSON.stringify({ main: entry, output: blob, disableExperimentalSEAWarning: true }));
    execFileSync(process.execPath, ['--experimental-sea-config', config], { stdio: 'inherit' });
    const bin = join(dir, name);
    await rm(bin, { force: true });
    await copyFile(process.execPath, bin);
    await chmod(bin, 0o755);
    if (process.platform === 'darwin') execFileSync('codesign', ['--remove-signature', bin], { stdio: 'inherit' });
    const postject = ['postject', bin, 'NODE_SEA_BLOB', blob, '--sentinel-fuse', 'NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2',
      ...(process.platform === 'darwin' ? ['--macho-segment-name', 'NODE_SEA'] : [])];
    execFileSync('npx', postject, { stdio: 'inherit' });
    if (process.platform === 'darwin') execFileSync('codesign', ['--sign', process.env.RAFT_CODESIGN_IDENTITY ?? '-', bin], { stdio: 'inherit' });
    files.push(`native/${platform}/${name}`);
  }
  await rm(work, { recursive: true, force: true });
}

const hash = async (p) => createHash('sha256').update(await readFile(p)).digest('hex');
const pkg = JSON.parse(await readFile('package.json', 'utf8'));
const inventory = { schema: 'raft-computer-installer/release/v2', installerVersion: pkg.version, node: process.version,
  files: Object.fromEntries(await Promise.all(files.map(async (f) => [f, { sha256: await hash(join(out, f)), size: (await stat(join(out, f))).size }]))) };
await writeFile(join(out, 'installer-manifest.json'), `${JSON.stringify(inventory, null, 2)}\n`);
await writeFile(join(out, 'SHA256SUMS'), Object.entries(inventory.files).map(([f, m]) => `${m.sha256}  ${f}\n`).join(''));
console.log(`built ${files.join(', ')} in ${out}`);
