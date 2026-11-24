import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import test from 'node:test';
import { startRuntime, webClient } from '../scripts/runtime.mjs';

const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const command = (call, name, args) => call(name, { key: randomUUID(), ...args });

async function upload(call, kind, bytes) {
  const digest = sha(bytes);
  const opened = await command(call, 'file_begin', {kind, size: bytes.length, sha256: digest});
  await command(call, 'file_write', {
    id: opened.id, offset: 0, chunk_sha256: digest, base64: bytes.toString('base64'),
  });
  return command(call, 'file_finish', {id: opened.id});
}

test('messages freeze template revisions, restore drafts and keep a website optional', async t => {
  const runtime = await startRuntime();
  t.after(() => runtime.close());
  const call = await webClient(runtime.origin);
  const template = await command(call, 'template_create', {
    name:'Simple notice', html:'<header>From the desk</header><p>{{body}}</p>', plain_text:'From the desk\n{{body}}',
  });
  const draft = {
    title:'Maintenance complete', subject:'Service restored', preheader:'Status update',
    template_id:template.id, html:'<p>Normal service has resumed.</p>',
    plain_text:'Normal service has resumed.',
  };
  const first = await command(call, 'issue_create', {draft});
  assert.equal(first.state, 'draft');
  assert.equal((await call('issues_list',{search:'service'})).items[0].id, first.id);
  const original = await call('issue_render', {id:first.id,version:first.version});
  assert.match(original.html,/From the desk/);
  await command(call,'template_update',{id:template.id,expected_version:template.version,name:'New notice',html:'<div>Updated heading</div>{{body}}',plain_text:'Updated heading\n{{body}}'});
  const frozen=await call('issue_render',{id:first.id,version:first.version});
  assert.equal(frozen.digest, original.digest);
  const second=await command(call,'issue_update',{id:first.id,expected_version:first.version,draft:{...draft,title:'Updated status'}});
  assert.match((await call('issue_render',{id:first.id,version:second.version})).plain_text,/Updated heading/);
  await assert.rejects(command(call,'issue_update',{id:first.id,expected_version:first.version,draft}),/revision_conflict/);
  const restored=await command(call,'issue_restore',{id:first.id,expected_version:second.version,restore_version:first.version});
  assert.equal(restored.draft.title, draft.title);
  assert.equal((await call('issue_render',{id:first.id,version:restored.version})).digest,original.digest);
  const versions=await call('issue_versions',{id:first.id});
  assert.deepEqual(versions.items.map(r=>r.version),[1,2,3]);
  assert.deepEqual(await call('issue_version_get',{id:first.id,version:first.version}),first);
  await assert.rejects(command(call,'archive_verify',{id:first.id,expected_version:restored.version}),/archive URL/);
  const job=await command(call,'archive_bundle',{id:first.id,version:restored.version});
  const blob=await call('file_read',{id:job.result_asset_id,offset:0});
  const bytes=Buffer.from(blob.base64,'base64');
  assert.equal(sha(bytes),blob.sha256);
  assert.equal(JSON.parse(bytes).plain_text,draft.plain_text);
});

test('generic import deduplicates a source key and signed images serve immutable bytes',async t=>{
  const runtime=await startRuntime();
  t.after(()=>runtime.close());
  const call=await webClient(runtime.origin);
  const image=Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a+RoAAAAASUVORK5CYII=','base64');
  const file=await upload(call,'hero',image);
  const source={external_key:'recall-2027',title:'Part recall',summary:'Recall notice',html:'<p>Return the part.</p>',hero_asset_id:file.id,hero_alt:'Part diagram'};
  async function importSource(record,expectedVersion) {
    const contents=Buffer.from(JSON.stringify({format_version:1,source:record}));
    const sealed=await upload(call,'issue_bundle',contents);
    return command(call,'issue_import',{
      asset_id:sealed.id,source_digest:sha(contents),external_key:record.external_key,
      ...(expectedVersion ? {expected_version:expectedVersion} : {}),
    });
  }
  const initial=await importSource(source);
  assert.equal(initial.draft.plain_text,'Return the part.');
  assert.deepEqual(await importSource(source),initial);
  await assert.rejects(importSource({...source,title:'Updated recall'}),/revision_conflict/);
  const updated=await importSource({...source,title:'Updated recall'},initial.version);
  assert.equal(updated.id,initial.id);
  assert.equal(updated.version,2);
  const rendered=await call('issue_render',{id:initial.id,version:initial.version});
  const url=new URL(rendered.html.match(/src="([^"]+)"/)[1]);
  assert.equal(url.origin,runtime.origin);
  const response=await fetch(url);
  assert.equal(response.headers.get('Content-Type'),'image/png');
  assert.deepEqual(Buffer.from(await response.arrayBuffer()),image);
  url.pathname=url.pathname.slice(0,url.pathname.lastIndexOf('/')+1)+'invalid-capability';
  assert.equal((await fetch(url)).status,404);
});

test('recent messages stay newest-first across an intervening edit and creation', async t => {
  const runtime = await startRuntime();
  t.after(() => runtime.close());
  const call = await webClient(runtime.origin);
  const draft = title => ({
    title, subject: title, preheader: '', html: '<p>Details.</p>', plain_text: 'Details.',
  });
  const first = await command(call, 'issue_create', {draft: draft('First')});
  const second = await command(call, 'issue_create', {draft: draft('Second')});
  const all = await call('issues_list', {});
  assert.deepEqual(all.items.map(row => row.id), [second.id, first.id]);
  const page = await call('issues_list', {limit: 1});
  assert.equal(page.items[0].id, second.id);
  assert.ok(page.next_cursor);
  await command(call, 'issue_update', {
    id: first.id, expected_version: first.version, draft: draft('Edited first'),
  });
  const third = await command(call, 'issue_create', {draft: draft('Third')});
  const following = await call('issues_list', {limit: 1, cursor: page.next_cursor});
  assert.deepEqual(following.items, [all.items[1]]);
  assert.equal(following.next_cursor, undefined);
  await assert.rejects(call('issues_list', {cursor: page.next_cursor, search: 'third'}), /Invalid cursor/);
  assert.deepEqual((await call('issues_list', {})).items.map(r => r.id), [third.id, first.id, second.id]);
});
