import {createHash} from 'node:crypto';
import {pathToFileURL} from 'node:url';
import {webClient} from './runtime.mjs';

const examples = [
  {title:'A first look at the open studio',subject:'Come inside our open studio',preheader:'An evening of work in progress, demonstrations and conversations.',
   paragraphs:['The doors open at six. Step inside the workshop for a look at the prototypes taking shape on our benches.','Bring a question, meet the makers behind each piece, and try a few materials yourself.']},
  {title:'Notes on the new collection',subject:'Three additions to the collection',preheader:'New materials and the small details that shaped the latest release.',
   paragraphs:['Our new collection brings together three tools designed for everyday use. Each began as a sketch, then went through months of testing at the studio.','Here are the details we kept coming back to, and the people who helped us refine them.']},
  {title:'The printmaking workshop',subject:'Join us for a printmaking workshop',preheader:'A hands-on introduction to carving, inking and pulling your first relief print.',
   paragraphs:['Join us at the studio for an afternoon of relief printing. You will carve a small block, mix an ink color and pull an edition to take home.','We will start at the workbench and finish at the press. Bring a small sketch if you have one. The studio will supply the tools, ink and paper.']}
];
function draft(data) {
  return {title:data.title,subject:data.subject,preheader:data.preheader,
    html:data.paragraphs.map(p=>'<p>'+p+'</p>').join('\n'),plain_text:data.paragraphs.join('\n\n')};
}

export async function seedDemo(origin) {
  if(!['127.0.0.1','localhost'].includes(new URL(origin).hostname)) throw new Error('Demo data requires a loopback installation.');
  const s=await (await fetch(origin+'/api/session')).json();
  if(!s.local) throw new Error('Demo data requires the local capture transport.');
  const api=await webClient(origin);
  const existing=await api('issues_list',{search:examples[2].title});
  if(existing.items.length)return existing.items[0].id;
  const settings=await api('settings_get');
  await api('settings_update',{key:'demo-settings-v1',expected_version:settings.version,settings:{...settings.settings,
    publication_name:'Field Notes Studio',from_address:'studio@example.org',reply_to:'hello@example.org',
    postal_footer:'Field Notes Studio · 10 Sample Lane · Example City',send_budget:5000,
    test_recipients:['proof@example.org']}});
  const csv=['email,status,evidence_kind,tags',...Array.from({length:36},(_,i)=>
    'reader'+String(i+1).padStart(2,'0')+'@example.org,active,double_opt_in,'+(i<12?'workshops':'studio'))].join('\n');
  const buf=Buffer.from(csv);const sha=createHash('sha256').update(buf).digest('hex');
  const file=await api('file_begin',{key:'demo-upload-v1',kind:'audience_import',size:buf.length,sha256:sha});
  await api('file_write',{key:'demo-chunk-v1',id:file.id,offset:0,base64:buf.toString('base64'),chunk_sha256:sha});
  await api('file_finish',{key:'demo-finish-v1',id:file.id});
  const plan=await api('import_prepare',{key:'demo-import-plan-v1',files:[file.id],mapping_version:'portable-v1',cutoff:'2026-11-01T00:00:00Z'});
  const job=await api('import_commit',{key:'demo-import-commit-v1',plan_id:plan.id,plan_digest:plan.digest});
  // Background batches continue independently through the local scheduler.
  if(job.state==='failed')throw new Error('Could not import demo audience.');
  let result;
  for(const [i,message] of examples.entries())result=await api('issue_create',{key:'demo-message-0'+i,draft:draft(message)});
  return result.id;
}
if(process.argv[1] && import.meta.url===pathToFileURL(process.argv[1]).href){
  const origin=process.argv[2]||'http://127.0.0.1:8787';
  console.log('Example draft:',origin+'/#/compose/'+await seedDemo(origin));
}
