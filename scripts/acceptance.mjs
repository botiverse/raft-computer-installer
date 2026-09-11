#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdtemp, writeFile, chmod } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
const dir=process.argv.at(-1)||'dist'; const bin=join(process.cwd(),dir,'cli.cjs');
const run=(args,input,env={},allowFailure=false)=>new Promise((resolve,reject)=>{const p=spawn(process.execPath,[bin,...args],{env:{...process.env,...env},stdio:['pipe','pipe','pipe']});let o='',e='';p.stdout.on('data',x=>o+=x);p.stderr.on('data',x=>e+=x);p.on('close',c=>(c===0||allowFailure)?resolve(o):reject(new Error(`exit ${c}: ${e} stdout=${o}`)));p.stdin.end(input??'')});
const version=(await run(['--version'])).trim(); const root=await mkdtemp(join(tmpdir(),'raft-installer-'));
const artifactFor=(reported)=>Buffer.from(`#!/bin/sh\ncase "$1" in stop|start) exit 0;; status) printf '{"service":{"running":true,"pid":%s,"version":{"version":"${reported}","evidenceWrittenAt":"acceptance-start"}}}\n' "$$";; esac\n`);
const product=(version,artifact)=>{const url=`data:application/octet-stream;base64,${artifact.toString('base64')}`;return `data:application/json,${encodeURIComponent(JSON.stringify({version,targets:{[`${process.platform}-${process.arch}`]:{file:url,sha256:createHash('sha256').update(artifact).digest('hex'),size:artifact.length}}}))}`};
const first=artifactFor('1.2.3'); const initial=join(root,'initial'); await writeFile(initial,first,{mode:0o755}); await chmod(initial,0o755);
const req={protocol:'raft-computer-installer/v1',installerVersion:version,computerVersion:'1.2.3',operation:'upgrade',operationId:'acceptance-1',installDir:root,artifactUrl:product('1.2.3',first)};
const receipt=JSON.parse(await run(['--request'],JSON.stringify(req),{RAFT_COMPUTER_BINARY:initial})); if(receipt.status!=='succeeded')throw new Error(`install acceptance failed: ${JSON.stringify(receipt)}`);
const bad=artifactFor('1.2.3'); const badReq={...req,computerVersion:'9.9.9',operationId:'acceptance-rollback',artifactUrl:product('9.9.9',bad)}; const rollback=JSON.parse(await run(['--request'],JSON.stringify(badReq),{},true)); if(rollback.status!=='rolled_back')throw new Error(`rollback acceptance failed: ${JSON.stringify(rollback)}`);
console.log(JSON.stringify({ok:true,version,receipt,rollback}));
