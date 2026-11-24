import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import { startRuntime } from '../scripts/runtime.mjs';

const contract = JSON.parse(await readFile(new URL('../spec/operations.json', import.meta.url)));
const callback = 'http://127.0.0.1:9876/callback';
const clientId = 'newsman-local';
const fullScope = 'read write send admin offline_access';
const sha = bytes => createHash('sha256').update(bytes).digest('hex');

async function workspace(t, bindings = {}) {
  const runtime = await startRuntime({ bindings, persist: false });
  t.after(() => runtime.close());
  return runtime;
}

async function json(response, status = 200) {
  const body = await response.text();
  assert.equal(response.status, status, 'Unexpected HTTP status: ' + body.slice(0, 650));
  return JSON.parse(body);
}

function form(origin, path, data) {
  return fetch(new URL(path, origin), {
    method: 'POST', redirect: 'manual', headers: { Origin: origin },
    body: new URLSearchParams(data),
  });
}

async function webSession(origin) {
  const session = await json(await fetch(origin + '/api/session'));
  assert.ok(session.csrf);
  return (name, args = {}) => fetch(origin + '/api/ops/' + name, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Origin: origin, 'X-CSRF-Token': session.csrf },
    body: JSON.stringify(args),
  });
}

async function authorizationCode(origin, scope) {
  const verifier = randomBytes(48).toString('base64url');
  const state = randomBytes(16).toString('hex');
  const challenge = createHash('sha256').update(verifier).digest('base64url');
  const resource = origin + '/mcp';
  const params = {
    client_id: clientId, redirect_uri: callback, response_type: 'code', state,
    resource, scope, code_challenge: challenge, code_challenge_method: 'S256',
  };
  const page = await fetch(origin + '/oauth/authorize?' + new URLSearchParams(params));
  const html = await page.text();
  assert.equal(page.status, 200, html.slice(0, 300));
  const field = name => {
    const match = html.match(new RegExp('name="' + name + '" value="([^"]+)"'));
    assert.ok(match, 'Missing OAuth consent field ' + name);
    return match[1];
  };
  const consent = await form(origin, '/oauth/authorize', {
    decision: 'approve', handle: field('handle'), csrf: field('csrf'),
  });
  assert.equal(consent.status, 303);
  const result = new URL(consent.headers.get('location'));
  assert.equal(result.origin + result.pathname, callback);
  assert.equal(result.searchParams.get('state'), state);
  const code = result.searchParams.get('code');
  assert.ok(code);
  return {
    client_id: clientId, grant_type: 'authorization_code', redirect_uri: callback,
    code_verifier: verifier, resource, code,
  };
}

async function token(origin, scope = fullScope) {
  const code = await authorizationCode(origin, scope);
  return json(await form(origin, '/oauth/token', code));
}

async function mcp(t, origin, bearer, path = '/mcp') {
  const client = new Client({ name: 'newsman-integration-test', version: '1.0.0' });
  const headers = bearer ? { Authorization: 'Bearer ' + bearer } : {};
  const transport = new StreamableHTTPClientTransport(new URL(path, origin), { requestInit: { headers } });
  await client.connect(transport);
  t.after(() => client.close());
  return client;
}

async function toolsFor(client) {
  const tools = [];
  let cursor;
  do {
    const page = await client.listTools(cursor ? { cursor } : {});
    tools.push(...page.tools);
    cursor = page.nextCursor;
  } while (cursor);
  return tools;
}

async function call(client, name, args = {}) {
  const response = await client.callTool({ name, arguments: args });
  assert.ok(!response.isError, name + ': ' + JSON.stringify(response.content));
  assert.deepEqual(JSON.parse(response.content[0].text), response.structuredContent);
  return response.structuredContent;
}

function rpc(origin, method, params = {}, access, path = '/mcp') {
  const headers = {
    'Content-Type': 'application/json', Accept: 'application/json, text/event-stream',
    'MCP-Protocol-Version': contract.protocol_baseline,
  };
  if (access) headers.Authorization = 'Bearer ' + access;
  return fetch(origin + path, {
    method: 'POST', headers, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  });
}

// Resolve the emitted schemas independently of the server's schema bundler.
function expanded(schema, defs) {
  if (schema.$ref) return expanded(defs[schema.$ref.split('/').at(-1)], defs);
  if (Array.isArray(schema)) return schema.map(item => typeof item === 'object' ? expanded(item, defs) : item);
  return Object.fromEntries(Object.entries(schema)
    .filter(([key]) => key !== '$defs')
    .map(([key, value]) => [key, value && typeof value === 'object' ? expanded(value, defs) : value]));
}

