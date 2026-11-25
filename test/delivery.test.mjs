import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { readFile } from 'node:fs/promises';
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

test('SNS can redeliver a signed notification after database contention', async t => {
  const certificate = await readFile(new URL('./fixtures/sns-certificate.pem', import.meta.url), 'utf8');
  const notice = JSON.parse(await readFile(new URL('./fixtures/sns-notification.json', import.meta.url), 'utf8'));
  const runtime = await startRuntime({
    bindings: {AWS_REGION:'us-west-2', SES_CONFIGURATION_SET:'newsman', SNS_TOPIC_ARN:notice.TopicArn},
    outboundService: request => new Response(request.url === notice.SigningCertURL ? certificate : null,
      {status: request.url === notice.SigningCertURL ? 200 : 404}),
  });
  t.after(() => runtime.close());
  const db = await runtime.mf.getD1Database('DB');
  const deliver = () => fetch(runtime.origin+'/webhooks/ses', {method:'POST', body:JSON.stringify(notice)});
  const countReceipts = () => db.prepare("SELECT COUNT(*) AS n FROM newsman_records WHERE kind='provider_receipt'").first('n');
  // Force the same SQLite error D1 returns when a competing writer advances the revision.
  await db.prepare(`CREATE TRIGGER competing_write BEFORE UPDATE OF revision ON newsman_state BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: revision >= 0'); END`).run();
  assert.equal((await deliver()).status, 503);
  assert.equal(await countReceipts(), 0);
  await db.prepare('DROP TRIGGER competing_write').run();
  const replays = await Promise.all([deliver(), deliver(), deliver()]);
  assert.deepEqual(replays.map(response => response.status), [200, 200, 200]);
  assert.equal(await countReceipts(), 1);
  assert.equal(await db.prepare('SELECT revision FROM newsman_state WHERE id=1').first('revision'), 1);
});

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

test('first opt-in queues the released weekly edition once and honors opt-out and pause', async t => {
  const runtime = await startRuntime({bindings:{WELCOME_CURRENT_ISSUE:'true'}});
  t.after(() => runtime.close());
  const {origin, mf} = runtime;
  const call = await webClient(origin);
  const db = await mf.getD1Database('DB');
  const settings = await call('settings_get');
  await command(call,'settings_update',{expected_version:settings.version,settings:{
    ...settings.settings,postal_footer:'Test publication address',click_tracking:true,
  }});
  const pause = async paused => command(call,'dispatch_set',{
    expected_version:(await call('health_get')).version, paused, reason:'Local opt-in exercise',
  });
  await pause(false);
  const at = days => new Date(Date.now()+days*86_400_000).toISOString();
  async function publish(title, edition, archive, audience) {
    const html = '<p>Full story with a <a href="https://example.org/source">primary source</a>.</p>';
    const issue = await command(call,'issue_create',{draft:{title,subject:title,preheader:'',html,
      plain_text:'Full story with a primary source: https://example.org/source',
      source:{title,summary:'',html,external_key:title,edition_at:at(edition),archive_at:at(archive)}}});
    const plan = await command(call,'send_prepare',{issue_id:issue.id,issue_version:issue.version,audience});
    await command(call,'send_commit',{plan_id:plan.id,plan_digest:plan.digest});
    return issue;
  }
  const weekly = await publish('Current edition',-2,5,{statuses:['active']});
  await publish('Future edition',1,7,{});
  await publish('Archived edition',-7,-1,{});
  await publish('Segment only',-1,3,{tags_all:['insiders']});
  await pause(true);
  async function start(email) {
    const signup = await fetch(origin+'/api/newsletter/subscribe',{
      method:'POST',body:new URLSearchParams({email}),redirect:'manual',
    });
    assert.equal(signup.status,303);
    const sub = (await call('subscribers_list',{search:email})).items[0];
    const mail = await db.prepare("SELECT json_extract(body,'$.text') AS mail FROM newsman_records WHERE kind='mail_queue' AND json_extract(body,'$.subscriber_id')=?").bind(sub.id).first('mail');
    const url = mail.match(/https?:\/\/[^\s]+\/confirm\/[^\s]+/)[0];
    assert.equal((await fetch(url)).status,200);
    return {url,id:sub.id};
  }
  const welcome = async () => (await db.prepare("SELECT body FROM newsman_records WHERE kind='attempt' AND json_extract(body,'$.kind')='welcome_issue'").all()).results.map(row=>JSON.parse(row.body));
  const yes = await start('new@example.test');
  const no = await start('unsub@example.test');
  assert.equal((await welcome()).length,0);
  const confirm = url => fetch(url,{method:'POST',redirect:'manual'});
  assert.equal((await confirm(yes.url)).headers.get('location'),'/confirmed/?issue=queued');
  assert.equal((await confirm(yes.url)).headers.get('location'),'/confirmed/');
  await confirm(no.url);
  const attempts = await welcome();
  assert.equal(attempts.length,2);
  assert.ok(attempts.every(a=>a.issue_id === weekly.id));
  const sub = await call('subscriber_get',{id:no.id});
  await command(call,'subscriber_suppress',{id:no.id,expected_version:sub.version,reason:'Reader requested opt-out'});
  await wake(mf);
  assert.ok((await welcome()).every(a=>a.state==='queued'));
  await pause(false);
  await wake(mf);
  await wake(mf);
  const weeklyMails = async () => (await captures(db)).filter(m=>m.subject==='Current edition');
  const mails = await weeklyMails();
  assert.equal(mails.length,1);
  assert.equal(mails[0].to,'new@example.test');
  assert.match(mails[0].html,/Full story/);
  assert.match(mails[0].text,/unsubscribe/i);
  assert.match(mails[0].html,/Test publication address/);
  const plan = await command(call,'send_prepare',{issue_id:weekly.id,issue_version:weekly.version,audience:{}});
  await command(call,'send_commit',{plan_id:plan.id,plan_digest:plan.digest});
  await wake(mf);
  assert.equal((await weeklyMails()).length,1);
  const counts = await call('report_get',{kind:'edition',issue_id:weekly.id,from:at(-1),to:at(1)});
  assert.equal(counts.items.find(m=>m.label==='accepted').value,1);
});

