import { mkdir, access } from 'node:fs/promises';
import { constants } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {createHash,randomBytes} from 'node:crypto';
import { chromium } from 'playwright-core';
import { startRuntime, webClient } from './runtime.mjs';
import { seedDemo } from './demo.mjs';

const output=fileURLToPath(new URL('../spec/shots/',import.meta.url));
const executable = process.env.CHROMIUM_BIN || (await access('/usr/bin/chromium',constants.X_OK).then(()=>'/usr/bin/chromium').catch(()=>undefined));
const callback='http://127.0.0.1:9876/callback';
const client='studio-assistant';
const app=await startRuntime({bindings:{LOCAL_OPERATOR_EMAIL:'editor@example.org',OAUTH_CLIENTS:JSON.stringify({[client]:[callback]})}});
let browser;
try {
  const id=await seedDemo(app.origin);
  // Allow the staged local import to finish before capturing the audience counters.
  const api=await webClient(app.origin);
  for(let i=0;i<6;i++){
    const rows=await api('subscribers_list',{limit:100});
    if(rows.items.length>=36)break;
    await (await app.mf.getWorker()).scheduled();
  }
  // Authorize the demo client through the same PKCE flow used by a real installation.
  const verifier=randomBytes(36).toString('base64url');
  const challenge=createHash('sha256').update(verifier).digest('base64url');
  const resource=app.origin+'/mcp';
  const params=new URLSearchParams({client_id:client,redirect_uri:callback,response_type:'code',state:randomBytes(16).toString('hex'),resource,scope:'read write',code_challenge:challenge,code_challenge_method:'S256'});
  const consent=await fetch(app.origin+'/oauth/authorize?'+params);
  if(!consent.ok) throw new Error('Demo client could not request consent: '+consent.status);
  const html=await consent.text();
  const field=name=>html.match(new RegExp('name="'+name+'" value="([^"]+)"'))?.[1];
  const submit=(path,values)=>fetch(app.origin+path,{method:'POST',headers:{Origin:app.origin},body:new URLSearchParams(values),redirect:'manual'});
  const approval=await submit('/oauth/authorize',{decision:'approve',handle:field('handle'),csrf:field('csrf')});
  if(approval.status!==303)throw new Error('Demo consent failed: '+approval.status);
  const code=new URL(approval.headers.get('location')).searchParams.get('code');
  const token=await submit('/oauth/token',{grant_type:'authorization_code',client_id:client,redirect_uri:callback,code_verifier:verifier,resource,code});
  if(!token.ok)throw new Error('Demo authorization exchange failed: '+token.status);
  browser=await chromium.launch({executablePath:executable,headless:true});
  const page=await browser.newPage({viewport:{width:1440,height:1000},deviceScaleFactor:1,colorScheme:'light'});
  await mkdir(output,{recursive:true});
  for(const [name,route,label] of [['overview','overview','Publishing desk'],['compose','compose/'+id,''],['mcp','connections','']]){
    await page.goto(app.origin+'/#/'+route);
    await page.waitForFunction(()=>document.querySelector('main[aria-busy="false"]')?.querySelector('h1'));
    await page.evaluate(()=>document.fonts.ready);
    if(label)await page.getByRole('heading',{name:label,exact:true}).waitFor();
    if(await page.getByText('Console unavailable',{exact:true}).count())throw new Error('Console failed to load.');
    await page.screenshot({path:output+name+'.png',animations:'disabled'});
    console.log(name,await page.locator('h1').innerText());
  }
}finally{await browser?.close();await app.close();}