function checkParity(catalog, tools, surface) {
  const expected = catalog.operations.filter(op => op.surface === surface);
  assert.deepEqual(tools.map(x => x.name).sort(), expected.map(x => x.name).sort());
  for (const tool of tools) {
    const op = expected.find(o => o.name === tool.name);
    for (const field of ['inputSchema', 'outputSchema']) {
      assert.equal(tool[field].type, 'object');
      assert.deepEqual(expanded(tool[field], tool[field].$defs), expanded(op[field], catalog.$defs),
        surface + '/' + op.name + '/' + field);
    }
  }
}

test('the real HTTP MCP server and console expose the same operator and reader contracts', async t => {
  const { origin } = await workspace(t);
  const noToken = await rpc(origin, 'tools/list');
  assert.equal(noToken.status, 401);
  const metadataUrl = /resource_metadata="([^"]+)"/.exec(noToken.headers.get('www-authenticate'))[1];
  const protectedResource = await json(await fetch(metadataUrl));
  assert.equal(protectedResource.resource, origin + '/mcp');
  assert.deepEqual(protectedResource.authorization_servers, [origin]);
  const server = await json(await fetch(origin + '/.well-known/oauth-authorization-server'));
  assert.ok(server.code_challenge_methods_supported.includes('S256'));
  assert.equal(server.token_endpoint, origin + '/oauth/token');

  const granted = await token(origin);
  const operator = await mcp(t, origin, granted.access_token);
  const reader = await mcp(t, origin, null, '/reader/mcp');
  const web = await json(await fetch(origin + '/api/registry'));
  assert.deepEqual(web, contract);
  const operatorTools = await toolsFor(operator);
  const readerTools = await toolsFor(reader);
  assert.equal(operatorTools.length, 59);
  assert.equal(readerTools.length, 5);
  checkParity(web, operatorTools, 'operator');
  checkParity(web, readerTools, 'reader');
  const drifted = { ...web, operations: [...web.operations, { name: 'test_only_web_action', surface: 'operator' }] };
  assert.throws(() => checkParity(drifted, operatorTools, 'operator'));
  const info = await call(reader, 'reader_info');
  assert.ok(info.public_origin);
  assert.ok(info.publication_name);
  assert.equal(info.archive_url, undefined);
  const denied = await rpc(origin, 'tools/call', { name: 'subscribers_list', arguments: {} }, null, '/reader/mcp');
  const result = await denied.json();
  assert.ok(result.error || result.result?.isError);
});

test('PKCE, scopes, operator allowlisting and revocation are enforced over HTTP', async t => {
  const { origin } = await workspace(t);
  const web = await webSession(origin);
  const code = await authorizationCode(origin, 'read offline_access');
  const wrongVerifier = { ...code, code_verifier: randomBytes(48).toString('base64url') };
  assert.equal((await json(await form(origin, '/oauth/token', wrongVerifier), 400)).error, 'invalid_grant');
  assert.equal((await json(await form(origin, '/oauth/token', { ...code, resource: origin + '/other' }), 400)).error, 'invalid_target');
  const grant = await json(await form(origin, '/oauth/token', code));
  assert.equal((await json(await form(origin, '/oauth/token', code), 400)).error, 'invalid_grant');
  const readOnly = await mcp(t, origin, grant.access_token);
  assert.deepEqual((await toolsFor(readOnly)).map(t => t.name).sort(),
    contract.operations.filter(op => op.scope === 'read').map(op => op.name).sort());
  const refused = await rpc(origin, 'tools/call', { name: 'send_test', arguments: {} }, grant.access_token);
  assert.equal(refused.status, 403);
  assert.match(refused.headers.get('www-authenticate'), /insufficient_scope/);
  const refresh = { client_id: clientId, grant_type: 'refresh_token', resource: origin + '/mcp', refresh_token: grant.refresh_token };
  const rotated = await json(await form(origin, '/oauth/token', refresh));
  assert.notEqual(rotated.refresh_token, grant.refresh_token);

  const invited = await json(await web('operators_add', { key: 'invite-test-operator', email: 'editor@example.test' }));
  const owner = (await json(await web('operators_list'))).items.find(row => row.state === 'active');
  assert.ok(owner);
  assert.equal((await web('operators_remove', { key: 'keep-bound-owner', id: owner.id, expected_version: owner.version })).status, 409);
  await json(await web('operators_remove', { key: 'remove-test-editor', id: invited.id, expected_version: invited.version }));
  const members = (await json(await web('operators_list'))).items;
  assert.equal(members.find(member => member.id === invited.id).state, 'removed');
  const [record] = (await json(await web('grants_list'))).items;
  await json(await web('grants_revoke', { key: 'revoke-read-client', id: record.id, expected_version: record.version }));
  assert.equal((await rpc(origin, 'tools/list', {}, rotated.access_token)).status, 401);
  assert.equal((await json(await form(origin, '/oauth/token', { ...refresh, refresh_token: rotated.refresh_token }), 400)).error, 'invalid_grant');

  const second = await token(origin, 'read offline_access');
  const renewal = { ...refresh, refresh_token: second.refresh_token };
  const renewed = await json(await form(origin, '/oauth/token', renewal));
  assert.equal((await json(await form(origin, '/oauth/token', renewal), 400)).error, 'invalid_grant');
  assert.equal((await rpc(origin, 'tools/list', {}, renewed.access_token)).status, 401);
  assert.equal((await json(await form(origin, '/oauth/token', { ...refresh, refresh_token: renewed.refresh_token }), 400)).error, 'invalid_grant');

  const other = await workspace(t, {
    BOOTSTRAP_OPERATORS: 'the-owner@example.test', LOCAL_OPERATOR_EMAIL: 'different-person@example.test',
  });
  assert.equal((await fetch(other.origin + '/api/session')).status, 403);
});

