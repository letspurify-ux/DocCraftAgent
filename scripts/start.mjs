import {spawnSync,spawn} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
const root=fileURLToPath(new URL('../',import.meta.url));
const npm=process.platform==='win32'?'npm.cmd':'npm';
for(const [cmd,args,cwd] of [[npm,['ci'],path.join(root,'frontend')],[npm,['run','build'],path.join(root,'frontend')],['cargo',['build','--locked'],root]]){
  const result=spawnSync(cmd,args,{cwd,stdio:'inherit',shell:process.platform==='win32'&&cmd===npm});
  if(result.status!==0)process.exit(result.status??1);
}
const app=spawn(path.join(root,'target','debug',process.platform==='win32'?'doccraft-agent.exe':'doccraft-agent'),[],{cwd:root,stdio:'inherit',env:process.env});
process.on('SIGINT',()=>app.kill('SIGINT'));process.on('SIGTERM',()=>app.kill('SIGTERM'));
app.on('exit',code=>process.exit(code??0));
