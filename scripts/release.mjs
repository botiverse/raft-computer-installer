#!/usr/bin/env node
import { readFile, readdir, stat } from 'node:fs/promises'; import { join, resolve } from 'node:path'; import { createHash } from 'node:crypto';
const dir=resolve(process.argv.at(-1)||'dist'); const manifest=JSON.parse(await readFile(join(dir,'installer-manifest.json'),'utf8')); const hex=async p=>createHash('sha256').update(await readFile(p)).digest('hex');
if(process.argv.includes('verify')){for(const [name,m] of Object.entries(manifest.files)){const p=join(dir,name); const s=await stat(p); if(s.size!==m.size||await hex(p)!==m.sha256) throw new Error(`release_integrity:${name}`)} console.log(JSON.stringify({ok:true,installerVersion:manifest.installerVersion,files:Object.keys(manifest.files)})); process.exit(0)}
console.log(JSON.stringify(manifest,null,2));
