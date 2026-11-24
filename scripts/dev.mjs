import {startRuntime} from './runtime.mjs';
const port=Number(process.env.PORT || 8787);
const runtime=await startRuntime({port,persist:true});
console.log('newsman operator console  '+runtime.origin);
console.log('HTTP MCP server          '+runtime.origin+'/mcp');
const tick=setInterval(async()=>{try{await (await runtime.mf.getWorker()).scheduled();}catch(err){console.error(err.message);}},15_000);
for(const sig of ['SIGINT','SIGTERM']) process.once(sig,async()=>{clearInterval(tick);await runtime.close();});