test('migration pin requires prior delivery evidence and uses the specified private revision', async t => {
  const id = 'issue_previous';
  const {mf,origin,close} = await startRuntime({bindings:{WELCOME_CURRENT_ISSUE:'true',WELCOME_SEED_ISSUE:id+'@1'}});
  t.after(close);
  const db = await mf.getD1Database('DB');
  const call = await webClient(origin);
  const settings = await call('settings_get');
  await command(call,'settings_update',{expected_version:settings.version,settings:{
    ...settings.settings,postal_footer:'Fixture publication address',
  }});
  const at = d => new Date(Date.now()+d*86_400_000).toISOString();
  const source = {external_key:'edition-05',title:'Original edition',summary:'',html:'<p>The original private article.</p>',edition_at:at(-2),archive_at:at(5)};
  const draft = {title:source.title,subject:source.title,preheader:'',html:source.html,plain_text:'The original private article.',source};
  await insert(db,'issue',{id,version:2,state:'draft',draft:{...draft,subject:'Later edit'}});
  await db.prepare("INSERT INTO newsman_records(kind,id,body) VALUES ('issue_revision',?,?)")
    .bind(id+':0000000000000001',JSON.stringify({id,version:1,state:'draft',draft})).run();
  async function join(email) {
    await fetch(origin+'/api/newsletter/subscribe',{method:'POST',body:new URLSearchParams({email})});
    const row = await db.prepare("SELECT json_extract(m.body,'$.text') message FROM newsman_records m JOIN newsman_records s ON s.kind='subscriber' AND s.id=json_extract(m.body,'$.subscriber_id') WHERE m.kind='mail_queue' AND json_extract(s.body,'$.email')=?").bind(email).first();
    const proof = row.message.match(/https?:\/\/[^\s]+\/confirm\/[^\s]+/)[0];
    return (await fetch(proof,{method:'POST',redirect:'manual'})).headers.get('location');
  }
  assert.equal(await join('before@example.test'),'/confirmed/');
  await insert(db,'historical_send',{id:'old-provider',external_key:'edition-05',status:'sent'});
  assert.equal(await join('after@example.test'),'/confirmed/?issue=queued');
  const health = await call('health_get');
  await command(call,'dispatch_set',{expected_version:health.version,paused:false,reason:'Check private migration snapshot'});
  await wake(mf);
  const delivered = (await captures(db)).filter(m=>m.subject==='Original edition');
  assert.equal(delivered.length,1);
  assert.equal(delivered[0].to,'after@example.test');
  assert.match(delivered[0].html,/The original private article/);
  await assert.rejects(command(call,'send_prepare',{issue_id:id,issue_version:2,audience:{}}),/already sent by the previous publisher/);
});
