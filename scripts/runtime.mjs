// Local Workers runtime. No live SES credentials are read by the development launcher.
import { Miniflare, convertV4MiniflareOptions } from 'miniflare';
import { readFile, readdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { resolve, join } from 'node:path';
import { createServer } from 'node:net';

const root = fileURLToPath(new URL('../', import.meta.url));
async function availablePort() {
  const socket = createServer();
  await new Promise((ok, fail) => { socket.once('error', fail); socket.listen(0, '127.0.0.1', ok); });
  const port = socket.address().port;
  await new Promise(ok => socket.close(ok));
  return port;
}

export async function startRuntime({ port = 0, persist = false, bindings = {} } = {}) {
  const localPort = port || await availablePort();
  const origin = 'http://127.0.0.1:' + localPort;
  const path = name => resolve(root, name);
  const persistence = persist ? path('.newsman/local') : false;
  const mf = new Miniflare(convertV4MiniflareOptions({
    host: '127.0.0.1', port: localPort, cf: false,
    compatibilityDate: '2026-11-23',
    modulesRoot: path('build'),
    modules: [
      {type:'ESModule', path:path('build/index.js')},
      {type:'CompiledWasm', path:path('build/index_bg.wasm')},
    ],
    d1Databases: {DB:'newsman-local'}, d1Persist: persistence,
    r2Buckets: {FILES:'newsman-files'}, r2Persist: persistence,
    queueProducers: {SEND_QUEUE:'newsman-send'},
    queueConsumers: {'newsman-send':{maxBatchSize:10,maxBatchTimeout:1}},
    serviceBindings: {ASSETS: async request => {
      const pathname = new URL(request.url).pathname;
      const relative = pathname === '/' ? 'index.html' : pathname.slice(1);
      if (!/^(?:[a-z-]+\.(?:html|css|js|svg)|fonts\/[a-z-]+\.woff2)$/.test(relative)) return new Response('Not found',{status:404});
      try {
        const bytes=await readFile(path('public/'+relative));
        const types={html:'text/html; charset=utf-8',css:'text/css',js:'text/javascript',svg:'image/svg+xml',woff2:'font/woff2'};
        return new Response(bytes,{headers:{'Content-Type':types[relative.split('.').pop()]}});
      } catch {return new Response('Not found',{status:404});}
    }},
    bindings: {
      NEWSMAN_LOCAL:'yes',PUBLIC_ORIGIN:origin,PUBLICATION_ORIGIN:origin,
      LOCAL_OPERATOR_EMAIL:'operator@newsman.test',
      OAUTH_CLIENTS:JSON.stringify({'newsman-local':['http://127.0.0.1:9876/callback']}),
      ...bindings,
    },
  }));
  try {
    await mf.ready;
    const db = await mf.getD1Database('DB');
    const directory = path('migrations');
    for (const name of (await readdir(directory)).filter(n=>n.endsWith('.sql')).sort()) {
      const sql = await readFile(join(directory,name),'utf8');
      // D1 exec uses a newline-delimited statement protocol.
      for (const stmt of sql.split(';').map(s=>s.trim()).filter(Boolean)) {
        await db.prepare(stmt).run();
      }
    }
    return {mf,origin,close:()=>mf.dispose()};
  } catch (error) { await mf.dispose(); throw error; }
}

export async function webClient(origin) {
  const session = await fetch(origin+'/api/session');
  if (!session.ok) throw new Error('Could not open local operator session: '+await session.text());
  const {csrf} = await session.json();
  return async (name,args={}) => {
    const res=await fetch(origin+'/api/ops/'+name,{method:'POST',headers:{'Content-Type':'application/json','X-CSRF-Token':csrf,Origin:origin},body:JSON.stringify(args)});
    const data=await res.json();
    if(!res.ok) throw new Error(name+' ('+res.status+'): '+JSON.stringify(data));
    return data;
  };
}