async function capturedMail(mf) {
  const db = await mf.getD1Database('DB');
  const { results } = await db.prepare("SELECT body FROM newsman_records WHERE kind='capture'").all();
  return results.map(row => JSON.parse(row.body));
}

// Workerd can return from the scheduled entrypoint while waitUntil is finishing.
// Observe the durable sink, then use a second scheduler invocation to test redelivery.
async function waitForMail(mf, predicate) {
  for (let poll = 0; poll < 60; poll++) {
    const matching = (await capturedMail(mf)).filter(predicate);
    if (matching.length) return matching;
    if (poll === 20) await wake(mf);
    await delay(50);
  }
  assert.fail('Mail did not reach the local capture transport');
}

async function wake(mf) {
  const service = await mf.getWorker();
  const result = await service.scheduled({ cron: '* * * * *', scheduledTime: new Date() });
  assert.equal(result.outcome, 'ok');
}

test('a one-off message goes from opt-in to capture once across the web console and MCP', async t => {
  const { origin, mf } = await workspace(t);
  const web = await webSession(origin);
  const op = await mcp(t, origin, (await token(origin)).access_token);
  const reader = await mcp(t, origin, null, '/reader/mcp');
  const email = 'attendee@example.test';
  const original = await json(await web('settings_get'));
  const settings = { ...original.settings, public_origin: origin, from_address: 'sender@example.test',
    reply_to: 'reply@example.test', postal_footer: 'Test Exhibits, 123 Example Road',
    test_recipients: ['proof@example.test'] };
  await json(await web('settings_update', { key: 'configure-workspace', expected_version: original.version, settings }));
  const health = await json(await web('health_get'));
  await call(op, 'dispatch_set', { key: 'enable-capture-only', expected_version: health.version, paused: false, reason: 'Local capture test' });

  const signup = await form(origin, '/api/newsletter/subscribe', { email, utm_campaign: 'exhibit' });
  assert.equal(signup.status, 303);
  assert.equal(signup.headers.get('location'), '/subscribe/#check-inbox');
  assert.equal((await call(reader, 'reader_subscribe', { email })).outcome, 'check_inbox');
  await wake(mf);
  const [confirmation] = await waitForMail(mf, mail => mail.to === email);
  const proofUrl = new URL(confirmation.text.match(/https?:\/\/[^\s]+\/confirm\/[^\s]+/)[0]);
  assert.equal(proofUrl.origin, origin);
  assert.equal((await fetch(proofUrl)).status, 200);
  assert.equal((await json(await web('subscribers_list', { search: email }))).items[0].status, 'pending');
  const tokenValue = proofUrl.pathname.split('/').at(-1);
  assert.equal((await call(reader, 'reader_confirm', { token: tokenValue })).outcome, 'confirmed');
  assert.equal((await call(reader, 'reader_confirm', { token: tokenValue })).outcome, 'already_handled');

  const create = { key: 'create-gallery-announcement', draft: {
    title: 'Gallery opening', subject: 'Your invitation', preheader: 'Doors open at six.',
    html: '<p>Join us for the exhibit.</p>', plain_text: 'Join us for the exhibit.',
  } };
  const draft = await json(await web('issue_create', create));
  assert.deepEqual(await call(op, 'issue_create', create), draft);
  const different = await web('issue_create', { ...create, draft: { ...create.draft, title: 'Changed title' } });
  assert.equal((await json(different, 409)).error.code, 'idempotency_conflict');
  const renderArgs = { id: draft.id, version: draft.version };
  assert.deepEqual(await call(op, 'issue_render', renderArgs), await json(await web('issue_render', renderArgs)));
  const plan = await call(op, 'send_prepare', { key: 'prepare-gallery-send', issue_id: draft.id,
    issue_version: draft.version, audience: { statuses: ['active'] } });
  assert.equal(plan.cost.estimated_messages, 1);
  const commit = { key: 'commit-gallery-send', plan_id: plan.id, plan_digest: plan.digest };
  const campaign = await json(await web('send_commit', commit));
  assert.deepEqual(await call(op, 'send_commit', commit), campaign);
  await wake(mf);
  await wake(mf);
  const deliveries = await waitForMail(mf, mail => mail.campaign_id === campaign.id);
  assert.equal(deliveries.length, 1);
  assert.equal(deliveries[0].to, email);
  assert.equal(deliveries[0].subject, 'Your invitation');
  assert.ok(deliveries[0].text.includes('Join us for the exhibit.'));
  const stop = await form(origin, new URL(deliveries[0].unsubscribe).pathname, { 'List-Unsubscribe': 'One-Click' });
  assert.ok(stop.ok);
  const contact = (await call(op, 'subscribers_list', { search: email })).items[0];
  assert.equal(contact.status, 'unsubscribed');
});

