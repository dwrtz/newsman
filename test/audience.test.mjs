import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import test from 'node:test';
import { startRuntime, webClient } from '../scripts/runtime.mjs';

const key = () => randomUUID();
const sha = bytes => createHash('sha256').update(bytes).digest('hex');

async function upload(call, text, kind = 'audience_import') {
  const data = Buffer.from(text);
  const file = await call('file_begin', { key:key(), kind, size:data.length, sha256:sha(data) });
  await call('file_write', { key:key(), id:file.id, offset:0, base64:data.toString('base64'), chunk_sha256:sha(data) });
  await call('file_finish', { key:key(), id:file.id });
  return file.id;
}

test('import pauses across batches, resumes in order, and a stale snapshot cannot undo an operator hold', async t => {
  const {mf, origin, close} = await startRuntime();
  t.after(close);
  const call = await webClient(origin);
  const timestamp = new Date().toISOString();
  const entries = Array.from({ length: 21 }, (_,i)=>`member-${String(i).padStart(2,'0')}@example.test,active,${timestamp}`);
  const input = await upload(call, ['email,status,confirmed_at', ...entries, 'member-20@example.test,unsubscribed,'].join('\n'));
  const plan = await call('import_prepare', {key:key(), files:[input], mapping_version:'portable-v1', cutoff:timestamp});
  const metrics = Object.fromEntries(plan.counts.map(m=>[m.label,m.value]));
  assert.deepEqual([metrics.input_rows,metrics.duplicates,metrics.identities,metrics.active,metrics.unsubscribed], [22,1,21,20,1]);
  const job = await call('import_commit',{key:key(),plan_id:plan.id,plan_digest:plan.digest});
  assert.equal(job.state,'running');
  const status = await call('migration_status');
  assert.equal(status.checks[0].state,'warning');
  const db = await mf.getD1Database('DB');
  const held = await db.prepare("SELECT body FROM newsman_records WHERE kind='subscriber' AND json_extract(body,'$.email')=?").bind('member-20@example.test').first();
  assert.equal(JSON.parse(held.body).status, 'unsubscribed', 'The first batch must process suppressions before positives');
  await (await mf.getWorker()).scheduled();
  assert.equal((await call('job_get',{id:job.id})).state,'succeeded');
  assert.equal((await call('migration_status')).eligible_digest,plan.eligible_digest);

  const sub=(await call('subscribers_list',{search:'member-01@example.test'})).items[0];
  const saved=await call('subscriber_tags_set',{key:key(),id:sub.id,expected_version:sub.version,tags:['attendee']});
  const filter={tags_all:['attendee'],statuses:['active']};
  const segment=await call('segment_create',{key:key(),name:'Attendees',filter});
  assert.equal((await call('segment_preview',{filter})).items[0].id,sub.id);
  assert.equal((await call('segments_list')).items[0].id,segment.id);
  await assert.rejects(call('subscriber_suppress',{key:key(),id:sub.id,expected_version:sub.version,reason:'Hold'}),/revision_conflict/);
  const suppressed=await call('subscriber_suppress',{key:key(),id:sub.id,expected_version:saved.version,reason:'Hold'});
  assert.equal(suppressed.status,'suppressed');

  const stale=await upload(call,'email,status,confirmed_at\nmember-01@example.test,active,'+timestamp);
  const later=await call('import_prepare',{key:key(),files:[stale],mapping_version:'portable-v1',cutoff:timestamp});
  await call('import_commit',{key:key(),plan_id:later.id,plan_digest:later.digest});
  assert.equal((await call('subscriber_get',{id:sub.id})).status,'suppressed');
  const reconciliation=await call('migration_reconcile',{key:key(),files:[stale],baseline_id:plan.id,watermark:timestamp});
  assert.equal(reconciliation.counts.find(x=>x.label==='discrepancies').value,1);
  const result=await call('file_read',{id:reconciliation.result_asset_id,offset:0});
  assert.equal(JSON.parse(Buffer.from(result.base64,'base64').toString()).destination,'suppressed');
});

