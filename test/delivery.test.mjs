import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import test from 'node:test';
import { startRuntime, webClient } from '../scripts/runtime.mjs';

const command = (call, name, args) => call(name, { key: randomUUID(), ...args });

async function insert(db, kind, document) {
  await db.prepare('INSERT INTO newsman_records(kind,id,body) VALUES (?,?,?)')
    .bind(kind, document.id, JSON.stringify(document)).run();
}

async function wake(mf) {
  const worker = await mf.getWorker();
  assert.equal((await worker.scheduled({ cron: '* * * * *', scheduledTime: new Date() })).outcome, 'ok');
}

async function captures(db) {
  return (await db.prepare("SELECT body FROM newsman_records WHERE kind='capture'").all())
    .results.map(row => JSON.parse(row.body));
}

test('an interrupted submission stays unknown across scheduler retries and has an auditable resolution', async t => {
  const runtime = await startRuntime();
  t.after(() => runtime.close());
  const db = await runtime.mf.getD1Database('DB');
  const call = await webClient(runtime.origin);
  const id = 'campaign_for_recovery';
  await insert(db, 'campaign', {
    id, issue_id: 'issue_1', version: 1, issue_digest: 'a'.repeat(64),
    state: 'sending', due_at: '2026-01-01T08:00:00Z', total: 1, cursor: 1,
  });
  await insert(db, 'attempt', {
    id: 'attempt_for_recovery', campaign_id: id, subscriber_id: 'sub_1', kind: 'campaign',
    state: 'submitting', version: 1, tries: 1, lease_until: 1, submission_id: 'interrupted',
  });
  await wake(runtime.mf);
  await wake(runtime.mf);
  const [unknown] = (await call('attempts_list', { campaign_id: id })).items;
  assert.equal(unknown.state, 'unknown');
  assert.equal((await call('campaign_get', { id })).state, 'needs_review');
  const resolved = await command(call, 'attempt_resolve', {
    id: unknown.id, expected_version: unknown.version, outcome: 'accepted_with_evidence',
    provider_id: 'verified-provider-id', evidence_note: 'Matched the provider delivery record and recipient.',
  });
  assert.equal(resolved.state, 'accepted');
  assert.equal((await call('campaign_get', { id })).state, 'finished');
  await wake(runtime.mf);
  assert.deepEqual(await captures(db), []);
});

test('suppression at dispatch, overlapping campaigns and HTTP link tracking honor the frozen issue', async t => {
  const runtime = await startRuntime();
  t.after(() => runtime.close());
  const call = await webClient(runtime.origin);
  const db = await runtime.mf.getD1Database('DB');
  const original = await call('settings_get');
  await command(call, 'settings_update', {
    expected_version: original.version,
    settings: {...original.settings, postal_footer: 'Synthetic publication address', click_tracking: true, send_budget: 50},
  });
  const current = await call('health_get');
  await command(call, 'dispatch_set', { expected_version: current.version, paused: false, reason: 'Synthetic capture exercise' });
  for (const id of ['active','optout']) {
    await insert(db, 'subscriber', {id, email: id+'@example.test',status:'active',version:1,first_touch:{},tags:[],consent_epoch:1});
  }
  const issue = await command(call, 'issue_create', {draft:{
    title:'Exhibit',subject:'Your invitation',preheader:'Open today',
    html:'<p><a href="https://example.org/exhibit?a=1&amp;b=2">Visit</a></p>',plain_text:'https://example.org/exhibit?a=1&b=2',
  }});
  const campaigns=[];
  for(let n=0;n<2;n++){
    const plan=await command(call,'send_prepare',{issue_id:issue.id,issue_version:issue.version,audience:{}});
    assert.equal(plan.cost.estimated_messages,2);
    campaigns.push(await command(call,'send_commit',{plan_id:plan.id,plan_digest:plan.digest}));
  }
  await command(call,'subscriber_suppress',{id:'optout',expected_version:1,reason:'Reader requested unsubscribe'});
  await wake(runtime.mf);
  await wake(runtime.mf);
  const mail=await captures(db);
  assert.equal(mail.length,1);
  assert.equal(mail[0].to,'active@example.test');
  const track=new URL(mail[0].text.match(/https?:\/\/[^\s]+\/r\/[^\s]+/)[0]);
  assert.equal(track.origin,runtime.origin);
  const click=await fetch(track,{redirect:'manual'});
  assert.equal(click.status,302);
  assert.equal(click.headers.get('Location'),'https://example.org/exhibit?a=1&b=2');
  for(const campaign of campaigns){
    assert.equal((await call('campaign_get',{id:campaign.id})).state,'finished');
  }
});

test('erasing a pending confirmation does not block later outbox jobs', async t => {
  const runtime = await startRuntime();
  t.after(() => runtime.close());
  const db = await runtime.mf.getD1Database('DB');
  await insert(db, 'attempt', {
    id: 'attempt_orphan', queue_id: 'deleted_payload', subscriber_id: 'deleted_reader',
    kind:'confirmation', state:'queued', tries:0, version:1,
  });
  const response = await fetch(runtime.origin+'/api/newsletter/subscribe', {
    method: 'POST', headers: {Origin:runtime.origin,'Content-Type':'application/x-www-form-urlencoded'},
    body: 'email=second%40example.test', redirect:'manual',
  });
  assert.equal(response.status,303);
  await wake(runtime.mf);
  assert.deepEqual((await captures(db)).map(item=>item.to),['second@example.test']);
  const orphan=await db.prepare("SELECT body FROM newsman_records WHERE kind='attempt' AND id='attempt_orphan'").first();
  assert.equal(JSON.parse(orphan.body).state,'skipped');
});

test('a proof for a confirmed seed includes a working one-click unsubscribe while campaigns are paused', async t => {
  const {origin,mf,close}=await startRuntime();
  t.after(close);
  const db=await mf.getD1Database('DB'), call=await webClient(origin);
  const email='seed@publisher.test';
  const saved=await call('settings_get');
  await command(call,'settings_update',{expected_version:saved.version,settings:{...saved.settings,test_recipients:[email],postal_footer:'Publisher test footer'}});
  await fetch(origin+'/api/newsletter/subscribe',{method:'POST',headers:{Origin:origin},body:new URLSearchParams({email})});
  await wake(mf);
  const [confirmation]=await captures(db);
  const link=confirmation.text.match(/https?:\/\/[^\s]+\/confirm\/[^\s]+/)[0];
  assert.equal((await fetch(link,{method:'POST',redirect:'manual'})).status,303);
  const issue=await command(call,'issue_create',{draft:{title:'Seed',subject:'Controlled proof',preheader:'',html:'<p>Seed</p>',plain_text:'Seed'}});
  await command(call,'send_test',{issue_id:issue.id,issue_version:issue.version,recipient:email});
  await wake(mf);
  const message=(await captures(db)).find(row=>row.subject==='[Test] Controlled proof');
  assert.ok(message.unsubscribe);
  assert.ok(message.html.includes(message.unsubscribe) && message.text.includes(message.unsubscribe));
  assert.equal((await fetch(message.unsubscribe,{method:'POST',body:new URLSearchParams({'List-Unsubscribe':'One-Click'})})).status,200);
  const status=await db.prepare("SELECT json_extract(body,'$.status') AS status FROM newsman_records WHERE kind='subscriber' AND json_extract(body,'$.email')=?").bind(email).first('status');
  assert.equal(status,'unsubscribed');
  assert.equal((await call('health_get')).paused,true);
});