test('an import and a complete private export can move between web and MCP', async t => {
  const { origin, mf } = await workspace(t);
  const web = await webSession(origin);
  const op = await mcp(t, origin, (await token(origin)).access_token);
  const bytes = Buffer.from([
    'email,status,confirmed_at',
    'eligible@example.test,active,2025-11-01T00:00:00Z',
    'removed@example.test,active,2025-11-01T00:00:00Z',
    'removed@example.test,unsubscribed,',
    'pending@example.test,pending,',
    'uncertain@example.test,active,',
    '',
  ].join('\r\n'));
  const file = await call(op, 'file_begin', {
    key: 'begin-portable-import', kind: 'audience_import', size: bytes.length, sha256: sha(bytes),
  });
  for (const [i, chunk] of [bytes.subarray(0, 50), bytes.subarray(50)].entries()) {
    const part = { key: 'write-import-part-' + i, id: file.id, offset: i === 0 ? 0 : 50,
      base64: chunk.toString('base64'), chunk_sha256: sha(chunk) };
    const result = await json(await web('file_write', part));
    assert.deepEqual(await call(op, 'file_write', part), result);
  }
  const sealed = await call(op, 'file_finish', { key: 'seal-portable-import', id: file.id });
  assert.equal(sealed.sha256, sha(bytes));
  const prepared = await json(await web('import_prepare', {
    key: 'plan-portable-import', files: [file.id], mapping_version: 'portable-v1', cutoff: new Date().toISOString(),
  }));
  assert.equal((await json(await web('subscribers_list'))).items.length, 0);
  const args = { key: 'apply-portable-import', plan_id: prepared.id, plan_digest: prepared.digest };
  const job = await call(op, 'import_commit', args);
  assert.deepEqual(await json(await web('import_commit', args)), job);
  assert.equal((await call(op, 'job_get', { id: job.id })).state, 'succeeded');
  const records = (await json(await web('subscribers_list'))).items;
  assert.deepEqual(Object.fromEntries(records.map(r => [r.email, r.status])), {
    'eligible@example.test': 'active', 'removed@example.test': 'unsubscribed',
    'pending@example.test': 'pending', 'uncertain@example.test': 'held',
  });
  assert.deepEqual(await capturedMail(mf), []);

  const exported = await call(op, 'export_create', { key: 'download-private-audience', kind: 'subscribers', format: 'jsonl' });
  const finished = await call(op, 'job_get', { id: exported.id });
  assert.ok(finished.result_asset_id);
  const chunks = [];
  let digest;
  let at = 0;
  do {
    const chunk = await call(op, 'file_read', { id: finished.result_asset_id, offset: at, length: 128 });
    assert.equal(chunk.offset, at);
    chunks.push(Buffer.from(chunk.base64, 'base64'));
    digest ??= chunk.sha256;
    assert.equal(chunk.sha256, digest);
    if (chunk.next_offset !== undefined) assert.ok(chunk.next_offset > at);
    at = chunk.next_offset;
  } while (at !== undefined);
  const contents = Buffer.concat(chunks);
  assert.equal(sha(contents), digest);
  assert.deepEqual(contents.toString('utf8').trim().split('\n').map(l => JSON.parse(l).email).sort(), records.map(r => r.email).sort());
  const readOnly = await mcp(t, origin, (await token(origin, 'read')).access_token);
  const privateFile = await readOnly.callTool({ name: 'file_read', arguments: { id: finished.result_asset_id, offset: 0 } });
  assert.equal(privateFile.isError, true);
});