test('reader self-service is scanner safe and erasure leaves only an address tombstone',async t=>{
  const {mf,origin,close}=await startRuntime();t.after(close);
  const call=await webClient(origin);
  const email='optin@example.test';
  const post=path=>fetch(origin+path,{method:'POST',body:new URLSearchParams({email}),redirect:'manual',headers:{Origin:origin}});
  assert.equal((await post('/api/newsletter/subscribe')).status,303);
  assert.equal((await post('/api/newsletter/resend')).status,303);
  const db=await mf.getD1Database('DB');
  const outbox=await db.prepare("SELECT body FROM newsman_records WHERE kind='mail_queue'").all();
  assert.equal(outbox.results.length,1);
  const token=JSON.parse(outbox.results[0].body).text.match(/\/confirm\/(\S+)/)[1];
  assert.equal((await fetch(origin+'/confirm/'+token)).status,200);
  let sub=(await call('subscribers_list',{search:email})).items[0];
  assert.equal(sub.status,'pending');
  assert.equal((await post('/confirm/'+token)).status,303);
  assert.equal((await post('/confirm/'+token)).status,303);
  sub=await call('subscriber_get',{id:sub.id});
  assert.equal(sub.status,'active');
  assert.equal((await call('subscriber_history',{id:sub.id})).items.filter(x=>x.kind==='first_confirmed').length,1);
  await call('subscriber_erase',{key:key(),id:sub.id,expected_version:sub.version,reason:'Recipient request'});
  assert.equal((await post('/api/newsletter/subscribe')).status,303);
  assert.equal((await call('subscribers_list',{search:email})).items.length,0);
  const tomb=await call('subscriber_get',{id:sub.id});
  assert.equal(tomb.status,'erased');assert.equal(tomb.email,undefined);
  assert.equal((await call('subscriber_history',{id:sub.id})).items.length,0);
  assert.equal((await db.prepare("SELECT count(*) n FROM newsman_records WHERE kind='mail_queue'").first()).n,0);
});

test('publisher signup, one 48-hour reminder, scanner-safe first confirmation and legacy opt-out', async t => {
  const secret = 'publication-integration-local-fixture-only';
  const {mf, origin, close} = await startRuntime({bindings:{PUBLISHER_INTEGRATION_KEY:secret, CONFIRMATION_REMINDER_ENABLED:'true'}});
  t.after(close);
  const db = await mf.getD1Database('DB');
  const service = async (path, body, credential = secret) => fetch(origin+'/integrations/publisher/'+path, {
    method:body ? 'POST' : 'GET', headers:{Authorization:'Bearer '+credential,'Content-Type':'application/json'},
    body:body ? JSON.stringify(body) : undefined
  });
  const email='reminder@example.test';
  const input={email,signup_id:key(),campaign:{utm_campaign:'music'}};
  assert.equal((await service('events', undefined, 'unrelated')).status,403);
  const send = async v => (await service('subscribe',v)).json();
  const first = await send(input);
  assert.ok(first.receipt.id);
  assert.deepEqual(await send(input),first);
  assert.equal((await send({...input,signup_id:key()})).receipt,null);
  const subscriber = async () => JSON.parse((await db.prepare("SELECT body FROM newsman_records WHERE kind='subscriber' AND id=?").bind(first.receipt.id).first()).body);
  const messages = async () => (await db.prepare("SELECT body FROM newsman_records WHERE kind='mail_queue' AND json_extract(body,'$.subscriber_id')=? ORDER BY json_extract(body,'$.created_at')").bind(first.receipt.id).all()).results.map(row=>JSON.parse(row.body));
  const tick = async () => (await mf.getWorker()).scheduled();
  const before=await subscriber();
  assert.equal(before.reminder_due_s-before.last_challenge_at,48*3600);
  await tick();
  assert.equal((await messages()).length,1);
  const due = () => db.prepare("UPDATE newsman_records SET body=json_set(body,'$.reminder_due_s',?) WHERE kind='subscriber' AND id=?").bind(Math.floor(Date.now()/1000)-1, first.receipt.id).run();
  await due(); await tick(); await tick();
  const mail=await messages();
  assert.equal(mail.length,2);
  assert.match(mail[1].subject,/Reminder/);
  assert.notEqual(mail[1].nonce,mail[0].nonce);
  const fresh=await subscriber();
  assert.equal(fresh.challenge_until-fresh.last_challenge_at,86400);
  const token=mail[1].text.match(/\/confirm\/(\S+)/)[1];
  const link=origin+'/confirm/'+token;
  assert.equal((await fetch(link)).status,200);
  assert.equal((await subscriber()).status,'pending');
  const confirm=()=>fetch(link,{method:'POST',headers:{Origin:origin},redirect:'manual'});
  assert.equal((await confirm()).status,303); assert.equal((await confirm()).status,303);
  const events=(await (await service('events')).json()).items;
  assert.deepEqual(events.map(e=>e.type).sort(),['accepted','confirmed']);
  assert.equal(events[0].campaign.utm_campaign,'music');
  assert.ok(events.every(e=>e.subscriber===first.receipt.id && e.signup_id===input.signup_id));
  await service('ack',{ids:events.map(e=>e.id)});
  assert.equal((await (await service('events')).json()).items.length,0);
  assert.equal((await subscriber()).reminder_due_s,undefined);
  const at=Math.floor(Date.now()/1000);
  await service('opt-out',{email,at:at-3600});
  assert.equal((await subscriber()).status,'active');
  await service('opt-out',{email,at});
  assert.equal((await subscriber()).status,'unsubscribed');
  await due(); await tick();
  assert.equal((await messages()).length,2);
});
