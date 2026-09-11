#!/usr/bin/env node
import { build } from 'esbuild';
import { mkdir, cp, chmod, writeFile, readFile, stat } from 'node:fs/promises';
import { resolve, join } from 'node:path';
import { createHash } from 'node:crypto';
const root=resolve(new URL('..',import.meta.url).pathname); const out=resolve(process.argv.includes('--native')?process.argv[process.argv.indexOf('--native')+1]||'dist':'dist');
await mkdir(out,{recursive:true});
await build({entryPoints:['src/cli.ts'],bundle:true,platform:'node',format:'cjs',target:'node24',outfile:join(out,'cli.cjs'),sourcemap:false});
await chmod(join(out,'cli.cjs'),0o755);
await cp('scripts/install.sh',join(out,'install.sh')); await cp('scripts/install.ps1',join(out,'install.ps1')); await chmod(join(out,'install.sh'),0o755);
const hash=async p=>createHash('sha256').update(await readFile(p)).digest('hex');
const files=['cli.cjs','install.sh','install.ps1']; const inventory={schema:'raft-computer-installer/release/v1',installerVersion:'0.1.0-rc.1',files:Object.fromEntries(await Promise.all(files.map(async f=>[f,{sha256:await hash(join(out,f)),size:(await stat(join(out,f))).size}])))};
await writeFile(join(out,'installer-manifest.json'),JSON.stringify(inventory,null,2)+'\n');
